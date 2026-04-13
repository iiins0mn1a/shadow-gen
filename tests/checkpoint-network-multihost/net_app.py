#!/usr/bin/env python3
"""Network app roles for multi-host checkpoint/restore verification.

Roles:
- tcp_server: listens and acknowledges sequence messages from tcp_client
- tcp_client: sends sequence messages and expects ack from tcp_server
- udp_peer_a / udp_peer_b: exchange heartbeat datagrams with sequence ids
"""

from __future__ import annotations

import json
import os
import select
import socket
import sys
import time


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


def tcp_server(role: str) -> None:
    bind_ip = os.environ.get("BIND_IP", "0.0.0.0")
    bind_port = _env_int("TCP_PORT", 5001)
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind((bind_ip, bind_port))
    srv.listen(16)
    emit(role, "tcp_listen", ip=bind_ip, port=bind_port)

    while True:
        conn, addr = srv.accept()
        emit(role, "tcp_accept", peer=f"{addr[0]}:{addr[1]}")
        with conn:
            file_obj = conn.makefile("rwb", buffering=0)
            while True:
                line = file_obj.readline()
                if not line:
                    emit(role, "tcp_disconnect")
                    break
                txt = line.decode("utf-8", errors="replace").strip()
                if not txt:
                    continue
                try:
                    obj = json.loads(txt)
                    seq = int(obj.get("seq", -1))
                except Exception:
                    emit(role, "tcp_parse_error", raw=txt)
                    continue
                emit(role, "tcp_rx", seq=seq)
                ack = {"ack": seq}
                file_obj.write((json.dumps(ack) + "\n").encode("utf-8"))
                emit(role, "tcp_ack", ack=seq)


def tcp_client(role: str) -> None:
    server_host = os.environ.get("TCP_SERVER_HOST", "host-b")
    server_port = _env_int("TCP_PORT", 5001)
    interval_sec = max(1, _env_int("TCP_INTERVAL_SEC", 1))
    seq = 0

    while True:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(3.0)
        try:
            sock.connect((server_host, server_port))
            file_obj = sock.makefile("rwb", buffering=0)
            emit(role, "tcp_connect_ok", host=server_host, port=server_port)
        except OSError as e:
            emit(role, "tcp_connect_retry", error=str(e), host=server_host, port=server_port)
            try:
                sock.close()
            except OSError:
                pass
            time.sleep(1.0)
            continue

        try:
            while True:
                seq += 1
                msg = {"seq": seq}
                file_obj.write((json.dumps(msg) + "\n").encode("utf-8"))
                emit(role, "tcp_tx", seq=seq)

                line = file_obj.readline()
                if not line:
                    emit(role, "tcp_ack_eof", seq=seq)
                    break
                try:
                    obj = json.loads(line.decode("utf-8", errors="replace").strip())
                    ack = int(obj.get("ack", -1))
                except Exception:
                    emit(role, "tcp_ack_parse_error", seq=seq)
                    break
                emit(role, "tcp_rx_ack", ack=ack)
                time.sleep(interval_sec)
        except OSError as e:
            emit(role, "tcp_io_error", error=str(e))
        finally:
            try:
                sock.close()
            except OSError:
                pass


def _udp_cfg(role: str) -> tuple[int, int]:
    if role == "udp_peer_a":
        return (_env_int("UDP_A_PORT", 6001), _env_int("UDP_B_PORT", 6002))
    return (_env_int("UDP_B_PORT", 6002), _env_int("UDP_A_PORT", 6001))


def udp_peer(role: str) -> None:
    peer_host = os.environ.get("UDP_PEER_HOST", "host-b" if role == "udp_peer_a" else "host-a")
    local_port, peer_port = _udp_cfg(role)
    interval_sec = max(1, _env_int("UDP_INTERVAL_SEC", 1))

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("0.0.0.0", local_port))
    sock.setblocking(False)
    emit(role, "udp_bind", local_port=local_port, peer_host=peer_host, peer_port=peer_port)

    tx_seq = 0
    while True:
        tx_seq += 1
        payload = {"seq": tx_seq, "role": role}
        data = json.dumps(payload).encode("utf-8")
        try:
            sock.sendto(data, (peer_host, peer_port))
            emit(role, "udp_tx", seq=tx_seq, peer_host=peer_host, peer_port=peer_port)
        except OSError as e:
            emit(role, "udp_tx_error", seq=tx_seq, error=str(e))

        end = time.monotonic() + float(interval_sec)
        while True:
            left = end - time.monotonic()
            if left <= 0:
                break
            rlist, _, _ = select.select([sock], [], [], min(left, 0.2))
            if not rlist:
                continue
            try:
                pkt, addr = sock.recvfrom(2048)
                txt = pkt.decode("utf-8", errors="replace")
                obj = json.loads(txt)
                seq = int(obj.get("seq", -1))
                emit(role, "udp_rx", seq=seq, peer=f"{addr[0]}:{addr[1]}")
            except Exception as e:
                emit(role, "udp_rx_parse_error", error=str(e))


def main() -> int:
    role = os.environ.get("ROLE", "").strip()
    if role not in {"tcp_server", "tcp_client", "udp_peer_a", "udp_peer_b"}:
        print(
            "ROLE must be one of tcp_server|tcp_client|udp_peer_a|udp_peer_b",
            file=sys.stderr,
        )
        return 2

    emit(role, "start")
    if role == "tcp_server":
        tcp_server(role)
    elif role == "tcp_client":
        tcp_client(role)
    else:
        udp_peer(role)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
