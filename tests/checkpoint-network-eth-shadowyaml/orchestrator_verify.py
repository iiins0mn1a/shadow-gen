#!/usr/bin/env python3
"""Verifier for the high-fidelity Ethereum-like Shadow checkpoint test."""

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


EXECUTION = "geth-node"
BEACONS = tuple(f"beacon-{idx}" for idx in range(1, 5))
RECORDERS = tuple(f"recorder-{idx}" for idx in range(1, 5))
VALIDATORS = tuple(f"validator-{idx}" for idx in range(1, 5))
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
    parser = argparse.ArgumentParser(description="Verify high-fidelity Ethereum-like cp/restore")
    parser.add_argument("--shadow-bin", default=shutil.which("shadow") or "shadow")
    parser.add_argument(
        "--config",
        default=str(Path(__file__).resolve().parent / "shadow_eth_shadowyaml.yaml"),
    )
    parser.add_argument(
        "--work-dir",
        default=str(Path(__file__).resolve().parent / "run"),
    )
    parser.add_argument("--socket-path", default="")
    parser.add_argument("--connect-timeout", type=float, default=60.0)
    parser.add_argument("--response-timeout", type=float, default=180.0)
    parser.add_argument("--clean-data", action="store_true")
    parser.add_argument("--verify-label", default="cp_eth_shadowyaml_verify")
    parser.add_argument("--scenario", choices=("stable", "peer-bootstrap"), default="stable")
    parser.add_argument("--post-restore-step-ns", type=int, default=1_000_000_000)
    parser.add_argument("--post-restore-steps", type=int, default=16)
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


def resolve_config(config_path: Path, work_dir: Path) -> tuple[Path, Path]:
    rendered = work_dir / "rendered.shadow.yaml"
    app_path = (Path(__file__).resolve().parent / "eth_shadowyaml_app.py").resolve()
    shared_dir = (work_dir / "shared").resolve()
    shared_dir.mkdir(parents=True, exist_ok=True)
    text = config_path.resolve().read_text(encoding="utf-8")
    text = text.replace("__APP_PATH__", str(app_path))
    text = text.replace("__SHARED_DIR__", str(shared_dir))
    rendered.write_text(text, encoding="utf-8")
    return rendered, shared_dir


def launch_shadow(
    shadow_bin: str,
    config_path: Path,
    work_dir: Path,
    socket_path: Path,
    clean_data: bool,
) -> tuple[ShadowSession, Path]:
    work_dir.mkdir(parents=True, exist_ok=True)
    shadow_data = work_dir / "shadow.data"
    shared_dir = work_dir / "shared"
    if clean_data:
        if shadow_data.exists():
            shutil.rmtree(shadow_data)
        if shared_dir.exists():
            shutil.rmtree(shared_dir)
    if socket_path.exists():
        socket_path.unlink()
    rendered, shared_dir = resolve_config(config_path, work_dir)
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
    return ShadowSession(process=proc, socket_path=socket_path), shared_dir


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
    if not isinstance(hosts, list) or len(hosts) != 9:
        raise AssertionError(f"expected 9 hosts in checkpoint, got {hosts!r}")
    process_counts = {
        host.get("hostname") or host.get("name"): len(host.get("processes", [])) for host in hosts
    }
    if process_counts.get("geth-node") != 1:
        raise AssertionError(f"unexpected geth-node process count: {process_counts.get('geth-node')}")
    for beacon_host in (f"prysm-beacon-{idx}" for idx in range(1, 5)):
        if process_counts.get(beacon_host) != 2:
            raise AssertionError(f"unexpected beacon-host process count for {beacon_host}: {process_counts.get(beacon_host)}")
    for validator_host in (f"prysm-validator-{idx}" for idx in range(1, 5)):
        if process_counts.get(validator_host) != 1:
            raise AssertionError(
                f"unexpected validator-host process count for {validator_host}: {process_counts.get(validator_host)}"
            )


