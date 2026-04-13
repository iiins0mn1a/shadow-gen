#!/usr/bin/env python3
"""Verifier for the Ethereum-like multi-process-per-host checkpoint test."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


EXECUTIONS = ("execution-0", "execution-1")
BEACONS = ("beacon-0", "beacon-1")
VALIDATORS = ("validator-0", "validator-1")
TCP_CHURN_EVENTS = {
    "tcp_connect_ok",
    "tcp_accept",
    "tcp_connect_retry",
    "tcp_io_error",
    "tcp_disconnect",
}


@dataclass
class ShadowSession:
    process: subprocess.Popen
    socket_path: Path
    sock: Optional[socket.socket] = None


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Verify multi-process Ethereum-like cp/restore")
    parser.add_argument("--shadow-bin", default=shutil.which("shadow") or "shadow")
    parser.add_argument(
        "--config",
        default=str(Path(__file__).resolve().parent / "shadow_eth_multiproc.yaml"),
    )
    parser.add_argument(
        "--work-dir",
        default=str(Path(__file__).resolve().parent / "run"),
    )
    parser.add_argument(
        "--socket-path",
        default="",
    )
    parser.add_argument("--connect-timeout", type=float, default=60.0)
    parser.add_argument("--response-timeout", type=float, default=120.0)
    parser.add_argument("--clean-data", action="store_true")
    parser.add_argument("--verify-label", default="cp_eth_multiproc_verify")
    parser.add_argument("--post-restore-step-ns", type=int, default=1_000_000_000)
    parser.add_argument("--post-restore-steps", type=int, default=12)
    return parser.parse_args()


def wait_for_socket(path: Path, timeout_sec: float, process: subprocess.Popen) -> socket.socket:
    start = time.time()
    while time.time() - start < timeout_sec:
        if process.poll() is not None:
            raise RuntimeError(
                f"Shadow exited before socket became available (code={process.returncode})"
            )
        if path.exists():
            try:
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                sock.connect(str(path))
                return sock
            except (ConnectionRefusedError, FileNotFoundError, OSError):
                pass
        time.sleep(0.3)
    raise TimeoutError(f"socket {path} not available within {timeout_sec}s")


def send_command(sock: socket.socket, cmd: dict, timeout_sec: float) -> dict:
    sock.sendall((json.dumps(cmd) + "\n").encode("utf-8"))
    sock.settimeout(timeout_sec)
    buf = b""
    while b"\n" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            raise ConnectionError("socket closed by Shadow")
        buf += chunk
    return json.loads(buf.split(b"\n", 1)[0].decode("utf-8"))


def expect_ok(resp: dict, label: str) -> None:
    if resp.get("status") != "ok":
        raise AssertionError(f"{label}: expected ok, got {resp!r}")


def resolve_config(config_path: Path, work_dir: Path) -> Path:
    rendered = work_dir / "rendered.shadow.yaml"
    app_path = (Path(__file__).resolve().parent / "eth_multiproc_app.py").resolve()
    text = config_path.resolve().read_text(encoding="utf-8").replace("__APP_PATH__", str(app_path))
    rendered.write_text(text, encoding="utf-8")
    return rendered


def launch_shadow(
    shadow_bin: str,
    config_path: Path,
    work_dir: Path,
    socket_path: Path,
    clean_data: bool,
) -> ShadowSession:
    work_dir.mkdir(parents=True, exist_ok=True)
    if clean_data:
        shadow_data = work_dir / "shadow.data"
        if shadow_data.exists():
            shutil.rmtree(shadow_data)
    if socket_path.exists():
        socket_path.unlink()
    rendered = resolve_config(config_path, work_dir)
    env = os.environ.copy()
    env["SHADOW_CONTROL_SOCKET"] = str(socket_path)
    shadow_bin_resolved = shadow_bin
    if os.sep in shadow_bin or shadow_bin.startswith("."):
        bin_path = Path(shadow_bin).expanduser()
        if not bin_path.is_absolute():
            bin_path = (Path.cwd() / bin_path).resolve()
        shadow_bin_resolved = str(bin_path)
    proc = subprocess.Popen(
        [shadow_bin_resolved, str(rendered)],
        cwd=str(work_dir),
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=False,
    )
    return ShadowSession(process=proc, socket_path=socket_path)


def ensure_connected(session: ShadowSession, timeout_sec: float) -> None:
    if session.sock is None:
        session.sock = wait_for_socket(session.socket_path, timeout_sec, session.process)


def collect_new_events(work_dir: Path, offsets: dict[str, int]) -> list[dict]:
    events: list[dict] = []
    for path in sorted((work_dir / "shadow.data" / "hosts").glob("*/*.stdout")):
        key = str(path)
        old = offsets.get(key, 0)
        try:
            with path.open("rb") as handle:
                handle.seek(old)
                data = handle.read()
                offsets[key] = old + len(data)
        except OSError:
            continue
        for line in data.decode("utf-8", errors="replace").splitlines():
            if not line.startswith("NETLOG "):
                continue
            try:
                payload = json.loads(line[len("NETLOG ") :].strip())
            except json.JSONDecodeError:
                continue
            if isinstance(payload, dict):
                events.append(payload)
    return events


def continue_and_collect(
    session: ShadowSession,
    work_dir: Path,
    offsets: dict[str, int],
    duration_ns: int,
    response_timeout: float,
) -> list[dict]:
    assert session.sock is not None
    resp = send_command(session.sock, {"cmd": "continue_for", "duration_ns": duration_ns}, response_timeout)
    expect_ok(resp, "continue_for")
    return collect_new_events(work_dir, offsets)


def send_restore_with_reconnect(
    session: ShadowSession,
    label: str,
    connect_timeout: float,
    response_timeout: float,
) -> dict:
    restore_timeout = max(response_timeout, 300.0)
    try:
        assert session.sock is not None
        return send_command(session.sock, {"cmd": "restore", "label": label}, restore_timeout)
    except ConnectionError:
        if session.sock is not None:
            try:
                session.sock.close()
            except OSError:
                pass
            session.sock = None
        ensure_connected(session, connect_timeout)
        assert session.sock is not None
        return send_command(session.sock, {"cmd": "status"}, restore_timeout)


def checkpoint_path(work_dir: Path, label: str) -> Path:
    return work_dir / "shadow.data" / "checkpoints" / f"{label}.checkpoint.json"


def verify_checkpoint(path: Path) -> None:
    raw = json.loads(path.read_text(encoding="utf-8"))
    hosts = raw.get("hosts")
    if not isinstance(hosts, list) or len(hosts) < 2:
        raise AssertionError(f"expected >=2 hosts in checkpoint, got {hosts!r}")
    bad = [host.get("name") for host in hosts if len(host.get("processes", [])) < 3]
    if bad:
        raise AssertionError(f"checkpoint missing expected multiprocess hosts: {bad}")


def max_value(events: list[dict], role: str, event: str, key: str) -> int:
    best = -1
    for item in events:
        if item.get("role") != role or item.get("event") != event:
            continue
        value = item.get(key)
        if isinstance(value, int) and value > best:
            best = value
    return best


def mono_span(events: list[dict], role: str, expected_events: set[str]) -> int:
    values = [
        item["mono_ns"]
        for item in events
        if item.get("role") == role
        and item.get("event") in expected_events
        and isinstance(item.get("mono_ns"), int)
    ]
    return max(values) - min(values) if len(values) >= 2 else 0


def churn_count(events: list[dict]) -> int:
    return sum(1 for item in events if item.get("event") in TCP_CHURN_EVENTS)


def metrics(events: list[dict]) -> dict[str, tuple[int, int, int, int]]:
    return {
        beacon: (
            max_value(events, beacon, "exec_rpc_tx", "seq"),
            max_value(events, beacon, "exec_rpc_rx_ack", "ack"),
            max_value(events, beacon, "p2p_tx", "seq"),
            max_value(events, beacon, "udp_rx_ack", "ack"),
        )
        for beacon in BEACONS
    }


def require_metrics_present(history: list[dict], stage: str) -> None:
    values = metrics(history)
    missing = [role for role, parts in values.items() if min(parts) < 0]
    if missing:
        raise AssertionError(f"{stage}: missing beacon metrics for {', '.join(missing)}")
    for execution in EXECUTIONS:
        if max_value(history, execution, "exec_rpc_rx", "seq") < 0:
            raise AssertionError(f"{stage}: missing execution RPC receive for {execution}")
        if max_value(history, execution, "exec_rpc_ack", "ack") < 0:
            raise AssertionError(f"{stage}: missing execution RPC ack for {execution}")
    for validator in VALIDATORS:
        if max_value(history, validator, "validator_rpc_tx", "seq") < 0:
            raise AssertionError(f"{stage}: missing validator tx for {validator}")
        if max_value(history, validator, "validator_rpc_rx_ack", "ack") < 0:
            raise AssertionError(f"{stage}: missing validator ack for {validator}")


def require_progress(
    before_events: list[dict],
    after_events: list[dict],
    stage: str,
) -> None:
    before = metrics(before_events)
    after = metrics(after_events)
    for role in BEACONS:
        before_exec_tx, before_exec_ack, before_p2p_tx, before_udp_ack = before[role]
        after_exec_tx, after_exec_ack, after_p2p_tx, after_udp_ack = after[role]
        if (
            after_exec_tx <= before_exec_tx
            or after_exec_ack <= before_exec_ack
            or after_p2p_tx <= before_p2p_tx
            or after_udp_ack <= before_udp_ack
        ):
            raise AssertionError(
                f"{stage}: insufficient progress for {role} "
                f"(exec {before_exec_tx}->{after_exec_tx}/{before_exec_ack}->{after_exec_ack}, "
                f"p2p {before_p2p_tx}->{after_p2p_tx}, udp {before_udp_ack}->{after_udp_ack})"
            )
    for validator in VALIDATORS:
        before_tx = max_value(before_events, validator, "validator_rpc_tx", "seq")
        before_ack = max_value(before_events, validator, "validator_rpc_rx_ack", "ack")
        after_tx = max_value(after_events, validator, "validator_rpc_tx", "seq")
        after_ack = max_value(after_events, validator, "validator_rpc_rx_ack", "ack")
        if after_tx <= before_tx or after_ack <= before_ack:
            raise AssertionError(
                f"{stage}: validator RPC did not progress for {validator} "
                f"(tx {before_tx}->{after_tx}, ack {before_ack}->{after_ack})"
            )


def run_verify(args: argparse.Namespace) -> None:
    session = launch_shadow(
        shadow_bin=args.shadow_bin,
        config_path=Path(args.config).expanduser(),
        work_dir=Path(args.work_dir).expanduser().resolve(),
        socket_path=(
            Path(args.socket_path)
            if args.socket_path
            else Path(f"/tmp/ethmp-{os.getpid()}.sock")
        ),
        clean_data=args.clean_data,
    )

    verify_failed = False
    try:
        work_dir = Path(args.work_dir).expanduser().resolve()
        ensure_connected(session, args.connect_timeout)
        offsets: dict[str, int] = {}

        history_events = continue_and_collect(
            session, work_dir, offsets, 14 * 1_000_000_000, args.response_timeout
        )
        if not history_events:
            raise AssertionError("no warmup NETLOG events observed")
        require_metrics_present(history_events, "warmup")

        assert session.sock is not None
        cp_resp = send_command(session.sock, {"cmd": "checkpoint", "label": args.verify_label}, args.response_timeout)
        expect_ok(cp_resp, "checkpoint")
        verify_checkpoint(checkpoint_path(work_dir, args.verify_label))

        history_events_before = list(history_events)
        advance = continue_and_collect(
            session, work_dir, offsets, 8 * 1_000_000_000, args.response_timeout
        )
        history_events.extend(advance)
        history_events_after = list(history_events)
        require_progress(history_events_before, history_events_after, "advance")

        restore_resp = send_restore_with_reconnect(
            session, args.verify_label, args.connect_timeout, args.response_timeout
        )
        expect_ok(restore_resp, "restore")

        post_events: list[dict] = []
        for _ in range(args.post_restore_steps):
            post_events.extend(
                continue_and_collect(
                    session,
                    work_dir,
                    offsets,
                    args.post_restore_step_ns,
                    args.response_timeout,
                )
            )
        if not post_events:
            raise AssertionError("no NETLOG events observed after restore")

        require_progress(history_events_before, post_events, "post-restore")

        churn = churn_count(post_events)
        if churn > 0:
            raise AssertionError(f"post-restore observed reconnect churn count={churn}")

        min_required_span = args.post_restore_step_ns * max(args.post_restore_steps // 2, 1)
        bad_spans: dict[str, int] = {}
        time_events = {
            "exec_rpc_tx",
            "exec_rpc_rx",
            "exec_rpc_ack",
            "exec_rpc_rx_ack",
            "p2p_tx",
            "p2p_rx",
            "p2p_ack",
            "p2p_rx_ack",
            "udp_tx",
            "udp_rx",
            "udp_rx_ack",
            "validator_rpc_tx",
            "validator_rpc_rx",
            "validator_rpc_ack",
            "validator_rpc_rx_ack",
        }
        for role in (*EXECUTIONS, *BEACONS, *VALIDATORS):
            span = mono_span(post_events, role, time_events)
            if span < min_required_span:
                bad_spans[role] = span
        if bad_spans:
            raise AssertionError(f"post-restore time spans too small: {bad_spans}")

        print("[verify] PASS: multi-process Ethereum-like traffic recovered across checkpoint/restore")
    except AssertionError as exc:
        print(f"[verify] FAIL: {exc}", file=sys.stderr)
        verify_failed = True
    finally:
        if session.sock is not None:
            try:
                session.sock.close()
            except OSError:
                pass
        if session.process.poll() is None:
            session.process.terminate()
            try:
                session.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                session.process.kill()
                session.process.wait(timeout=10)
        if session.process.stderr is not None:
            stderr = session.process.stderr.read()
            if stderr:
                lines = stderr.decode("utf-8", errors="replace").splitlines()
                print(f"[shadow stderr tail] ({len(lines)} lines total)")
                for line in lines[-120:]:
                    print(line)
        print(f"[orchestrator] shadow exit code: {session.process.returncode or 0}")

    if verify_failed:
        raise SystemExit(1)


def main() -> int:
    args = parse_args()
    run_verify(args)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
