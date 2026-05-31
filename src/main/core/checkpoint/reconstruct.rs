//! Reconstruct live simulation objects from checkpoint snapshot types.

use super::snapshot_types::{LegacyTcpDeferredActionSnapshot, TaskDescriptor};
use crate::core::work::task::TaskRef;
use crate::host::descriptor::socket::inet::InetSocket;
use crate::host::descriptor::socket::Socket;
use crate::host::descriptor::File;
use crate::host::process::ProcessId;
use crate::host::thread::ThreadId;

fn deterministic_restore_enabled() -> bool {
    matches!(
        std::env::var("SHADOW_RESTORE_PROTOCOL_MODE")
            .ok()
            .as_deref(),
        Some("deterministic") | Some("deterministic_v2") | Some("strict_deterministic")
    )
}

fn for_each_live_descriptor(
    host: &crate::host::host::Host,
    mut f: impl FnMut(&crate::host::descriptor::Descriptor),
) {
    let processes = host.processes_borrow();
    for process_rc in processes.values() {
        let process = process_rc.borrow(host.root());
        let Some(thread_rc) = process.first_live_thread_borrow(host.root()) else {
            continue;
        };
        let thread = thread_rc.borrow(host.root());
        let table = thread.descriptor_table_borrow(host);
        for (_, descriptor) in table.iter() {
            f(descriptor);
        }
    }
}

fn legacy_tcp_by_canonical_handle(
    host: &crate::host::host::Host,
    canonical_handle: u64,
) -> Option<*mut crate::cshadow::TCP> {
    let translated_handle = host.translate_restored_canonical_handle(canonical_handle);
    let mut tcp = None;
    for_each_live_descriptor(host, |descriptor| {
        if tcp.is_some() {
            return;
        }
        let crate::host::descriptor::CompatFile::New(open_file) = descriptor.file() else {
            return;
        };
        let File::Socket(socket) = open_file.inner_file() else {
            return;
        };
        let Socket::Inet(InetSocket::LegacyTcp(socket)) = socket else {
            return;
        };
        let socket_ref = socket.borrow();
        let live_handle = socket_ref.canonical_handle() as u64;
        if live_handle == canonical_handle || translated_handle == Some(live_handle) {
            tcp = Some(socket_ref.as_legacy_tcp());
        }
    });
    tcp
}