def read_peer_lines(shared_dir: Path) -> list[str]:
    path = shared_dir / "beacon_peers.txt"
    if not path.exists():
        return []
    return [line.strip() for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def backup_shared_dir(shared_dir: Path, label: str) -> Path:
    backup_dir = shared_dir.parent / f"{shared_dir.name}.backup.{label}"
    if backup_dir.exists():
        shutil.rmtree(backup_dir)
    if shared_dir.exists():
        shutil.copytree(shared_dir, backup_dir)
    else:
        backup_dir.mkdir(parents=True, exist_ok=True)
    return backup_dir


def restore_shared_dir(shared_dir: Path, backup_dir: Path) -> None:
    if shared_dir.exists():
        shutil.rmtree(shared_dir)
    shutil.copytree(backup_dir, shared_dir)


def role_event_count(events: list[dict], role: str, wanted: set[str]) -> int:
    return sum(1 for item in events if item.get("role") == role and item.get("event") in wanted)


def max_value(events: list[dict], role: str, wanted: set[str], key: str) -> int:
    best = -1
    for item in events:
        if item.get("role") != role or item.get("event") not in wanted:
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


def require_peer_file(shared_dir: Path, expected_count: int, stage: str) -> list[str]:
    lines = read_peer_lines(shared_dir)
    if len(lines) < expected_count:
        raise AssertionError(f"{stage}: expected at least {expected_count} peer-file lines, got {lines!r}")
    return lines


def require_warmup_ready(events: list[dict], shared_dir: Path, scenario: str) -> None:
    expected_peer_lines = 4 if scenario == "stable" else 2
    require_peer_file(shared_dir, expected_peer_lines, "warmup")

    if role_event_count(events, EXECUTION, {"exec_rpc_rx", "exec_rpc_ack"}) < 4:
        raise AssertionError("warmup: execution endpoint did not receive enough beacon RPC traffic")

    ready_beacons = 4 if scenario == "stable" else 2
    for idx, beacon in enumerate(BEACONS, start=1):
        if idx > ready_beacons:
            continue
        if role_event_count(events, beacon, {"peer_publish", "peer_catalog"}) == 0:
            raise AssertionError(f"warmup: beacon peer-file flow missing for {beacon}")
        if role_event_count(events, beacon, {"exec_rpc_tx", "exec_rpc_rx_ack"}) < 2:
            raise AssertionError(f"warmup: execution RPC missing for {beacon}")
        if role_event_count(events, beacon, {"validator_rpc_rx", "validator_rpc_ack"}) < 2:
            raise AssertionError(f"warmup: validator RPC missing for {beacon}")
        if idx >= 2 and role_event_count(events, beacon, {"p2p_tx", "p2p_rx", "p2p_ack", "p2p_rx_ack"}) < 2:
            raise AssertionError(f"warmup: p2p traffic missing for {beacon}")
        if idx >= 2 and role_event_count(events, beacon, {"udp_tx", "udp_rx", "udp_rx_ack"}) < 2:
            raise AssertionError(f"warmup: udp traffic missing for {beacon}")

    for idx, validator in enumerate(VALIDATORS, start=1):
        if idx > ready_beacons:
            continue
        if role_event_count(events, validator, {"validator_rpc_tx", "validator_rpc_rx_ack"}) < 2:
            raise AssertionError(f"warmup: validator RPC missing for {validator}")

    for idx, recorder in enumerate(RECORDERS, start=1):
        if idx > ready_beacons:
            continue
        if role_event_count(events, recorder, {"peer_recorded", "peer_file_count"}) == 0:
            raise AssertionError(f"warmup: recorder flow missing for {recorder}")


def require_post_restore_progress(
    baseline_events: list[dict],
    post_events: list[dict],
    scenario: str,
) -> None:
    ready_beacons = 4 if scenario == "stable" else 2
    for idx, beacon in enumerate(BEACONS, start=1):
        if idx > ready_beacons:
            continue
        before_exec_tx = max_value(baseline_events, beacon, {"exec_rpc_tx"}, "seq")
        after_exec_tx = max_value(post_events, beacon, {"exec_rpc_tx"}, "seq")
        before_exec_ack = max_value(baseline_events, beacon, {"exec_rpc_rx_ack"}, "ack")
        after_exec_ack = max_value(post_events, beacon, {"exec_rpc_rx_ack"}, "ack")
        if after_exec_tx <= before_exec_tx or after_exec_ack <= before_exec_ack:
            raise AssertionError(f"post-restore: execution RPC did not progress for {beacon}")

        before_validator_rx = max_value(baseline_events, beacon, {"validator_rpc_rx"}, "seq")
        after_validator_rx = max_value(post_events, beacon, {"validator_rpc_rx"}, "seq")
        before_validator_ack = max_value(baseline_events, beacon, {"validator_rpc_ack"}, "ack")
        after_validator_ack = max_value(post_events, beacon, {"validator_rpc_ack"}, "ack")
        if after_validator_rx <= before_validator_rx or after_validator_ack <= before_validator_ack:
            raise AssertionError(f"post-restore: validator RPC did not progress for {beacon}")

        if idx == 1:
            before_p2p_rx = max_value(baseline_events, beacon, {"p2p_rx"}, "seq")
            after_p2p_rx = max_value(post_events, beacon, {"p2p_rx"}, "seq")
            before_udp_rx = max_value(baseline_events, beacon, {"udp_rx"}, "seq")
            after_udp_rx = max_value(post_events, beacon, {"udp_rx"}, "seq")
            if after_p2p_rx <= before_p2p_rx:
                raise AssertionError(f"post-restore: inbound p2p did not progress for {beacon}")
            if after_udp_rx <= before_udp_rx:
                raise AssertionError(f"post-restore: inbound udp did not progress for {beacon}")
        else:
            before_p2p_tx = max_value(baseline_events, beacon, {"p2p_tx"}, "seq")
            after_p2p_tx = max_value(post_events, beacon, {"p2p_tx"}, "seq")
            before_p2p_ack = max_value(post_events if False else baseline_events, beacon, {"p2p_rx_ack", "p2p_ack"}, "ack")
            after_p2p_ack = max_value(post_events, beacon, {"p2p_rx_ack", "p2p_ack"}, "ack")
            if after_p2p_tx <= before_p2p_tx or after_p2p_ack <= before_p2p_ack:
                raise AssertionError(f"post-restore: p2p traffic did not progress for {beacon}")

            before_udp_tx = max_value(baseline_events, beacon, {"udp_tx"}, "seq")
            after_udp_tx = max_value(post_events, beacon, {"udp_tx"}, "seq")
            before_udp_ack = max_value(baseline_events, beacon, {"udp_rx_ack"}, "ack")
            after_udp_ack = max_value(post_events, beacon, {"udp_rx_ack"}, "ack")
            if after_udp_tx <= before_udp_tx or after_udp_ack <= before_udp_ack:
                raise AssertionError(f"post-restore: udp traffic did not progress for {beacon}")

    for idx, validator in enumerate(VALIDATORS, start=1):
        if idx > ready_beacons:
            continue
        before_tx = max_value(baseline_events, validator, {"validator_rpc_tx"}, "seq")
        after_tx = max_value(post_events, validator, {"validator_rpc_tx"}, "seq")
        before_ack = max_value(baseline_events, validator, {"validator_rpc_rx_ack"}, "ack")
        after_ack = max_value(post_events, validator, {"validator_rpc_rx_ack"}, "ack")
        if after_tx <= before_tx or after_ack <= before_ack:
            raise AssertionError(f"post-restore: validator traffic did not progress for {validator}")

    for idx, recorder in enumerate(RECORDERS, start=1):
        if idx > ready_beacons:
            continue
        after = role_event_count(post_events, recorder, {"recorder_tick", "peer_file_count"})
        if after == 0:
            raise AssertionError(f"post-restore: recorder did not resume for {recorder}")


def require_time_progress(post_events: list[dict], step_ns: int, steps: int, scenario: str) -> None:
    min_required_span = step_ns * max(steps // 2, 1)
    time_events = {
        "execution_tick",
        "beacon_tick",
        "validator_tick",
        "recorder_tick",
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
        "peer_file_count",
    }
    bad: dict[str, int] = {}
    roles = [EXECUTION, *BEACONS, *RECORDERS, *VALIDATORS]
    if scenario != "stable":
        roles = [EXECUTION, *BEACONS[:2], *RECORDERS[:2], *VALIDATORS[:2]]
    for role in roles:
        span = mono_span(post_events, role, time_events)
        if span < min_required_span:
            bad[role] = span
    if bad:
        raise AssertionError(f"post-restore time spans too small: {bad}")


def run_verify(args: argparse.Namespace) -> None:
    session, shared_dir = launch_shadow(
        shadow_bin=args.shadow_bin,
        config_path=Path(args.config).expanduser(),
        work_dir=Path(args.work_dir).expanduser().resolve(),
        socket_path=Path(args.socket_path) if args.socket_path else Path(f"/tmp/ethshadow-{os.getpid()}.sock"),
        clean_data=args.clean_data,
    )

    verify_failed = False
    try:
        work_dir = Path(args.work_dir).expanduser().resolve()
        ensure_connected(session, args.connect_timeout)
        offsets: dict[str, int] = {}

        warmup_ns = 30 * 1_000_000_000 if args.scenario == "stable" else 18 * 1_000_000_000
        history_events = continue_and_collect(session, work_dir, offsets, warmup_ns, args.response_timeout)
        if not history_events:
            raise AssertionError("no warmup NETLOG events observed")
        require_warmup_ready(history_events, shared_dir, args.scenario)

        backup_dir = backup_shared_dir(shared_dir, args.verify_label)

        assert session.sock is not None
        cp_resp = send_command(session.sock, {"cmd": "checkpoint", "label": args.verify_label}, args.response_timeout)
        expect_ok(cp_resp, "checkpoint")
        verify_checkpoint(checkpoint_path(work_dir, args.verify_label))

        baseline_events = list(history_events)
        advance_events = continue_and_collect(session, work_dir, offsets, 8 * 1_000_000_000, args.response_timeout)
        history_events.extend(advance_events)

        restore_shared_dir(shared_dir, backup_dir)
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

        require_post_restore_progress(baseline_events, post_events, args.scenario)
        require_time_progress(post_events, args.post_restore_step_ns, args.post_restore_steps, args.scenario)
        require_peer_file(shared_dir, 4 if args.scenario == "stable" else 2, "post-restore")

        churn = churn_count(post_events)
        if churn > 0:
            raise AssertionError(f"post-restore observed reconnect churn count={churn}")

        print(
            f"[verify] PASS: high-fidelity Ethereum-like synthetic traffic recovered across checkpoint/restore (scenario={args.scenario})"
        )
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
