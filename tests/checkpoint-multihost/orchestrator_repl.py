#!/usr/bin/env python3
"""Interactive multi-host checkpoint/restore orchestrator for Shadow.

Features:
- Input Shadow config (`--config`)
- Host->database dependency map via JSON string (`--database`)
- Interactive REPL for pause/continue/continue_for/checkpoint/restore/status
- Backup/restore external database dependencies around checkpoint/restore
"""

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
from typing import Dict, Optional


@dataclass
class ShadowSession:
    process: subprocess.Popen
    socket_path: Path
    sock: Optional[socket.socket] = None


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Multi-host Shadow REPL orchestrator")
    parser.add_argument(
        "--shadow-bin",
        default=shutil.which("shadow") or "shadow",
        help="Path to shadow binary (default: shadow from PATH)",
    )
    parser.add_argument(
        "--config",
        required=True,
        help="Path to shadow config (yaml).",
    )
    parser.add_argument(
        "--database",
        required=True,
        help='JSON map host->external-db-path. Example: \'{"hostA":"/tmp/dbA","hostB":"/tmp/dbB"}\'',
    )
    parser.add_argument(
        "--work-dir",
        default=str(Path(__file__).resolve().parent / "run"),
        help="Working directory for running Shadow",
    )
    parser.add_argument(
        "--socket-path",
        default=f"/tmp/shadow_control_{os.getpid()}.sock",
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
        "--scenario",
        choices=("repl", "verify"),
        default="repl",
        help=(
            "repl: interactive commands; verify: non-interactive run that checks (1) Shadow "
            "checkpoint JSON contains per-host snapshots, (2) external DB backup/restore matches "
            "checkpoint-time files, (3) counters advance ~1/s after restore from restored sim time."
        ),
    )
    parser.add_argument(
        "--verify-label",
        default="cp_verify",
        help="Checkpoint label used by --scenario verify (default: cp_verify)",
    )
    return parser.parse_args()


def parse_database_map(raw: str) -> Dict[str, Path]:
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as e:
        raise ValueError(f"Invalid --database JSON: {e}") from e

    if not isinstance(value, dict):
        raise ValueError("--database must be a JSON object: host->path")

    out: Dict[str, Path] = {}
    for host, p in value.items():
        if not isinstance(host, str) or not isinstance(p, str):
            raise ValueError("--database must be map[str, str]")
        out[host] = Path(p).expanduser().resolve()
    return out


def wait_for_socket(
    path: Path,
    timeout_sec: float,
    process: Optional[subprocess.Popen] = None,
) -> socket.socket:
    import time

    start = time.time()
    while time.time() - start < timeout_sec:
        if process is not None and process.poll() is not None:
            raise RuntimeError(
                f"Shadow exited before socket became available (code={process.returncode})"
            )
        if path.exists():
            try:
                s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                s.connect(str(path))
                return s
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

    line = buf.split(b"\n", 1)[0]
    resp = json.loads(line.decode("utf-8"))
    print(f"  <- {json.dumps(resp)}")
    return resp


def backup_databases(db_map: Dict[str, Path], label: str) -> None:
    for host, src in db_map.items():
        dst = Path(f"{src}.backup.{label}")
        if src.exists():
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, dst)
            print(f"[backup] {host}: {src} -> {dst}")
        else:
            print(f"[backup] {host}: source missing, skipped: {src}")


def restore_databases(db_map: Dict[str, Path], label: str) -> None:
    for host, dst in db_map.items():
        src = Path(f"{dst}.backup.{label}")
        if src.exists():
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, dst)
            print(f"[restore-db] {host}: {src} -> {dst}")
        else:
            print(f"[restore-db] {host}: backup missing, skipped: {src}")


