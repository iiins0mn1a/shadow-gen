# Multi-host Checkpoint/Restore Test

This test adds a second checkpoint/restore scenario under `tests/`, focused on
multi-host simulations and interactive orchestration.

## Files

- `orchestrator_repl.py`: interactive REPL orchestrator
- `counter_app_multi.py`: simple per-host counter application
- `shadow_multi.yaml`: two-host sample Shadow config

## Inputs

The orchestrator uses:

- `--config`: a Shadow YAML config
- `--database`: host-to-path JSON map for external data dependencies

Example:

```bash
python3 ./tests/checkpoint-multihost/orchestrator_repl.py \
  --shadow-bin /home/ins0/Repos/Event-Driven-Testnet/repos/shadow-gen/build/src/main/shadow \
  --config ./tests/checkpoint-multihost/shadow_multi.yaml \
  --database '{"host-a":"/home/ins0/Repos/Event-Driven-Testnet/repos/shadow-gen/tests/checkpoint-multihost/run/shadow.data/hosts/host-a/counterA.db","host-b":"/home/ins0/Repos/Event-Driven-Testnet/repos/shadow-gen/tests/checkpoint-multihost/run/shadow.data/hosts/host-b/counterB.db"}' \
  --work-dir ./tests/checkpoint-multihost/run \
  --clean-data \
  --scenario verify
```

## REPL Commands

- `pause`
- `continue`
- `continue_for <seconds>`
- `step [seconds]` (approximate step via `continue_for`)
- `checkpoint <label>` (backs up all mapped DBs, then asks Shadow checkpoint)
- `restore <label>` (restores all mapped DBs, then asks Shadow restore)
- `status`
- `show db`
- `help`
- `quit`

## Suggested Minimal Validation Path

After entering REPL:

1. `continue_for 10`
2. `checkpoint cp1`
3. `continue_for 10`
4. `show db` (observe larger values)
5. `restore cp1`
6. `continue_for 5`
7. `show db` (values should be lower than pre-restore and close to checkpoint+5s)

## Automated verify scenario (`--scenario verify`)

Non-interactive run that checks **both**:

1. **Shadow host checkpoint**: `shadow.data/checkpoints/<label>.checkpoint.json` exists, has a
   `hosts` entry per `--database` key, each with `event_queue` and `processes` (scheduler /
   process graph was snapshotted, not an empty stub).
2. **External DB restore**: after advancing simulation and mutating on-disk counters, restoring
   the checkpoint returns DB file contents to exactly what they were at checkpoint time.
3. **Post-restore timeline**: after `continue_for 5s` from the restored state, each counter grows
   by about five (virtual time continues from the restored point). If DB restore works but this
   fails, host-side simulation state may not match the checkpoint.

Example:

```bash
python3 tests/checkpoint-multihost/orchestrator_repl.py \
  --shadow-bin build/src/main/shadow \
  --config tests/checkpoint-multihost/shadow_multi.yaml \
  --database '{"host-a":"'"$(pwd)"'/tests/checkpoint-multihost/run/shadow.data/hosts/host-a/counterA.db","host-b":"'"$(pwd)"'/tests/checkpoint-multihost/run/shadow.data/hosts/host-b/counterB.db"}' \
  --work-dir tests/checkpoint-multihost/run \
  --clean-data \
  --scenario verify \
  --verify-label cp_verify
```

Use `--verify-label` to pick the checkpoint label (default `cp_verify`). Exit code `0` means all
assertions passed; `1` means a check failed (message on stderr).

## Using a custom CRIU via `CRIU_BIN` (WSL2-friendly)

On WSL2, the system `criu` may lack the capabilities or kernel support needed for
Shadow's process-level checkpoint/restore. The companion `criu-demo` in your workspace
(`../criu_demo/criu-demo`) shows how to build and configure a CRIU binary that works
with `--unprivileged`.

If you have built and `setcap`'d a CRIU at:

```bash
/home/ins0/workspace-for-agent/user_data/task/criu_demo/criu-src/criu/criu
```

you can point Shadow at it via the `CRIU_BIN` environment variable:

```bash
CRIU_BIN=/home/ins0/workspace-for-agent/user_data/task/criu_demo/criu-src/criu/criu \
python3 tests/checkpoint-multihost/orchestrator_repl.py \
  --shadow-bin build/src/main/shadow \
  --config tests/checkpoint-multihost/shadow_multi.yaml \
  --database '{"host-a":"'"$(pwd)"'/tests/checkpoint-multihost/run/shadow.data/hosts/host-a/counterA.db","host-b":"'"$(pwd)"'/tests/checkpoint-multihost/run/shadow.data/hosts/host-b/counterB.db"}' \
  --work-dir tests/checkpoint-multihost/run \
  --clean-data \
  --scenario verify \
  --verify-label cp_verify
```

Shadow's CRIU integration (`src/main/core/checkpoint/criu.rs`) respects `CRIU_BIN` and
will use this binary for `criu check --unprivileged`, `dump`, and `restore`. If the
preflight CRIU check in the verify scenario fails, consult the `criu-demo` README and
`WSL_CRIU_DEMO_GUIDE.md` to ensure CRIU has been built and configured correctly on
your system before re-running the multi-host test.

## Notes

- `step` is intentionally approximate, implemented as small `continue_for`.
- On `restore`, Shadow may restart the simulation loop. The orchestrator handles
  socket reconnection automatically if needed.
- For real external dependencies (outside `shadow.data`), pass their absolute
  paths in `--database`.