/// Reconstruct a [`TaskRef`] closure from a serialized [`TaskDescriptor`].
///
/// Returns `None` for descriptors that cannot be meaningfully reconstructed
/// (e.g. `Opaque`, `StartApplication`).
pub fn reconstruct_task(desc: &TaskDescriptor) -> Option<TaskRef> {
    match desc {
        TaskDescriptor::ResumeProcess {
            process_id,
            thread_id,
        } => {
            let pid = ProcessId::try_from(*process_id).unwrap();
            let tid = ThreadId::from(ProcessId::try_from(*thread_id).unwrap());
            Some(TaskRef::new_with_result_and_descriptor(
                move |host| host.resume(pid, tid),
                desc.clone(),
            ))
        }
        TaskDescriptor::StartApplication {
            plugin_name,
            plugin_path,
            argv,
            envv,
            pause_for_debugging,
            shutdown_signal,
            shutdown_time_ns,
            expected_final_state,
        } => {
            let descriptor = desc.clone();
            let plugin_name = plugin_name.clone();
            let plugin_path = plugin_path.clone();
            let argv = argv.clone();
            let envv = envv.clone();
            let pause_for_debugging = *pause_for_debugging;
            let shutdown_signal = *shutdown_signal;
            let shutdown_time_ns = *shutdown_time_ns;
            let expected_final_state = *expected_final_state;
            Some(TaskRef::new_with_descriptor(
                move |host| {
                    use std::ffi::CString;

                    let plugin_name_c = CString::new(plugin_name.clone()).unwrap();
                    let plugin_path_c = CString::new(plugin_path.clone()).unwrap();
                    let argv_c = argv
                        .iter()
                        .map(|arg| CString::new(arg.as_str()).unwrap())
                        .collect();
                    let envv_c = envv
                        .iter()
                        .map(|env| CString::new(env.as_str()).unwrap())
                        .collect();

                    let process = crate::host::process::Process::spawn(
                        host,
                        plugin_name_c,
                        &plugin_path_c,
                        argv_c,
                        envv_c,
                        pause_for_debugging,
                        host.params.strace_logging_options,
                        expected_final_state,
                    )
                    .unwrap_or_else(|e| {
                        panic!("Failed to restore-start application {plugin_path:?}: {e:?}")
                    });
                    let (process_id, thread_id) = {
                        let process = process.borrow(host.root());
                        (process.id(), process.thread_group_leader_id())
                    };
                    host.processes_borrow_mut().insert(process_id, process);

                    if let Some(shutdown_time_ns) = shutdown_time_ns {
                        let task = TaskRef::new_with_descriptor(
                            move |host| {
                                use linux_api::signal::{siginfo_t, Signal};
                                let Some(process) = host.process_borrow(process_id) else {
                                    return;
                                };
                                let process = process.borrow(host.root());
                                let siginfo = siginfo_t::new_for_kill(
                                    Signal::try_from(shutdown_signal).unwrap(),
                                    1,
                                    0,
                                );
                                process.signal(host, None, &siginfo);
                            },
                            TaskDescriptor::ShutdownProcess {
                                process_id: u32::from(process_id),
                                signal: shutdown_signal,
                            },
                        );
                        host.schedule_task_at_emulated_time(
                            task,
                            shadow_shim_helper_rs::emulated_time::EmulatedTime::SIMULATION_START
                                + shadow_shim_helper_rs::simulation_time::SimulationTime::from_nanos(
                                    shutdown_time_ns,
                                ),
                        );
                    }

                    host.resume(process_id, thread_id);
                },
                descriptor,
            ))
        }
        TaskDescriptor::ShutdownProcess { process_id, signal } => {
            let pid = ProcessId::try_from(*process_id).unwrap();
            let sig = *signal;
            Some(TaskRef::new_with_descriptor(
                move |host| {
                    use linux_api::signal::{siginfo_t, Signal};
                    let Some(process) = host.process_borrow(pid) else {
                        log::debug!(
                            "Can't send shutdown signal to process {pid:?}; it no longer exists"
                        );
                        return;
                    };
                    let process = process.borrow(host.root());
                    let siginfo = siginfo_t::new_for_kill(Signal::try_from(sig).unwrap(), 1, 0);
                    process.signal(host, None, &siginfo);
                },
                desc.clone(),
            ))
        }
        TaskDescriptor::RelayForward { relay_id } => Some(TaskRef::new_with_descriptor(
            {
                let relay_id = *relay_id;
                move |host| {
                    let Some(relay) = host.relay_by_descriptor_id(relay_id) else {
                        log::warn!(
                            "Relay id {} disappeared before restore task execution",
                            relay_id
                        );
                        return;
                    };
                    relay.run_scheduled_forward(host);
                }
            },
            desc.clone(),
        )),
        TaskDescriptor::SyscallConditionWake {
            process_id,
            thread_id,
        } => {
            let pid = ProcessId::try_from(*process_id).unwrap();
            let tid = ThreadId::from(ProcessId::try_from(*thread_id).unwrap());
            Some(TaskRef::new_with_descriptor(
                move |host| {
                    host.resume(pid, tid);
                },
                desc.clone(),
            ))
        }
        TaskDescriptor::PreparePollTimeoutCompletion {
            process_id,
            thread_id,
            syscall_nr,
        } => {
            let pid = ProcessId::try_from(*process_id).unwrap();
            let tid = ThreadId::from(ProcessId::try_from(*thread_id).unwrap());
            let syscall_nr = *syscall_nr;
            Some(TaskRef::new_with_descriptor(
                move |host| {
                    let Some(process_rc) = host.process_borrow(pid) else {
                        return;
                    };
                    let process = process_rc.borrow(host.root());
                    let Some(thread_rc) = process.thread_borrow(tid) else {
                        return;
                    };
                    let thread = thread_rc.borrow(host.root());
                    thread
                        .syscallhandler_borrow_mut(host)
                        .prepare_restored_poll_timeout_completion(syscall_nr);
                },
                desc.clone(),
            ))
        }
        TaskDescriptor::RestoreBlockedSyscallCondition {
            process_id,
            thread_id,
        } => {
            let pid = ProcessId::try_from(*process_id).unwrap();
            let tid = ThreadId::from(ProcessId::try_from(*thread_id).unwrap());
            Some(TaskRef::new_with_descriptor(
                move |host| {
                    log::debug!(
                        "RestoreBlockedSyscallCondition task executed without restore context pid={pid:?} tid={tid:?}"
                    );
                    host.resume(pid, tid);
                },
                desc.clone(),
            ))
        }
        TaskDescriptor::LegacyTcpDeferredAction {
            canonical_handle,
            action,
        } => {
            let canonical_handle = *canonical_handle;
            let action = *action;
            Some(TaskRef::new_with_descriptor(
                move |host| {
                    let Some(tcp) = legacy_tcp_by_canonical_handle(host, canonical_handle) else {
                        log::warn!(
                            "Missing legacy TCP socket for deferred action {:?} canonical_handle={}",
                            action,
                            canonical_handle
                        );
                        return;
                    };
                    unsafe {
                        match action {
                            LegacyTcpDeferredActionSnapshot::CloseTimerExpired => {
                                crate::cshadow::tcp_runCloseTimerExpiredTask(tcp, host)
                            }
                            LegacyTcpDeferredActionSnapshot::RetransmitTimerExpired => {
                                crate::cshadow::tcp_runRetransmitTimerExpiredTask(tcp, host)
                            }
                            LegacyTcpDeferredActionSnapshot::SendAck => {
                                crate::cshadow::tcp_sendACKTask(tcp, host)
                            }
                            LegacyTcpDeferredActionSnapshot::SendWindowUpdate => {
                                crate::cshadow::tcp_sendWindowUpdateTask(tcp, host)
                            }
                        }
                    }
                },
                desc.clone(),
            ))
        }
        TaskDescriptor::TimerExpire {
            timer_id,
            expire_id,
        } => {
            if deterministic_restore_enabled() {
                log::debug!(
                    "Skipping TimerExpire(timer_id={}, expire_id={}) during deterministic restore; owner/runtime restore is authoritative",
                    timer_id,
                    expire_id
                );
                return None;
            }
            Some(TaskRef::new_with_descriptor(
                {
                    let timer_id = *timer_id;
                    let expire_id = *expire_id;
                    move |host| {
                        log::debug!(
                            "TimerExpire(timer_id={}, expire_id={}) replayed via compatibility wakeup",
                            timer_id,
                            expire_id
                        );
                        let to_resume: Vec<_> = host
                            .processes_borrow()
                            .iter()
                            .filter_map(|(pid, process_rc)| {
                                let process = process_rc.borrow(host.root());
                                process
                                    .is_running()
                                    .then_some((*pid, process.thread_group_leader_id()))
                            })
                            .collect();
                        for (pid, tid) in to_resume {
                            host.resume(pid, tid);
                        }
                    }
                },
                desc.clone(),
            ))
        }
        TaskDescriptor::ExecContinuation { process_id } => {
            let pid = ProcessId::try_from(*process_id).unwrap();
            Some(TaskRef::new_with_descriptor(
                move |host| {
                    log::warn!(
                        "ExecContinuation for process {:?} restored conservatively via resume",
                        pid
                    );
                    host.resume(pid, ThreadId::from(pid));
                },
                desc.clone(),
            ))
        }
        TaskDescriptor::Opaque { description } => {
            if deterministic_restore_enabled() {
                log::error!(
                    "Opaque task cannot be reconstructed in deterministic restore mode: {}",
                    description
                );
                return None;
            }
            let description = description.clone();
            Some(TaskRef::new_with_descriptor(
                move |host| {
                    log::warn!(
                        "Replayed opaque task via compatibility wakeup during restore: {}",
                        description
                    );
                    let to_resume: Vec<_> = host
                        .processes_borrow()
                        .iter()
                        .filter_map(|(pid, process_rc)| {
                            let process = process_rc.borrow(host.root());
                            process
                                .is_running()
                                .then_some((*pid, process.thread_group_leader_id()))
                        })
                        .collect();
                    for (pid, tid) in to_resume {
                        host.resume(pid, tid);
                    }
                },
                desc.clone(),
            ))
        }
    }
}
