#!/usr/bin/env python3
"""Automated multi-host network checkpoint/restore verifier for Shadow."""

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


MetricSpec = tuple[str, str, str]
CountSpec = tuple[str, str]

FULL_BASELINE_SPECS: dict[str, MetricSpec] = {
    "tcp_tx": ("tcp_client", "tcp_tx", "seq"),
    "tcp_ack": ("tcp_client", "tcp_rx_ack", "ack"),
    "udp_a_tx": ("udp_peer_a", "udp_tx", "seq"),
    "udp_b_tx": ("udp_peer_b", "udp_tx", "seq"),
}

TCP_BASELINE_SPECS: dict[str, MetricSpec] = {
    "tcp_tx": ("tcp_client", "tcp_tx", "seq"),
    "tcp_ack": ("tcp_client", "tcp_rx_ack", "ack"),
    "srv_rx": ("tcp_server", "tcp_rx", "seq"),
    "srv_ack": ("tcp_server", "tcp_ack", "ack"),
}

FULL_POST_SPECS: dict[str, MetricSpec] = {
    **FULL_BASELINE_SPECS,
    "udp_a_rx": ("udp_peer_a", "udp_rx", "seq"),
    "udp_b_rx": ("udp_peer_b", "udp_rx", "seq"),
}

TCP_CHURN_COUNT_SPECS: dict[str, CountSpec] = {
    "connect_ok": ("tcp_client", "tcp_connect_ok"),
    "accept": ("tcp_server", "tcp_accept"),
    "retry": ("tcp_client", "tcp_connect_retry"),
    "io_error": ("tcp_client", "tcp_io_error"),
    "disconnect": ("tcp_server", "tcp_disconnect"),
}

TCP_STEP_COUNT_SPECS: dict[str, CountSpec] = {
    "step_tcp_tx": ("tcp_client", "tcp_tx"),
    "step_tcp_rx_ack": ("tcp_client", "tcp_rx_ack"),
    "step_tcp_server_rx": ("tcp_server", "tcp_rx"),
    "step_tcp_server_ack": ("tcp_server", "tcp_ack"),
}


@dataclass
class ShadowSession:
    process: subprocess.Popen
    socket_path: Path
    sock: Optional[socket.socket] = None


@dataclass
class VerifyPrep:
    offsets: dict[str, int]
    cp_metrics: dict[str, int]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Checkpoint/restore network verifier")
    parser.add_argument(
        "--shadow-bin",
        default=shutil.which("shadow") or "shadow",
        help="Path to shadow binary (default: shadow from PATH)",
    )
    parser.add_argument(
        "--config",
        default=str(Path(__file__).resolve().parent / "shadow_network.yaml"),
        help="Path to shadow config (yaml).",
    )
    parser.add_argument(
        "--work-dir",
        default=str(Path(__file__).resolve().parent / "run"),
        help="Working directory for running Shadow",
    )
    parser.add_argument(
        "--socket-path",
        default=f"/tmp/shadow_network_control_{os.getpid()}.sock",
        help="Unix socket path for SHADOW_CONTROL_SOCKET",
    )
    parser.add_argument(
        "--connect-timeout",
        type=float,
        default=60.0,
        help="Socket connect timeout in seconds",
    )
    parser.add_argument(
        "--response-timeout",
        type=float,
        default=120.0,
        help="Socket response timeout in seconds",
    )
    parser.add_argument(
        "--clean-data",
        action="store_true",
        help="Remove work-dir/shadow.data before launch",
    )
    parser.add_argument(
        "--verify-label",
        default="cp_network_verify",
        help="Checkpoint label used by this verifier",
    )
    parser.add_argument(
        "--diagnostics",
        action="store_true",
        help="Enable heavy restore diagnostics (tcp/proc dumps)",
    )
    parser.add_argument(
        "--mode",
        choices=("full", "tcp"),
        default="full",
        help="Verification mode: full=TCP+UDP, tcp=focus on TCP restore only",
    )
    parser.add_argument(
        "--post-restore-step-ns",
        type=int,
        default=1_000_000_000,
        help="In tcp mode, step size for post-restore continue_for",
    )
    parser.add_argument(
        "--post-restore-steps",
        type=int,
        default=10,
        help="In tcp mode, number of post-restore steps to execute",
    )
    parser.add_argument(
        "--strict-tcp-time",
        action="store_true",
        help="In tcp mode, fail if post-restore app monotonic timestamps do not span the stepped window",
    )
    return parser.parse_args()


