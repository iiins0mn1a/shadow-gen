#!/usr/bin/env python3
"""High-fidelity Ethereum-like synthetic workload inspired by shadow-ethereum/shadow.yaml."""

from __future__ import annotations

import ctypes
import errno
import json
import os
import re
import selectors
import socket
import struct
import time
from dataclasses import dataclass, field
from pathlib import Path


LIBC = ctypes.CDLL("libc.so.6", use_errno=True)
CLOCK_MONOTONIC = 1
TFD_CLOEXEC = getattr(os, "O_CLOEXEC", 0o2000000)
TFD_NONBLOCK = getattr(os, "O_NONBLOCK", 0o4000)
MULTIADDR_RE = re.compile(r"^/ip4/([^/]+)/tcp/([0-9]+)/p2p/([A-Za-z0-9._-]+)$")


class Timespec(ctypes.Structure):
    _fields_ = [("tv_sec", ctypes.c_long), ("tv_nsec", ctypes.c_long)]


class Itimerspec(ctypes.Structure):
    _fields_ = [("it_interval", Timespec), ("it_value", Timespec)]


LIBC.timerfd_create.argtypes = [ctypes.c_int, ctypes.c_int]
LIBC.timerfd_create.restype = ctypes.c_int
LIBC.timerfd_settime.argtypes = [
    ctypes.c_int,
    ctypes.c_int,
    ctypes.POINTER(Itimerspec),
    ctypes.c_void_p,
]
LIBC.timerfd_settime.restype = ctypes.c_int


def emit(role: str, event: str, **fields: object) -> None:
    payload: dict[str, object] = {
        "role": role,
        "event": event,
        "mono_ns": time.monotonic_ns(),
    }
    payload.update(fields)
    print(f"NETLOG {json.dumps(payload, sort_keys=True)}", flush=True)


def _env_int(key: str, default: int) -> int:
    raw = os.environ.get(key)
    if raw is None:
        return default
    try:
        return int(raw)
    except ValueError:
        return default


def _role_index(role: str) -> int:
    return int(role.rsplit("-", 1)[1])


def _ensure_parent(path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)


def _safe_read_lines(path: Path) -> list[str]:
    if not path.exists():
        return []
    try:
        return [line.strip() for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
    except OSError:
        return []


def _append_unique_line(path: Path, line: str) -> bool:
    existing = set(_safe_read_lines(path))
    if line in existing:
        return False
    _ensure_parent(path)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(line + "\n")
    return True


def _parse_multiaddr(line: str) -> tuple[str, str, int, int] | None:
    match = MULTIADDR_RE.match(line.strip())
    if not match:
        return None
    host = match.group(1)
    tcp_port = int(match.group(2))
    peer_name = match.group(3)
    udp_port = tcp_port + 100
    return peer_name, host, tcp_port, udp_port


class TimerFd:
    def __init__(self, interval_sec: int) -> None:
        fd = LIBC.timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC | TFD_NONBLOCK)
        if fd < 0:
            err = ctypes.get_errno()
            raise OSError(err, os.strerror(err))
        self.fd = fd
        spec = Itimerspec(
            it_interval=Timespec(interval_sec, 0),
            it_value=Timespec(interval_sec, 0),
        )
        if LIBC.timerfd_settime(self.fd, 0, ctypes.byref(spec), None) != 0:
            err = ctypes.get_errno()
            raise OSError(err, os.strerror(err))

    def drain(self) -> int:
        try:
            data = os.read(self.fd, 8)
        except BlockingIOError:
            return 0
        return struct.unpack("Q", data)[0] if len(data) == 8 else 0


@dataclass
class Conn:
    sock: socket.socket
    channel: str
    peer: str
    outbound: bool
    connected: bool
    in_buffer: bytearray = field(default_factory=bytearray)
    out_buffer: bytearray = field(default_factory=bytearray)
    next_seq: int = 0


