# Checkpoint/Restore in Shadow: Current Architecture, Challenges, and Solutions

## Abstract

This report documents the current checkpoint/restore implementation in Shadow after the recent refactoring and network-recovery work. The system is intentionally hybrid. Native process state is recovered with CRIU, while Shadow-specific simulation state is serialized into a Rust-defined checkpoint schema and replayed during restore. The recent work extends this baseline with explicit restore-protocol metadata, descriptor-level runtime snapshots, and an object-rebinding strategy for network-facing state. The resulting design is sufficient to recover the current multihost checkpoint tests, the TCP strict-time test, and an Ethereum-like mesh proof of concept.

## 1. System Model

Shadow’s checkpoint/restore design is based on a separation of concerns between three state domains:

1. Native managed-process state, including registers, memory, and the host-kernel view of threads and file descriptors.
2. Shadow-internal simulation state, including host event queues, CPU clocks, deterministic counters, RNG state, process/thread runtime metadata, and Shadow-managed descriptor state.
3. Shared-memory backing state, particularly `/dev/shm` objects that must exist before restored processes remap them.

The implementation therefore does not use a single monolithic checkpoint mechanism. Instead, it combines:

- CRIU for native process images.
- A JSON-serializable simulation checkpoint for Shadow-owned state.
- A shared-memory backup/restore phase for Shadow-managed shmem files.

This decomposition remains the central architectural choice of the current implementation.

## 2. Checkpoint Pipeline

At a scheduling-window boundary, Shadow performs checkpointing in four ordered stages.

### 2.1 Shared-memory backup

Shadow first enumerates the `/dev/shm` objects relevant to the simulation and copies them into the checkpoint directory. This step is necessary because CRIU restore expects the backing files to exist before native process reconstruction.

### 2.2 Simulation snapshot

Shadow then walks every host and serializes the simulation state into `HostCheckpoint` and `ProcessCheckpoint` objects. The serialized state includes:

- Event queues and event-ordering metadata.
- CPU time and availability.
- Deterministic counters and RNG state.
- Process and thread metadata.
- Descriptor-table snapshots.
- Socket runtime snapshots.
- Blocked-syscall metadata.

This stage is implemented in Rust and is independent of CRIU.

### 2.3 Native process dump

For every running managed process, Shadow invokes CRIU with `leave_running=true`. The resulting image directory is recorded in the simulation checkpoint so that the JSON checkpoint and CRIU artifacts remain linked.

### 2.4 Checkpoint assembly

Finally, Shadow assembles `SimulationCheckpoint`, which now includes explicit restore-protocol metadata:

- `restore_protocol.mode`
- `restore_protocol.restore_epoch`
- `restore_protocol.connections`
- `restore_protocol.blocked_syscalls`

This protocol layer is not a complete DMTCP-style runtime, but it makes restore intent explicit and reduces reliance on purely heuristic post-restore patching.

## 3. Restore Pipeline

Restore is likewise staged.

### 3.1 Metadata and shmem restoration

Shadow first loads the serialized checkpoint metadata and restores the shared-memory files into place.

### 3.2 Native process restoration

CRIU then reconstructs the native process trees. At this point, the native process images exist again, but Shadow-specific object graphs and runtime contracts have not yet been replayed.

### 3.3 Host replay

Once Worker TLS and host-local execution context are ready, Shadow replays each `HostCheckpoint`. This stage reconstructs:

- Restored `HostShmem` attachment.
- Host CPU clock and runahead state.
- Process objects.
- Thread objects.
- Event queues.
- Descriptor tables.
- Blocked-syscall rearming tasks.
- Post-restore resume scheduling.

The core routine is `apply_host_checkpoint(...)`, which is responsible for translating serialized runtime metadata back into live simulation objects.

## 4. Descriptor Recovery and Object Rebinding

### 4.1 Motivation

The most important recent failure mode was not transport failure per se, but descriptor-graph inconsistency. In the Ethereum-like mesh proof of concept, each process owned a real `epoll` descriptor via Python’s `selectors.DefaultSelector`. Earlier restore code recreated sockets but not epoll descriptors, which caused restored applications to fail with `EBADF` as soon as their event loops resumed.

### 4.2 Current solution

The current design addresses this using an object-rebinding strategy.

During checkpoint, Shadow records:

- File-descriptor kind.
- Stable `canonical_handle` identifiers for restore-time correspondence.
- Socket runtime metadata.
- Epoll watch registrations, including watched file descriptor, watched canonical handle, interest bits, and callback data.

During restore, Shadow performs descriptor replay in two phases:

1. Recreate descriptor objects themselves, including sockets and epoll instances.
2. Rebind higher-level relationships between those recreated objects using the preserved canonical handles.

This design is implemented through a dedicated descriptor-restore context in `Process::replay_descriptor_entries(...)`. The context maintains the mapping from old canonical handles to newly created `OpenFile` objects, and then uses that mapping to rebuild epoll watch relationships.

This is a substantial conceptual improvement over ad hoc replay because the restore logic no longer assumes that object identity survives checkpoint/restore. Instead, it reconstructs object identity explicitly.

