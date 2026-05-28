#!/usr/bin/env python3
"""Ethereum-like network checkpoint/restore PoC.

This is not an Ethereum protocol implementation. It models a bootnode and
multiple peers that maintain persistent TCP neighbor sessions and UDP
discovery-like traffic, which is closer to a testnet topology than the simple
single-flow fullnet smoke test.
"""

from __future__ import annotations

import json
import os
import selectors
import socket
import sys
import time
from dataclasses import dataclass, field


def _env_int(key: str, default: int) -> int:
    raw = os.environ.get(key)
    if raw is None:
        return default
    try:
        return int(raw)
    except ValueError:
        return default


def emit(role: str, event: str, **fields: object) -> None:
    payload: dict[str, object] = {
        "role": role,
        "event": event,
        "mono_ns": time.monotonic_ns(),
    }
    payload.update(fields)
    print(f"NETLOG {json.dumps(payload, sort_keys=True)}", flush=True)


@dataclass(frozen=True)
class Target:
    name: str
    host: str
    port: int


@dataclass
class PeerConn:
    sock: socket.socket
    outbound: bool
    peer_name: str | None
    connected: bool
    in_buffer: bytearray = field(default_factory=bytearray)
    out_buffer: bytearray = field(default_factory=bytearray)
    next_ping_at: float = 0.0
    next_seq: int = 0


def parse_targets(raw: str) -> list[Target]:
    targets: list[Target] = []
    for item in raw.split(","):
        item = item.strip()
        if not item:
            continue
        try:
            name, endpoint = item.split("@", 1)
            host, port_raw = endpoint.rsplit(":", 1)
            targets.append(Target(name=name, host=host, port=int(port_raw)))
        except ValueError:
            raise SystemExit(f"bad target spec: {item!r}; expected name@host:port")
    return targets