class BaseProcess:
    def __init__(self) -> None:
        self.role = os.environ["ROLE"]
        self.selector = selectors.DefaultSelector()
        self.interval_sec = max(1, _env_int("INTERVAL_SEC", 1))
        self.event_fd = os.eventfd(0, os.EFD_CLOEXEC | os.EFD_NONBLOCK)
        self.timer_fd = TimerFd(self.interval_sec)
        self.selector.register(self.event_fd, selectors.EVENT_READ, ("eventfd", None))
        self.selector.register(self.timer_fd.fd, selectors.EVENT_READ, ("timerfd", None))
        self.listen_socks: dict[str, socket.socket] = {}
        self.outbound_by_name: dict[str, Conn] = {}
        self.inbound_by_fd: dict[int, Conn] = {}
        self.retry_deadline: dict[str, float] = {}
        self._passive_listeners: set[str] = set()

    def register_listener(self, name: str, host: str, port: int, *, passive: bool = False) -> None:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind((host, port))
        sock.listen(64)
        sock.setblocking(False)
        self.listen_socks[name] = sock
        self.selector.register(sock, selectors.EVENT_READ, ("listen", name))
        if passive:
            self._passive_listeners.add(name)

    def register_udp(self, host: str, port: int) -> None:
        self.udp_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.udp_sock.bind((host, port))
        self.udp_sock.setblocking(False)
        self.selector.register(self.udp_sock, selectors.EVENT_READ, ("udp", None))

    def queue(self, conn: Conn, obj: dict[str, object]) -> None:
        conn.out_buffer.extend((json.dumps(obj, sort_keys=True) + "\n").encode("utf-8"))
        self._update_interest(conn)

    def _update_interest(self, conn: Conn) -> None:
        events = selectors.EVENT_READ
        if not conn.connected or conn.out_buffer:
            events |= selectors.EVENT_WRITE
        self.selector.modify(conn.sock, events, ("conn", conn))

    def _register_conn(self, conn: Conn) -> None:
        self.selector.register(conn.sock, selectors.EVENT_READ, ("conn", conn))
        self._update_interest(conn)

    def open_outbound(self, name: str, host: str, port: int, channel: str) -> None:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.setblocking(False)
        err = sock.connect_ex((host, port))
        connected = err == 0
        conn = Conn(sock=sock, channel=channel, peer=name, outbound=True, connected=connected)
        self.outbound_by_name[name] = conn
        self._register_conn(conn)
        if connected:
            emit(self.role, "tcp_connect_ok", peer=name, channel=channel)
            self._queue_hello(conn)

    def _queue_hello(self, conn: Conn) -> None:
        self.queue(
            conn,
            {
                "kind": "hello",
                "from": self.role,
                "channel": conn.channel,
            },
        )

    def close_conn(self, conn: Conn, *, event: str) -> None:
        fd = conn.sock.fileno()
        try:
            self.selector.unregister(conn.sock)
        except Exception:
            pass
        try:
            conn.sock.close()
        except OSError:
            pass
        if conn.outbound:
            self.outbound_by_name.pop(conn.peer, None)
            self.retry_deadline[conn.peer] = time.monotonic() + 1.0
        else:
            self.inbound_by_fd.pop(fd, None)
        emit(self.role, event, peer=conn.peer, channel=conn.channel)

    def maybe_open_outbounds(self) -> None:
        now = time.monotonic()
        for name, host, port, channel in self.outbound_targets():
            if name in self.outbound_by_name:
                continue
            if now < self.retry_deadline.get(name, 0.0):
                continue
            try:
                self.open_outbound(name, host, port, channel)
            except OSError as exc:
                self.retry_deadline[name] = now + 1.0
                emit(self.role, "tcp_connect_retry", peer=name, channel=channel, error=str(exc))

    def handle_listen(self, listen_name: str) -> None:
        sock = self.listen_socks[listen_name]
        while True:
            try:
                accepted, addr = sock.accept()
            except BlockingIOError:
                return
            accepted.setblocking(False)
            if listen_name in self._passive_listeners:
                emit(self.role, "passive_accept", listener=listen_name, peer=f"{addr[0]}:{addr[1]}")
                accepted.close()
                continue
            conn = Conn(
                sock=accepted,
                channel=listen_name,
                peer=f"{addr[0]}:{addr[1]}",
                outbound=False,
                connected=True,
            )
            self.inbound_by_fd[accepted.fileno()] = conn
            self._register_conn(conn)
            emit(self.role, "tcp_accept", peer=conn.peer, channel=listen_name)

    def handle_conn_read(self, conn: Conn) -> None:
        while True:
            try:
                chunk = conn.sock.recv(4096)
            except BlockingIOError:
                break
            except OSError as exc:
                emit(self.role, "tcp_io_error", peer=conn.peer, channel=conn.channel, error=str(exc))
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
                emit(self.role, "tcp_parse_error", peer=conn.peer, channel=conn.channel)
                continue
            peer = str(obj.get("from", conn.peer))
            conn.peer = peer
            self.on_message(conn, obj)

    def handle_conn_write(self, conn: Conn) -> None:
        if not conn.connected:
            err = conn.sock.getsockopt(socket.SOL_SOCKET, socket.SO_ERROR)
            if err != 0:
                emit(
                    self.role,
                    "tcp_connect_retry",
                    peer=conn.peer,
                    channel=conn.channel,
                    error=os.strerror(err),
                )
                self.close_conn(conn, event="tcp_disconnect")
                return
            conn.connected = True
            emit(self.role, "tcp_connect_ok", peer=conn.peer, channel=conn.channel)
            self._queue_hello(conn)

        if conn.out_buffer:
            try:
                sent = conn.sock.send(conn.out_buffer)
            except (BlockingIOError, InterruptedError):
                return
            except OSError as exc:
                emit(self.role, "tcp_io_error", peer=conn.peer, channel=conn.channel, error=str(exc))
                self.close_conn(conn, event="tcp_disconnect")
                return
            if sent > 0:
                del conn.out_buffer[:sent]
        self._update_interest(conn)

    def handle_timerfd(self) -> None:
        expirations = self.timer_fd.drain()
        if expirations > 0:
            os.eventfd_write(self.event_fd, expirations)

    def handle_eventfd(self) -> None:
        ticks = os.eventfd_read(self.event_fd)
        for _ in range(max(1, ticks)):
            self.on_tick()

    def on_start(self) -> None:
        return

    def on_tick(self) -> None:
        raise NotImplementedError

    def on_message(self, conn: Conn, obj: dict[str, object]) -> None:
        raise NotImplementedError

    def outbound_targets(self) -> list[tuple[str, str, int, str]]:
        return []

    def handle_udp(self) -> None:
        return

    def run(self) -> int:
        emit(self.role, "start")
        self.on_start()
        while True:
            self.maybe_open_outbounds()
            for key, mask in self.selector.select(timeout=0.1):
                kind, data = key.data
                if kind == "listen":
                    self.handle_listen(str(data))
                elif kind == "conn":
                    conn = data
                    if mask & selectors.EVENT_READ:
                        self.handle_conn_read(conn)
                    if mask & selectors.EVENT_WRITE:
                        self.handle_conn_write(conn)
                elif kind == "timerfd":
                    self.handle_timerfd()
                elif kind == "eventfd":
                    self.handle_eventfd()
                elif kind == "udp":
                    self.handle_udp()
        return 0


