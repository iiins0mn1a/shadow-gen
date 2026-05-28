# TDT Checkpoint Study

This experiment keeps the frequently edited logic inside `shadow-gen`, while
reusing the real-client TDT harness for runtime preparation and external-state
management.

The root goal is to answer two questions with real clients:

1. Does `checkpoint -> run -> restore -> rerun` preserve deterministic
   application progress in the post-checkpoint window?
2. What are the checkpoint and restore overheads for `1 / 4 / 8` beacon-node
   setups with one shared geth node?

## Layout

- `experiment.toml`: experiment-specific config
- `run_study.py`: local runner
- `results/`: generated JSON, report, and diffs

The runner references:

- `TDT/tdt_config.toml`
- `TDT/scripts/tdt_config.py`
- `TDT/scripts/tdt_logcheck.py`
- `TDT/scripts/tdt_orchestrator.py`

Only the stable TDT helpers are reused. Socket control, log slicing, window
comparison, and experiment reporting are all implemented locally so iteration
stays inside the writable workspace.

## Determinism Procedure

For one setup:

1. Prepare a fresh TDT runtime in `work_root`.
2. Launch Shadow with a control socket.
3. Advance in fixed steps until the network is checkpoint-ready.
4. Wait one additional settle window.
5. Issue `checkpoint <label>`.
6. Back up the managed external state bundle.
7. Record role-log offsets at the checkpoint boundary.
8. Continue for the comparison window and capture the appended application logs.
9. Restore the managed external bundle and Shadow checkpoint.
10. Continue for the same comparison window and capture the replay logs.
11. Compare the two windows host-by-host.

The determinism oracle uses only application logs:

- `geth`
- `beacon`
- `validator`

Recorder helper logs and host-side `shadow.data` artifacts are excluded.

## Performance Procedure

For each setup and trial:

1. Prepare a fresh runtime.
2. Warm up until checkpoint-ready and settle.
3. Measure checkpoint latency from command send to completion response.
4. Measure restore latency from restore start to pause/reconnect completion.

The runner also records managed external backup/restore time separately, so the
report can distinguish Shadow-side latency from orchestration-side overhead.

## Usage

Run all experiments:

```bash
python3 experiments/tdt-checkpoint-study/run_study.py --mode all
```

Run determinism for one setup:

```bash
python3 experiments/tdt-checkpoint-study/run_study.py --mode determinism --setup 4
```

Run performance only:

```bash
python3 experiments/tdt-checkpoint-study/run_study.py --mode performance --setup all --trials 5
```

## Outputs

- `results/determinism-setup-<N>.json`
- `results/performance-setup-<N>.json`
- `results/REPORT.md`
- `results/diffs/` for determinism mismatches
- `results/windows/setup-<N>/reference/<hostname>/{stdout,stderr}.log`
- `results/windows/setup-<N>/replay/<hostname>/{stdout,stderr}.log`
