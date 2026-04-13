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
    /// Explicit restore protocol metadata used to recover network and blocked syscall
    /// semantics without relying on legacy heuristics.
    #[serde(default)]
    pub restore_protocol: RestoreProtocolSnapshot,
}

impl SimulationCheckpoint {
    pub const CURRENT_VERSION: u32 = 13;
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RestoreProtocolModeSnapshot {
    #[default]
    LegacyHeuristic,
    ProtocolV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreProtocolSnapshot {
    #[serde(default)]
    pub mode: RestoreProtocolModeSnapshot,
    #[serde(default)]
    pub restore_epoch: u64,
    #[serde(default)]
    pub connections: Vec<ConnectionProtocolSnapshot>,
    #[serde(default)]
    pub blocked_syscalls: Vec<BlockedSyscallProtocolSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionProtocolSnapshot {
    pub connection_id: u64,
    pub host_id: u32,
    pub process_id: u32,
    pub fd: u32,
    #[serde(default)]
    pub canonical_handle: Option<u64>,
    pub role: ConnectionProtocolRoleSnapshot,
    pub transport: DescriptorSocketTransport,
    pub implementation: Option<DescriptorSocketImplementation>,
    pub local_ip: Option<String>,
    pub local_port: Option<u16>,
    pub peer_ip: Option<String>,
    pub peer_port: Option<u16>,
    pub is_listening: bool,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ConnectionProtocolRoleSnapshot {
    #[default]
    Unspecified,
    Listener,
    Connected,
    Unconnected,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BlockedSyscallPhaseSnapshot {
    #[default]
    Unknown,
    Waiting,
    Completing,
    Resuming,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedSyscallProtocolSnapshot {
    pub host_id: u32,
    pub process_id: u32,
    pub thread_id: u32,
    pub syscall_nr: i64,
    pub instance_id: u64,
    pub phase: BlockedSyscallPhaseSnapshot,
    #[serde(default)]
    pub action: BlockedSyscallRestoreActionSnapshot,
    pub timeout_ns: Option<u64>,
    #[serde(default)]
    pub poll_watches: Vec<PollWatchSnapshot>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BlockedSyscallRestoreActionSnapshot {
    #[default]
    None,
    ResumeImmediately,
    RearmTimeout,
    RearmCondition,
    RearmPoll,
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
    ResumeProcess { process_id: u32, thread_id: u32 },
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
    ShutdownProcess { process_id: u32, signal: i32 },
    /// Relay packet forwarding (intra-host).
    RelayForward { relay_id: u64 },
    /// Timer expiry callback.
    TimerExpire { timer_id: u64, expire_id: u64 },
    /// Continuation after execve replaces a process image.
    ExecContinuation { process_id: u32 },
    /// A generic opaque task that cannot be meaningfully serialized. This
    /// variant exists as a fallback; checkpointing will skip events
    /// containing opaque tasks and log a warning.
    Opaque { description: String },
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
    /// Best-effort visibility into descriptor state at checkpoint time.
    /// Used for restore diagnostics and sanity checks.
    pub descriptor_count_hint: u32,
    /// Descriptor table snapshot (fd-level metadata) for restore diagnostics and replay.
    pub descriptors: Vec<DescriptorEntrySnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DescriptorEntrySnapshot {
    pub fd: u32,
    pub descriptor_flags_bits: i32,
    pub file_status_bits: i32,
    pub file_mode_bits: u32,
    pub file_kind: DescriptorFileKind,
    #[serde(default)]
    pub canonical_handle: Option<u64>,
    #[serde(default)]
    pub socket_transport: Option<DescriptorSocketTransport>,
    #[serde(default)]
    pub socket_implementation: Option<DescriptorSocketImplementation>,
    #[serde(default)]
    pub socket_local_ip: Option<String>,
    #[serde(default)]
    pub socket_local_port: Option<u16>,
    #[serde(default)]
    pub socket_peer_ip: Option<String>,
    #[serde(default)]
    pub socket_peer_port: Option<u16>,
    #[serde(default)]
    pub socket_is_listening: bool,
    #[serde(default)]
    pub socket_runtime: Option<SocketRuntimeSnapshot>,
    #[serde(default)]
    pub epoll_watches: Vec<EpollWatchSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SocketRuntimeSnapshot {
    LegacyTcp(LegacyTcpSocketRuntimeSnapshot),
    Tcp(TcpSocketRuntimeSnapshot),
    Udp(UdpSocketRuntimeSnapshot),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpollWatchSnapshot {
    pub watched_fd: u32,
    #[serde(default)]
    pub watched_canonical_handle: Option<u64>,
    pub interest_bits: u32,
    pub data: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyTcpSocketRuntimeSnapshot {
    pub tcp_state: u32,
    pub tcp_flags: u32,
    pub tcp_error: u32,
    pub is_server: bool,
    #[serde(default)]
    pub server_pending_max: Option<u32>,
    #[serde(default)]
    pub server_pending_count: Option<u32>,
    #[serde(default)]
    pub server_process_for_children: Option<u32>,
    #[serde(default)]
    pub server_last_peer_ip: Option<String>,
    #[serde(default)]
    pub server_last_peer_port: Option<u16>,
    #[serde(default)]
    pub server_last_ip: Option<String>,
    pub recv_start: u32,
    pub recv_next: u32,
    pub recv_window: u32,
    pub recv_end: u32,
    pub recv_last_window: u32,
    pub recv_last_ack: u32,
    pub recv_last_seq: u32,
    pub send_unacked: u32,
    pub send_next: u32,
    pub send_window: u32,
    pub send_end: u32,
    pub send_last_ack: u32,
    pub send_last_window: u32,
    pub send_highest_seq: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TcpSocketRuntimeSnapshot {
    pub file_state_bits: u16,
    pub connect_result_is_pending: bool,
    pub shutdown_read: bool,
    pub shutdown_write: bool,
    pub has_association: bool,
    pub has_data_to_send: bool,
    pub tcp_state_kind: String,
    pub tcp_poll_state_bits: u32,
    pub tcp_local_ip: Option<String>,
    pub tcp_local_port: Option<u16>,
    pub tcp_remote_ip: Option<String>,
    pub tcp_remote_port: Option<u16>,
    #[serde(default)]
    pub tcp_listen_child_count: Option<u32>,
    #[serde(default)]
    pub tcp_listen_accept_queue_len: Option<u32>,
    #[serde(default)]
    pub tcp_send_buffer_len: Option<u32>,
    #[serde(default)]
    pub tcp_send_transmitted_up_to: Option<u32>,
    #[serde(default)]
    pub tcp_send_next_seq: Option<u32>,
    #[serde(default)]
    pub tcp_recv_buffer_len: Option<u32>,
    #[serde(default)]
    pub tcp_recv_next_seq: Option<u32>,
    #[serde(default)]
    pub tcp_recv_window_len: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UdpBufferedSendMessageSnapshot {
    pub payload: Vec<u8>,
    pub src_ip: String,
    pub src_port: u16,
    pub dst_ip: String,
    pub dst_port: u16,
    pub packet_priority: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UdpBufferedRecvMessageSnapshot {
    pub payload: Vec<u8>,
    pub src_ip: String,
    pub src_port: u16,
    pub dst_ip: String,
    pub dst_port: u16,
    pub recv_time_ns: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UdpSocketRuntimeSnapshot {
    pub state_bits: u16,
    pub shutdown_read: bool,
    pub shutdown_write: bool,
    pub peer_ip: Option<String>,
    pub peer_port: Option<u16>,
    pub bound_ip: Option<String>,
    pub bound_port: Option<u16>,
    pub has_association: bool,
    pub recv_time_of_last_read_packet_ns: Option<u64>,
    pub send_buffer_soft_limit_bytes: usize,
    pub recv_buffer_soft_limit_bytes: usize,
    pub send_buffer_len_bytes: usize,
    pub recv_buffer_len_bytes: usize,
    pub send_queue: Vec<UdpBufferedSendMessageSnapshot>,
    pub recv_queue: Vec<UdpBufferedRecvMessageSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DescriptorFileKind {
    Legacy,
    Pipe,
    EventFd,
    Socket,
    TimerFd,
    Epoll,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DescriptorSocketTransport {
    Tcp,
    Udp,
    Unix,
    Netlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DescriptorSocketImplementation {
    LegacyTcp,
    Tcp,
    Udp,
    Unix,
    Netlink,
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
    #[serde(default)]
    pub runtime: Option<ThreadRuntimeSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRuntimeSnapshot {
    pub event_kind: ThreadEventKindSnapshot,
    #[serde(default)]
    pub restore_policy: ThreadRestorePolicySnapshot,
    #[serde(default)]
    pub restore_epoch: u64,
    #[serde(default)]
    pub blocked_syscall_active: bool,
    #[serde(default)]
    pub blocked_syscall_instance_id: Option<u64>,
    #[serde(default)]
    pub blocked_syscall_phase: BlockedSyscallPhaseSnapshot,
    #[serde(default)]
    pub blocked_restore_action: BlockedSyscallRestoreActionSnapshot,
    #[serde(default)]
    pub blocked_timeout_ns: Option<u64>,
    #[serde(default)]
    pub blocked_trigger_fd: Option<u32>,
    #[serde(default)]
    pub blocked_trigger_state_bits: Option<u16>,
    #[serde(default)]
    pub blocked_active_file_fd: Option<u32>,
    #[serde(default)]
    pub blocked_trigger_kind: Option<BlockedTriggerKindSnapshot>,
    #[serde(default)]
    pub poll_watches: Vec<PollWatchSnapshot>,
    #[serde(default)]
    pub pending_result: Option<PendingSyscallResultSnapshot>,
    pub blocked_syscall_nr: Option<i64>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThreadRestorePolicySnapshot {
    #[default]
    LegacyHeuristic,
    ProtocolV1,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockedTriggerKindSnapshot {
    File,
    LegacyDescriptor,
    Futex,
    Child,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollWatchSnapshot {
    pub fd: u32,
    pub epoll_events: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendingSyscallResultSnapshot {
    Done { retval_raw: u64 },
    Failed { errno: i32, restartable: bool },
    Native,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadEventKindSnapshot {
    StartReq,
    ProcessDeath,
    Syscall,
    AddThreadRes,
    SyscallComplete,
    Other,
}