class ExecutionProcess(BaseProcess):
    def __init__(self) -> None:
        super().__init__()
        host_ip = os.environ["HOST_IP"]
        self.register_listener("engine", host_ip, _env_int("AUTHRPC_PORT", 8200))
        self.register_listener("http", host_ip, _env_int("HTTP_PORT", 8000), passive=True)
        self.register_listener("ws", host_ip, _env_int("WS_PORT", 8100), passive=True)
        self.register_listener("metrics", host_ip, _env_int("METRICS_PORT", 8300), passive=True)
        self.register_listener("p2p", host_ip, _env_int("P2P_PORT", 8400), passive=True)

    def on_tick(self) -> None:
        emit(self.role, "execution_tick")

    def on_message(self, conn: Conn, obj: dict[str, object]) -> None:
        kind = obj.get("kind")
        if kind == "hello":
            return
        if kind == "engine_ping":
            seq = int(obj.get("seq", -1))
            emit(self.role, "exec_rpc_rx", peer=conn.peer, seq=seq)
            self.queue(conn, {"kind": "engine_pong", "from": self.role, "seq": seq})
            emit(self.role, "exec_rpc_ack", peer=conn.peer, ack=seq)


class BeaconProcess(BaseProcess):
    def __init__(self) -> None:
        super().__init__()
        self.idx = _role_index(self.role)
        self.host_ip = os.environ["HOST_IP"]
        self.exec_ip = os.environ["EXEC_HOST_IP"]
        self.exec_port = _env_int("EXEC_AUTHRPC_PORT", 8200)
        self.rpc_port = _env_int("RPC_PORT", 4000)
        self.grpc_port = _env_int("GRPC_PORT", 4100)
        self.tcp_port = _env_int("P2P_TCP_PORT", 4200)
        self.udp_port = _env_int("P2P_UDP_PORT", 4300)
        self.mon_port = _env_int("MON_PORT", 4400)
        self.max_peer_fanout = max(1, _env_int("MAX_PEER_FANOUT", 3))
        self.common_peer_file = Path(os.environ["COMMON_PEER_FILE"])
        self.publish_peer_file = Path(os.environ["PUBLISH_PEER_FILE"])
        self.peer_targets: list[tuple[str, str, int, int]] = []
        self.last_peer_catalog: tuple[str, ...] = ()
        self.udp_next_send = time.monotonic()

        self.register_listener("validator", self.host_ip, self.rpc_port)
        self.register_listener("p2p", self.host_ip, self.tcp_port)
        self.register_listener("grpc", self.host_ip, self.grpc_port, passive=True)
        self.register_listener("monitoring", self.host_ip, self.mon_port, passive=True)
        self.register_udp(self.host_ip, self.udp_port)

    def on_start(self) -> None:
        peer_line = f"/ip4/{self.host_ip}/tcp/{self.tcp_port}/p2p/{self.role}"
        _ensure_parent(self.publish_peer_file)
        self.publish_peer_file.write_text(peer_line + "\n", encoding="utf-8")
        emit(self.role, "peer_publish", line=peer_line)
        self.refresh_peer_targets()

    def refresh_peer_targets(self) -> None:
        selected: list[tuple[str, str, int, int]] = []
        for raw in _safe_read_lines(self.common_peer_file):
            parsed = _parse_multiaddr(raw)
            if parsed is None:
                continue
            peer_name, host, tcp_port, udp_port = parsed
            if peer_name == self.role:
                continue
            peer_idx = _role_index(peer_name)
            if peer_idx >= self.idx:
                continue
            selected.append((peer_name, host, tcp_port, udp_port))
        selected.sort(key=lambda item: _role_index(item[0]), reverse=True)
        selected = selected[: self.max_peer_fanout]
        snapshot = tuple(peer for peer, _, _, _ in selected)
        if snapshot != self.last_peer_catalog:
            emit(self.role, "peer_catalog", peers=list(snapshot), count=len(snapshot))
            self.last_peer_catalog = snapshot
        self.peer_targets = selected

    def outbound_targets(self) -> list[tuple[str, str, int, str]]:
        self.refresh_peer_targets()
        targets = [("geth-node", self.exec_ip, self.exec_port, "engine")]
        targets.extend((peer, host, tcp_port, "p2p") for peer, host, tcp_port, _ in self.peer_targets)
        return targets

    def on_tick(self) -> None:
        emit(self.role, "beacon_tick")

        exec_conn = self.outbound_by_name.get("geth-node")
        if exec_conn and exec_conn.connected:
            exec_conn.next_seq += 1
            seq = exec_conn.next_seq
            self.queue(exec_conn, {"kind": "engine_ping", "from": self.role, "seq": seq})
            emit(self.role, "exec_rpc_tx", peer="geth-node", seq=seq)

        for peer, _, _, _ in self.peer_targets:
            conn = self.outbound_by_name.get(peer)
            if conn and conn.connected:
                conn.next_seq += 1
                seq = conn.next_seq
                self.queue(conn, {"kind": "p2p_ping", "from": self.role, "seq": seq})
                emit(self.role, "p2p_tx", peer=peer, seq=seq)

        now = time.monotonic()
        if now >= self.udp_next_send:
            self.udp_next_send = now + self.interval_sec
            for peer, host, _, udp_port in self.peer_targets:
                seq = int(now * 1000)
                payload = {"kind": "discovery_ping", "from": self.role, "seq": seq}
                self.udp_sock.sendto(json.dumps(payload, sort_keys=True).encode("utf-8"), (host, udp_port))
                emit(self.role, "udp_tx", peer=peer, seq=seq)

    def on_message(self, conn: Conn, obj: dict[str, object]) -> None:
        kind = obj.get("kind")
        if kind == "hello":
            return
        seq = int(obj.get("seq", -1))
        if kind == "engine_pong":
            emit(self.role, "exec_rpc_rx_ack", peer=conn.peer, ack=seq)
        elif kind == "validator_req":
            emit(self.role, "validator_rpc_rx", peer=conn.peer, seq=seq)
            self.queue(conn, {"kind": "validator_resp", "from": self.role, "seq": seq})
            emit(self.role, "validator_rpc_ack", peer=conn.peer, ack=seq)
        elif kind == "p2p_ping":
            emit(self.role, "p2p_rx", peer=conn.peer, seq=seq)
            self.queue(conn, {"kind": "p2p_pong", "from": self.role, "seq": seq})
            emit(self.role, "p2p_ack", peer=conn.peer, ack=seq)
        elif kind == "p2p_pong":
            emit(self.role, "p2p_rx_ack", peer=conn.peer, ack=seq)

    def handle_udp(self) -> None:
        while True:
            try:
                pkt, addr = self.udp_sock.recvfrom(4096)
            except BlockingIOError:
                return
            except OSError as exc:
                if exc.errno == errno.EWOULDBLOCK:
                    return
                raise
            obj = json.loads(pkt.decode("utf-8", errors="replace"))
            peer = str(obj.get("from", f"{addr[0]}:{addr[1]}"))
            seq = int(obj.get("seq", -1))
            if obj.get("kind") == "discovery_ping":
                emit(self.role, "udp_rx", peer=peer, seq=seq)
                payload = {"kind": "discovery_pong", "from": self.role, "seq": seq}
                self.udp_sock.sendto(json.dumps(payload, sort_keys=True).encode("utf-8"), addr)
            elif obj.get("kind") == "discovery_pong":
                emit(self.role, "udp_rx_ack", peer=peer, ack=seq)