## 5. Restore-Protocol Metadata

The current checkpoint format contains explicit restore-protocol metadata. This layer records:

- A protocol mode (`LegacyHeuristic` or `ProtocolV1`).
- A restore epoch.
- Connection-level metadata for sockets.
- Blocked-syscall metadata, including instance IDs, phases, timeout semantics, and poll-watch sets.

This information does not yet constitute a full distributed recovery protocol in the DMTCP sense. However, it serves three important purposes:

1. It makes the expected restore semantics explicit.
2. It allows the restore path to distinguish between legacy heuristic recovery and protocol-oriented replay.
3. It provides stable identifiers for blocked syscalls and network-facing descriptors.

The protocol layer is therefore best understood as a structured intermediate stage between heuristic replay and a more fully protocol-driven design.

## 6. Major Challenges and Implemented Solutions

### 6.1 Challenge: Divergence between restored native state and restored simulation state

Because CRIU restores native processes independently of Shadow’s Rust object graph, the two sides can drift unless their interfaces are explicitly re-synchronized.

#### Implemented solution

Shadow restores its own object graph from `SimulationCheckpoint` and reattaches restored host/process/thread shared-memory state. The design is therefore intentionally bifurcated, but the replay phase reconstructs the simulation-side contracts needed by the restored native threads.

### 6.2 Challenge: Post-restore time semantics were broken

Earlier testing showed that TCP data could continue to flow after restore even while application-visible `monotonic_ns` values remained frozen. This indicated a mismatch between the active Shadow host clock and the restored shim-visible host shmem clock.

#### Implemented solution

Shadow now mirrors time updates into the restored host-shmem view that restored native processes still observe. As a result, the application-visible time source advances correctly after restore, and the TCP strict-time regression now passes.

### 6.3 Challenge: Epoll-based event loops failed after restore

The Ethereum-like mesh proof of concept revealed that restoring sockets alone was insufficient. Applications using epoll retained a dependency on a descriptor object that had not been reconstructed.

#### Implemented solution

Shadow now checkpoints epoll watch registrations and restores epoll descriptors explicitly. The restore logic then rebinds watches to newly created target objects using canonical-handle correspondence.

### 6.4 Challenge: Blocked-syscall replay required more structure than a one-shot signal-style heuristic

The earlier restore path relied heavily on heuristics such as one-shot EINTR injection and socket fixups. These mechanisms were useful for early experimentation but were difficult to reason about and difficult to generalize.

#### Implemented solution

The current implementation introduces blocked-syscall instance IDs, phases, and restore actions. While heuristic compatibility paths still exist, the checkpoint format now preserves the information needed to drive replay more explicitly.

## 7. Refactoring Outcomes

The recent refactoring focused on maintainability rather than semantic expansion.

### 7.1 Descriptor replay readability

Descriptor replay in `process.rs` is now organized around an explicit restore context rather than manually threading temporary state through multiple loops and helper calls. This reduces local redundancy and makes the two-phase replay strategy easier to follow.

### 7.2 Checkpoint flow readability

The checkpoint path in `manager.rs` now uses named helper routines for shmem-path collection and CRIU image generation. This preserves behavior while making the high-level checkpoint sequence easier to read and audit.

### 7.3 Restore policy readability

Environment-driven compatibility behavior is now concentrated into small helper routines instead of being re-parsed inline at each decision site. This reduces noise in the core restore logic.

## 8. Validation Status

After the current refactoring and network-recovery work, the following checkpoints pass:

- `tests/checkpoint-multihost`
- `tests/checkpoint-network-multihost`
- `tests/checkpoint-network-multihost --mode tcp --strict-tcp-time`
- `tests/checkpoint-network-eth-poc`

This does not imply that checkpoint/restore is fully general for all future workloads. It does imply that the current implementation can recover:

- Basic multihost process state.
- TCP and UDP communication across restore.
- Application-visible post-restore time progression.
- Single-process multi-socket event loops based on epoll.

## 9. Remaining Limitations

Several limitations remain visible.

1. Descriptor replay is still incomplete for some non-socket descriptor classes, such as `pipe`, `eventfd`, and `timerfd`.
2. The restore protocol remains only partially protocol-driven; some compatibility heuristics are still retained.
3. The system does not yet implement a DMTCP-style globally coordinated network cut with transport draining and restart-time peer rewiring across all protocol classes.
4. Epoll recovery currently restores registrations, but not every internal intermediate state that may matter for more pathological edge-triggered or one-shot corner cases.

## 10. Conclusion

The current Shadow checkpoint/restore system is a hybrid architecture in which CRIU restores native process images and Shadow replays simulation-specific semantics from a structured checkpoint format. The principal technical challenge is not merely serialization, but re-establishing the semantic bindings between restored native objects and restored simulation objects.

The recent work shows that explicit protocol metadata, descriptor-level snapshots, and object rebinding can substantially improve restore correctness without abandoning the existing architecture. The implementation now supports a materially broader class of network workloads than the original heuristic design, while remaining testable and incrementally refactorable.
