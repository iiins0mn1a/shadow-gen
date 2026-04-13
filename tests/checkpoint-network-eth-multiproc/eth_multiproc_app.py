#!/usr/bin/env python3
"""Ethereum-like multi-process checkpoint/restore workload for Shadow.

Each Shadow host runs three processes:
- execution: local RPC server for the co-located beacon
- beacon: local RPC client/server plus cross-host P2P TCP/UDP
- validator: local RPC client for the co-located beacon

The process event loop intentionally uses epoll via selectors, plus eventfd and
timerfd, to exercise richer async-runtime descriptor restore paths.
"""

from __future__ import annotations

import ctypes
import errno
import json
import os
import selectors
import socket
import struct
import time
from dataclasses import dataclass, field


LIBC = ctypes.CDLL("libc.so.6", use_errno=True)
CLOCK_MONOTONIC = 1
TFD_CLOEXEC = getattr(os, "O_CLOEXEC", 0o2000000)
TFD_NONBLOCK = getattr(os, "O_NONBLOCK", 0o4000)


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


def parse_targets(raw: str) -> list[tuple[str, str, int]]:
    targets: list[tuple[str, str, int]] = []
    for item in raw.split(","):
        item = item.strip()
        if not item:
            continue
        name, endpoint = item.split("@", 1)
        host, port_raw = endpoint.rsplit(":", 1)
        targets.append((name, host, int(port_raw)))
    return targets


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

    def register_listener(self, name: str, host: str, port: int) -> None:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind((host, port))
        sock.listen(64)
        sock.setblocking(False)
        self.listen_socks[name] = sock
        self.selector.register(sock, selectors.EVENT_READ, ("listen", name))

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
            self.queue(conn, {"kind": "hello", "from": self.role, "channel": channel})

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
            self.queue(conn, {"kind": "hello", "from": self.role, "channel": conn.channel})

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
        self.register_listener("engine", "127.0.0.1", _env_int("EXEC_RPC_PORT", 8200))

    def on_tick(self) -> None:
        emit(self.role, "tick")

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
        self.node = os.environ["NODE_NAME"]
        self.register_listener("validator", "127.0.0.1", _env_int("BEACON_RPC_PORT", 4000))
        self.register_listener("p2p", "0.0.0.0", _env_int("P2P_PORT", 13000))
        self.register_udp("0.0.0.0", _env_int("DISCOVERY_PORT", 12000))
        self.exec_target = (
            "execution",
            "127.0.0.1",
            _env_int("EXEC_RPC_PORT", 8200),
            "engine",
        )
        self.p2p_targets = [
            (name, host, port, "p2p") for name, host, port in parse_targets(os.environ.get("P2P_PEERS", ""))
        ]
        self.udp_targets = parse_targets(os.environ.get("UDP_PEERS", ""))
        self.udp_next_send = time.monotonic()

    def outbound_targets(self) -> list[tuple[str, str, int, str]]:
        return [self.exec_target, *self.p2p_targets]

    def on_tick(self) -> None:
        exec_conn = self.outbound_by_name.get("execution")
        if exec_conn and exec_conn.connected:
            exec_conn.next_seq += 1
            seq = exec_conn.next_seq
            self.queue(exec_conn, {"kind": "engine_ping", "from": self.role, "seq": seq})
            emit(self.role, "exec_rpc_tx", peer="execution", seq=seq)

        for name, _, _, _ in self.p2p_targets:
            conn = self.outbound_by_name.get(name)
            if conn and conn.connected:
                conn.next_seq += 1
                seq = conn.next_seq
                self.queue(conn, {"kind": "p2p_ping", "from": self.role, "seq": seq})
                emit(self.role, "p2p_tx", peer=name, seq=seq)

        now = time.monotonic()
        if now >= self.udp_next_send:
            self.udp_next_send = now + self.interval_sec
            for idx, (name, host, port) in enumerate(self.udp_targets, start=1):
                seq = int(now) * 100 + idx
                payload = {"kind": "discovery_ping", "from": self.role, "seq": seq}
                self.udp_sock.sendto(json.dumps(payload, sort_keys=True).encode("utf-8"), (host, port))
                emit(self.role, "udp_tx", peer=name, seq=seq)

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


class ValidatorProcess(BaseProcess):
    def __init__(self) -> None:
        super().__init__()
        self.beacon_target = (
            "beacon",
            "127.0.0.1",
            _env_int("BEACON_RPC_PORT", 4000),
            "validator",
        )

    def outbound_targets(self) -> list[tuple[str, str, int, str]]:
        return [self.beacon_target]

    def on_tick(self) -> None:
        conn = self.outbound_by_name.get("beacon")
        if conn and conn.connected:
            conn.next_seq += 1
            seq = conn.next_seq
            self.queue(conn, {"kind": "validator_req", "from": self.role, "seq": seq})
            emit(self.role, "validator_rpc_tx", peer="beacon", seq=seq)

    def on_message(self, conn: Conn, obj: dict[str, object]) -> None:
        kind = obj.get("kind")
        if kind == "hello":
            return
        if kind == "validator_resp":
            seq = int(obj.get("seq", -1))
            emit(self.role, "validator_rpc_rx_ack", peer=conn.peer, ack=seq)


def build_process() -> BaseProcess:
    role = os.environ["ROLE"]
    if role.startswith("execution-"):
        return ExecutionProcess()
    if role.startswith("beacon-"):
        return BeaconProcess()
    if role.startswith("validator-"):
        return ValidatorProcess()
    raise SystemExit(f"unsupported ROLE={role!r}")


def main() -> int:
    process = build_process()
    return process.run()


if __name__ == "__main__":
    raise SystemExit(main())
