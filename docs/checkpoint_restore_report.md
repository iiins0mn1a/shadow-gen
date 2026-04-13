# Checkpoint/Restore in Shadow: Architecture, Workflow, Challenges, and Current Status

## Abstract

This report summarizes the current checkpoint/restore design in Shadow after the recent network-recovery refactoring. The implementation remains hybrid: CRIU restores native process images, while Shadow serializes and replays simulation-owned state such as event queues, clocks, descriptor graphs, and blocked-syscall metadata. Recent work extended the restore path in three directions. First, restore behavior is now described by explicit protocol metadata instead of relying solely on post-restore heuristics. Second, descriptor replay has been restructured around object recreation followed by explicit rebinding. Third, the system now restores epoll-, eventfd-, and timerfd-based event loops, including multi-process-per-host workloads that better approximate an Ethereum deployment. The result is a materially stronger foundation for checkpoint/restore of complex networked applications.

## 1. System Model

Shadow checkpoint/restore spans three distinct state domains:

1. Native process state.
   This includes registers, address spaces, thread stacks, and the kernel-visible process tree. Shadow delegates this part to CRIU.
2. Simulation state.
   This includes host clocks, event queues, deterministic counters, RNG state, process/thread runtime metadata, descriptor tables, socket runtime, and blocked-syscall bookkeeping. Shadow serializes and restores this state itself.
3. Shared-memory backing state.
   Shadow-managed shared-memory objects must exist before restored native processes can remap them.

The implementation is therefore intentionally not monolithic. It is a coordinated recovery protocol across CRIU, Shadow-owned checkpoint metadata, and shared-memory backing files.

## 2. Checkpoint Pipeline

Checkpoint is taken at a simulation-consistent cut point and proceeds in ordered stages.

### 2.1 Shared-memory backup

Shadow first copies relevant shared-memory backing files into the checkpoint directory. This ensures that CRIU restore will later find the files required by the restored process images.

### 2.2 Simulation snapshot

Shadow then serializes host and process state into Rust-defined checkpoint structures. The snapshot includes:

- host event queues and scheduling metadata
- CPU clock state and deterministic counters
- RNG state
- process and thread metadata
- descriptor-table snapshots
- socket runtime snapshots
- blocked-syscall runtime metadata
- restore-protocol metadata

### 2.3 Native process dump

Shadow invokes CRIU for each running managed process and records the corresponding image directory in the simulation checkpoint. This step captures the native process image, but not Shadow’s internal object graph.

### 2.4 Descriptor census and protocol metadata

To support more realistic workloads, the checkpoint now records richer fd-level state:

- stable `canonical_handle` identifiers
- epoll watch registrations
- eventfd state
- timerfd state

The checkpoint also stores explicit restore-protocol metadata:

- `restore_protocol.mode`
- `restore_protocol.restore_epoch`
- `restore_protocol.connections`
- `restore_protocol.blocked_syscalls`

This protocol metadata is not yet a full DMTCP-style distributed protocol, but it makes restore semantics explicit and makes later replay decisions auditable.

## 3. Restore Pipeline

Restore is also staged, but the ordering is critical.

### 3.1 Metadata loading and shmem restoration

Shadow loads checkpoint metadata and restores shared-memory backing files into place.

### 3.2 Native process restoration

CRIU restores native processes, threads, and address spaces. At this stage, native processes exist again, but Shadow’s own runtime objects have not yet been reconstructed.

### 3.3 Host-global replay

Shadow reattaches restored host shmem and restores host-global state:

- serialized event queue
- host event/time counters
- deterministic counters
- RNG state
- CPU timing state

This ordering is important. The restored queue and host-global timing context must exist before per-process descriptor restore can safely schedule new local events.

### 3.4 Process and descriptor replay

Each process is then reconstructed from checkpoint metadata. Descriptor replay proceeds in two phases:

1. recreate restore-capable objects
2. rebind relationships among those recreated objects

This is the key restore pattern of the current implementation.

### 3.5 Blocked-syscall rearming and resume scheduling

After object replay, Shadow reestablishes blocked-syscall conditions, timeout completions, and compatibility resume tasks. This includes the current protocol-aware handling of restored blocked syscalls and legacy compatibility nudges when required.

## 4. Descriptor Recovery as Object Recreation Plus Rebinding

### 4.1 Why rebinding is necessary

Checkpoint/restore does not preserve Rust object identity. After restore, a descriptor graph must be reconstructed from new live objects. It is therefore incorrect to assume that a restored epoll instance can simply resume watching “the same” fd object it watched before checkpoint.

