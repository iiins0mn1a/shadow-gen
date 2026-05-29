//! A thread of a managed process.
//!
//! This contains the code where the simulator can create or communicate with a managed process.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::io::Write;
use std::ops::Deref;
use std::os::fd::AsRawFd;
use std::os::unix::prelude::OsStrExt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, atomic};
use std::time::Instant;

use linux_api::errno::Errno;
use linux_api::posix_types::Pid;
use linux_api::sched::CloneFlags;
use linux_api::signal::tgkill;
use linux_api::syscall::SyscallNum;
use log::{Level, debug, error, log_enabled, trace};
use rand::Rng as _;
use rustix::pipe::PipeFlags;
use rustix::process::WaitOptions;
use shadow_shim_helper_rs::ipc::IPCData;
use shadow_shim_helper_rs::shim_event::{
    ShimEventAddThreadReq, ShimEventAddThreadRes, ShimEventStartRes, ShimEventSyscall,
    ShimEventSyscallComplete, ShimEventToShadow, ShimEventToShim,
};
use shadow_shim_helper_rs::syscall_types::{
    ForeignPtr, SyscallArgs, SyscallReg, UntypedForeignPtr,
};
use shadow_shmem::allocator::{ShMemBlock, ShMemBlockAlias, ShMemBlockSerialized, shdeserialize};
use vasi_sync::scchannel::SelfContainedChannelError;

use super::context::ThreadContext;
use super::descriptor::descriptor_table::DescriptorHandle;
use super::descriptor::{CompatFile, File};
use super::host::Host;
use super::syscall::condition::SyscallCondition;
use crate::core::checkpoint::snapshot_types::{
    BlockedSyscallPhaseSnapshot, BlockedSyscallRestoreActionSnapshot, ThreadEventKindSnapshot,
    ThreadRestorePolicySnapshot, ThreadRuntimeSnapshot,
};
use crate::core::worker::{WORKER_SHARED, Worker};
use crate::cshadow;
use crate::host::syscall::handler::SyscallHandler;
use crate::host::syscall::types::{ForeignArrayPtr, SyscallReturn};
use crate::utility::{VerifyPluginPathError, inject_preloads, syscall, verify_plugin_path};

#[derive(Clone, Copy, Debug, Default)]
struct SyscallPerf {
    handler_calls: u64,
    handler_wall_ns: u64,
    continue_calls: u64,
    continue_wall_ns: u64,
    done_results: u64,
    blocked_results: u64,
    native_results: u64,
    synthetic_completions: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct ContinueExchangePerf {
    calls: u64,
    wall_ns: u64,
    receive_wall_ns: u64,
}

#[derive(Default)]
struct ManagedThreadPerfStats {
    continue_plugin_calls: AtomicU64,
    continue_plugin_wall_ns: AtomicU64,
    continue_plugin_receive_wall_ns: AtomicU64,
    continue_plugin_lock_wall_ns: AtomicU64,
    continue_plugin_prepare_wall_ns: AtomicU64,
    continue_plugin_send_wall_ns: AtomicU64,
    continue_plugin_time_update_wall_ns: AtomicU64,
    syscall_handler_calls: AtomicU64,
    syscall_handler_wall_ns: AtomicU64,
    syscall_continue_calls: AtomicU64,
    syscall_continue_wall_ns: AtomicU64,
    syscalls: Mutex<HashMap<u32, SyscallPerf>>,
    syscall_fds: Mutex<HashMap<(u32, i32), u64>>,
    syscall_fd_kinds: Mutex<HashMap<(u32, &'static str), u64>>,
    continue_exchanges: Mutex<HashMap<(&'static str, &'static str), ContinueExchangePerf>>,
}

static MANAGED_THREAD_PERF_STATS: OnceLock<ManagedThreadPerfStats> = OnceLock::new();
static TDT_PERF_COUNTERS_ENABLED: OnceLock<bool> = OnceLock::new();
static TDT_ASYNC_CONTINUE_ENABLED: OnceLock<bool> = OnceLock::new();
static TDT_ASYNC_SCOPE_DRAIN_ENABLED: OnceLock<bool> = OnceLock::new();

thread_local! {
    static TDT_WORKER_BODY_CONTINUE_RECEIVE_WALL_NS: Cell<u64> = const { Cell::new(0) };
}

fn tdt_perf_counters_enabled() -> bool {
    *TDT_PERF_COUNTERS_ENABLED.get_or_init(|| {
        std::env::var("SHADOW_TDT_PERF_COUNTERS")
            .map(|raw| {
                let raw = raw.trim();
                !raw.is_empty() && raw != "0"
            })
            .unwrap_or(false)
    })
}

fn managed_thread_perf_stats() -> Option<&'static ManagedThreadPerfStats> {
    tdt_perf_counters_enabled()
        .then(|| MANAGED_THREAD_PERF_STATS.get_or_init(ManagedThreadPerfStats::default))
}

pub fn tdt_async_continue_enabled() -> bool {
    *TDT_ASYNC_CONTINUE_ENABLED.get_or_init(|| {
        std::env::var("SHADOW_TDT_ASYNC_CONTINUE")
            .map(|raw| {
                let raw = raw.trim();
                !raw.is_empty() && raw != "0"
            })
            .unwrap_or(false)
    })
}

pub fn tdt_async_continue_scope_drain_enabled() -> bool {
    *TDT_ASYNC_SCOPE_DRAIN_ENABLED.get_or_init(|| {
        std::env::var("SHADOW_TDT_ASYNC_SCOPE_DRAIN")
            .map(|raw| {
                let raw = raw.trim();
                !raw.is_empty() && raw != "0"
            })
            .unwrap_or(false)
    })
}

fn tdt_async_continue_syscall_allowed(syscall_nr: u32) -> bool {
    let syscall_nr = i64::from(syscall_nr);
    syscall_nr == libc::SYS_epoll_wait
        || syscall_nr == libc::SYS_epoll_pwait
        || syscall_nr == libc::SYS_epoll_pwait2
        || syscall_nr == libc::SYS_poll
        || syscall_nr == libc::SYS_ppoll
        || syscall_nr == libc::SYS_select
        || syscall_nr == libc::SYS_pselect6
}

pub fn tdt_perf_reset_worker_body_continue_receive_wall_ns() {
    if !tdt_perf_counters_enabled() {
        return;
    }
    TDT_WORKER_BODY_CONTINUE_RECEIVE_WALL_NS.with(|counter| counter.set(0));
}

pub fn tdt_perf_take_worker_body_continue_receive_wall_ns() -> u64 {
    if !tdt_perf_counters_enabled() {
        return 0;
    }
    TDT_WORKER_BODY_CONTINUE_RECEIVE_WALL_NS.with(|counter| counter.replace(0))
}

fn tdt_perf_add_worker_body_continue_receive_wall_ns(delta_ns: u64) {
    TDT_WORKER_BODY_CONTINUE_RECEIVE_WALL_NS.with(|counter| {
        counter.set(counter.get().saturating_add(delta_ns));
    });
}

fn syscall_name(syscall_nr: u32) -> &'static str {
    SyscallNum::new(syscall_nr)
        .to_str()
        .unwrap_or("unknown-syscall")
}

fn syscall_first_fd(syscall_nr: u32, args: &SyscallArgs) -> Option<i32> {
    match i64::from(syscall_nr) {
        libc::SYS_read
        | libc::SYS_write
        | libc::SYS_readv
        | libc::SYS_writev
        | libc::SYS_pread64
        | libc::SYS_pwrite64
        | libc::SYS_preadv
        | libc::SYS_pwritev
        | libc::SYS_preadv2
        | libc::SYS_pwritev2
        | libc::SYS_epoll_wait
        | libc::SYS_epoll_pwait
        | libc::SYS_epoll_pwait2
        | libc::SYS_close
        | libc::SYS_fsync
        | libc::SYS_fdatasync => i64::from(args.args[0]).try_into().ok(),
        _ => None,
    }
}

fn descriptor_kind_for_fd(ctx: &ThreadContext, fd: i32) -> &'static str {
    let Ok(fd) = u32::try_from(fd) else {
        return "invalid-fd";
    };
    let Some(handle) = DescriptorHandle::new(fd) else {
        return "invalid-fd";
    };
    let desc_table = ctx.thread.descriptor_table_borrow(ctx.host);
    let Some(desc) = desc_table.get(handle) else {
        return "missing";
    };
    match desc.file() {
        CompatFile::New(file) => match file.inner_file() {
            File::Pipe(_) => "pipe",
            File::EventFd(_) => "eventfd",
            File::Socket(_) => "socket",
            File::TimerFd(_) => "timerfd",
            File::Epoll(_) => "epoll",
        },
        CompatFile::Legacy(file) => match unsafe { cshadow::legacyfile_getType(file.ptr()) } {
            cshadow::_LegacyFileType_DT_TCPSOCKET => "legacy-tcp",
            cshadow::_LegacyFileType_DT_EPOLL => "legacy-epoll",
            cshadow::_LegacyFileType_DT_FILE => "legacy-file",
            _ => "legacy-other",
        },
    }
}

