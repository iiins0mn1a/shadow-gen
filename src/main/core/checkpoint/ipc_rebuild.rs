//! IPC channel rebuild helpers after checkpoint/restore.
//!
//! After CRIU restores managed processes, the following must happen:
//! 1. `/dev/shm` files have been restored (done by `shmem_backup`)
//! 2. `HostShmem.shadow_pid` must be updated to Shadow's current PID
//! 3. `child_pid_watcher` callbacks must be re-registered for new PIDs
//!
//! At window boundaries, the `SelfContainedChannel`s are in `Empty` state,
//! so no channel state reconstruction is needed — the restored shmem content
//! is already consistent.

use linux_api::posix_types::Pid;

use crate::utility::childpid_watcher::ChildPidWatcher;

/// Returns Shadow's own PID (for updating `HostShmem.shadow_pid` after restore).
pub fn current_shadow_pid() -> libc::pid_t {
    std::process::id() as libc::pid_t
}

/// Update `shadow_pid` in a `HostShmem` block.
///
/// # Safety
///
/// The `host_shmem_ptr` must point to a valid `HostShmem` in shared memory.
pub unsafe fn update_shadow_pid_in_host_shmem(
    host_shmem_ptr: *mut shadow_shim_helper_rs::shim_shmem::HostShmem,
) {
    let pid = current_shadow_pid();
    unsafe {
        (*host_shmem_ptr).shadow_pid = pid;
    }
    log::debug!("Updated HostShmem.shadow_pid to {}", pid);
}

/// Re-register a restored child process with the `ChildPidWatcher`.
///
/// First registers the PID, then registers a callback for when it exits.
pub fn reregister_child_watcher(
    watcher: &ChildPidWatcher,
    new_pid: Pid,
    on_exit: impl FnOnce(Pid) + Send + 'static,
) {
    watcher.register_pid(new_pid);
    watcher.register_callback(new_pid, on_exit);
    log::debug!(
        "Registered child_pid_watcher for restored PID {:?}",
        new_pid
    );
}