def wait_for_socket(
    path: Path,
    timeout_sec: float,
    process: Optional[subprocess.Popen] = None,
) -> socket.socket:
    start = time.time()
    while time.time() - start < timeout_sec:
        if process is not None and process.poll() is not None:
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
    raise TimeoutError(f"Socket {path} not available within {timeout_sec}s")


def send_command(sock: socket.socket, cmd: dict, timeout_sec: float) -> dict:
    payload = (json.dumps(cmd) + "\n").encode("utf-8")
    print(f"  -> {payload.decode('utf-8').strip()}")
    sock.sendall(payload)
    sock.settimeout(timeout_sec)

    buf = b""
    while b"\n" not in buf:
        data = sock.recv(4096)
        if not data:
            raise ConnectionError("Socket closed by Shadow")
        buf += data

    resp = json.loads(buf.split(b"\n", 1)[0].decode("utf-8"))
    print(f"  <- {json.dumps(resp)}")
    return resp


def expect_ok(resp: dict, what: str) -> None:
    if resp.get("status") != "ok":
        raise AssertionError(f"{what}: expected status ok, got {resp!r}")


def checkpoint_json_path(work_dir: Path, label: str) -> Path:
    return work_dir / "shadow.data" / "checkpoints" / f"{label}.checkpoint.json"


def verify_shadow_checkpoint_reflects_hosts(path: Path, min_hosts: int) -> dict[str, object]:
    if not path.is_file():
        raise AssertionError(f"checkpoint file missing: {path}")

    raw = json.loads(path.read_text(encoding="utf-8"))
    hosts = raw.get("hosts")
    if not isinstance(hosts, list) or len(hosts) < min_hosts:
        raise AssertionError(f"expected >= {min_hosts} hosts, got {hosts!r}")

    hostnames: list[str] = []
    for host in hosts:
        if not isinstance(host, dict):
            raise AssertionError(f"invalid host entry: {host!r}")
        name = host.get("hostname")
        if not isinstance(name, str) or not name:
            raise AssertionError(f"host missing hostname: {host!r}")
        if "event_queue" not in host or "processes" not in host:
            raise AssertionError(f"host {name!r} missing event_queue or processes")
        hostnames.append(name)

    return {
        "path": str(path),
        "host_count": len(hosts),
        "hostnames": hostnames,
        "sim_time_ns": raw.get("sim_time_ns"),
    }