fn record_syscall_perf(syscall_nr: u32, update: impl FnOnce(&mut SyscallPerf)) {
    let Some(stats) = managed_thread_perf_stats() else {
        return;
    };
    record_syscall_perf_with_stats(stats, syscall_nr, update);
}

fn record_syscall_perf_with_stats(
    stats: &ManagedThreadPerfStats,
    syscall_nr: u32,
    update: impl FnOnce(&mut SyscallPerf),
) {
    let mut syscalls = stats.syscalls.lock().unwrap();
    update(syscalls.entry(syscall_nr).or_default());
}

fn record_syscall_fd_with_stats(stats: &ManagedThreadPerfStats, syscall_nr: u32, fd: i32) {
    let mut fds = stats.syscall_fds.lock().unwrap();
    *fds.entry((syscall_nr, fd)).or_default() += 1;
}

fn record_syscall_fd_kind_with_stats(
    stats: &ManagedThreadPerfStats,
    syscall_nr: u32,
    kind: &'static str,
) {
    let mut kinds = stats.syscall_fd_kinds.lock().unwrap();
    *kinds.entry((syscall_nr, kind)).or_default() += 1;
}

fn shim_event_to_shadow_kind(event: &ShimEventToShadow) -> &'static str {
    match event {
        ShimEventToShadow::StartReq(_) => "StartReq",
        ShimEventToShadow::ProcessDeath => "ProcessDeath",
        ShimEventToShadow::Syscall(_) => "Syscall",
        ShimEventToShadow::SyscallComplete(_) => "SyscallComplete",
        ShimEventToShadow::AddThreadRes(_) => "AddThreadRes",
    }
}

fn shim_event_to_shim_kind(event: &ShimEventToShim) -> &'static str {
    match event {
        ShimEventToShim::StartRes(_) => "StartRes",
        ShimEventToShim::Syscall(_) => "Syscall",
        ShimEventToShim::AddThreadReq(_) => "AddThreadReq",
        ShimEventToShim::SyscallComplete(_) => "SyscallComplete",
        ShimEventToShim::SyscallDoNative => "SyscallDoNative",
    }
}

fn record_continue_exchange(
    stats: &ManagedThreadPerfStats,
    sent: &'static str,
    received: &'static str,
    wall_ns: u64,
    receive_wall_ns: u64,
) {
    let mut exchanges = stats.continue_exchanges.lock().unwrap();
    let entry = exchanges.entry((sent, received)).or_default();
    entry.calls += 1;
    entry.wall_ns += wall_ns;
    entry.receive_wall_ns += receive_wall_ns;
}