### 4.2 Current descriptor strategy

During checkpoint, each descriptor entry records:

- descriptor kind
- file status and descriptor flags
- `canonical_handle`
- transport/runtime metadata for sockets
- epoll watch registrations
- eventfd state
- timerfd state

During restore, Shadow:

1. recreates the underlying descriptor objects
2. records a mapping from old `canonical_handle` to new `OpenFile`
3. uses that mapping to rebind epoll registrations and other cross-object references

This strategy is implemented through an explicit descriptor-restore context in process replay. The result is significantly easier to reason about than earlier ad hoc fixups.

## 5. Restore-Protocol Layer

The explicit restore-protocol layer should be understood as an intermediate step between heuristic replay and a fully protocol-driven design.

The current checkpoint records:

- protocol mode
- restore epoch
- connection metadata
- blocked-syscall instance metadata
- poll-watch information
- restore actions such as `RearmCondition`, `RearmTimeout`, or `ResumeImmediately`

This layer serves three purposes:

1. it encodes restore intent explicitly
2. it provides stable identifiers for blocked syscalls and network-visible objects
3. it narrows the scope of heuristic compatibility paths

The current design therefore remains hybrid, but it is no longer purely heuristic.

## 6. Overall Engineering Workflow

The recent work followed a repeated workflow that proved effective.

### 6.1 Start from a failing restore symptom

The initial failures were not treated as generic “network restore is broken”. Instead, each failure was narrowed to a concrete post-restore behavioral gap:

- TCP data plane moved, but application-visible monotonic time did not
- UDP recovery failed in the mixed network test
- epoll-based applications failed with missing descriptors
- multi-process-per-host event loops restored, but produced no post-restore events

### 6.2 Build a sharper reproducer

Synthetic tests were introduced to isolate each missing capability:

- `checkpoint-network-multihost` for mixed TCP/UDP traffic
- strict TCP time checks to detect frozen application clocks
- `checkpoint-network-eth-poc` for epoll-based multi-socket event loops
- `checkpoint-network-eth-multiproc` for multi-process-per-host workloads with loopback RPC, cross-host traffic, epoll, eventfd, and timerfd

These tests were intentionally capability-driven rather than protocol-faithful reimplementations of Ethereum clients.

### 6.3 Make hidden state explicit

Once the failing capability was isolated, the next step was to expose the restore-relevant state:

- stable descriptor identity via `canonical_handle`
- epoll interest sets
- eventfd state
- timerfd remaining time and periodic interval
- blocked-syscall metadata
- descriptor census diagnostics

This repeated pattern was essential. In nearly every failure, the blocker was not that Shadow lacked a generic “restore call”, but that restore-relevant state was still implicit in live runtime objects.

### 6.4 Reconstruct objects first, then rebind semantics

The design rule that emerged from the debugging work is:

> restore containers and naming context first, then recreate objects, then rebind relationships.

This rule now governs descriptor replay and, more recently, host event-queue replay.

### 6.5 Regress continuously

Every change was validated against the previously working tests before being accepted. This prevented the network-focused work from regressing basic multihost or TCP restore behavior.

## 7. Major Challenges and Implemented Solutions

### 7.1 Challenge: Native state and simulation state can diverge

CRIU restores native processes independently of Shadow’s Rust object graph. Without explicit replay, restored native processes can observe a runtime that no longer matches Shadow’s internal state.

#### Solution

Shadow restores its own object graph from `SimulationCheckpoint`, reattaches restored shmem, and reconstructs process/thread/descriptor state explicitly.

### 7.2 Challenge: Post-restore time semantics were initially incorrect

Earlier runs showed that TCP could continue after restore while `monotonic_ns` remained effectively frozen in the restored application.

#### Solution

Shadow now mirrors time into the restored host-shmem view that restored native processes still consult. This fixed the strict TCP time regression and restored application-visible progress after checkpoint/restore.

### 7.3 Challenge: Epoll event loops required more than socket replay

The first Ethereum-like proof of concept used `epoll` through Python’s selector interface. Earlier restore code recreated sockets but not epoll descriptors or their registration state, leading to immediate `EBADF` failures.

#### Solution

Shadow now checkpoints epoll descriptors and their watch registrations, recreates epoll objects during restore, and rebinds each watch to the newly recreated target object using canonical-handle identity.

### 7.4 Challenge: Async runtimes rely on more than sockets