class RecorderProcess(BaseProcess):
    def __init__(self) -> None:
        super().__init__()
        self.publish_peer_file = Path(os.environ["PUBLISH_PEER_FILE"])
        self.common_peer_file = Path(os.environ["COMMON_PEER_FILE"])
        self.last_count = -1

    def on_tick(self) -> None:
        emit(self.role, "recorder_tick")
        lines = _safe_read_lines(self.publish_peer_file)
        if lines:
            line = lines[0]
            if _append_unique_line(self.common_peer_file, line):
                emit(self.role, "peer_recorded", line=line)
        count = len(_safe_read_lines(self.common_peer_file))
        if count != self.last_count:
            self.last_count = count
            emit(self.role, "peer_file_count", count=count)

    def on_message(self, conn: Conn, obj: dict[str, object]) -> None:
        return


class ValidatorProcess(BaseProcess):
    def __init__(self) -> None:
        super().__init__()
        self.host_ip = os.environ["HOST_IP"]
        self.beacon_ip = os.environ["BEACON_HOST_IP"]
        self.beacon_port = _env_int("BEACON_RPC_PORT", 4000)
        self.register_listener("rpc", self.host_ip, _env_int("RPC_PORT", 7000), passive=True)
        self.register_listener("grpc", self.host_ip, _env_int("GRPC_PORT", 7100), passive=True)
        self.register_listener("monitoring", self.host_ip, _env_int("MON_PORT", 7200), passive=True)

    def outbound_targets(self) -> list[tuple[str, str, int, str]]:
        beacon_name = f"beacon-{_role_index(self.role)}"
        return [(beacon_name, self.beacon_ip, self.beacon_port, "validator")]

    def on_tick(self) -> None:
        emit(self.role, "validator_tick")
        beacon_name = f"beacon-{_role_index(self.role)}"
        conn = self.outbound_by_name.get(beacon_name)
        if conn and conn.connected:
            conn.next_seq += 1
            seq = conn.next_seq
            self.queue(conn, {"kind": "validator_req", "from": self.role, "seq": seq})
            emit(self.role, "validator_rpc_tx", peer=beacon_name, seq=seq)

    def on_message(self, conn: Conn, obj: dict[str, object]) -> None:
        if obj.get("kind") == "hello":
            return
        if obj.get("kind") == "validator_resp":
            seq = int(obj.get("seq", -1))
            emit(self.role, "validator_rpc_rx_ack", peer=conn.peer, ack=seq)


def build_process() -> BaseProcess:
    role = os.environ["ROLE"]
    if role == "geth-node":
        return ExecutionProcess()
    if role.startswith("beacon-"):
        return BeaconProcess()
    if role.startswith("recorder-"):
        return RecorderProcess()
    if role.startswith("validator-"):
        return ValidatorProcess()
    raise SystemExit(f"unsupported ROLE={role!r}")


def main() -> int:
    process = build_process()
    return process.run()


if __name__ == "__main__":
    raise SystemExit(main())