pub fn log_tdt_managed_thread_perf_stats() {
    let Some(stats) = MANAGED_THREAD_PERF_STATS.get() else {
        return;
    };

    let continue_plugin_calls = stats.continue_plugin_calls.load(Ordering::Relaxed);
    let continue_plugin_wall_ns = stats.continue_plugin_wall_ns.load(Ordering::Relaxed);
    let continue_plugin_receive_wall_ns = stats
        .continue_plugin_receive_wall_ns
        .load(Ordering::Relaxed);
    let continue_plugin_lock_wall_ns = stats.continue_plugin_lock_wall_ns.load(Ordering::Relaxed);
    let continue_plugin_prepare_wall_ns = stats
        .continue_plugin_prepare_wall_ns
        .load(Ordering::Relaxed);
    let continue_plugin_send_wall_ns = stats.continue_plugin_send_wall_ns.load(Ordering::Relaxed);
    let continue_plugin_time_update_wall_ns = stats
        .continue_plugin_time_update_wall_ns
        .load(Ordering::Relaxed);
    let syscall_handler_calls = stats.syscall_handler_calls.load(Ordering::Relaxed);
    let syscall_handler_wall_ns = stats.syscall_handler_wall_ns.load(Ordering::Relaxed);
    let syscall_continue_calls = stats.syscall_continue_calls.load(Ordering::Relaxed);
    let syscall_continue_wall_ns = stats.syscall_continue_wall_ns.load(Ordering::Relaxed);
    let syscall_top = {
        let fd_counts = stats.syscall_fds.lock().unwrap();
        let fd_kind_counts = stats.syscall_fd_kinds.lock().unwrap();
        let mut items: Vec<_> = stats
            .syscalls
            .lock()
            .unwrap()
            .iter()
            .map(|(nr, perf)| (*nr, *perf))
            .collect();
        items.sort_by_key(|(_, perf)| {
            std::cmp::Reverse(perf.handler_wall_ns + perf.continue_wall_ns)
        });
        items
            .into_iter()
            .take(8)
            .map(|(nr, perf)| {
                let mut top_fds = fd_counts
                    .iter()
                    .filter_map(|((fd_nr, fd), count)| (*fd_nr == nr).then_some((*fd, *count)))
                    .collect::<Vec<_>>();
                top_fds.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
                let fd_top = top_fds
                    .into_iter()
                    .take(4)
                    .map(|(fd, count)| format!("{fd}={count}"))
                    .collect::<Vec<_>>()
                    .join(";");
                let mut top_fd_kinds = fd_kind_counts
                    .iter()
                    .filter_map(|((fd_nr, kind), count)| {
                        (*fd_nr == nr).then_some((*kind, *count))
                    })
                    .collect::<Vec<_>>();
                top_fd_kinds.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
                let fd_kind_top = top_fd_kinds
                    .into_iter()
                    .take(4)
                    .map(|(kind, count)| format!("{kind}={count}"))
                    .collect::<Vec<_>>()
                    .join(";");
                let avg_continue_ns = if perf.continue_calls == 0 {
                    0.0
                } else {
                    perf.continue_wall_ns as f64 / perf.continue_calls as f64
                };
                format!(
                    "{}({}):handler_ms={:.3}:continue_ms={:.3}:continue_avg_ns={:.1}:handler_calls={}:continue_calls={}:done={}:block={}:native={}:synthetic={}:fd_top={}:fd_kind_top={}",
                    syscall_name(nr),
                    nr,
                    perf.handler_wall_ns as f64 / 1_000_000.0,
                    perf.continue_wall_ns as f64 / 1_000_000.0,
                    avg_continue_ns,
                    perf.handler_calls,
                    perf.continue_calls,
                    perf.done_results,
                    perf.blocked_results,
                    perf.native_results,
                    perf.synthetic_completions,
                    fd_top,
                    fd_kind_top,
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let continue_exchange_top = {
        let mut items: Vec<_> = stats
            .continue_exchanges
            .lock()
            .unwrap()
            .iter()
            .map(|(kind, perf)| (*kind, *perf))
            .collect();
        items.sort_by_key(|(_, perf)| std::cmp::Reverse(perf.receive_wall_ns));
        items
            .into_iter()
            .take(8)
            .map(|((sent, received), perf)| {
                format!(
                    "{}->{}:calls={}:wall_ms={:.3}:receive_ms={:.3}",
                    sent,
                    received,
                    perf.calls,
                    perf.wall_ns as f64 / 1_000_000.0,
                    perf.receive_wall_ns as f64 / 1_000_000.0,
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    };

    log::info!(
        "TDT managed-thread counters: continue_plugin_calls={} continue_plugin_wall_ns={} continue_plugin_receive_wall_ns={} continue_plugin_lock_wall_ns={} continue_plugin_prepare_wall_ns={} continue_plugin_send_wall_ns={} continue_plugin_time_update_wall_ns={} syscall_handler_calls={} syscall_handler_wall_ns={} syscall_continue_calls={} syscall_continue_wall_ns={} syscall_top={} continue_exchange_top={}",
        continue_plugin_calls,
        continue_plugin_wall_ns,
        continue_plugin_receive_wall_ns,
        continue_plugin_lock_wall_ns,
        continue_plugin_prepare_wall_ns,
        continue_plugin_send_wall_ns,
        continue_plugin_time_update_wall_ns,
        syscall_handler_calls,
        syscall_handler_wall_ns,
        syscall_continue_calls,
        syscall_continue_wall_ns,
        syscall_top,
        continue_exchange_top,
    );
}

/// The ManagedThread's state after having been allowed to execute some code.
#[derive(Debug)]
#[must_use]
pub enum ResumeResult {
    /// Blocked on a SyscallCondition.
    Blocked(SyscallCondition),
    /// A syscall completion was sent to the native thread and must be drained
    /// before the next checkpoint/pause safepoint.
    AsyncPending,
    /// The native thread has exited with the given code.
    ExitedThread(i32),
    /// The thread's process has exited.
    ExitedProcess,
}

#[must_use]
enum ContinuePluginResult {
    Ready(ShimEventToShadow),
    AsyncPending,
}

pub struct ManagedThread {
    ipc_shmem: Arc<IpcShmem>,
    is_running: Cell<bool>,
    return_code: Cell<Option<i32>>,

    /* holds the event for the most recent call from the plugin/shim */
    current_event: RefCell<ShimEventToShadow>,

    native_pid: linux_api::posix_types::Pid,
    native_tid: linux_api::posix_types::Pid,

    // Value storing the current CPU affinity of the thread (more precisely,
    // of the native thread backing this thread object). This value will be set
    // to AFFINITY_UNINIT if CPU pinning is not enabled or if the thread has
    // not yet been pinned to a CPU.
    affinity: Cell<i32>,
    // If true, force one synthetic EINTR completion for the first restored syscall.
    // This de-stales blocked syscall handshakes after checkpoint/restore.
    force_syscall_eintr_once: Cell<bool>,
    // If true, send a one-way shim refresh event before returning the first
    // post-restore syscall result to the application.
    needs_post_restore_refresh: Cell<bool>,
    // True between sending an async continuation to the shim and receiving the
    // next shim event. While set, Shadow does not own this host's shim shmem lock.
    async_continue_pending: Cell<bool>,
}

enum IpcShmem {
    Owned(Arc<ShMemBlock<'static, IPCData>>),
    Restored {
        block: ShMemBlockAlias<'static, IPCData>,
        handle: String,
    },
}

impl Deref for IpcShmem {
    type Target = IPCData;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Owned(block) => block,
            Self::Restored { block, .. } => block,
        }
    }
}

impl ManagedThread {
    pub fn runtime_snapshot(&self) -> ThreadRuntimeSnapshot {
        assert!(
            !self.async_continue_pending.get(),
            "checkpoint attempted with a managed thread async continuation in flight"
        );
        let event = *self.current_event.borrow();
        match event {
            ShimEventToShadow::StartReq(_) => ThreadRuntimeSnapshot {
                event_kind: ThreadEventKindSnapshot::StartReq,
                restore_policy: ThreadRestorePolicySnapshot::ProtocolV1,
                restore_epoch: 0,
                blocked_syscall_active: false,
                blocked_syscall_instance_id: None,
                blocked_syscall_phase: BlockedSyscallPhaseSnapshot::Unknown,
                blocked_restore_action: BlockedSyscallRestoreActionSnapshot::None,
                blocked_timeout_ns: None,
                blocked_trigger_fd: None,
                blocked_trigger_state_bits: None,
                blocked_active_file_fd: None,
                blocked_trigger_kind: None,
                blocked_futex_word: None,
                blocked_listener_sequence_value: None,
                poll_watches: Vec::new(),
                pending_result: None,
                blocked_syscall_nr: None,
            },
            ShimEventToShadow::ProcessDeath => ThreadRuntimeSnapshot {
                event_kind: ThreadEventKindSnapshot::ProcessDeath,
                restore_policy: ThreadRestorePolicySnapshot::ProtocolV1,
                restore_epoch: 0,
                blocked_syscall_active: false,
                blocked_syscall_instance_id: None,
                blocked_syscall_phase: BlockedSyscallPhaseSnapshot::Unknown,
                blocked_restore_action: BlockedSyscallRestoreActionSnapshot::None,
                blocked_timeout_ns: None,
                blocked_trigger_fd: None,
                blocked_trigger_state_bits: None,
                blocked_active_file_fd: None,
                blocked_trigger_kind: None,
                blocked_futex_word: None,
                blocked_listener_sequence_value: None,
                poll_watches: Vec::new(),
                pending_result: None,
                blocked_syscall_nr: None,
            },
            ShimEventToShadow::Syscall(syscall) => ThreadRuntimeSnapshot {
                event_kind: ThreadEventKindSnapshot::Syscall,
                restore_policy: ThreadRestorePolicySnapshot::ProtocolV1,
                restore_epoch: 0,
                blocked_syscall_active: true,
                blocked_syscall_instance_id: None,
                blocked_syscall_phase: BlockedSyscallPhaseSnapshot::Waiting,
                blocked_restore_action: BlockedSyscallRestoreActionSnapshot::None,
                blocked_timeout_ns: None,
                blocked_trigger_fd: None,
                blocked_trigger_state_bits: None,
                blocked_active_file_fd: None,
                blocked_trigger_kind: None,
                blocked_futex_word: None,
                blocked_listener_sequence_value: None,
                poll_watches: Vec::new(),
                pending_result: None,
                blocked_syscall_nr: Some(syscall.syscall_args.number),
            },
            ShimEventToShadow::AddThreadRes(_) => ThreadRuntimeSnapshot {
                event_kind: ThreadEventKindSnapshot::AddThreadRes,
                restore_policy: ThreadRestorePolicySnapshot::ProtocolV1,
                restore_epoch: 0,
                blocked_syscall_active: false,
                blocked_syscall_instance_id: None,
                blocked_syscall_phase: BlockedSyscallPhaseSnapshot::Unknown,
                blocked_restore_action: BlockedSyscallRestoreActionSnapshot::None,
                blocked_timeout_ns: None,
                blocked_trigger_fd: None,
                blocked_trigger_state_bits: None,
                blocked_active_file_fd: None,
                blocked_trigger_kind: None,
                blocked_futex_word: None,
                blocked_listener_sequence_value: None,
                poll_watches: Vec::new(),
                pending_result: None,
                blocked_syscall_nr: None,
            },
            ShimEventToShadow::SyscallComplete(_) => ThreadRuntimeSnapshot {
                event_kind: ThreadEventKindSnapshot::SyscallComplete,
                restore_policy: ThreadRestorePolicySnapshot::ProtocolV1,
                restore_epoch: 0,
                blocked_syscall_active: false,
                blocked_syscall_instance_id: None,
                blocked_syscall_phase: BlockedSyscallPhaseSnapshot::Unknown,
                blocked_restore_action: BlockedSyscallRestoreActionSnapshot::None,
                blocked_timeout_ns: None,
                blocked_trigger_fd: None,
                blocked_trigger_state_bits: None,
                blocked_active_file_fd: None,
                blocked_trigger_kind: None,
                blocked_futex_word: None,
                blocked_listener_sequence_value: None,
                poll_watches: Vec::new(),
                pending_result: None,
                blocked_syscall_nr: None,
            },
        }
    }

    pub fn native_pid(&self) -> linux_api::posix_types::Pid {
        self.native_pid
    }

    pub fn native_tid(&self) -> linux_api::posix_types::Pid {
        self.native_tid
    }

    pub fn ipc_shmem_handle(&self) -> String {
        match self.ipc_shmem.as_ref() {
            IpcShmem::Owned(block) => block.serialize().to_string(),
            IpcShmem::Restored { handle, .. } => handle.clone(),
        }
    }

    pub fn current_event_bytes(&self) -> Vec<u8> {
        let event = *self.current_event.borrow();
        let size = std::mem::size_of_val(&event);
        let ptr = std::ptr::from_ref(&event).cast::<u8>();
        unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec()
    }

    pub fn current_syscall_args(
        &self,
    ) -> Option<shadow_shim_helper_rs::syscall_types::SyscallArgs> {
        match *self.current_event.borrow() {
            ShimEventToShadow::Syscall(syscall) => Some(syscall.syscall_args),
            _ => None,
        }
    }

    /// Make the specified syscall on the native thread.
    ///
    /// Panics if the native thread is dead or dies during the syscall,
    /// including if the syscall itself is SYS_exit or SYS_exit_group.
    pub fn native_syscall(&self, ctx: &ThreadContext, n: i64, args: &[SyscallReg]) -> SyscallReg {
        let mut syscall_args = SyscallArgs {
            number: n,
            args: [SyscallReg::from(0u64); 6],
        };
        syscall_args.args[..args.len()].copy_from_slice(args);
        match self.continue_plugin(
            ctx.host,
            &ShimEventToShim::Syscall(ShimEventSyscall { syscall_args }),
        ) {
            ShimEventToShadow::SyscallComplete(res) => res.retval,
            other => panic!("Unexpected response from plugin: {other:?}"),
        }
    }

    pub fn spawn(
        plugin_path: &CStr,
        argv: Vec<CString>,
        envv: Vec<CString>,
        strace_file: Option<&std::fs::File>,
        log_file: &std::fs::File,
        injected_preloads: &[PathBuf],
    ) -> Result<Self, Errno> {
        debug!(
            "spawning new mthread '{plugin_path:?}' with environment '{envv:?}', arguments '{argv:?}'"
        );

        let envv = inject_preloads(envv, injected_preloads);

        debug!("env after preload injection: {envv:?}");

        let ipc_shmem_block = Arc::new(shadow_shmem::allocator::shmalloc(IPCData::new()));
        let ipc_shmem_serialized = ipc_shmem_block.serialize();
        let ipc_shmem = Arc::new(IpcShmem::Owned(ipc_shmem_block.clone()));

        let child_pid = Self::spawn_native(
            plugin_path,
            argv,
            envv,
            strace_file,
            log_file,
            &ipc_shmem_serialized,
        )?;

        // In Linux, the PID is equal to the TID of its first thread.
        let native_pid = child_pid;
        let native_tid = child_pid;

        // Configure the child_pid_watcher to close the IPC channel when the child dies.
        {
            let worker = WORKER_SHARED.borrow();
            let watcher = worker.as_ref().unwrap().child_pid_watcher();

            watcher.register_pid(child_pid);
            let ipc = ipc_shmem.clone();
            watcher.register_callback(child_pid, move |_pid| {
                ipc.from_plugin().close_writer();
            })
        };

        trace!("waiting for start event from shim with native pid {native_pid:?}");
        // SAFETY: Each IPC channel has a single Shadow-side consumer.
        let start_req = unsafe {
            ipc_shmem
                .from_plugin()
                .receive_assuming_single_consumer()
                .unwrap()
        };
        match &start_req {
            ShimEventToShadow::StartReq(_) => {
                // Expected result; shim is ready to initialize.
            }
            ShimEventToShadow::ProcessDeath => {
                // The process died before initializing the shim.
                //
                // Reap the dead process and return an error.
                let status =
                    rustix::process::waitpid(Some(native_pid.into()), WaitOptions::empty())
                        .unwrap()
                        .unwrap();
                if status.exit_status() == Some(127) {
                    // posix_spawn(3):
                    // > If  the child  fails  in  any  of the
                    // > housekeeping steps described below, or fails to
                    // > execute the desired file, it exits with a status of
                    // > 127.
                    debug!("posix_spawn failed to exec the process");
                    // Assume that execve failed, and return a plausible reason
                    // why it might have done so.
                    // TODO: replace our usage of posix_spawn with a custom
                    // implementation that can return the execve failure code?
                    return Err(Errno::EPERM);
                }
                // TODO: handle more gracefully.
                // * The native stdout/stderr might have a clue as to
                // why the process died.  Consider logging a hint to
                // check it (currently in the corresponding shimlog), or
                // directly capture it and display it here.
                // https://github.com/shadow/shadow/issues/3142
                // * Consider logging a warning here and continuing on to handle
                // the managed process exit normally. e.g. when this happens
                // as part of an emulated `execve`, we might want to continue
                // the simulation.
                panic!("Child process died unexpectedly before initialization: {status:?}");
            }
            other => panic!("Unexpected result from shim: {other:?}"),
        };

        Ok(Self {
            ipc_shmem,
            is_running: Cell::new(true),
            return_code: Cell::new(None),
            current_event: RefCell::new(start_req),
            native_pid,
            native_tid,
            affinity: Cell::new(cshadow::AFFINITY_UNINIT),
            force_syscall_eintr_once: Cell::new(false),
            needs_post_restore_refresh: Cell::new(false),
            async_continue_pending: Cell::new(false),
        })
    }

    pub fn resume(
        &self,
        ctx: &ThreadContext,
        syscall_handler: &mut SyscallHandler,
    ) -> ResumeResult {
        debug_assert!(self.is_running());

        self.sync_affinity_with_worker();

        // Flush any pending writes, e.g. from a previous mthread that exited
        // without flushing.
        ctx.process.free_unsafe_borrows_flush().unwrap();

        loop {
            let mut current_event = self.current_event.borrow_mut();
            let last_event = *current_event;
            *current_event = match last_event {
                ShimEventToShadow::StartReq(start_req) => {
                    // Write the serialized thread shmem handle directly to shim
                    // memory.
                    ctx.process
                        .memory_borrow_mut()
                        .write(
                            start_req.thread_shmem_block_to_init,
                            &ctx.thread.shmem().serialize(),
                        )
                        .unwrap();

                    if !start_req.process_shmem_block_to_init.is_null() {
                        // Write the serialized process shmem handle directly to
                        // shim memory.
                        ctx.process
                            .memory_borrow_mut()
                            .write(
                                start_req.process_shmem_block_to_init,
                                &ctx.process.shmem().serialize(),
                            )
                            .unwrap();
                    }

                    if !start_req.initial_working_dir_to_init.is_null() {
                        // Write the working dir.
                        let mut mem = ctx.process.memory_borrow_mut();
                        let mut writer = mem.writer(ForeignArrayPtr::new(
                            start_req.initial_working_dir_to_init,
                            start_req.initial_working_dir_to_init_len,
                        ));
                        writer
                            .write_all(ctx.process.current_working_dir().to_bytes_with_nul())
                            .unwrap();
                        writer.flush().unwrap();
                    }

                    // send the message to the shim to call main().
                    trace!("sending start event code to shim");
                    self.continue_plugin(
                        ctx.host,
                        &ShimEventToShim::StartRes(ShimEventStartRes {
                            auxvec_random: ctx.host.random_mut().random(),
                        }),
                    )
                }
                ShimEventToShadow::ProcessDeath => {
                    // The native threads are all dead or zombies. Nothing to do but
                    // clean up.
                    self.cleanup_after_exit_initiated();
                    return ResumeResult::ExitedProcess;
                }
                ShimEventToShadow::Syscall(syscall) => {
                    let syscall_nr = u32::try_from(syscall.syscall_args.number).unwrap_or_default();
                    let is_poll_family = matches!(
                        syscall.syscall_args.number,
                        x if x == libc::SYS_pselect6
                            || x == libc::SYS_ppoll
                            || x == libc::SYS_poll
                            || x == libc::SYS_select
                    );
                    let has_restored_timeout = ctx
                        .thread
                        .syscall_condition()
                        .and_then(|cond| cond.timeout())
                        .is_some();
                    let has_restored_poll_trigger = ctx
                        .thread
                        .syscall_condition()
                        .is_some_and(|cond| {
                            cond.trigger_kind()
                                == crate::core::checkpoint::snapshot_types::BlockedTriggerKindSnapshot::LegacyDescriptor
                        });
                    let should_force_synthetic_completion = self
                        .force_syscall_eintr_once
                        .replace(false)
                        && !syscall_handler.has_pending_result()
                        && (!has_restored_timeout || (is_poll_family && has_restored_poll_trigger));
                    if should_force_synthetic_completion {
                        if ctx.host.matches_restore_thread_trace_host_phase() {
                            let sim_time_ns = crate::core::worker::Worker::current_time()
                                .map(|t| t.to_abs_simtime().as_nanos())
                                .unwrap_or_default();
                            log::info!(
                                "restore-thread-trace host={} sim_time_ns={} stage=managed_thread_synthetic_completion pid={} tid={} syscall_nr={}",
                                ctx.host.name(),
                                sim_time_ns,
                                self.native_pid.as_raw_nonzero().get(),
                                self.native_tid.as_raw_nonzero().get(),
                                syscall.syscall_args.number,
                            );
                        }
                        if syscall.syscall_args.number == libc::SYS_pselect6
                            || syscall.syscall_args.number == libc::SYS_select
                        {
                            let zero_set =
                                [0u8; std::mem::size_of::<linux_api::posix_types::kernel_fd_set>()];
                            for arg_idx in [1usize, 2, 3] {
                                let fdset_ptr: ForeignPtr<linux_api::posix_types::kernel_fd_set> =
                                    syscall.syscall_args.args[arg_idx].into();
                                if fdset_ptr.is_null() {
                                    continue;
                                }
                                let untyped_ptr: UntypedForeignPtr = fdset_ptr.cast::<()>();
                                let _ = unsafe {
                                    crate::host::process::export::process_writePtr(
                                        std::ptr::from_ref(ctx.process),
                                        untyped_ptr,
                                        zero_set.as_ptr().cast(),
                                        zero_set.len(),
                                    )
                                };
                            }
                        }
                        let synthetic_retval = match syscall.syscall_args.number {
                            x if x == libc::SYS_pselect6
                                || x == libc::SYS_ppoll
                                || x == libc::SYS_nanosleep
                                || x == libc::SYS_clock_nanosleep =>
                            {
                                SyscallReg::from(0i64)
                            }
                            _ => SyscallReg::from(Errno::EINTR.to_negated_i64()),
                        };
                        log::debug!(
                            "restore de-stale: forcing synthetic completion pid={:?} tid={:?} syscall_nr={} retval={}",
                            self.native_pid,
                            self.native_tid,
                            syscall.syscall_args.number,
                            <i64 as From<SyscallReg>>::from(synthetic_retval)
                        );
                        record_syscall_perf(syscall_nr, |perf| {
                            perf.synthetic_completions += 1;
                        });
                        syscall_handler.clear_blocked_syscall();
                        let event = ShimEventToShim::SyscallComplete(ShimEventSyscallComplete {
                            retval: synthetic_retval,
                            restartable: false,
                        });
                        match self.continue_plugin_after_syscall(ctx.host, &event, syscall_nr) {
                            ContinuePluginResult::Ready(event) => event,
                            ContinuePluginResult::AsyncPending => {
                                ctx.host.record_async_continuation(
                                    ctx.process.id(),
                                    ctx.thread.id(),
                                    Worker::current_time().unwrap(),
                                );
                                return ResumeResult::AsyncPending;
                            }
                        }
                    } else {
                        // Emulate the given syscall.
                        // `exit` is tricky since it only exits the *mthread*, and we don't have a way
                        // to be notified that the mthread has exited. We have to "fire and forget"
                        // the command to execute the syscall natively.
                        //
                        // TODO: We could use a tid futex in shared memory, as set by
                        // `set_tid_address`, to block here until the thread has
                        // actually exited.
                        if syscall.syscall_args.number == libc::SYS_exit {
                            let return_code = syscall.syscall_args.args[0].into();
                            debug!("Short-circuiting syscall exit({return_code})");
                            self.return_code.set(Some(return_code));
                            // Tell mthread to go ahead and make the exit syscall itself.
                            // We *don't* call `_managedthread_continuePlugin` here,
                            // since that'd release the ShimSharedMemHostLock, and we
                            // aren't going to get a message back to know when it'd be
                            // safe to take it again.
                            self.ipc_shmem
                                .to_plugin()
                                .send(ShimEventToShim::SyscallDoNative);
                            self.cleanup_after_exit_initiated();
                            return ResumeResult::ExitedThread(return_code);
                        }

                        let handler_stats = managed_thread_perf_stats();
                        let fd_perf = handler_stats.and_then(|_| {
                            syscall_first_fd(syscall_nr, &syscall.syscall_args)
                                .map(|fd| (fd, descriptor_kind_for_fd(ctx, fd)))
                        });
                        let handler_started = handler_stats.map(|_| Instant::now());
                        let scr: crate::host::syscall::types::SyscallReturn =
                            syscall_handler.syscall(ctx, &syscall.syscall_args).into();
                        if let (Some(stats), Some(started)) = (handler_stats, handler_started) {
                            let elapsed_ns = started.elapsed().as_nanos() as u64;
                            stats.syscall_handler_calls.fetch_add(1, Ordering::Relaxed);
                            stats
                                .syscall_handler_wall_ns
                                .fetch_add(elapsed_ns, Ordering::Relaxed);
                            record_syscall_perf_with_stats(stats, syscall_nr, |perf| {
                                perf.handler_calls += 1;
                                perf.handler_wall_ns += elapsed_ns;
                                match scr {
                                    SyscallReturn::Done(_) => perf.done_results += 1,
                                    SyscallReturn::Block(_) => perf.blocked_results += 1,
                                    SyscallReturn::Native => perf.native_results += 1,
                                }
                            });
                            if let Some(fd) = syscall_first_fd(syscall_nr, &syscall.syscall_args) {
                                record_syscall_fd_with_stats(stats, syscall_nr, fd);
                            }
                            if let Some((_, kind)) = fd_perf {
                                record_syscall_fd_kind_with_stats(stats, syscall_nr, kind);
                            }
                        }

                        if ctx.host.matches_restore_thread_trace_host_phase() {
                            let sim_time_ns = crate::core::worker::Worker::current_time()
                                .map(|t| t.to_abs_simtime().as_nanos())
                                .unwrap_or_default();
                            log::info!(
                                "restore-thread-trace host={} sim_time_ns={} stage=managed_thread_syscall_result pid={} tid={} syscall_nr={} result={:?}",
                                ctx.host.name(),
                                sim_time_ns,
                                self.native_pid.as_raw_nonzero().get(),
                                self.native_tid.as_raw_nonzero().get(),
                                syscall.syscall_args.number,
                                scr,
                            );
                        }

                        // remove the mthread's old syscall condition since it's no longer needed
                        ctx.thread.cleanup_syscall_condition();

                        assert!(self.is_running());

                        // Flush any writes that legacy C syscallhandlers may have
                        // made.
                        ctx.process.free_unsafe_borrows_flush().unwrap();

                        match scr {
                            SyscallReturn::Block(b) => {
                                return ResumeResult::Blocked(unsafe {
                                    SyscallCondition::consume_from_c(b.cond)
                                });
                            }
                            SyscallReturn::Done(d) => {
                                let event =
                                    ShimEventToShim::SyscallComplete(ShimEventSyscallComplete {
                                        retval: d.retval,
                                        restartable: d.restartable,
                                    });
                                match self
                                    .continue_plugin_after_syscall(ctx.host, &event, syscall_nr)
                                {
                                    ContinuePluginResult::Ready(event) => event,
                                    ContinuePluginResult::AsyncPending => {
                                        ctx.host.record_async_continuation(
                                            ctx.process.id(),
                                            ctx.thread.id(),
                                            Worker::current_time().unwrap(),
                                        );
                                        return ResumeResult::AsyncPending;
                                    }
                                }
                            }
                            SyscallReturn::Native => {
                                match self.continue_plugin_after_syscall(
                                    ctx.host,
                                    &ShimEventToShim::SyscallDoNative,
                                    syscall_nr,
                                ) {
                                    ContinuePluginResult::Ready(event) => event,
                                    ContinuePluginResult::AsyncPending => {
                                        ctx.host.record_async_continuation(
                                            ctx.process.id(),
                                            ctx.thread.id(),
                                            Worker::current_time().unwrap(),
                                        );
                                        return ResumeResult::AsyncPending;
                                    }
                                }
                            }
                        }
                    }
                }
                ShimEventToShadow::AddThreadRes(res) => {
                    // We get here in the child process after forking.

                    // Child should have gotten 0 back from its native clone syscall.
                    assert_eq!(res.clone_res, 0);

                    // Complete the virtualized clone syscall.
                    self.continue_plugin(
                        ctx.host,
                        &ShimEventToShim::SyscallComplete(ShimEventSyscallComplete {
                            retval: 0.into(),
                            restartable: false,
                        }),
                    )
                }
                e @ ShimEventToShadow::SyscallComplete(_) => panic!("Unexpected event: {e:?}"),
            };
            assert!(self.is_running());
        }
    }

    pub fn handle_process_exit(&self) {
        // TODO: Only do this once per process; maybe by moving into `Process`.
        WORKER_SHARED
            .borrow()
            .as_ref()
            .unwrap()
            .child_pid_watcher()
            .unregister_pid(self.native_pid());

        self.cleanup_after_exit_initiated();
    }

    pub fn return_code(&self) -> Option<i32> {
        self.return_code.get()
    }

    pub fn is_running(&self) -> bool {
        self.is_running.get()
    }

    /// Execute the specified `clone` syscall in `self`, and use create a new
    /// `ManagedThread` object to manage it. The new thread will be managed
    /// by Shadow, and suitable for use with `Thread::wrap_mthread`.
    ///
    /// If the `clone` syscall fails, the native error is returned.
    pub fn native_clone(
        &self,
        ctx: &ThreadContext,
        flags: CloneFlags,
        child_stack: ForeignPtr<()>,
        ptid: ForeignPtr<libc::pid_t>,
        ctid: ForeignPtr<libc::pid_t>,
        newtls: libc::c_ulong,
    ) -> Result<ManagedThread, linux_api::errno::Errno> {
        let child_ipc_shmem = Arc::new(shadow_shmem::allocator::shmalloc(IPCData::new()));

        // Send the IPC block for the new mthread to use.
        let clone_res: i64 = match self.continue_plugin(
            ctx.host,
            &ShimEventToShim::AddThreadReq(ShimEventAddThreadReq {
                ipc_block: child_ipc_shmem.serialize(),
                flags: flags.bits(),
                child_stack,
                ptid: ptid.cast::<()>(),
                ctid: ctid.cast::<()>(),
                newtls,
            }),
        ) {
            ShimEventToShadow::AddThreadRes(ShimEventAddThreadRes { clone_res }) => clone_res,
            r => panic!("Unexpected result: {r:?}"),
        };
        let clone_res: SyscallReg = syscall::raw_return_value_to_result(clone_res)?;
        let child_native_tid = Pid::from_raw(libc::pid_t::from(clone_res)).unwrap();
        trace!("native clone treated tid {child_native_tid:?}");

        trace!("waiting for start event from shim with native tid {child_native_tid:?}");
        // SAFETY: Each IPC channel has a single Shadow-side consumer.
        let start_req = unsafe {
            child_ipc_shmem
                .from_plugin()
                .receive_assuming_single_consumer()
                .unwrap()
        };
        match &start_req {
            ShimEventToShadow::StartReq(_) => (),
            other => panic!("Unexpected result from shim: {other:?}"),
        };

        let native_pid = if flags.contains(CloneFlags::CLONE_THREAD) {
            self.native_pid
        } else {
            child_native_tid
        };

        if !flags.contains(CloneFlags::CLONE_THREAD) {
            // Child is a new process; register it.
            WORKER_SHARED
                .borrow()
                .as_ref()
                .unwrap()
                .child_pid_watcher()
                .register_pid(native_pid);
        }

        // Register the child thread's IPC block with the ChildPidWatcher.
        {
            let child_ipc_shmem = child_ipc_shmem.clone();
            WORKER_SHARED
                .borrow()
                .as_ref()
                .unwrap()
                .child_pid_watcher()
                .register_callback(native_pid, move |_pid| {
                    child_ipc_shmem.from_plugin().close_writer();
                })
        };

        Ok(Self {
            ipc_shmem: Arc::new(IpcShmem::Owned(child_ipc_shmem)),
            is_running: Cell::new(true),
            return_code: Cell::new(None),
            current_event: RefCell::new(start_req),
            native_pid,
            native_tid: child_native_tid,
            // TODO: can we assume it's inherited from the current thread affinity?
            affinity: Cell::new(cshadow::AFFINITY_UNINIT),
            force_syscall_eintr_once: Cell::new(false),
            needs_post_restore_refresh: Cell::new(false),
            async_continue_pending: Cell::new(false),
        })
    }

    #[must_use]
    fn continue_plugin_after_syscall(
        &self,
        host: &Host,
        event: &ShimEventToShim,
        syscall_nr: u32,
    ) -> ContinuePluginResult {
        if tdt_async_continue_enabled()
            && !self.needs_post_restore_refresh.get()
            && matches!(event, ShimEventToShim::SyscallComplete(_))
            && tdt_async_continue_syscall_allowed(syscall_nr)
        {
            self.begin_async_continue(host, event);
            return ContinuePluginResult::AsyncPending;
        }

        let stats = managed_thread_perf_stats();
        let started = stats.map(|_| Instant::now());
        let next_event = self.continue_plugin(host, event);
        if let (Some(stats), Some(started)) = (stats, started) {
            let elapsed_ns = started.elapsed().as_nanos() as u64;
            stats.syscall_continue_calls.fetch_add(1, Ordering::Relaxed);
            stats
                .syscall_continue_wall_ns
                .fetch_add(elapsed_ns, Ordering::Relaxed);
            record_syscall_perf_with_stats(stats, syscall_nr, |perf| {
                perf.continue_calls += 1;
                perf.continue_wall_ns += elapsed_ns;
            });
        }
        ContinuePluginResult::Ready(next_event)
    }

    fn begin_async_continue(&self, host: &Host, event: &ShimEventToShim) {
        assert!(!self.async_continue_pending.replace(true));

        let max_runahead_time = Worker::max_event_runahead_time(host);
        let sim_time = Worker::current_time().unwrap();
        log::info!(
            "tdt-async-continue begin host={} sim_time_ns={} native_pid={} native_tid={} event={:?}",
            host.name(),
            sim_time.to_abs_simtime().as_nanos(),
            self.native_pid.as_raw_nonzero().get(),
            self.native_tid.as_raw_nonzero().get(),
            event,
        );
        host.set_shim_clock_state(sim_time, max_runahead_time);

        // Release the host shmem lock while the native thread runs. The scheduler
        // must drain this pending continuation before checkpoint/pause.
        host.unlock_shmem();
        self.ipc_shmem.to_plugin().send(*event);
    }

    pub fn complete_async_continue(&self, host: &Host) {
        assert!(self.async_continue_pending.replace(false));

        let receive_started = managed_thread_perf_stats().map(|_| Instant::now());
        // SAFETY: Each IPC channel has a single Shadow-side consumer, and the
        // scheduler only drains one pending continuation per managed thread.
        let event = match unsafe {
            self.ipc_shmem
                .from_plugin()
                .receive_assuming_single_consumer()
        } {
            Ok(e) => e,
            Err(SelfContainedChannelError::WriterIsClosed) => ShimEventToShadow::ProcessDeath,
        };
        let receive_wall_ns = receive_started
            .map(|started| started.elapsed().as_nanos() as u64)
            .unwrap_or_default();

        host.lock_shmem();

        let shim_time = host.shim_shmem().sim_time.load(atomic::Ordering::Relaxed);
        Worker::set_current_time(shim_time);
        log::info!(
            "tdt-async-continue complete host={} sim_time_ns={} native_pid={} native_tid={} event={:?}",
            host.name(),
            shim_time.to_abs_simtime().as_nanos(),
            self.native_pid.as_raw_nonzero().get(),
            self.native_tid.as_raw_nonzero().get(),
            event,
        );
        *self.current_event.borrow_mut() = event;

        if let Some(stats) = managed_thread_perf_stats() {
            stats.continue_plugin_calls.fetch_add(1, Ordering::Relaxed);
            stats
                .continue_plugin_receive_wall_ns
                .fetch_add(receive_wall_ns, Ordering::Relaxed);
            tdt_perf_add_worker_body_continue_receive_wall_ns(receive_wall_ns);
        }
    }

    #[must_use]
    fn continue_plugin(&self, host: &Host, event: &ShimEventToShim) -> ShimEventToShadow {
        let stats = managed_thread_perf_stats();
        let perf_started = stats.map(|_| Instant::now());
        let sent_kind = stats.map(|_| shim_event_to_shim_kind(event));
        let prepare_started = stats.map(|_| Instant::now());
        // Update shared state before transferring control.
        let max_runahead_time = Worker::max_event_runahead_time(host);
        let sim_time = Worker::current_time().unwrap();
        host.set_shim_clock_state(sim_time, max_runahead_time);

        // Release lock so that plugin can take it. Reacquired in `wait_for_next_event`.
        host.unlock_shmem();
        let prepare_wall_ns = prepare_started
            .map(|started| started.elapsed().as_nanos() as u64)
            .unwrap_or_default();

        let supports_post_restore_refresh = matches!(
            event,
            ShimEventToShim::Syscall(_)
                | ShimEventToShim::SyscallComplete(_)
                | ShimEventToShim::SyscallDoNative
        );
        if supports_post_restore_refresh && self.needs_post_restore_refresh.replace(false) {
            self.ipc_shmem
                .to_plugin()
                .send(ShimEventToShim::StartRes(ShimEventStartRes {
                    auxvec_random: [0u8; 16],
                }));
            // SAFETY: Each IPC channel has a single Shadow-side consumer.
            let refresh_ack = match unsafe {
                self.ipc_shmem
                    .from_plugin()
                    .receive_assuming_single_consumer()
            } {
                Ok(e) => e,
                Err(SelfContainedChannelError::WriterIsClosed) => ShimEventToShadow::ProcessDeath,
            };
            match refresh_ack {
                ShimEventToShadow::SyscallComplete(ShimEventSyscallComplete {
                    retval,
                    restartable: false,
                }) if i64::from(retval) == 0 => {}
                other => panic!("Unexpected post-restore refresh ack: {other:?}"),
            }
        }
        let send_started = stats.map(|_| Instant::now());
        self.ipc_shmem.to_plugin().send(*event);
        let send_wall_ns = send_started
            .map(|started| started.elapsed().as_nanos() as u64)
            .unwrap_or_default();

        let receive_started = stats.map(|_| Instant::now());
        // SAFETY: Each IPC channel has a single Shadow-side consumer.
        let event = match unsafe {
            self.ipc_shmem
                .from_plugin()
                .receive_assuming_single_consumer()
        } {
            Ok(e) => e,
            Err(SelfContainedChannelError::WriterIsClosed) => ShimEventToShadow::ProcessDeath,
        };
        let receive_wall_ns = receive_started
            .map(|started| started.elapsed().as_nanos() as u64)
            .unwrap_or_default();

        // Reacquire the shared memory lock, now that the shim has yielded control
        // back to us.
        let lock_started = stats.map(|_| Instant::now());
        host.lock_shmem();
        let lock_wall_ns = lock_started
            .map(|started| started.elapsed().as_nanos() as u64)
            .unwrap_or_default();

        // Update time, which may have been incremented in the shim.
        let time_update_started = stats.map(|_| Instant::now());
        let shim_time = host.shim_shmem().sim_time.load(atomic::Ordering::Relaxed);
        if log_enabled!(Level::Trace) {
            let worker_time = Worker::current_time().unwrap();
            if shim_time != worker_time {
                trace!(
                    "Updating time from {worker_time:?} to {shim_time:?} (+{:?})",
                    shim_time - worker_time
                );
            }
        }
        Worker::set_current_time(shim_time);
        let time_update_wall_ns = time_update_started
            .map(|started| started.elapsed().as_nanos() as u64)
            .unwrap_or_default();

        if let (Some(stats), Some(started)) = (stats, perf_started) {
            let continue_wall_ns = started.elapsed().as_nanos() as u64;
            stats.continue_plugin_calls.fetch_add(1, Ordering::Relaxed);
            stats
                .continue_plugin_wall_ns
                .fetch_add(continue_wall_ns, Ordering::Relaxed);
            stats
                .continue_plugin_receive_wall_ns
                .fetch_add(receive_wall_ns, Ordering::Relaxed);
            tdt_perf_add_worker_body_continue_receive_wall_ns(receive_wall_ns);
            stats
                .continue_plugin_lock_wall_ns
                .fetch_add(lock_wall_ns, Ordering::Relaxed);
            stats
                .continue_plugin_prepare_wall_ns
                .fetch_add(prepare_wall_ns, Ordering::Relaxed);
            stats
                .continue_plugin_send_wall_ns
                .fetch_add(send_wall_ns, Ordering::Relaxed);
            stats
                .continue_plugin_time_update_wall_ns
                .fetch_add(time_update_wall_ns, Ordering::Relaxed);
            if let Some(sent_kind) = sent_kind {
                record_continue_exchange(
                    stats,
                    sent_kind,
                    shim_event_to_shadow_kind(&event),
                    continue_wall_ns,
                    receive_wall_ns,
                );
            }
        }

        event
    }

    /// To be called after we expect the native thread to have exited, or to
    /// exit imminently.
    fn cleanup_after_exit_initiated(&self) {
        if !self.is_running.get() {
            return;
        }
        self.wait_for_native_exit();
        trace!("child {:?} exited", self.native_tid());
        self.is_running.set(false);
    }

    /// Wait until the managed thread is no longer running.
    fn wait_for_native_exit(&self) {
        let native_pid = self.native_pid();
        let native_tid = self.native_tid();

        // We use `tgkill` and `/proc/x/stat` to detect whether the thread is still running,
        // looping until it doesn't.
        //
        // Alternatively we could use `set_tid_address` or `set_robust_list` to
        // be notified on a futex. Those are a bit underdocumented and fragile,
        // though. In practice this shouldn't have to loop significantly.
        trace!("Waiting for native thread {native_pid:?}.{native_tid:?} to exit");
        loop {
            if self.ipc_shmem.from_plugin().writer_is_closed() {
                // This indicates that the whole process has stopped executing;
                // no need to poll the individual thread.
                break;
            }
            match tgkill(native_pid, native_tid, None) {
                Err(Errno::ESRCH) => {
                    trace!("Thread is done exiting; proceeding with cleanup");
                    break;
                }
                Err(e) => {
                    error!("Unexpected tgkill error: {e:?}");
                    break;
                }
                Ok(()) if native_pid == native_tid => {
                    // Thread leader could be in a zombie state waiting for
                    // the other threads to exit.
                    let filename = format!("/proc/{}/stat", native_pid.as_raw_nonzero().get());
                    let stat = match std::fs::read_to_string(filename) {
                        Err(e) => {
                            assert!(e.kind() == std::io::ErrorKind::NotFound);
                            trace!("tgl {native_pid:?} is fully dead");
                            break;
                        }
                        Ok(s) => s,
                    };
                    if stat.contains(") Z") {
                        trace!("tgl {native_pid:?} is a zombie");
                        break;
                    }
                    // Still alive and in a non-zombie state; continue
                }
                Ok(()) => {
                    // Thread is still alive; continue.
                }
            };
            std::thread::yield_now();
        }
    }

    fn sync_affinity_with_worker(&self) {
        let current_affinity = scheduler::core_affinity()
            .map(|x| i32::try_from(x).unwrap())
            .unwrap_or(cshadow::AFFINITY_UNINIT);
        self.affinity.set(unsafe {
            cshadow::affinity_setProcessAffinity(
                self.native_tid().as_raw_nonzero().get(),
                current_affinity,
                self.affinity.get(),
            )
        });
    }

    fn spawn_native(
        plugin_path: &CStr,
        argv: Vec<CString>,
        envv: Vec<CString>,
        strace_file: Option<&std::fs::File>,
        shimlog_file: &std::fs::File,
        shmem_block: &ShMemBlockSerialized,
    ) -> Result<Pid, Errno> {
        // Preemptively check for likely reasons that execve might fail.
        // In particular we want to ensure that we  don't launch a statically
        // linked executable, since we'd then deadlock the whole simulation
        // waiting for the plugin to initialize.
        //
        // This is also helpful since we can't retrieve specific `execve` errors
        // through `posix_spawn`.
        fn map_verify_err(e: VerifyPluginPathError) -> Errno {
            match e {
                // execve(2): ENOENT The file pathname [...] does not exist.
                VerifyPluginPathError::NotFound => Errno::ENOENT,
                // execve(2): EACCES The file or a script interpreter is not a regular file.
                VerifyPluginPathError::NotFile => Errno::EACCES,
                // execve(2): EACCES Execute permission is denied for the file or a script or ELF interpreter.
                VerifyPluginPathError::NotExecutable => Errno::EACCES,
                // execve(2): ENOEXEC An executable is not in a recognized
                // format, is for the wrong architecture, or has some other
                // format error that means it cannot be executed.
                VerifyPluginPathError::UnknownFileType => Errno::ENOEXEC,
                VerifyPluginPathError::NotDynamicallyLinkedElf => Errno::ENOEXEC,
                VerifyPluginPathError::IncompatibleInterpreter(e) => map_verify_err(*e),
                // execve(2): EACCES Search permission is denied on a component
                // of the path prefix of pathname or the name of a script
                // interpreter.
                VerifyPluginPathError::PathPermissionDenied => Errno::EACCES,
                VerifyPluginPathError::UnhandledIoError(_) => {
                    // Arbitrary error that should be handled by callers.
                    Errno::ENOEXEC
                }
            }
        }
        verify_plugin_path(std::ffi::OsStr::from_bytes(plugin_path.to_bytes()))
            .map_err(map_verify_err)?;

        // posix_spawn is documented as taking pointers to *mutable* char for argv and
        // envv. It *probably* doesn't actually mutate them, but we
        // conservatively give it what it asks for. We have to "reconstitute"
        // the CString's after the fork + exec to deallocate them.
        let argv_ptrs: Vec<*mut i8> = argv
            .into_iter()
            .map(CString::into_raw)
            // the last element of argv must be NULL
            .chain(std::iter::once(std::ptr::null_mut()))
            .collect();
        let envv_ptrs: Vec<*mut i8> = envv
            .into_iter()
            .map(CString::into_raw)
            // the last element of argv must be NULL
            .chain(std::iter::once(std::ptr::null_mut()))
            .collect();

        let mut file_actions: libc::posix_spawn_file_actions_t = shadow_pod::zeroed();
        Errno::result_from_libc_errnum(unsafe {
            libc::posix_spawn_file_actions_init(&mut file_actions)
        })
        .unwrap();

        // Set up stdin
        let (stdin_reader, stdin_writer) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
        Errno::result_from_libc_errnum(unsafe {
            libc::posix_spawn_file_actions_adddup2(
                &mut file_actions,
                stdin_reader.as_raw_fd(),
                libc::STDIN_FILENO,
            )
        })
        .unwrap();

        // Dup straceFd; the dup'd descriptor won't have O_CLOEXEC set.
        //
        // Since dup2 is a no-op when the new and old file descriptors are equal, we have
        // to arrange to call dup2 twice - first to a temporary descriptor, and then back
        // to the original descriptor number.
        //
        // Here we use STDOUT_FILENO as the temporary descriptor, since we later
        // replace that below.
        //
        // Once we drop support for platforms with glibc older than 2.29, we *could*
        // consider taking advantage of a new feature that would let us just use a
        // single `posix_spawn_file_actions_adddup2` call with equal descriptors.
        // OTOH it's a non-standard extension, and I think ultimately uses the same
        // number of syscalls, so it might be better to continue using this slightly
        // more awkward method anyway.
        // https://github.com/bminor/glibc/commit/805334b26c7e6e83557234f2008497c72176a6cd
        // https://austingroupbugs.net/view.php?id=411
        if let Some(strace_file) = strace_file {
            Errno::result_from_libc_errnum(unsafe {
                libc::posix_spawn_file_actions_adddup2(
                    &mut file_actions,
                    strace_file.as_raw_fd(),
                    libc::STDOUT_FILENO,
                )
            })
            .unwrap();
            Errno::result_from_libc_errnum(unsafe {
                libc::posix_spawn_file_actions_adddup2(
                    &mut file_actions,
                    libc::STDOUT_FILENO,
                    strace_file.as_raw_fd(),
                )
            })
            .unwrap();
        }

        // set stdout/stderr as the shim log. This also clears the FD_CLOEXEC flag.
        Errno::result_from_libc_errnum(unsafe {
            libc::posix_spawn_file_actions_adddup2(
                &mut file_actions,
                shimlog_file.as_raw_fd(),
                libc::STDOUT_FILENO,
            )
        })
        .unwrap();
        Errno::result_from_libc_errnum(unsafe {
            libc::posix_spawn_file_actions_adddup2(
                &mut file_actions,
                shimlog_file.as_raw_fd(),
                libc::STDERR_FILENO,
            )
        })
        .unwrap();

        let mut spawn_attr: libc::posix_spawnattr_t = shadow_pod::zeroed();
        Errno::result_from_libc_errnum(unsafe { libc::posix_spawnattr_init(&mut spawn_attr) })
            .unwrap();

        // In versions of glibc before 2.24, we need this to tell posix_spawn
        // to use vfork instead of fork. In later versions it's a no-op.
        Errno::result_from_libc_errnum(unsafe {
            libc::posix_spawnattr_setflags(
                &mut spawn_attr,
                libc::POSIX_SPAWN_USEVFORK.try_into().unwrap(),
            )
        })
        .unwrap();

        let child_pid_res = {
            let mut child_pid = -1;
            Errno::result_from_libc_errnum(unsafe {
                libc::posix_spawn(
                    &mut child_pid,
                    plugin_path.as_ptr(),
                    &file_actions,
                    &spawn_attr,
                    argv_ptrs.as_ptr(),
                    envv_ptrs.as_ptr(),
                )
            })
            .map(|_| Pid::from_raw(child_pid).unwrap_or_else(|| panic!("Invalid pid: {child_pid}")))
        };

        // Write the serialized shmem descriptor to the stdin pipe. The pipe
        // buffer should be large enough that we can write it all without having
        // to wait for data to be read.
        if let Ok(child_pid) = child_pid_res {
            // we avoid using the rustix write wrapper here, since we can't guarantee
            // that all bytes of the serialized shmem block are initd, and hence
            // can't safely construct the &[u8] that it wants.
            let serialized_bytes = shadow_pod::as_u8_slice(shmem_block);
            let write_res = Errno::result_from_libc_errno(-1, unsafe {
                libc::write(
                    stdin_writer.as_raw_fd(),
                    serialized_bytes.as_ptr().cast(),
                    serialized_bytes.len(),
                )
            });
            let _ = (|| -> std::io::Result<()> {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open("/tmp/shadow-spawn-ipc.log")?;
                writeln!(
                    f,
                    "shadow-spawn-ipc-attempt pid={:?} stdin_writer_fd={} expected={} result={:?}",
                    child_pid,
                    stdin_writer.as_raw_fd(),
                    serialized_bytes.len(),
                    write_res
                )?;
                Ok(())
            })();
            let written = write_res.unwrap();
            let _ = (|| -> std::io::Result<()> {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open("/tmp/shadow-spawn-ipc.log")?;
                writeln!(
                    f,
                    "shadow-spawn-ipc pid={:?} stdin_writer_fd={} wrote={} expected={}",
                    child_pid,
                    stdin_writer.as_raw_fd(),
                    written,
                    serialized_bytes.len()
                )?;
                Ok(())
            })();
            // TODO: loop if needed. Shouldn't be in practice, though.
            assert_eq!(written, isize::try_from(serialized_bytes.len()).unwrap());
        }

        Errno::result_from_libc_errnum(unsafe {
            libc::posix_spawn_file_actions_destroy(&mut file_actions)
        })
        .unwrap();
        Errno::result_from_libc_errnum(unsafe { libc::posix_spawnattr_destroy(&mut spawn_attr) })
            .unwrap();

        // Drop the cloned argv and env.
        drop(
            argv_ptrs
                .into_iter()
                .filter(|p| !p.is_null())
                .map(|p| unsafe { CString::from_raw(p) }),
        );
        drop(
            envv_ptrs
                .into_iter()
                .filter(|p| !p.is_null())
                .map(|p| unsafe { CString::from_raw(p) }),
        );

        debug!(
            "starting process {}, result: {child_pid_res:?}",
            plugin_path.to_str().unwrap()
        );

        child_pid_res
    }

    /// `ManagedThread` panics if dropped while the underlying process is still running,
    /// since otherwise that process could continue writing to shared memory regions
    /// that shadow reallocates.
    ///
    /// This method kills the process that `self` belongs to (not just the
    /// thread!) and then drops `self`.
    pub fn kill_and_drop(self) {
        if let Err(err) =
            rustix::process::kill_process(self.native_pid().into(), rustix::process::Signal::Kill)
        {
            log::warn!(
                "Couldn't kill managed process {:?}. kill: {:?}",
                self.native_pid(),
                err
            );
        }
        self.handle_process_exit();
    }

    /// Reconstruct a `ManagedThread` for a process that was CRIU-restored.
    ///
    /// After CRIU restore, the process is alive but the original `ManagedThread`
    /// object was lost. This constructor rebuilds the Shadow-side state:
    ///
    /// - The IPC shared memory block is reattached from a serialized handle.
    /// - The most recent shim event is restored byte-for-byte so that the
    ///   Shadow/shim protocol resumes at the same point as checkpoint time.
    pub fn from_checkpoint(
        native_pid: Pid,
        native_tid: Pid,
        ipc_shmem_handle: &str,
        current_event_bytes: &[u8],
        force_syscall_eintr_once: bool,
    ) -> Self {
        let ipc_shmem_serialized = ShMemBlockSerialized::from_str(ipc_shmem_handle).unwrap();
        let ipc_shmem = Arc::new(IpcShmem::Restored {
            block: unsafe { shdeserialize::<IPCData>(&ipc_shmem_serialized) },
            handle: ipc_shmem_handle.to_string(),
        });
        log::debug!(
            "Rebuilding ManagedThread from checkpoint: pid={:?} tid={:?}",
            native_pid,
            native_tid,
        );

        let restored_event = event_from_bytes(current_event_bytes);
        if !matches!(restored_event, ShimEventToShadow::ProcessDeath) {
            ipc_shmem.from_plugin().reopen_writer_after_restore();
        }
        log::debug!(
            "Rebuilt ManagedThread event kind at restore: pid={:?} tid={:?} event={:?}",
            native_pid,
            native_tid,
            restored_event
        );
        Self {
            ipc_shmem,
            is_running: Cell::new(true),
            return_code: Cell::new(None),
            current_event: RefCell::new(restored_event),
            native_pid,
            native_tid,
            affinity: Cell::new(cshadow::AFFINITY_UNINIT),
            force_syscall_eintr_once: Cell::new(force_syscall_eintr_once),
            needs_post_restore_refresh: Cell::new(true),
            async_continue_pending: Cell::new(false),
        }
    }
}

fn event_from_bytes(bytes: &[u8]) -> ShimEventToShadow {
    assert_eq!(bytes.len(), std::mem::size_of::<ShimEventToShadow>());
    unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<ShimEventToShadow>()) }
}

impl Drop for ManagedThread {
    fn drop(&mut self) {
        // Dropping while the thread is running is unsound because the running
        // thread still has access to shared memory regions that will be
        // deallocated, and potentially reallocated for another purpose. The
        // running thread accessing a deallocated or repurposed memory region
        // can cause numerous problems.
        assert!(!self.is_running());
    }
}
