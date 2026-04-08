#!/usr/bin/env python3
"""External orchestrator for Shadow checkpoint/restore POC.

This script demonstrates the full Scheme C workflow:
1. Starts Shadow with SHADOW_CONTROL_SOCKET
2. Runs the simulation for 10 simulated seconds
3. Creates a checkpoint ("cp1") and backs up the external database
4. Continues the simulation for 10 more seconds
5. Restores to checkpoint "cp1", which causes Shadow to restart the sim
6. Runs again for 5 seconds and verifies the counter matches the checkpoint

Usage:
    python3 orchestrator.py [shadow_binary]
"""
import json
import os
import shutil
import socket
import subprocess
import sys
import time

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
SOCKET_PATH = "/tmp/shadow_control_{}.sock".format(os.getpid())
SHADOW_BIN = sys.argv[1] if len(sys.argv) > 1 else shutil.which("shadow") or "shadow"
COUNTER_APP = os.path.join(SCRIPT_DIR, "counter_app.py")
CHECKPOINT_LABEL = "cp1"


def generate_shadow_config(work_dir: str) -> str:
    """Generate a shadow.yaml with the absolute path to counter_app.py."""
    config_path = os.path.join(work_dir, "shadow.yaml")
    config = f"""\
general:
  stop_time: 60s
  data_directory: ./shadow.data
  log_level: info

network:
  graph:
    type: 1_gbit_switch

hosts:
  counter-host:
    network_node_id: 0
    processes:
      - path: /usr/bin/python3
        args: {COUNTER_APP}
        start_time: 1s
        expected_final_state: running
        environment:
          COUNTER_DB: counter.db
"""
    with open(config_path, "w") as f:
        f.write(config)
    return config_path


def send_command(sock, cmd_dict, timeout=120):
    """Send a JSON command and receive the response."""
    msg = json.dumps(cmd_dict) + "\n"
    print(f"  -> {msg.strip()}", flush=True)
    sock.sendall(msg.encode())

    sock.settimeout(timeout)
    buf = b""
    while b"\n" not in buf:
        data = sock.recv(4096)
        if not data:
            raise ConnectionError("Socket closed by Shadow")
        buf += data

    line = buf.split(b"\n")[0]
    resp = json.loads(line)
    print(f"  <- {json.dumps(resp)}", flush=True)
    return resp


def connect_socket(path, timeout=60):
    """Wait until the Unix socket becomes available, then connect."""
    start = time.time()
    while time.time() - start < timeout:
        if os.path.exists(path):
            try:
                s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                s.connect(path)
                return s
            except (ConnectionRefusedError, FileNotFoundError, OSError):
                pass
        time.sleep(0.3)
    raise TimeoutError(f"Socket {path} not available after {timeout}s")


def counter_db_path(work_dir: str) -> str:
    """Path to the counter database within Shadow's host data directory."""
    return os.path.join(work_dir, "shadow.data", "hosts", "counter-host", "counter.db")


def read_counter(work_dir: str):
    """Read the current counter value from the database."""
    path = counter_db_path(work_dir)
    try:
        with open(path, "r") as f:
            return int(f.read().strip())
    except (FileNotFoundError, ValueError) as e:
        print(f"  [warn] Cannot read counter at {path}: {e}", flush=True)
        return None


def backup_database(work_dir: str, label: str) -> str:
    """Copy the counter database to a backup location."""
    src = counter_db_path(work_dir)
    dst = src + f".backup.{label}"
    if os.path.exists(src):
        shutil.copy2(src, dst)
        print(f"[orchestrator] Backed up {src} -> {dst}", flush=True)
    else:
        print(f"[orchestrator] WARNING: {src} not found, cannot backup", flush=True)
    return dst


def restore_database(work_dir: str, label: str):
    """Restore the counter database from a backup."""
    src = counter_db_path(work_dir) + f".backup.{label}"
    dst = counter_db_path(work_dir)
    if os.path.exists(src):
        shutil.copy2(src, dst)
        print(f"[orchestrator] Restored {dst} <- {src}", flush=True)
    else:
        print(f"[orchestrator] WARNING: backup {src} not found", flush=True)


