//! Reconstruct live simulation objects from checkpoint snapshot types.

use super::snapshot_types::TaskDescriptor;
use crate::core::work::task::TaskRef;
use crate::host::process::ProcessId;
use crate::host::thread::ThreadId;

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
            Some(TaskRef::new_with_descriptor(
                move |host| {
                    host.resume(pid, tid);
                },
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
                                use linux_api::signal::{Signal, siginfo_t};
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
                    use linux_api::signal::{Signal, siginfo_t};
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
        TaskDescriptor::TimerExpire {
            timer_id,
            expire_id,
        } => Some(TaskRef::new_with_descriptor(
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
        )),
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
            let description = description.clone();
            Some(TaskRef::new_with_descriptor(
                move |_host| {
                    log::warn!(
                        "Replayed opaque task as no-op during restore: {}",
                        description
                    );
                },
                desc.clone(),
            ))
        }
    }
}