def dump_native_tcp_state(work_dir: Path, label: str) -> None:
    cp = checkpoint_json_path(work_dir, label)
    if not cp.exists():
        return

    try:
        raw = json.loads(cp.read_text(encoding="utf-8"))
    except Exception as exc:
        print(f"[diag] unable to parse checkpoint json for tcp dump: {exc}")
        return

    pids: set[int] = set()
    for host in raw.get("hosts", []):
        for proc in host.get("processes", []):
            if proc.get("is_running") and isinstance(proc.get("native_pid"), int):
                pids.add(proc["native_pid"])

    print(f"[diag] native running pids from checkpoint: {sorted(pids)}")
    for pid in sorted(pids):
        tcp_path = Path(f"/proc/{pid}/net/tcp")
        if not tcp_path.exists():
            print(f"[diag] /proc/{pid}/net/tcp missing")
            continue
        try:
            lines = tcp_path.read_text(encoding="utf-8", errors="replace").splitlines()
            print(f"[diag] /proc/{pid}/net/tcp lines={len(lines)}")
            for line in lines[:8]:
                print(f"[diag] {line}")
        except Exception as exc:
            print(f"[diag] read /proc/{pid}/net/tcp failed: {exc}")

    try:
        ss = subprocess.run(
            ["ss", "-tanp"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        lines = ss.stdout.decode("utf-8", errors="replace").splitlines()
        print(f"[diag] ss -tanp lines={len(lines)}")
        for line in lines[:30]:
            print(f"[diag] {line}")
    except Exception as exc:
        print(f"[diag] ss -tanp failed: {exc}")


def dump_shadow_children_state(shadow_pid: int) -> None:
    try:
        ps = subprocess.run(
            ["ps", "-o", "pid=,ppid=,stat=,cmd=", "-ax"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        lines = ps.stdout.decode("utf-8", errors="replace").splitlines()
    except Exception as exc:
        print(f"[diag] ps failed: {exc}")
        return

    children: list[tuple[int, int, str, str]] = []
    for line in lines:
        parts = line.strip().split(None, 3)
        if len(parts) < 4:
            continue
        try:
            pid = int(parts[0])
            ppid = int(parts[1])
        except ValueError:
            continue
        if ppid == shadow_pid:
            children.append((pid, ppid, parts[2], parts[3]))

    print(f"[diag] shadow children (ppid={shadow_pid}): {len(children)}")
    for pid, ppid, stat, cmd in children:
        print(f"[diag] child pid={pid} ppid={ppid} stat={stat} cmd={cmd}")
        status_path = Path(f"/proc/{pid}/status")
        wchan_path = Path(f"/proc/{pid}/wchan")
        cmdline_path = Path(f"/proc/{pid}/cmdline")

        if status_path.exists():
            try:
                status_lines = status_path.read_text(
                    encoding="utf-8", errors="replace"
                ).splitlines()
                keep = [
                    line
                    for line in status_lines
                    if line.startswith(("Name:", "State:", "Tgid:", "Pid:", "PPid:", "Threads:"))
                ]
                for line in keep:
                    print(f"[diag]   {line}")
            except Exception as exc:
                print(f"[diag]   read status failed: {exc}")

        if wchan_path.exists():
            try:
                print(
                    f"[diag]   wchan={wchan_path.read_text(encoding='utf-8', errors='replace').strip()}"
                )
            except Exception as exc:
                print(f"[diag]   read wchan failed: {exc}")

        if cmdline_path.exists():
            try:
                cmdline = (
                    cmdline_path.read_bytes()
                    .replace(b"\x00", b" ")
                    .decode("utf-8", errors="replace")
                    .strip()
                )
                print(f"[diag]   cmdline={cmdline}")
            except Exception as exc:
                print(f"[diag]   read cmdline failed: {exc}")

        try:
            out = Path(f"/tmp/shadow_udp_diag_strace_{pid}.log")
            proc = subprocess.Popen(
                ["strace", "-f", "-tt", "-p", str(pid), "-o", str(out)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            time.sleep(1.2)
            proc.terminate()
            try:
                proc.wait(timeout=1.0)
            except subprocess.TimeoutExpired:
                proc.kill()
            if out.exists():
                lines = out.read_text(encoding="utf-8", errors="replace").splitlines()
                print(f"[diag]   strace_lines={len(lines)}")
                for line in lines[-5:]:
                    print(f"[diag]   strace_tail {line}")
        except Exception as exc:
            print(f"[diag]   strace failed: {exc}")


def resolve_config(config_path: Path, work_dir: Path) -> Path:
    rendered = work_dir / "rendered.shadow.yaml"
    net_app = (Path(__file__).resolve().parent / "net_app.py").resolve()
    text = config_path.resolve().read_text(encoding="utf-8").replace(
        "__NET_APP_PATH__", str(net_app)
    )
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
        if not Path(shadow_bin_resolved).exists():
            raise FileNotFoundError(f"shadow binary not found: {shadow_bin_resolved}")

    proc = subprocess.Popen(
        [shadow_bin_resolved, str(rendered)],
        cwd=str(work_dir),
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=False,
    )
    return ShadowSession(process=proc, socket_path=socket_path, sock=None)


def ensure_connected(session: ShadowSession, timeout_sec: float) -> None:
    if session.sock is None:
        session.sock = wait_for_socket(
            session.socket_path,
            timeout_sec,
            process=session.process,
        )
        print(f"[orchestrator] connected: {session.socket_path}")


def reconnect_if_needed(session: ShadowSession, timeout_sec: float) -> None:
    if session.sock is not None:
        try:
            session.sock.close()
        except OSError:
            pass
        session.sock = None
    ensure_connected(session, timeout_sec)


def send_restore_with_reconnect(
    session: ShadowSession,
    label: str,
    connect_timeout: float,
    response_timeout: float,
) -> dict:
    try:
        assert session.sock is not None
        return send_command(
            session.sock,
            {"cmd": "restore", "label": label},
            response_timeout,
        )
    except ConnectionError:
        print("[orchestrator] restore closed socket; reconnecting...")
        reconnect_if_needed(session, connect_timeout)
        assert session.sock is not None
        return send_command(session.sock, {"cmd": "status"}, response_timeout)


def _stderr_mentions_unrecognized_unprivileged(stderr: str) -> bool:
    return ("unrecognized option '--unprivileged'" in stderr) or (
        'unrecognized option "--unprivileged"' in stderr
    )


def criu_preflight() -> tuple[bool, str]:
    criu_bin = os.environ.get("CRIU_BIN", "criu")

    def run(args: list[str]) -> subprocess.CompletedProcess[bytes]:
        return subprocess.run(
            args,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

    check = run([criu_bin, "check", "--unprivileged"])
    if check.returncode == 0:
        return True, "criu check --unprivileged: ok"

    stderr = check.stderr.decode("utf-8", errors="replace")
    if _stderr_mentions_unrecognized_unprivileged(stderr):
        fallback = run([criu_bin, "check"])
        if fallback.returncode == 0:
            return True, "criu check: ok (no --unprivileged support)"
        fallback_stderr = fallback.stderr.decode("utf-8", errors="replace")
        return False, f"criu check failed: {fallback_stderr.strip()}"

    return False, f"criu check --unprivileged failed: {stderr.strip()}"


def discover_stdout_files(work_dir: Path) -> list[Path]:
    return sorted((work_dir / "shadow.data" / "hosts").glob("*/*.stdout"))


def collect_new_events(work_dir: Path, offsets: dict[str, int]) -> list[dict]:
    events: list[dict] = []
    for path in discover_stdout_files(work_dir):
        key = str(path)
        old_offset = offsets.get(key, 0)
        try:
            with path.open("rb") as handle:
                handle.seek(old_offset)
                data = handle.read()
                offsets[key] = old_offset + len(data)
        except OSError:
            continue

        if not data:
            continue

        for raw_line in data.decode("utf-8", errors="replace").splitlines():
            if not raw_line.startswith("NETLOG "):
                continue
            try:
                payload = json.loads(raw_line[len("NETLOG ") :].strip())
            except json.JSONDecodeError:
                continue
            if isinstance(payload, dict):
                events.append(payload)

    return events


def max_seq(events: list[dict], *, role: str, event: str, key: str) -> int:
    best = -1
    for item in events:
        if item.get("role") != role or item.get("event") != event:
            continue
        value = item.get(key)
        if isinstance(value, int) and value > best:
            best = value
    return best


def count_events(events: list[dict], *, role: str, event: str) -> int:
    return sum(1 for item in events if item.get("role") == role and item.get("event") == event)


def event_values(events: list[dict], *, role: str, event: str, key: str) -> list[int]:
    values: list[int] = []
    for item in events:
        if item.get("role") != role or item.get("event") != event:
            continue
        value = item.get(key)
        if isinstance(value, int):
            values.append(value)
    return values


def value_span(values: list[int]) -> int:
    if len(values) < 2:
        return 0
    return max(values) - min(values)


def collect_metric_map(events: list[dict], specs: dict[str, MetricSpec]) -> dict[str, int]:
    return {
        name: max_seq(events, role=role, event=event, key=key)
        for name, (role, event, key) in specs.items()
    }


def collect_count_map(events: list[dict], specs: dict[str, CountSpec]) -> dict[str, int]:
    return {
        name: count_events(events, role=role, event=event)
        for name, (role, event) in specs.items()
    }


def require_event_progress(name: str, before: int, after: int) -> None:
    if after <= before:
        raise AssertionError(f"{name}: expected progress, got before={before}, after={after}")


def require_all_present(metrics: dict[str, int], names: list[str], what: str) -> None:
    missing = [name for name in names if metrics.get(name, -1) < 0]
    if missing:
        raise AssertionError(f"{what}: missing metrics {missing}")


def log_preflight() -> None:
    ok, msg = criu_preflight()
    if not ok:
        raise AssertionError(
            "CRIU is not usable in this environment; cannot verify full process checkpoint/restore. "
            f"Preflight: {msg}"
        )
    print(f"[verify] CRIU preflight: {msg} (CRIU_BIN={os.environ.get('CRIU_BIN')!r})")


def continue_and_collect(
    session: ShadowSession,
    work_dir: Path,
    offsets: dict[str, int],
    duration_ns: int,
    response_timeout: float,
    label: str,
) -> list[dict]:
    assert session.sock is not None
    resp = send_command(session.sock, {"cmd": "continue_for", "duration_ns": duration_ns}, response_timeout)
    expect_ok(resp, label)
    return collect_new_events(work_dir, offsets)


def perform_restore_prep(
    *,
    session: ShadowSession,
    work_dir: Path,
    label: str,
    connect_timeout: float,
    response_timeout: float,
    diagnostics: bool,
    metric_specs: dict[str, MetricSpec],
    required_metrics: list[str],
    log_prefix: str,
) -> VerifyPrep:
    log_preflight()
    ensure_connected(session, connect_timeout)

    offsets: dict[str, int] = {}
    history: list[dict] = []

    print(f"{log_prefix} warmup continue_for 10s")
    warmup_events = continue_and_collect(
        session,
        work_dir,
        offsets,
        10 * 1_000_000_000,
        response_timeout,
        "warmup continue_for",
    )
    history.extend(warmup_events)
    if not warmup_events:
        raise AssertionError("no NETLOG events observed during warmup")

    print(f"{log_prefix} checkpoint")
    assert session.sock is not None
    checkpoint_resp = send_command(
        session.sock,
        {"cmd": "checkpoint", "label": label},
        response_timeout,
    )
    expect_ok(checkpoint_resp, "checkpoint")
    summary = verify_shadow_checkpoint_reflects_hosts(
        checkpoint_json_path(work_dir, label), min_hosts=2
    )
    print(f"{log_prefix} checkpoint summary: {summary}")

    cp_metrics = collect_metric_map(history, metric_specs)
    require_all_present(cp_metrics, required_metrics, "warmup did not produce expected communication events")

    print(f"{log_prefix} advance continue_for 10s")
    advance_events = continue_and_collect(
        session,
        work_dir,
        offsets,
        10 * 1_000_000_000,
        response_timeout,
        "advance continue_for",
    )
    history.extend(advance_events)
    dirty_metrics = collect_metric_map(history, metric_specs)
    for name in required_metrics:
        require_event_progress(name, cp_metrics[name], dirty_metrics[name])

    print(f"{log_prefix} restore checkpoint")
    restore_resp = send_restore_with_reconnect(
        session,
        label,
        connect_timeout,
        response_timeout,
    )
    expect_ok(restore_resp, "restore")
    if diagnostics:
        dump_native_tcp_state(work_dir, label)
        dump_shadow_children_state(session.process.pid)

    return VerifyPrep(offsets=offsets, cp_metrics=cp_metrics)


def run_verify(
    session: ShadowSession,
    work_dir: Path,
    label: str,
    connect_timeout: float,
    response_timeout: float,
    diagnostics: bool,
) -> None:
    prep = perform_restore_prep(
        session=session,
        work_dir=work_dir,
        label=label,
        connect_timeout=connect_timeout,
        response_timeout=response_timeout,
        diagnostics=diagnostics,
        metric_specs=FULL_BASELINE_SPECS,
        required_metrics=list(FULL_BASELINE_SPECS),
        log_prefix="[verify]",
    )

    print("[verify] post-restore continue_for 20s")
    post_events = continue_and_collect(
        session,
        work_dir,
        prep.offsets,
        20 * 1_000_000_000,
        response_timeout,
        "post-restore continue_for",
    )
    if not post_events:
        raise AssertionError("no NETLOG events observed during post-restore window")

    post_metrics = collect_metric_map(post_events, FULL_POST_SPECS)
    require_all_present(
        post_metrics,
        list(FULL_POST_SPECS),
        "post-restore window missing expected TCP/UDP events",
    )

    churn = collect_count_map(post_events, TCP_CHURN_COUNT_SPECS)
    if not (
        post_metrics["tcp_tx"] > prep.cp_metrics["tcp_tx"]
        and post_metrics["tcp_ack"] > prep.cp_metrics["tcp_ack"]
    ):
        raise AssertionError(
            "post-restore TCP missing transparent seq/ack progress on existing connection"
        )

    reconnect_churn = sum(churn.values())
    if reconnect_churn > 0:
        raise AssertionError(
            "post-restore TCP observed reconnect churn "
            f"(connect_ok={churn['connect_ok']}, accept={churn['accept']}, retry={churn['retry']}, "
            f"io_error={churn['io_error']}, disconnect={churn['disconnect']})"
        )

    require_event_progress(
        "udp_peer_a.tx(post-restore)",
        prep.cp_metrics["udp_a_tx"],
        post_metrics["udp_a_tx"],
    )
    require_event_progress(
        "udp_peer_b.tx(post-restore)",
        prep.cp_metrics["udp_b_tx"],
        post_metrics["udp_b_tx"],
    )
    require_event_progress(
        "udp_peer_a.rx(post-restore)",
        prep.cp_metrics["udp_a_tx"],
        post_metrics["udp_a_rx"],
    )
    require_event_progress(
        "udp_peer_b.rx(post-restore)",
        prep.cp_metrics["udp_b_tx"],
        post_metrics["udp_b_rx"],
    )

    print("[verify] PASS: network communication recovered across checkpoint/restore")


def run_verify_tcp(
    session: ShadowSession,
    work_dir: Path,
    label: str,
    connect_timeout: float,
    response_timeout: float,
    diagnostics: bool,
    post_restore_step_ns: int,
    post_restore_steps: int,
    strict_tcp_time: bool,
) -> None:
    if post_restore_step_ns <= 0:
        raise AssertionError(f"post_restore_step_ns must be > 0, got {post_restore_step_ns}")
    if post_restore_steps <= 0:
        raise AssertionError(f"post_restore_steps must be > 0, got {post_restore_steps}")

    prep = perform_restore_prep(
        session=session,
        work_dir=work_dir,
        label=label,
        connect_timeout=connect_timeout,
        response_timeout=response_timeout,
        diagnostics=diagnostics,
        metric_specs=TCP_BASELINE_SPECS,
        required_metrics=list(TCP_BASELINE_SPECS),
        log_prefix="[verify][tcp]",
    )

    post_events: list[dict] = []
    churn_totals = {name: 0 for name in TCP_CHURN_COUNT_SPECS}
    progress_steps = 0
    prev_metrics = dict(prep.cp_metrics)

    for step_idx in range(post_restore_steps):
        print(
            f"[verify][tcp] post-restore step {step_idx + 1}/{post_restore_steps} "
            f"continue_for {post_restore_step_ns}ns"
        )
        step_events = continue_and_collect(
            session,
            work_dir,
            prep.offsets,
            post_restore_step_ns,
            response_timeout,
            f"post-restore continue_for step {step_idx + 1}",
        )
        post_events.extend(step_events)

        step_counts = collect_count_map(step_events, TCP_STEP_COUNT_SPECS)
        step_churn = collect_count_map(step_events, TCP_CHURN_COUNT_SPECS)
        for name, value in step_churn.items():
            churn_totals[name] += value

        current_metrics = collect_metric_map(post_events, TCP_BASELINE_SPECS)
        step_progress = all(
            current_metrics[name] > prev_metrics[name]
            for name in ("tcp_tx", "tcp_ack", "srv_rx", "srv_ack")
        )
        if step_progress:
            progress_steps += 1

        prev_metrics = {
            name: max(prev_metrics[name], current_metrics[name]) for name in TCP_BASELINE_SPECS
        }
        print(
            "[verify][tcp] step-summary "
            + json.dumps(
                {
                    "step": step_idx + 1,
                    "events": len(step_events),
                    **step_counts,
                    "cur_tcp_tx": current_metrics["tcp_tx"],
                    "cur_tcp_ack": current_metrics["tcp_ack"],
                    "cur_srv_rx": current_metrics["srv_rx"],
                    "cur_srv_ack": current_metrics["srv_ack"],
                    "progress": step_progress,
                },
                sort_keys=True,
            )
        )

    if not post_events:
        raise AssertionError("no NETLOG events observed during post-restore TCP window")

    post_metrics = collect_metric_map(post_events, TCP_BASELINE_SPECS)
    require_all_present(
        post_metrics,
        list(TCP_BASELINE_SPECS),
        "post-restore TCP window missing expected bidirectional events",
    )
    for name, before in prep.cp_metrics.items():
        require_event_progress(f"{name}(post-restore)", before, post_metrics[name])

    reconnect_churn = sum(churn_totals.values())
    if reconnect_churn > 0:
        raise AssertionError(
            "post-restore TCP observed reconnect churn "
            f"(connect_ok={churn_totals['connect_ok']}, accept={churn_totals['accept']}, "
            f"retry={churn_totals['retry']}, io_error={churn_totals['io_error']}, "
            f"disconnect={churn_totals['disconnect']})"
        )

    min_progress_steps = max(3, post_restore_steps // 2)
    if progress_steps < min_progress_steps:
        raise AssertionError(
            f"post-restore TCP progress was too bursty: progress_steps={progress_steps}, "
            f"required>={min_progress_steps}"
        )

    client_mono = event_values(post_events, role="tcp_client", event="tcp_tx", key="mono_ns")
    client_mono.extend(
        event_values(post_events, role="tcp_client", event="tcp_rx_ack", key="mono_ns")
    )
    server_mono = event_values(post_events, role="tcp_server", event="tcp_rx", key="mono_ns")
    server_mono.extend(
        event_values(post_events, role="tcp_server", event="tcp_ack", key="mono_ns")
    )
    client_span = value_span(client_mono)
    server_span = value_span(server_mono)
    min_required_span = post_restore_step_ns * max(post_restore_steps // 2, 1)
    tcp_time_semantics_ok = client_span >= min_required_span and server_span >= min_required_span

    summary = {
        "final_tx": post_metrics["tcp_tx"],
        "final_ack": post_metrics["tcp_ack"],
        "final_srv_rx": post_metrics["srv_rx"],
        "final_srv_ack": post_metrics["srv_ack"],
        "progress_steps": progress_steps,
        "expected_span_ns": post_restore_step_ns * max(post_restore_steps - 1, 1),
        "client_mono_span_ns": client_span,
        "server_mono_span_ns": server_span,
        "tcp_time_semantics_ok": tcp_time_semantics_ok,
    }
    print(f"[verify][tcp] summary: {summary}")

    if strict_tcp_time and not tcp_time_semantics_ok:
        raise AssertionError(
            "post-restore TCP data plane progressed, but app monotonic timestamps did not span "
            f"the stepped window: {summary}"
        )
    if not tcp_time_semantics_ok:
        print(
            "[verify][tcp] NOTE: TCP seq/ack progressed without reconnect churn, "
            "but app monotonic timestamps did not advance across the stepped window"
        )

    print("[verify] PASS: TCP connection progressed across restore without reconnect churn")


def terminate_session(
    session: ShadowSession,
    *,
    graceful_continue: bool = False,
    response_timeout: float = 3.0,
) -> int:
    if graceful_continue and session.process.poll() is None and session.sock is not None:
        deadline = time.time() + 30.0
        while session.process.poll() is None and time.time() < deadline and session.sock is not None:
            try:
                send_command(
                    session.sock,
                    {"cmd": "continue_for", "duration_ns": 10_000_000_000},
                    response_timeout,
                )
            except Exception:
                break
            time.sleep(0.1)

    if session.sock is not None:
        try:
            session.sock.close()
        except OSError:
            pass
        session.sock = None

    if session.process.poll() is None:
        session.process.terminate()
        try:
            session.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            session.process.kill()
            session.process.wait(timeout=10)

    rc = session.process.returncode or 0
    stderr_raw = b""
    if session.process.stderr is not None:
        try:
            stderr_raw = session.process.stderr.read()
        except Exception:
            stderr_raw = b""

    if stderr_raw:
        lines = stderr_raw.decode("utf-8", errors="replace").splitlines()
        print(f"[shadow stderr tail] ({len(lines)} lines total)")
        for line in lines[-120:]:
            print(line)

    try:
        if session.socket_path.exists():
            session.socket_path.unlink()
    except OSError:
        pass
    return rc


def main() -> int:
    args = parse_args()
    config_path = Path(args.config).expanduser()
    if not config_path.exists():
        print(f"error: config not found: {config_path}", file=sys.stderr)
        return 2

    work_dir = Path(args.work_dir).expanduser().resolve()
    socket_path = Path(args.socket_path)
    print(f"[orchestrator] shadow_bin={args.shadow_bin}")
    print(f"[orchestrator] config={config_path.resolve()}")
    print(f"[orchestrator] work_dir={work_dir}")
    print(f"[orchestrator] socket={socket_path}")

    session = launch_shadow(
        shadow_bin=args.shadow_bin,
        config_path=config_path,
        work_dir=work_dir,
        socket_path=socket_path,
        clean_data=args.clean_data,
    )

    verify_failed = False
    try:
        if args.mode == "tcp":
            run_verify_tcp(
                session=session,
                work_dir=work_dir,
                label=args.verify_label,
                connect_timeout=args.connect_timeout,
                response_timeout=args.response_timeout,
                diagnostics=args.diagnostics,
                post_restore_step_ns=args.post_restore_step_ns,
                post_restore_steps=args.post_restore_steps,
                strict_tcp_time=args.strict_tcp_time,
            )
        else:
            run_verify(
                session=session,
                work_dir=work_dir,
                label=args.verify_label,
                connect_timeout=args.connect_timeout,
                response_timeout=args.response_timeout,
                diagnostics=args.diagnostics,
            )
    except AssertionError as exc:
        print(f"[verify] FAIL: {exc}", file=sys.stderr)
        verify_failed = True
    finally:
        rc_shadow = terminate_session(
            session,
            graceful_continue=False,
            response_timeout=args.response_timeout,
        )
        print(f"[orchestrator] shadow exit code: {rc_shadow}")

    return 1 if verify_failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