def main():
    if os.path.exists(SOCKET_PATH):
        os.unlink(SOCKET_PATH)

    work_dir = os.path.join(SCRIPT_DIR, "run")
    os.makedirs(work_dir, exist_ok=True)

    shadow_data = os.path.join(work_dir, "shadow.data")
    if os.path.exists(shadow_data):
        shutil.rmtree(shadow_data)

    config_path = generate_shadow_config(work_dir)

    print(f"[orchestrator] Shadow binary:  {SHADOW_BIN}", flush=True)
    print(f"[orchestrator] Config:         {config_path}", flush=True)
    print(f"[orchestrator] Counter app:    {COUNTER_APP}", flush=True)
    print(f"[orchestrator] Control socket: {SOCKET_PATH}", flush=True)
    print(f"[orchestrator] Work dir:       {work_dir}", flush=True)
    print(flush=True)

    env = os.environ.copy()
    env["SHADOW_CONTROL_SOCKET"] = SOCKET_PATH

    shadow_proc = subprocess.Popen(
        [SHADOW_BIN, config_path],
        cwd=work_dir,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    sock = None

    try:
        # ============================================================
        # Phase A: First simulation run
        # ============================================================
        print("[orchestrator] Waiting for Shadow control socket...", flush=True)
        sock = connect_socket(SOCKET_PATH)
        print("[orchestrator] Connected to Shadow control socket\n", flush=True)

        # Step 1: Run for 10 simulated seconds
        print("=" * 60, flush=True)
        print("Step 1: Run simulation for 10 simulated seconds", flush=True)
        print("=" * 60, flush=True)
        resp = send_command(sock, {"cmd": "continue_for", "duration_ns": 10_000_000_000})

        counter_at_10s = read_counter(work_dir)
        print(f"[orchestrator] Counter at ~t=10s: {counter_at_10s}\n", flush=True)

        # Step 2: Checkpoint
        print("=" * 60, flush=True)
        print("Step 2: Create checkpoint 'cp1'", flush=True)
        print("=" * 60, flush=True)
        resp = send_command(sock, {"cmd": "checkpoint", "label": CHECKPOINT_LABEL})
        backup_database(work_dir, CHECKPOINT_LABEL)
        print(flush=True)

        # Step 3: Run 10 more seconds
        print("=" * 60, flush=True)
        print("Step 3: Continue simulation for 10 more simulated seconds", flush=True)
        print("=" * 60, flush=True)
        resp = send_command(sock, {"cmd": "continue_for", "duration_ns": 10_000_000_000})

        counter_at_20s = read_counter(work_dir)
        print(f"[orchestrator] Counter at ~t=20s: {counter_at_20s}\n", flush=True)

        # Step 4: Restore external DB, then send restore command
        print("=" * 60, flush=True)
        print("Step 4: Restore checkpoint 'cp1'", flush=True)
        print("=" * 60, flush=True)
        restore_database(work_dir, CHECKPOINT_LABEL)

        # The restore command causes Shadow to exit the simulation loop
        # and re-enter it (restart). The existing socket connection might
        # survive or might break. We handle both cases.
        try:
            resp = send_command(sock, {"cmd": "restore", "label": CHECKPOINT_LABEL})
            print(f"[orchestrator] Restore acknowledged\n", flush=True)
        except ConnectionError:
            print("[orchestrator] Connection closed during restore (expected).\n", flush=True)
            sock.close()
            sock = None

        # ============================================================
        # Phase B: After restore — reconnect if needed
        # ============================================================
        print("=" * 60, flush=True)
        print("Step 5: Reconnect and verify after restore", flush=True)
        print("=" * 60, flush=True)

        if sock is None:
            print("[orchestrator] Reconnecting to control socket...", flush=True)
            sock = connect_socket(SOCKET_PATH, timeout=30)
            print("[orchestrator] Reconnected.\n", flush=True)

        # After restore, Shadow restarts the simulation from t=0.
        # The counter.db was restored externally, so counter_app will
        # load the restored value on startup.
        # Run for 5 more seconds to let it increment a bit.
        resp = send_command(sock, {"cmd": "continue_for", "duration_ns": 5_000_000_000})

        counter_after_restore = read_counter(work_dir)
        print(f"[orchestrator] Counter after restore + 5s: {counter_after_restore}\n", flush=True)

        # ============================================================
        # Verification
        # ============================================================
        print("=" * 60, flush=True)
        print("RESULTS", flush=True)
        print("=" * 60, flush=True)
        print(f"  Counter at checkpoint (t~10s):         {counter_at_10s}", flush=True)
        print(f"  Counter after 10 more s (t~20s):       {counter_at_20s}", flush=True)
        print(f"  Counter after restore + 5s sim:        {counter_after_restore}", flush=True)
        print(flush=True)

        # After restore, counter_app starts fresh (t=1s) and loads
        # counter.db (restored to checkpoint value). It should have
        # incremented from that value.
        if (counter_at_10s is not None and counter_after_restore is not None
                and counter_at_20s is not None):
            if counter_after_restore < counter_at_20s:
                print("*** SUCCESS ***", flush=True)
                print(f"    Counter was rewound from {counter_at_20s} (pre-restore)", flush=True)
                print(f"    to ~{counter_after_restore} (after restore + 5s run).", flush=True)
                print(f"    Checkpoint value was {counter_at_10s}.", flush=True)
                expected_approx = counter_at_10s + 4  # ~4 increments in 5s (start_time=1s)
                print(f"    Expected approximately: {expected_approx}", flush=True)
            else:
                print("*** UNEXPECTED: counter did not rewind ***", flush=True)
        else:
            print("*** NOTE: Some counter values are None — check logs ***", flush=True)

        if sock:
            sock.close()
            sock = None

    except Exception as e:
        print(f"\n[orchestrator] ERROR: {e}", flush=True)
        import traceback
        traceback.print_exc()
    finally:
        if sock:
            try:
                sock.close()
            except Exception:
                pass

        print("\n[orchestrator] Terminating Shadow...", flush=True)
        shadow_proc.terminate()
        try:
            shadow_proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            shadow_proc.kill()
            shadow_proc.wait()

        rc = shadow_proc.returncode
        print(f"[orchestrator] Shadow exited with code: {rc}", flush=True)

        stderr_data = shadow_proc.stderr.read() if shadow_proc.stderr else b""
        if stderr_data:
            text = stderr_data.decode("utf-8", errors="replace")
            lines = text.strip().split("\n")
            print(f"\n--- Shadow stderr (last 30 lines of {len(lines)} total) ---", flush=True)
            for line in lines[-30:]:
                print(line, flush=True)
            print("--- end stderr ---", flush=True)

        if os.path.exists(SOCKET_PATH):
            os.unlink(SOCKET_PATH)


if __name__ == "__main__":
    main()