def read_db_map(db_map: Dict[str, Path]) -> Dict[str, str]:
    """Return host -> file text (strip) for each DB path."""
    out: Dict[str, str] = {}
    for host, path in db_map.items():
        if not path.exists():
            out[host] = ""
            continue
        try:
            out[host] = path.read_text(encoding="utf-8").strip()
        except OSError as e:
            out[host] = f"<read_error:{e}>"
    return out


def db_maps_equal(a: Dict[str, str], b: Dict[str, str]) -> bool:
    return a == b


def checkpoint_json_path(work_dir: Path, label: str) -> Path:
    return work_dir / "shadow.data" / "checkpoints" / f"{label}.checkpoint.json"


def verify_shadow_checkpoint_reflects_hosts(
    path: Path,
    *,
    min_hosts: int,
    db_hostnames: Optional[set[str]] = None,
) -> Dict[str, object]:
    """Assert the on-disk checkpoint captures multi-host simulation state.

    Returns a small summary dict for logging. Raises AssertionError on failure.
    """
    if not path.is_file():
        raise AssertionError(f"checkpoint file missing: {path}")

    raw = json.loads(path.read_text(encoding="utf-8"))
    version = raw.get("version")
    if not isinstance(version, int) or version < 1:
        raise AssertionError(f"unexpected checkpoint version: {version!r}")

    hosts = raw.get("hosts")
    if not isinstance(hosts, list) or len(hosts) < min_hosts:
        raise AssertionError(
            f"expected hosts list with at least {min_hosts} entries, got {hosts!r}"
        )

    names = []
    for h in hosts:
        if not isinstance(h, dict):
            raise AssertionError(f"host entry is not an object: {h!r}")
        hn = h.get("hostname")
        if not isinstance(hn, str) or not hn:
            raise AssertionError(f"host missing hostname: {h!r}")
        names.append(hn)
        # Minimal signal that scheduler / process graph was snapshotted.
        if "event_queue" not in h or "processes" not in h:
            raise AssertionError(f"host {hn!r} missing event_queue or processes")

    if db_hostnames is not None:
        missing = db_hostnames - set(names)
        if missing:
            raise AssertionError(
                f"checkpoint hostnames {names!r} do not cover db map hosts {sorted(db_hostnames)} "
                f"(missing: {sorted(missing)})"
            )

    sim_time_ns = raw.get("sim_time_ns")
    if not isinstance(sim_time_ns, int) or sim_time_ns < 0:
        raise AssertionError(f"invalid sim_time_ns: {sim_time_ns!r}")

    return {
        "path": str(path),
        "version": version,
        "sim_time_ns": sim_time_ns,
        "hostnames": names,
        "host_count": len(hosts),
    }


def expect_ok(resp: dict, what: str) -> None:
    if resp.get("status") != "ok":
        raise AssertionError(f"{what}: expected status ok, got {resp!r}")