class MeshNode:
    def __init__(self) -> None:
        self.role = os.environ.get("ROLE", "").strip()
        if not self.role:
            raise SystemExit("ROLE is required")

        self.tcp_port = _env_int("TCP_PORT", 7100)
        self.udp_port = _env_int("UDP_PORT", 7200)
        self.interval_sec = max(1, _env_int("INTERVAL_SEC", 1))
        self.selector = selectors.DefaultSelector()
        self.tcp_targets = {t.name: t for t in parse_targets(os.environ.get("TCP_PEERS", ""))}
        self.udp_targets = parse_targets(os.environ.get("UDP_PEERS", ""))
        self.retry_deadline: dict[str, float] = {name: 0.0 for name in self.tcp_targets}
        self.outbound: dict[str, PeerConn] = {}
        self.inbound: dict[int, PeerConn] = {}
        self.udp_next_send_at = time.monotonic()

        self.listen_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listen_sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listen_sock.bind(("0.0.0.0", self.tcp_port))
        self.listen_sock.listen(32)
        self.listen_sock.setblocking(False)
        self.selector.register(self.listen_sock, selectors.EVENT_READ, ("listen", None))

        self.udp_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.udp_sock.bind(("0.0.0.0", self.udp_port))
        self.udp_sock.setblocking(False)
        self.selector.register(self.udp_sock, selectors.EVENT_READ, ("udp", None))

        emit(
            self.role,
            "start",
            tcp_port=self.tcp_port,
            udp_port=self.udp_port,
            tcp_targets=sorted(self.tcp_targets),
            udp_targets=[target.name for target in self.udp_targets],
        )

    def update_interest(self, conn: PeerConn) -> None:
        events = selectors.EVENT_READ
        if not conn.connected or conn.out_buffer:
            events |= selectors.EVENT_WRITE
        self.selector.modify(conn.sock, events, ("tcp", conn))

    def queue_line(self, conn: PeerConn, obj: dict[str, object]) -> None:
        conn.out_buffer.extend((json.dumps(obj, sort_keys=True) + "\n").encode("utf-8"))
        self.update_interest(conn)

    def open_outbound(self, target: Target) -> None:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.setblocking(False)
        err = sock.connect_ex((target.host, target.port))
        connected = err == 0
        conn = PeerConn(
            sock=sock,
            outbound=True,
            peer_name=target.name,
            connected=connected,
            next_ping_at=time.monotonic() + self.interval_sec,
        )
        self.outbound[target.name] = conn
        self.selector.register(sock, selectors.EVENT_READ, ("tcp", conn))
        if connected:
            emit(self.role, "tcp_connect_ok", peer=target.name, port=target.port)
            self.queue_line(conn, {"kind": "hello", "from": self.role})
        else:
            self.update_interest(conn)

    def close_conn(self, conn: PeerConn, *, event: str) -> None:
        fd = conn.sock.fileno()
        try:
            self.selector.unregister(conn.sock)
        except Exception:
            pass
        try:
            conn.sock.close()
        except OSError:
            pass

        if conn.outbound and conn.peer_name:
            self.outbound.pop(conn.peer_name, None)
            self.retry_deadline[conn.peer_name] = time.monotonic() + 1.0
        else:
            self.inbound.pop(fd, None)

        if conn.peer_name:
            emit(self.role, event, peer=conn.peer_name)

    def maybe_start_outbound(self) -> None:
        now = time.monotonic()
        for name, target in self.tcp_targets.items():
            if name in self.outbound:
                continue
            if now < self.retry_deadline.get(name, 0.0):
                continue
            try:
                self.open_outbound(target)
            except OSError as exc:
                self.retry_deadline[name] = now + 1.0
                emit(self.role, "tcp_connect_retry", peer=name, error=str(exc))

    def handle_listen(self) -> None:
        while True:
            try:
                sock, addr = self.listen_sock.accept()
            except BlockingIOError:
                return
            sock.setblocking(False)
            conn = PeerConn(
                sock=sock,
                outbound=False,
                peer_name=None,
                connected=True,
                next_ping_at=time.monotonic() + self.interval_sec,
            )
            self.inbound[sock.fileno()] = conn
            self.selector.register(sock, selectors.EVENT_READ, ("tcp", conn))
            emit(self.role, "tcp_accept", peer=f"{addr[0]}:{addr[1]}")

    def handle_udp(self) -> None:
        while True:
            try:
                pkt, addr = self.udp_sock.recvfrom(4096)
            except BlockingIOError:
                return
            try:
                obj = json.loads(pkt.decode("utf-8", errors="replace"))
            except json.JSONDecodeError as exc:
                emit(self.role, "udp_rx_parse_error", error=str(exc))
                continue

            peer = str(obj.get("from", f"{addr[0]}:{addr[1]}"))
            kind = obj.get("kind")
            seq = int(obj.get("seq", -1))
            if kind == "discover_ping":
                emit(self.role, "udp_rx", peer=peer, seq=seq)
                payload = {"kind": "discover_pong", "from": self.role, "seq": seq}
                self.udp_sock.sendto(json.dumps(payload, sort_keys=True).encode("utf-8"), addr)
            elif kind == "discover_pong":
                emit(self.role, "udp_rx_ack", peer=peer, ack=seq)

    def handle_tcp_read(self, conn: PeerConn) -> None:
        while True:
            try:
                chunk = conn.sock.recv(4096)
            except BlockingIOError:
                break
            except OSError as exc:
                emit(self.role, "tcp_io_error", peer=conn.peer_name or "unknown", error=str(exc))
                self.close_conn(conn, event="tcp_disconnect")
                return
            if not chunk:
                self.close_conn(conn, event="tcp_disconnect")
                return
            conn.in_buffer.extend(chunk)

        while b"\n" in conn.in_buffer:
            line, _, rest = conn.in_buffer.partition(b"\n")
            conn.in_buffer = bytearray(rest)
            try:
                obj = json.loads(line.decode("utf-8", errors="replace"))
            except json.JSONDecodeError:
                emit(self.role, "tcp_parse_error", peer=conn.peer_name or "unknown")
                continue

            peer = str(obj.get("from", conn.peer_name or "unknown"))
            conn.peer_name = peer
            kind = obj.get("kind")
            if kind == "hello":
                continue

            seq = int(obj.get("seq", -1))
            if kind == "ping":
                emit(self.role, "tcp_rx", peer=peer, seq=seq)
                self.queue_line(conn, {"kind": "pong", "from": self.role, "seq": seq})
                emit(self.role, "tcp_ack", peer=peer, ack=seq)
            elif kind == "pong":
                emit(self.role, "tcp_rx_ack", peer=peer, ack=seq)

    def handle_tcp_write(self, conn: PeerConn) -> None:
        if not conn.connected:
            err = conn.sock.getsockopt(socket.SOL_SOCKET, socket.SO_ERROR)
            if err != 0:
                emit(
                    self.role,
                    "tcp_connect_retry",
                    peer=conn.peer_name or "unknown",
                    error=os.strerror(err),
                )
                self.close_conn(conn, event="tcp_disconnect")
                return
            conn.connected = True
            emit(self.role, "tcp_connect_ok", peer=conn.peer_name or "unknown", port=self.tcp_targets[conn.peer_name].port)
            self.queue_line(conn, {"kind": "hello", "from": self.role})

        if conn.out_buffer:
            try:
                sent = conn.sock.send(conn.out_buffer)
                if sent > 0:
                    del conn.out_buffer[:sent]
            except (BlockingIOError, InterruptedError):
                return
            except OSError as exc:
                emit(self.role, "tcp_io_error", peer=conn.peer_name or "unknown", error=str(exc))
                self.close_conn(conn, event="tcp_disconnect")
                return
        self.update_interest(conn)

    def maybe_send_tcp_pings(self) -> None:
        now = time.monotonic()
        for conn in list(self.outbound.values()) + list(self.inbound.values()):
            if not conn.connected or now < conn.next_ping_at:
                continue
            conn.next_ping_at = now + self.interval_sec
            conn.next_seq += 1
            seq = conn.next_seq
            self.queue_line(conn, {"kind": "ping", "from": self.role, "seq": seq})
            emit(self.role, "tcp_tx", peer=conn.peer_name or "unknown", seq=seq)

    def maybe_send_udp(self) -> None:
        now = time.monotonic()
        if now < self.udp_next_send_at:
            return
        self.udp_next_send_at = now + self.interval_sec
        for idx, target in enumerate(self.udp_targets, start=1):
            seq = int(now) * 100 + idx
            payload = {"kind": "discover_ping", "from": self.role, "seq": seq}
            data = json.dumps(payload, sort_keys=True).encode("utf-8")
            try:
                self.udp_sock.sendto(data, (target.host, target.port))
                emit(self.role, "udp_tx", peer=target.name, seq=seq)
            except OSError as exc:
                emit(self.role, "udp_tx_error", peer=target.name, error=str(exc))

    def run(self) -> int:
        while True:
            self.maybe_start_outbound()
            self.maybe_send_tcp_pings()
            self.maybe_send_udp()

            events = self.selector.select(timeout=0.1)
            for key, mask in events:
                kind, conn = key.data
                if kind == "listen":
                    self.handle_listen()
                elif kind == "udp":
                    self.handle_udp()
                elif kind == "tcp" and conn is not None:
                    if mask & selectors.EVENT_READ:
                        self.handle_tcp_read(conn)
                    if mask & selectors.EVENT_WRITE:
                        self.handle_tcp_write(conn)


def main() -> int:
    node = MeshNode()
    return node.run()


if __name__ == "__main__":
    raise SystemExit(main())
