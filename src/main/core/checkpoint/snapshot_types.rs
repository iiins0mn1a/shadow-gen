//! Serializable snapshot types that mirror the live simulation structures.
//!
//! These types are designed to be fully `serde`-compatible so that simulation
//! state can be persisted to disk or transmitted over a network. Each snapshot
//! type has conversion routines to/from the corresponding live type.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::core::configuration::ProcessFinalState;

// ---------------------------------------------------------------------------
// Top-level checkpoint
// ---------------------------------------------------------------------------

/// The complete state of a Shadow simulation at a window boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulationCheckpoint {
    pub version: u32,
    pub sim_time_ns: u64,
    pub window_start_ns: u64,
    pub window_end_ns: u64,
    pub prng_state: PrngSnapshot,
    pub runahead: RunaheadSnapshot,
    pub hosts: Vec<HostCheckpoint>,
    /// Serialized handle for the Manager-level shared memory block.
    pub manager_shmem_handle: String,
    /// Directory containing backed-up `/dev/shm` files.
    pub shmem_backup_dir: PathBuf,
    /// CRIU images base directory.
    pub criu_base_dir: PathBuf,
}

impl SimulationCheckpoint {
    pub const CURRENT_VERSION: u32 = 4;
}

// ---------------------------------------------------------------------------
// PRNG state
// ---------------------------------------------------------------------------

/// Snapshot of the PRNG state (Xoshiro256PlusPlus).
/// The generator's internal state is 4 x u64.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrngSnapshot {
    pub s: [u64; 4],
}

// ---------------------------------------------------------------------------
// Runahead
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunaheadSnapshot {
    pub is_dynamic: bool,
    pub min_possible_latency_ns: u64,
    pub min_used_latency_ns: Option<u64>,
    pub min_runahead_config_ns: Option<u64>,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// A serializable representation of a single simulation event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSnapshot {
    pub time_ns: u64,
    pub data: EventDataSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventDataSnapshot {
    Packet(PacketEventSnapshot),
    Local(LocalEventSnapshot),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PacketEventSnapshot {
    pub src_host_id: u32,
    pub src_host_event_id: u64,
    pub packet: PacketSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalEventSnapshot {
    pub event_id: u64,
    pub task: TaskDescriptor,
}

// ---------------------------------------------------------------------------
// Task descriptors (serializable replacement for TaskRef closures)
// ---------------------------------------------------------------------------

/// A serializable description of a scheduled task. When restoring from a
/// checkpoint, the descriptor is used to reconstruct the corresponding
/// `TaskRef` closure.
///
/// New variants can be added as more task types are identified in the
/// codebase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskDescriptor {
    /// Resume a process thread that was blocked on a syscall condition.
    ResumeProcess {
        process_id: u32,
        thread_id: u32,
    },
    /// Start an application process at the configured start time.
    StartApplication {
        plugin_name: String,
        plugin_path: String,
        argv: Vec<String>,
        envv: Vec<String>,
        pause_for_debugging: bool,
        shutdown_signal: i32,
        shutdown_time_ns: Option<u64>,
        expected_final_state: ProcessFinalState,
    },
    /// Send a shutdown signal to a process.
    ShutdownProcess {
        process_id: u32,
        signal: i32,
    },
    /// Relay packet forwarding (intra-host).
    RelayForward {
        relay_id: u64,
    },
    /// Timer expiry callback.
    TimerExpire {
        timer_id: u64,
        expire_id: u64,
    },
    /// Continuation after execve replaces a process image.
    ExecContinuation {
        process_id: u32,
    },
    /// A generic opaque task that cannot be meaningfully serialized. This
    /// variant exists as a fallback; checkpointing will skip events
    /// containing opaque tasks and log a warning.
    Opaque {
        description: String,
    },
}

// ---------------------------------------------------------------------------
// Packet
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PacketSnapshot {
    pub protocol: PacketProtocolSnapshot,
    pub payload: Vec<u8>,
    pub priority: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PacketProtocolSnapshot {
    Tcp(TcpHeaderSnapshot),
    Udp(UdpHeaderSnapshot),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TcpHeaderSnapshot {
    pub src_ip: u32,
    pub src_port: u16,
    pub dst_ip: u32,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub selective_acks: Vec<(u32, u32)>,
    pub window_scale: Option<u8>,
    pub timestamp: Option<u32>,
    pub timestamp_echo: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UdpHeaderSnapshot {
    pub src_ip: u32,
    pub src_port: u16,
    pub dst_ip: u32,
    pub dst_port: u16,
}

// ---------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCheckpoint {
    pub host_id: u32,
    pub hostname: String,
    /// Serialized event queue.
    pub event_queue: Vec<EventSnapshot>,
    /// The time of the last popped event (nanoseconds since simulation start).
    pub last_popped_event_time_ns: u64,
    /// Counters to ensure IDs don't collide after restore.
    pub next_event_id: u64,
    pub next_thread_id: u64,
    pub next_packet_id: u64,
    pub determinism_sequence_counter: u64,
    pub packet_priority_counter: u64,
    pub cpu_now_ns: u64,
    pub cpu_available_ns: u64,
    /// Per-host PRNG state.
    pub random_state: PrngSnapshot,
    /// Process metadata.
    pub processes: Vec<ProcessCheckpoint>,
    /// Serialized handle for HostShmem (`ShMemBlockSerialized.to_string()`).
    pub host_shmem_handle: String,
}

// ---------------------------------------------------------------------------
// Process (high-level metadata; CRIU handles the real process state)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessCheckpoint {
    pub process_id: u32,
    /// Path to the CRIU image directory.
    pub criu_image_dir: Option<PathBuf>,
    /// The native PID at checkpoint time.
    pub native_pid: i32,
    pub is_running: bool,
    pub parent_pid: u32,
    pub group_id: u32,
    pub session_id: u32,
    pub dumpable: i32,
    /// Thread metadata.
    pub threads: Vec<ThreadCheckpoint>,
    /// Serialized handle for ProcessShmem.
    pub process_shmem_handle: String,
}

// ---------------------------------------------------------------------------
// Thread
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadCheckpoint {
    pub thread_id: u32,
    pub native_tid: i32,
    /// Serialized handle for the per-thread IPC shared memory block.
    pub ipc_shmem_handle: String,
    /// Serialized handle for ThreadShmem.
    pub thread_shmem_handle: String,
    /// Raw bytes of `ShimEventToShadow`, preserving where the shim was parked.
    pub current_event_bytes: Vec<u8>,
}