def send_restore_with_reconnect(
    session: ShadowSession,
    db_map: Dict[str, Path],
    label: str,
    connect_timeout: float,
    response_timeout: float,
) -> dict:
    restore_databases(db_map, label)
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
    """Best-effort check whether CRIU is usable in this environment.

    Returns (ok, message). We check both CLI compatibility and typical kernel/capability issues.
    """
    criu_bin = os.environ.get("CRIU_BIN", "criu")
    def run(args: list[str]) -> subprocess.CompletedProcess[bytes]:
        return subprocess.run(
            args,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

    # Try newer flag first, then fall back.
    p = run([criu_bin, "check", "--unprivileged"])
    if p.returncode == 0:
        return True, "criu check --unprivileged: ok"

    stderr = p.stderr.decode("utf-8", errors="replace")
    if _stderr_mentions_unrecognized_unprivileged(stderr):
        p2 = run([criu_bin, "check"])
        if p2.returncode == 0:
            return True, "criu check: ok (no --unprivileged support)"
        stderr2 = p2.stderr.decode("utf-8", errors="replace")
        return False, f"criu check failed: {stderr2.strip()}"

    return False, f"criu check --unprivileged failed: {stderr.strip()}"


def run_verify_scenario(
    session: ShadowSession,
    db_map: Dict[str, Path],
    work_dir: Path,
    label: str,
    connect_timeout: float,
    response_timeout: float,
) -> None:
    """Automated checks for external DB restore + Shadow host checkpoint content and replay."""
    ok, msg = criu_preflight()
    if not ok:
        raise AssertionError(
            "CRIU is not usable in this environment, so Shadow cannot provide full "
            "checkpoint/restore of real processes (shim rollback). "
            f"Preflight: {msg}"
        )
    print(f"[verify] CRIU preflight: {msg} (CRIU_BIN={os.environ.get('CRIU_BIN')!r})")

    ensure_connected(session, connect_timeout)
    assert session.sock is not None

    warmup_ns = 10 * 1_000_000_000
    advance_ns = 10 * 1_000_000_000
    after_restore_ns = 5 * 1_000_000_000

    print("[verify] warmup continue_for 10s")
    r0 = send_command(
        session.sock,
        {"cmd": "continue_for", "duration_ns": warmup_ns},
        response_timeout,
    )
    expect_ok(r0, "warmup continue_for")

    db_at_checkpoint = read_db_map(db_map)
    print(f"[verify] DB at checkpoint time: {db_at_checkpoint}")

    backup_databases(db_map, label)
    r_cp = send_command(
        session.sock,
        {"cmd": "checkpoint", "label": label},
        response_timeout,
    )
    expect_ok(r_cp, "checkpoint")

    cp_path = checkpoint_json_path(work_dir, label)
    summary = verify_shadow_checkpoint_reflects_hosts(
        cp_path,
        min_hosts=len(db_map),
        db_hostnames=set(db_map.keys()),
    )
    print(f"[verify] Shadow checkpoint OK: {summary}")

    print("[verify] advance 10s (diverge from checkpoint DB state)")
    r1 = send_command(
        session.sock,
        {"cmd": "continue_for", "duration_ns": advance_ns},
        response_timeout,
    )
    expect_ok(r1, "post-checkpoint continue_for")

    db_dirty = read_db_map(db_map)
    print(f"[verify] DB after advance: {db_dirty}")
    for host in db_map:
        try:
            a, b = int(db_at_checkpoint[host]), int(db_dirty[host])
        except ValueError:
            raise AssertionError(
                f"non-integer counter for host {host}: cp={db_at_checkpoint[host]!r} "
                f"dirty={db_dirty[host]!r}"
            ) from None
        if b <= a:
            raise AssertionError(
                f"expected counter to increase after advance ({host}: {a} -> {b})"
            )

    print("[verify] restore external DB + Shadow restore")
    r_rs = send_restore_with_reconnect(
        session, db_map, label, connect_timeout, response_timeout
    )
    expect_ok(r_rs, "restore")

    db_restored = read_db_map(db_map)
    print(f"[verify] DB after restore: {db_restored}")
    if not db_maps_equal(db_restored, db_at_checkpoint):
        raise AssertionError(
            "external DB restore mismatch:\n"
            f"  at checkpoint: {db_at_checkpoint}\n"
            f"  after restore:  {db_restored}"
        )
    print("[verify] external DB matches checkpoint-time snapshot")

    print("[verify] continue_for 5s — counters should advance from restored baseline")
    assert session.sock is not None
    r2 = send_command(
        session.sock,
        {"cmd": "continue_for", "duration_ns": after_restore_ns},
        response_timeout,
    )
    expect_ok(r2, "post-restore continue_for")

    db_final = read_db_map(db_map)
    print(f"[verify] DB after post-restore run: {db_final}")

    # ~1 increment per simulated second; allow slack for scheduling.
    slack = 2
    for host in db_map:
        try:
            base = int(db_at_checkpoint[host])
            final = int(db_final[host])
        except ValueError as e:
            raise AssertionError(f"non-integer counter for host {host}") from e
        delta = final - base
        expected = 5
        if not (expected - slack <= delta <= expected + slack):
            raise AssertionError(
                f"host {host}: expected counter delta ~{expected}s (+/-{slack}) from restored "
                f"simulation, got delta={delta} (base={base}, final={final}). "
                "If DB restore works but this fails, host-side time/task state may not match "
                "the checkpoint."
            )

    print("[verify] PASS: Shadow host checkpoint present; DB restore OK; post-restore timeline OK")


def resolve_config(config_path: Path, work_dir: Path) -> Path:
    """Render known placeholders into a workdir config."""
    src = config_path.resolve()
    text = src.read_text(encoding="utf-8")

    app_multi = (Path(__file__).resolve().parent / "counter_app_multi.py").resolve()
    text = text.replace("__COUNTER_APP_MULTI_PATH__", str(app_multi))

    rendered = work_dir / "rendered.shadow.yaml"
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

    rendered_config = resolve_config(config_path, work_dir)

    env = os.environ.copy()
    env["SHADOW_CONTROL_SOCKET"] = str(socket_path)

    # If the user provided a path (relative or absolute), resolve it before we
    # change cwd to work_dir. If they provided a bare command (e.g., "shadow"),
    # keep it and let PATH resolution work.
    shadow_bin_resolved = shadow_bin
    if (os.sep in shadow_bin) or shadow_bin.startswith("."):
        bin_path = Path(shadow_bin).expanduser()
        if not bin_path.is_absolute():
            bin_path = (Path.cwd() / bin_path).resolve()
        shadow_bin_resolved = str(bin_path)
        if not Path(shadow_bin_resolved).exists():
            raise FileNotFoundError(f"shadow binary not found: {shadow_bin_resolved}")

    proc = subprocess.Popen(
        [shadow_bin_resolved, str(rendered_config)],
        cwd=str(work_dir),
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=False,
    )
    return ShadowSession(process=proc, socket_path=socket_path, sock=None)


def ensure_connected(session: ShadowSession, timeout_sec: float) -> None:
    if session.sock is not None:
        return
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


def print_help() -> None:
    print(
        "\nCommands:\n"
        "  pause\n"
        "  continue\n"
        "  continue_for <seconds>\n"
        "  step [seconds]            # approx step via continue_for, default 1s\n"
        "  checkpoint <label>        # backup db map, then send checkpoint\n"
        "  restore <label>           # restore db map, then send restore\n"
        "  status\n"
        "  show db\n"
        "  help\n"
        "  quit\n"
    )


def repl(
    session: ShadowSession,
    db_map: Dict[str, Path],
    connect_timeout: float,
    response_timeout: float,
) -> None:
    ensure_connected(session, connect_timeout)
    print_help()

    while True:
        try:
            line = input("shadow-repl> ").strip()
        except EOFError:
            line = "quit"
        except KeyboardInterrupt:
            print()
            line = "quit"

        if not line:
            continue

        parts = line.split()
        cmd = parts[0].lower()

        if cmd in ("quit", "exit"):
            break
        if cmd == "help":
            print_help()
            continue
        if cmd == "show" and len(parts) >= 2 and parts[1] == "db":
            for host, path in db_map.items():
                exists = path.exists()
                value = "N/A"
                if exists:
                    try:
                        value = path.read_text(encoding="utf-8").strip()
                    except OSError:
                        value = "read_error"
                print(f"  {host}: {path} (exists={exists}, value={value})")
            continue

        # Build command payload
        payload = None
        restore_label = None

        if cmd == "pause":
            payload = {"cmd": "pause"}
        elif cmd == "continue":
            payload = {"cmd": "continue"}
        elif cmd == "continue_for":
            if len(parts) != 2:
                print("usage: continue_for <seconds>")
                continue
            try:
                secs = float(parts[1])
            except ValueError:
                print("invalid seconds")
                continue
            payload = {"cmd": "continue_for", "duration_ns": int(secs * 1_000_000_000)}
        elif cmd == "step":
            step_secs = 1.0
            if len(parts) == 2:
                try:
                    step_secs = float(parts[1])
                except ValueError:
                    print("invalid seconds")
                    continue
            payload = {
                "cmd": "continue_for",
                "duration_ns": int(step_secs * 1_000_000_000),
            }
            print("[step] note: this is an approximate step via continue_for.")
        elif cmd == "checkpoint":
            if len(parts) != 2:
                print("usage: checkpoint <label>")
                continue
            label = parts[1]
            backup_databases(db_map, label)
            payload = {"cmd": "checkpoint", "label": label}
        elif cmd == "restore":
            if len(parts) != 2:
                print("usage: restore <label>")
                continue
            label = parts[1]
            restore_databases(db_map, label)
            payload = {"cmd": "restore", "label": label}
            restore_label = label
        elif cmd == "status":
            payload = {"cmd": "status"}
        else:
            print("unknown command; use help")
            continue

        try:
            assert session.sock is not None
            send_command(session.sock, payload, response_timeout)
        except ConnectionError as e:
            print(f"[orchestrator] connection error: {e}")
            if restore_label is not None:
                print("[orchestrator] restore may have restarted simulation; reconnecting...")
                reconnect_if_needed(session, connect_timeout)
                try:
                    assert session.sock is not None
                    send_command(session.sock, {"cmd": "status"}, response_timeout)
                except Exception as inner:
                    print(f"[orchestrator] reconnect/status failed: {inner}")
            else:
                print("[orchestrator] socket disconnected; trying reconnect...")
                reconnect_if_needed(session, connect_timeout)
        except Exception as e:
            print(f"[orchestrator] command failed: {e}")


def terminate_session(
    session: ShadowSession,
    *,
    graceful_continue: bool = False,
    response_timeout: float = 3.0,
) -> int:
    if graceful_continue and session.process.poll() is None and session.sock is not None:
        deadline = time.time() + 60.0
        while session.process.poll() is None and time.time() < deadline and session.sock is not None:
            try:
                # Drive the simulation forward in chunks until Shadow exits on its own.
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
        # Backtraces can be long; keep enough context for debugging.
        tail = lines[-200:]
        print(f"[shadow stderr tail] ({len(lines)} lines total)")
        for ln in tail:
            print(ln)

    try:
        if session.socket_path.exists():
            session.socket_path.unlink()
    except OSError:
        pass
    return rc


def main() -> int:
    args = parse_args()

    try:
        db_map = parse_database_map(args.database)
    except ValueError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2

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
    print("[orchestrator] database map:")
    for host, path in db_map.items():
        print(f"  - {host}: {path}")

    session = launch_shadow(
        shadow_bin=args.shadow_bin,
        config_path=config_path,
        work_dir=work_dir,
        socket_path=socket_path,
        clean_data=args.clean_data,
    )

    verify_failed = False
    try:
        if args.scenario == "verify":
            try:
                run_verify_scenario(
                    session=session,
                    db_map=db_map,
                    work_dir=work_dir,
                    label=args.verify_label,
                    connect_timeout=args.connect_timeout,
                    response_timeout=args.response_timeout,
                )
            except AssertionError as e:
                print(f"[verify] FAIL: {e}", file=sys.stderr)
                verify_failed = True
        else:
            repl(
                session=session,
                db_map=db_map,
                connect_timeout=args.connect_timeout,
                response_timeout=args.response_timeout,
            )
    finally:
        rc_shadow = terminate_session(
            session,
            graceful_continue=(args.scenario == "verify" and not verify_failed),
            response_timeout=args.response_timeout,
        )
        print(f"[orchestrator] shadow exit code: {rc_shadow}")

    if verify_failed:
        return 1
    return 0 if rc_shadow == 0 else rc_shadow


if __name__ == "__main__":
    raise SystemExit(main())