More realistic workloads use descriptor classes such as `eventfd` and `timerfd` as wakeup and scheduling sources. These descriptors were initially outside the restore-capable set.

#### Solution

The checkpoint schema and descriptor replay path were extended to include:

- `EventFdSnapshot`
- `TimerFdSnapshot`
- descriptor recreation for eventfd and timerfd
- epoll rebinding to eventfd/timerfd targets

This was necessary for multi-process event-loop workloads.

### 7.5 Challenge: Restored multi-process event loops produced no post-restore activity

The multiprocess Ethereum-like test revealed a subtler failure. The restore path succeeded, but all `restored.*` outputs remained empty. Investigation showed that all affected threads were blocked in `epoll_wait`, and no wakeup source fired after restore.

The root cause was restore ordering. During descriptor replay, `timerfd` restoration correctly scheduled fresh timer-expiration events, but those events were then discarded because `apply_host_checkpoint()` later replaced the host event queue with the serialized checkpoint queue.

#### Solution

Host-global state is now restored before process reconstruction. Concretely:

1. restore host shmem alias
2. restore serialized event queue and host-global counters
3. reconstruct processes and descriptors

This change preserved newly scheduled timerfd events and restored the self-activation path of post-restore epoll loops.

This fix is a strong example of the current design principle: restore the container first, then restore the objects that will attach new behavior to it.

## 8. Refactoring Outcomes

The code changes were not limited to feature growth; several refactorings improved readability and reduced redundancy.

### 8.1 Descriptor replay structure

Descriptor replay is now centralized in an explicit restore context. The context owns:

- the target descriptor table
- the checkpoint descriptor list
- the canonical-handle-to-open-file map
- the set of already rebound epoll instances

This structure makes the replay process legible and localizes restore bookkeeping.

### 8.2 Shared helper paths for descriptor recreation

Socket, epoll, eventfd, and timerfd restore paths now share common helper routines for:

- reusing already recreated open files
- registering recreated open files into the descriptor table

This reduced repetition and made the rebinding pattern consistent across descriptor kinds.

### 8.3 Host-global restore helper

Host-global restore is now factored into a dedicated helper that restores:

- event queue
- timing counters
- deterministic counters
- RNG state
- CPU state

This makes `apply_host_checkpoint(...)` read more clearly as a sequence of phases rather than as a single large procedure.

## 9. Validation Status

At the current stage, the following regression tests pass:

- `tests/checkpoint-multihost`
- `tests/checkpoint-network-multihost`
- `tests/checkpoint-network-multihost --mode tcp --strict-tcp-time`
- `tests/checkpoint-network-eth-poc`
- `tests/checkpoint-network-eth-multiproc`

These tests collectively validate:

- multihost process restore
- TCP and UDP post-restore communication
- application-visible monotonic time progression
- epoll-based event loops
- eventfd/timerfd-based async wakeups
- multiple processes per host
- loopback TCP RPC plus cross-host TCP/UDP traffic

This does not imply full generality for arbitrary workloads, but it does establish that Shadow now supports a significantly broader and more realistic class of networked restore scenarios than before.

## 10. Remaining Limitations

Several limitations remain.

1. The restore-protocol layer is still only partially protocol-driven. Legacy compatibility paths still exist.
2. Shadow still does not implement a full DMTCP-style globally coordinated network cut with transport draining and restart-time peer rewiring for every protocol class.
3. Descriptor replay is not yet complete for all descriptor kinds that may appear in real Ethereum clients, especially `pipe` and any client-specific auxiliary fds not yet covered by the synthetic tests.
4. Epoll restore currently focuses on registrations and restore-time readiness reconstruction, but highly pathological edge-triggered or one-shot corner cases may still need further validation.
5. The current synthetic Ethereum-like tests model the topology and async structure of client workloads, but they are not yet full geth/prysm integration tests.

## 11. Conclusion

The current Shadow checkpoint/restore design remains hybrid, but it is now substantially more structured than the original heuristic implementation. The key technical lesson of this work is that correct restore depends less on replaying isolated objects than on restoring the surrounding semantic context in the correct order.

Two principles emerged repeatedly:

1. Make restore-relevant state explicit at checkpoint time.
2. Restore containers first, then recreate objects, then rebind relationships.

These principles guided the recovery of time semantics, epoll state, eventfd/timerfd-driven wakeups, and multi-process-per-host event loops. They also provide a concrete foundation for the next stage: moving from synthetic Ethereum-like workloads toward real geth and prysm checkpoint/restore experiments.
