#!/usr/bin/env python3
"""Ethereum-like network checkpoint/restore verifier for Shadow."""

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


PEERS = ("peer-a", "peer-b", "peer-c")
BOOTNODE = "bootnode"
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
    parser = argparse.ArgumentParser(description="Ethereum-like network verifier")
    parser.add_argument(
        "--shadow-bin",
        default=shutil.which("shadow") or "shadow",
        help="Path to shadow binary",
    )
    parser.add_argument(
        "--config",
        default=str(Path(__file__).resolve().parent / "shadow_eth_poc.yaml"),
        help="Path to shadow config",
    )
    parser.add_argument(
        "--work-dir",
        default=str(Path(__file__).resolve().parent / "run"),
        help="Working directory for running Shadow",
    )
    parser.add_argument(
        "--socket-path",
        default=f"/tmp/shadow_eth_poc_control_{os.getpid()}.sock",
        help="Unix socket path for SHADOW_CONTROL_SOCKET",
    )
    parser.add_argument("--connect-timeout", type=float, default=60.0)
    parser.add_argument("--response-timeout", type=float, default=120.0)
    parser.add_argument("--clean-data", action="store_true")
    parser.add_argument("--verify-label", default="cp_eth_poc_verify")
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


def resolve_config(config_path: Path, work_dir: Path) -> Path:
    rendered = work_dir / "rendered.shadow.yaml"
    mesh_app = (Path(__file__).resolve().parent / "mesh_app.py").resolve()
    text = config_path.resolve().read_text(encoding="utf-8").replace(
        "__MESH_APP_PATH__", str(mesh_app)
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


def criu_preflight() -> tuple[bool, str]:
    criu_bin = os.environ.get("CRIU_BIN", "criu")
    for args in ([criu_bin, "check", "--unprivileged"], [criu_bin, "check"]):
        proc = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
        if proc.returncode == 0:
            suffix = " --unprivileged" if "--unprivileged" in args else ""
            return True, f"criu check{suffix}: ok"
    return False, proc.stderr.decode("utf-8", errors="replace").strip()


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


def max_event_value(events: list[dict], role: str, event: str, key: str) -> int:
    best = -1
    for item in events:
        if item.get("role") != role or item.get("event") != event:
            continue
        value = item.get(key)
        if isinstance(value, int) and value > best:
            best = value
    return best


def count_churn(events: list[dict]) -> int:
    return sum(1 for item in events if item.get("event") in TCP_CHURN_EVENTS)


def mono_span(events: list[dict], role: str) -> int:
    values = [
        item["mono_ns"]
        for item in events
        if item.get("role") == role
        and item.get("event") in {"tcp_tx", "tcp_rx_ack", "udp_tx", "udp_rx_ack"}
        and isinstance(item.get("mono_ns"), int)
    ]
    if len(values) < 2:
        return 0
    return max(values) - min(values)


def distinct_peers(events: list[dict], role: str, event: str) -> set[str]:
    peers: set[str] = set()
    for item in events:
        if item.get("role") != role or item.get("event") != event:
            continue
        peer = item.get("peer")
        if isinstance(peer, str) and peer:
            peers.add(peer)
    return peers


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


def checkpoint_path(work_dir: Path, label: str) -> Path:
    return work_dir / "shadow.data" / "checkpoints" / f"{label}.checkpoint.json"


def verify_checkpoint(path: Path, min_hosts: int) -> None:
    raw = json.loads(path.read_text(encoding="utf-8"))
    hosts = raw.get("hosts")
    if not isinstance(hosts, list) or len(hosts) < min_hosts:
        raise AssertionError(f"expected >= {min_hosts} hosts in checkpoint, got {hosts!r}")


def peer_metrics(events: list[dict]) -> dict[str, tuple[int, int, int]]:
    return {
        peer: (
            max_event_value(events, peer, "tcp_tx", "seq"),
            max_event_value(events, peer, "tcp_rx_ack", "ack"),
            max_event_value(events, peer, "udp_rx_ack", "ack"),
        )
        for peer in PEERS
    }


def require_peer_progress(before: dict[str, tuple[int, int, int]], after: dict[str, tuple[int, int, int]], stage: str) -> None:
    for peer in PEERS:
        before_tx, before_ack, before_udp = before[peer]
        after_tx, after_ack, after_udp = after[peer]
        if min(before_tx, before_ack, before_udp, after_tx, after_ack, after_udp) < 0:
            raise AssertionError(f"{stage}: missing peer metrics for {peer}")
        if after_tx <= before_tx or after_ack <= before_ack or after_udp <= before_udp:
            raise AssertionError(
                f"{stage}: peer {peer} did not progress enough "
                f"(tx {before_tx}->{after_tx}, ack {before_ack}->{after_ack}, udp {before_udp}->{after_udp})"
            )


def require_peer_metrics_present(metrics: dict[str, tuple[int, int, int]], stage: str) -> None:
    missing = [
        peer
        for peer, values in metrics.items()
        if min(values) < 0
    ]
    if missing:
        raise AssertionError(f"{stage}: missing peer metrics for {', '.join(missing)}")


def run_verify(args: argparse.Namespace) -> None:
    ok, msg = criu_preflight()
    if not ok:
        raise AssertionError(f"CRIU preflight failed: {msg}")
    print(f"[verify] CRIU preflight: {msg} (CRIU_BIN={os.environ.get('CRIU_BIN')!r})")

    session = launch_shadow(
        shadow_bin=args.shadow_bin,
        config_path=Path(args.config).expanduser(),
        work_dir=Path(args.work_dir).expanduser().resolve(),
        socket_path=Path(args.socket_path),
        clean_data=args.clean_data,
    )

    verify_failed = False
    try:
        work_dir = Path(args.work_dir).expanduser().resolve()
        ensure_connected(session, args.connect_timeout)

        offsets: dict[str, int] = {}
        history: list[dict] = []

        print("[verify] warmup continue_for 12s")
        warmup = continue_and_collect(
            session,
            work_dir,
            offsets,
            12 * 1_000_000_000,
            args.response_timeout,
            "warmup continue_for",
        )
        history.extend(warmup)
        if not warmup:
            raise AssertionError("no NETLOG events observed during warmup")

        cp_metrics = peer_metrics(history)
        require_peer_metrics_present(cp_metrics, "warmup")

        boot_tcp_peers = distinct_peers(history, BOOTNODE, "tcp_rx")
        boot_udp_peers = distinct_peers(history, BOOTNODE, "udp_rx")
        if len(boot_tcp_peers) < 2 or len(boot_udp_peers) < 2:
            raise AssertionError(
                "warmup did not establish broad enough bootnode fan-in "
                f"(tcp_peers={sorted(boot_tcp_peers)}, udp_peers={sorted(boot_udp_peers)})"
            )

        print("[verify] checkpoint")
        assert session.sock is not None
        cp_resp = send_command(
            session.sock,
            {"cmd": "checkpoint", "label": args.verify_label},
            args.response_timeout,
        )
        expect_ok(cp_resp, "checkpoint")
        verify_checkpoint(checkpoint_path(work_dir, args.verify_label), min_hosts=4)

        print("[verify] advance continue_for 10s")
        advance = continue_and_collect(
            session,
            work_dir,
            offsets,
            10 * 1_000_000_000,
            args.response_timeout,
            "advance continue_for",
        )
        history.extend(advance)
        require_peer_progress(cp_metrics, peer_metrics(history), "advance")

        print("[verify] restore checkpoint")
        restore = send_restore_with_reconnect(
            session,
            args.verify_label,
            args.connect_timeout,
            args.response_timeout,
        )
        expect_ok(restore, "restore")

        post_events: list[dict] = []
        shadow_closed_early = False
        for step in range(args.post_restore_steps):
            print(
                f"[verify] post-restore step {step + 1}/{args.post_restore_steps} "
                f"continue_for {args.post_restore_step_ns}ns"
            )
            try:
                step_events = continue_and_collect(
                    session,
                    work_dir,
                    offsets,
                    args.post_restore_step_ns,
                    args.response_timeout,
                    f"post-restore continue_for step {step + 1}",
                )
            except ConnectionError:
                shadow_closed_early = True
                print("[verify] control socket closed during post-restore stepped window")
                break
            post_events.extend(step_events)

        if not post_events:
            detail = (
                "shadow exited before restored mesh traffic resumed"
                if shadow_closed_early
                else "no NETLOG events observed during post-restore window"
            )
            raise AssertionError(
                f"{detail}; current restore likely cannot yet support this multi-socket-per-process topology"
            )

        require_peer_progress(cp_metrics, peer_metrics(post_events), "post-restore")

        boot_post_tcp = distinct_peers(post_events, BOOTNODE, "tcp_rx")
        boot_post_udp = distinct_peers(post_events, BOOTNODE, "udp_rx")
        if len(boot_post_tcp) < 2 or len(boot_post_udp) < 2:
            raise AssertionError(
                "post-restore bootnode lost too much peer fan-in "
                f"(tcp_peers={sorted(boot_post_tcp)}, udp_peers={sorted(boot_post_udp)})"
            )

        churn = count_churn(post_events)
        if churn > 0:
            raise AssertionError(f"post-restore observed TCP reconnect churn count={churn}")

        min_required_span = args.post_restore_step_ns * max(args.post_restore_steps // 2, 1)
        spans = {peer: mono_span(post_events, peer) for peer in PEERS}
        bad_spans = {peer: span for peer, span in spans.items() if span < min_required_span}
        if bad_spans:
            raise AssertionError(f"post-restore peer time spans too small: {bad_spans}")

        print(
            "[verify] PASS: Ethereum-like mesh traffic recovered across checkpoint/restore "
            f"(boot_tcp_peers={sorted(boot_post_tcp)}, boot_udp_peers={sorted(boot_post_udp)}, spans={spans})"
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
            session.sock = None
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
    config_path = Path(args.config).expanduser()
    if not config_path.exists():
        print(f"error: config not found: {config_path}", file=sys.stderr)
        return 2

    print(f"[orchestrator] shadow_bin={args.shadow_bin}")
    print(f"[orchestrator] config={config_path.resolve()}")
    print(f"[orchestrator] work_dir={Path(args.work_dir).expanduser().resolve()}")
    print(f"[orchestrator] socket={Path(args.socket_path)}")
    run_verify(args)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
