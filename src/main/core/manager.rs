use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use atomic_refcell::AtomicRefCell;
use linux_api::prctl::ArchPrctlOp;
use log::{debug, warn};
use rand::seq::SliceRandom;
use rand_xoshiro::Xoshiro256PlusPlus;
use scheduler::thread_per_core::ThreadPerCoreSched;
use scheduler::thread_per_host::ThreadPerHostSched;
use scheduler::{HostIter, Scheduler};
use shadow_shim_helper_rs::HostId;
use shadow_shim_helper_rs::emulated_time::EmulatedTime;
use shadow_shim_helper_rs::option::FfiOption;
use shadow_shim_helper_rs::shim_shmem::{ManagerShmem, NativePreemptionConfig};
use shadow_shim_helper_rs::simulation_time::SimulationTime;
use shadow_shmem::allocator::ShMemBlock;

use crate::core::checkpoint::criu;
use crate::core::checkpoint::event_conversion::event_to_snapshot;
use crate::core::checkpoint::event_conversion::rebuild_event_queue;
use crate::core::checkpoint::shmem_backup;
use crate::core::checkpoint::snapshot_types::*;
use crate::core::checkpoint::store::{CheckpointStore, FilesystemStore};
use crate::core::configuration::{self, ConfigOptions, Flatten};
use crate::core::controller::{Controller, ShadowStatusBarState, SimController};
use crate::core::cpu;
use crate::core::resource_usage;
use crate::core::run_control::commands::{ControlDecision, SimulationRunResult};
use crate::core::run_control::controller::WindowBoundaryContext;
use crate::core::run_control::TimeController;
use crate::core::runahead::Runahead;
use crate::core::sim_config::{Bandwidth, HostInfo};
use crate::core::sim_stats;
use crate::core::work::task::TaskRef;
use crate::core::worker;
use crate::cshadow as c;
use crate::host::host::{Host, HostParameters};
use crate::host::process::Process;
use crate::network::dns::DnsBuilder;
use crate::network::graph::{IpAssignment, RoutingInfo};
use crate::utility;
use crate::utility::childpid_watcher::ChildPidWatcher;
use crate::utility::status_bar::Status;

pub struct Manager<'a> {
    manager_config: Option<ManagerConfig>,
    controller: &'a Controller<'a>,
    config: &'a ConfigOptions,
    time_controller: &'a dyn TimeController,

    raw_frequency: u64,
    native_tsc_frequency: u64,
    end_time: EmulatedTime,

    data_path: PathBuf,
    hosts_path: PathBuf,

    preload_paths: Arc<Vec<PathBuf>>,

    check_fd_usage: bool,
    check_mem_usage: bool,

    meminfo_file: std::fs::File,
    shmem: ShMemBlock<'static, ManagerShmem>,
    restore_checkpoint: Option<SimulationCheckpoint>,
}

impl<'a> Manager<'a> {
    pub fn new(
        manager_config: ManagerConfig,
        controller: &'a Controller<'a>,
        config: &'a ConfigOptions,
        end_time: EmulatedTime,
        time_controller: &'a dyn TimeController,
        restore_checkpoint: Option<SimulationCheckpoint>,
    ) -> anyhow::Result<Self> {
        // get the system's CPU frequency
        let raw_frequency = get_raw_cpu_frequency_hz().unwrap_or_else(|e| {
            let default_freq = 2_500_000_000; // 2.5 GHz
            log::info!("Failed to get raw CPU frequency, using {default_freq} Hz instead: {e}");
            default_freq
        });
        log::info!("Raw CPU frequency: {raw_frequency} Hz");

        let native_tsc_frequency = if let Some(f) = asm_util::tsc::Tsc::native_cycles_per_second() {
            f
        } else {
            warn!(
                "Couldn't find native TSC frequency. Emulated rdtsc may use a rate different than managed code expects"
            );
            raw_frequency
        };

        let mut preload_paths = Vec::new();

        // we always preload the injector lib to ensure that the shim is loaded into the managed
        // processes
        const PRELOAD_INJECTOR_LIB: &str = "libshadow_injector.so";
        preload_paths.push(
            get_required_preload_path(PRELOAD_INJECTOR_LIB).with_context(|| {
                format!("Failed to get path to preload library '{PRELOAD_INJECTOR_LIB}'")
            })?,
        );

        // preload libc lib if option is enabled
        const PRELOAD_LIBC_LIB: &str = "libshadow_libc.so";
        if config.experimental.use_preload_libc.unwrap() {
            let path = get_required_preload_path(PRELOAD_LIBC_LIB).with_context(|| {
                format!("Failed to get path to preload library '{PRELOAD_LIBC_LIB}'")
            })?;
            preload_paths.push(path);
        } else {
            log::info!("Preloading the libc library is disabled");
        };

        // preload openssl rng lib if option is enabled
        const PRELOAD_OPENSSL_RNG_LIB: &str = "libshadow_openssl_rng.so";
        if config.experimental.use_preload_openssl_rng.unwrap() {
            let path = get_required_preload_path(PRELOAD_OPENSSL_RNG_LIB).with_context(|| {
                format!("Failed to get path to preload library '{PRELOAD_OPENSSL_RNG_LIB}'")
            })?;
            preload_paths.push(path);
        } else {
            log::info!("Preloading the openssl rng library is disabled");
        };

        // preload openssl crypto lib if option is enabled
        const PRELOAD_OPENSSL_CRYPTO_LIB: &str = "libshadow_openssl_crypto.so";
        if config.experimental.use_preload_openssl_crypto.unwrap() {
            let path =
                get_required_preload_path(PRELOAD_OPENSSL_CRYPTO_LIB).with_context(|| {
                    format!("Failed to get path to preload library '{PRELOAD_OPENSSL_CRYPTO_LIB}'")
                })?;
            preload_paths.push(path);
        } else {
            log::info!("Preloading the openssl crypto library is disabled");
        };

        // use the working dir to generate absolute paths
        let cwd = std::env::current_dir()?;
        let template_path = config
            .general
            .template_directory
            .flatten_ref()
            .map(|x| cwd.clone().join(x));
        let data_path = cwd.join(config.general.data_directory.as_ref().unwrap());
        let hosts_path = data_path.join("hosts");

        if let Some(template_path) = template_path {
            log::debug!(
                "Copying template directory '{}' to '{}'",
                template_path.display(),
                data_path.display()
            );

            // copy the template directory to the data directory path
            utility::copy_dir_all(&template_path, &data_path).with_context(|| {
                format!(
                    "Failed to copy template directory '{}' to '{}'",
                    template_path.display(),
                    data_path.display()
                )
            })?;

            // create the hosts directory if it doesn't exist
            let result = std::fs::create_dir(&hosts_path);
            if let Err(e) = result
                && e.kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(e).context(format!(
                    "Failed to create hosts directory '{}'",
                    hosts_path.display()
                ));
            }
        } else {
            // create the data and hosts directories (tolerate already-existing
            // for restart / restore cycles)
            std::fs::create_dir_all(&data_path).with_context(|| {
                format!("Failed to create data directory '{}'", data_path.display())
            })?;
            std::fs::create_dir_all(&hosts_path).with_context(|| {
                format!(
                    "Failed to create hosts directory '{}'",
                    hosts_path.display(),
                )
            })?;
        }

        // save the processed config as yaml
        let config_out_filename = data_path.join("processed-config.yaml");
        let config_out_file = std::fs::File::create(&config_out_filename).with_context(|| {
            format!("Failed to create file '{}'", config_out_filename.display())
        })?;

        serde_yaml::to_writer(config_out_file, &config).with_context(|| {
            format!(
                "Failed to write processed config yaml to file '{}'",
                config_out_filename.display()
            )
        })?;

        let meminfo_file =
            std::fs::File::open("/proc/meminfo").context("Failed to open '/proc/meminfo'")?;

        // Determind whether we can and should emulate cpuid in the shim.
        let emulate_cpuid = {
            // SAFETY: we don't support running in esoteric environments where cpuid isn't available.
            let supports_rdrand = unsafe { asm_util::cpuid::supports_rdrand() };
            let supports_rdseed = unsafe { asm_util::cpuid::supports_rdseed() };
            if !(supports_rdrand || supports_rdseed) {
                // No need to emulate cpuid.
                debug!(
                    "No rdrand nor rdseed support. cpuid emulation is unnecessary, so skipping."
                );
                false
            } else {
                // CPU has `rdrand` and/or `rdseed`, which produce
                // non-deterministic results by design.  We want to trap and
                // emulate `cpuid` in the shim to mask this support so that
                // managed programs (hopefully) don't use it.

                // Test whether the current platform actually supports intercepting cpuid.
                // This is dependent on the CPU model and kernel version.
                let res = unsafe { linux_api::prctl::arch_prctl(ArchPrctlOp::ARCH_SET_CPUID, 0) };
                match res {
                    Ok(_) => {
                        // Re-enable cpuid for ourselves.
                        unsafe { linux_api::prctl::arch_prctl(ArchPrctlOp::ARCH_SET_CPUID, 1) }
                            .unwrap_or_else(|e| panic!("Couldn't re-enable cpuid: {e:?}"));
                        debug!(
                            "CPU supports rdrand and/or rdseed, and platform supports intercepting cpuid. Enabling cpuid emulation."
                        );
                        true
                    }
                    Err(e) => {
                        warn!(
                            "CPU appears to support rdrand and/or rdseed, but platform doesn't support emulating cpuid ({e:?}). This may break determinism."
                        );
                        false
                    }
                }
            }
        };

        let shmem = shadow_shmem::allocator::shmalloc(ManagerShmem {
            log_start_time_micros: unsafe { c::logger_get_global_start_time_micros() },
            native_preemption_config: if config.native_preemption_enabled() {
                FfiOption::Some(NativePreemptionConfig {
                    native_duration: config.native_preemption_native_interval()?,
                    sim_duration: config.native_preemption_sim_interval(),
                })
            } else {
                FfiOption::None
            },
            emulate_cpuid,
        });

        Ok(Self {
            manager_config: Some(manager_config),
            controller,
            config,
            time_controller,
            raw_frequency,
            native_tsc_frequency,
            end_time,
            data_path,
            hosts_path,
            preload_paths: Arc::new(preload_paths),
            check_fd_usage: true,
            check_mem_usage: true,
            meminfo_file,
            shmem,
            restore_checkpoint,
        })
    }

    pub fn run(
        mut self,
        status_logger_state: Option<&Arc<Status<ShadowStatusBarState>>>,
    ) -> anyhow::Result<SimulationRunResult> {
        let mut manager_config = self.manager_config.take().unwrap();

        let min_runahead_config: Option<Duration> = self
            .config
            .experimental
            .runahead
            .flatten()
            .map(|x| x.into());
        let min_runahead_config: Option<SimulationTime> =
            min_runahead_config.map(|x| x.try_into().unwrap());

        let bootstrap_end_time: Duration = self.config.general.bootstrap_end_time.unwrap().into();
        let bootstrap_end_time: SimulationTime = bootstrap_end_time.try_into().unwrap();
        let bootstrap_end_time = EmulatedTime::SIMULATION_START + bootstrap_end_time;

        let smallest_latency = SimulationTime::from_nanos(
            manager_config
                .routing_info
                .get_smallest_latency_ns()
                .unwrap(),
        );

        let parallelism: usize = match self.config.general.parallelism.unwrap() {
            0 => {
                let cores = cpu::count_physical_cores().try_into().unwrap();
                log::info!("The parallelism option was 0, so using parallelism={cores}");
                cores
            }
            x => x.try_into().unwrap(),
        };

        // Set up the global DNS before building the hosts
        let mut dns_builder = DnsBuilder::new();

        // Assign the host id only once to guarantee it stays associated with its host.
        let host_init: Vec<(&HostInfo, HostId)> = manager_config
            .hosts
            .iter()
            .enumerate()
            .map(|(i, info)| (info, HostId::from(u32::try_from(i).unwrap())))
            .collect();

        for (info, id) in &host_init {
            // Extract the host address.
            let std::net::IpAddr::V4(addr) = info.ip_addr.unwrap() else {
                unreachable!("IPv6 not supported");
            };

            // Register in the global DNS.
            dns_builder
                .register(*id, addr, info.name.clone())
                .with_context(|| {
                    format!(
                        "Failed to register a host with id='{:?}', addr='{}', and name='{}' in the DNS module",
                        *id, addr, info.name
                    )
                })?;
        }

        // Convert to a global read-only DNS struct.
        let dns = dns_builder.into_dns()?;

        // Now build the hosts using the assigned host ids.
        let mut hosts: Vec<_> = host_init
            .iter()
            .map(|(info, id)| {
                self.build_host(*id, info)
                    .with_context(|| format!("Failed to build host '{}'", info.name))
            })
            .collect::<anyhow::Result<_>>()?;

        // shuffle the list of hosts to make sure that they are randomly assigned by the scheduler
        hosts.shuffle(&mut manager_config.random);

        let use_cpu_pinning = self.config.experimental.use_cpu_pinning.unwrap();

        let cpu_iter =
            std::iter::from_fn(|| {
                Some(use_cpu_pinning.then(|| {
                    u32::try_from(unsafe { c::affinity_getGoodWorkerAffinity() }).unwrap()
                }))
            });

        // shadow is parallelized at the host level
        let parallelism = std::cmp::min(parallelism, hosts.len());

        let cpus: Vec<Option<u32>> = cpu_iter.take(parallelism).collect();
        if cpus[0].is_some() {
            log::debug!("Pinning to cpus: {cpus:?}");
            assert!(cpus.iter().all(|x| x.is_some()));
        } else {
            log::debug!("Not pinning to CPUs");
            assert!(cpus.iter().all(|x| x.is_none()));
        }
        assert_eq!(cpus.len(), parallelism);

        // set the simulation's global state
        worker::WORKER_SHARED
            .borrow_mut()
            .replace(worker::WorkerShared {
                ip_assignment: manager_config.ip_assignment,
                routing_info: manager_config.routing_info,
                host_bandwidths: manager_config.host_bandwidths,
                dns,
                num_plugin_errors: AtomicU32::new(0),
                status_logger_state: status_logger_state.map(Arc::clone),
                runahead: Runahead::new(
                    self.config.experimental.use_dynamic_runahead.unwrap(),
                    smallest_latency,
                    min_runahead_config,
                ),
                child_pid_watcher: ChildPidWatcher::new(),
                event_queues: hosts
                    .iter()
                    .map(|x| (x.id(), x.event_queue().clone()))
                    .collect(),
                bootstrap_end_time,
                sim_end_time: self.end_time,
            });

        let mut restart_request: Option<(Option<u64>, crate::core::run_control::commands::RestartSource)> = None;

        // scope used so that the scheduler is dropped before we log the global counters below
        {
            let mut scheduler = match self.config.experimental.scheduler.unwrap() {
                configuration::Scheduler::ThreadPerHost => {
                    std::thread_local! {
                        static SCHED_HOST_STORAGE: RefCell<Option<Box<Host>>> = const { RefCell::new(None) };
                    }
                    Scheduler::ThreadPerHost(ThreadPerHostSched::new(
                        &cpus,
                        &SCHED_HOST_STORAGE,
                        hosts,
                    ))
                }
                configuration::Scheduler::ThreadPerCore => {
                    Scheduler::ThreadPerCore(ThreadPerCoreSched::new(
                        &cpus,
                        hosts,
                        self.config.experimental.use_worker_spinning.unwrap(),
                    ))
                }
            };

            // initialize the thread-local Worker
            scheduler.scope(|s| {
                s.run(|thread_id| {
                    worker::Worker::new_for_this_thread(worker::WorkerThreadID(thread_id as u32))
                });
            });

            // Phase 2 restore: replay host checkpoints only after Worker TLS is ready.
            if let Some(sim_checkpoint) = self.restore_checkpoint.as_ref() {
                let checkpoints: Arc<HashMap<String, HostCheckpoint>> = Arc::new(
                    sim_checkpoint
                        .hosts
                        .iter()
                        .map(|h| (h.hostname.clone(), h.clone()))
                        .collect(),
                );
                let replay_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

                scheduler.scope(|s| {
                    let checkpoints = Arc::clone(&checkpoints);
                    let replay_err = Arc::clone(&replay_err);
                    s.run_with_hosts(move |_, hosts| {
                        for_each_host(hosts, |host| {
                            if replay_err.lock().unwrap().is_some() {
                                return;
                            }
                            let Some(checkpoint) = checkpoints.get(host.name()) else {
                                *replay_err.lock().unwrap() = Some(format!(
                                    "missing restore checkpoint for host '{}'",
                                    host.name()
                                ));
                                return;
                            };

                            host.lock_shmem();
                            let apply_res = apply_host_checkpoint(host, checkpoint);
                            host.unlock_shmem();

                            if let Err(e) = apply_res {
                                *replay_err.lock().unwrap() = Some(format!(
                                    "failed to replay host checkpoint for '{}': {e:#}",
                                    host.name()
                                ));
                            }
                        });
                    });
                });

                if let Some(err) = replay_err.lock().unwrap().take() {
                    anyhow::bail!("{err}");
                }
            }

            // the current simulation interval
            let mut window = self.restore_checkpoint.as_ref().map_or_else(
                || {
                    Some((
                        EmulatedTime::SIMULATION_START,
                        EmulatedTime::SIMULATION_START + SimulationTime::NANOSECOND,
                    ))
                },
                |checkpoint| {
                    Some((
                        EmulatedTime::SIMULATION_START
                            + SimulationTime::from_nanos(checkpoint.window_start_ns),
                        EmulatedTime::SIMULATION_START
                            + SimulationTime::from_nanos(checkpoint.window_end_ns),
                    ))
                },
            );

            let thread_next_event_times: Vec<AtomicRefCell<Option<EmulatedTime>>> =
                vec![AtomicRefCell::new(None); scheduler.parallelism()];

            let heartbeat_interval = self
                .config
                .general
                .heartbeat_interval
                .flatten()
                .map(|x| Duration::from(x).try_into().unwrap());

            let mut last_heartbeat = EmulatedTime::SIMULATION_START;
            let mut time_of_last_usage_check = std::time::Instant::now();

            // Notify the time controller that the simulation is starting.
            self.time_controller.on_simulation_start();

            // the scheduling loop
            while let Some((window_start, window_end)) = window {
                #[cfg(feature = "enable_perf_logging")]
                let active_hosts_in_window = {
                    let shared = worker::WORKER_SHARED.borrow();
                    let ws = shared.as_ref().unwrap();
                    ws.event_queues
                        .values()
                        .filter(|q| {
                            let q = q.lock().unwrap();
                            matches!(q.next_event_time(), Some(t) if t < window_end)
                        })
                        .count()
                };

                // update the status logger
                let display_time = std::cmp::min(window_start, window_end);
                worker::WORKER_SHARED
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .update_status_logger(|state| {
                        state.current = display_time;
                    });

                // run the events
                scheduler.scope(|s| {
                    s.run_with_data(
                        &thread_next_event_times,
                        move |_, hosts, next_event_time| {
                            let mut next_event_time = next_event_time.borrow_mut();

                            worker::Worker::reset_next_event_time();
                            worker::Worker::set_round_end_time(window_end);

                            for_each_host(hosts, |host| {
                                let host_next_event_time = {
                                    host.lock_shmem();
                                    host.execute(window_end);
                                    let host_next_event_time = host.next_event_time();
                                    host.unlock_shmem();
                                    host_next_event_time
                                };
                                *next_event_time = [*next_event_time, host_next_event_time]
                                    .into_iter()
                                    .flatten()
                                    .reduce(std::cmp::min);
                            });

                            let packet_next_event_time = worker::Worker::get_next_event_time();

                            *next_event_time = [*next_event_time, packet_next_event_time]
                                .into_iter()
                                .flatten()
                                .reduce(std::cmp::min);
                        },
                    );

                    if let Some(heartbeat_interval) = heartbeat_interval
                        && window_start > last_heartbeat + heartbeat_interval
                    {
                        last_heartbeat = window_start;
                        self.log_heartbeat(window_start);
                    }

                    let current_time = std::time::Instant::now();
                    if current_time.duration_since(time_of_last_usage_check)
                        > Duration::from_secs(30)
                    {
                        time_of_last_usage_check = current_time;
                        self.check_resource_usage();
                    }
                });

                let min_next_event_time = thread_next_event_times
                    .iter()
                    .filter_map(|x| x.borrow_mut().take())
                    .reduce(std::cmp::min)
                    .unwrap_or(EmulatedTime::MAX);

                #[cfg(feature = "enable_perf_logging")]
                {
                    log::debug!(
                        "Finished execution window [{}--{}], next event at {}, active_hosts_in_window={}",
                        (window_start - EmulatedTime::SIMULATION_START).as_nanos(),
                        (window_end - EmulatedTime::SIMULATION_START).as_nanos(),
                        (min_next_event_time - EmulatedTime::SIMULATION_START).as_nanos(),
                        active_hosts_in_window,
                    );

                    eprintln!(
                        "[window-agg] active_hosts_in_window={} window_start_ns={} window_end_ns={} next_event_ns={}",
                        active_hosts_in_window,
                        (window_start - EmulatedTime::SIMULATION_START).as_nanos(),
                        (window_end - EmulatedTime::SIMULATION_START).as_nanos(),
                        (min_next_event_time - EmulatedTime::SIMULATION_START).as_nanos(),
                    );
                }

                let next_window =
                    self.controller
                        .manager_finished_current_round(min_next_event_time);

                // Consult the time controller at the window boundary.
                let current_sim_time_ns =
                    (min_next_event_time - EmulatedTime::SIMULATION_START).as_nanos() as u64;

                let fmt_s = |ns: u64| -> String {
                    if ns % 1_000_000_000 == 0 {
                        format!("{}s", ns / 1_000_000_000)
                    } else {
                        format!("{:.6}s", (ns as f64) / 1_000_000_000.0)
                    }
                };

                let mut print_next_window_info = || {
                    let Some((next_window_start, next_window_end)) = next_window else {
                        eprintln!("** No next window (simulation ending)");
                        return;
                    };

                    let info = Arc::new(Mutex::new(Vec::new()));
                    scheduler.scope(|s| {
                        let info = Arc::clone(&info);
                        s.run_with_hosts(move |_, hosts| {
                            for_each_host(hosts, |host| {
                                let next_time = host.next_event_time();
                                if let Some(t) = next_time
                                    && t < next_window_end
                                {
                                    let mut pids = Vec::new();
                                    for (_proc_id, proc_rc) in host.processes_borrow().iter() {
                                        let proc = proc_rc.borrow(host.root());
                                        if proc.is_running() {
                                            pids.push(proc.native_pid());
                                        }
                                    }
                                    info.lock().unwrap().push((
                                        host.id(),
                                        host.name().to_string(),
                                        t,
                                        pids,
                                    ));
                                }
                            });
                        });
                    });

                    let mut info = info.lock().unwrap();
                    if info.is_empty() {
                        eprintln!("** No hosts scheduled in next window");
                        return;
                    }
                    info.sort_by_key(|(id, _, _, _)| *id);

                    eprintln!("**");
                    eprintln!(
                        "** Next window: t=[{}, {}]",
                        fmt_s(
                            (next_window_start - EmulatedTime::SIMULATION_START).as_nanos()
                                as u64
                        ),
                        fmt_s(
                            (next_window_end - EmulatedTime::SIMULATION_START).as_nanos()
                                as u64
                        )
                    );
                    eprintln!("** Hosts scheduled for next window:");
                    for (host_id, hostname, next_time, pids) in info.iter() {
                        eprintln!(
                            "**   Host {:?} ({}) - next event at t={}",
                            host_id,
                            hostname,
                            fmt_s(
                                (*next_time - EmulatedTime::SIMULATION_START).as_nanos() as u64
                            )
                        );
                        if pids.is_empty() {
                            eprintln!("**     <no running processes>");
                        } else {
                            for pid in pids {
                                eprintln!(
                                    "**     pid={} (attach: s:{})",
                                    pid.as_raw_nonzero().get(),
                                    pid.as_raw_nonzero().get()
                                );
                            }
                        }
                    }
                };

                let boundary_ctx = WindowBoundaryContext {
                    current_sim_time_ns,
                    min_next_event_time,
                    window_start,
                    window_end,
                };

                let decision = self.time_controller.on_window_boundary(
                    &boundary_ctx,
                    &mut print_next_window_info,
                );

                match decision {
                    ControlDecision::Continue => {
                        window = next_window;
                    }
                    ControlDecision::PauseAtBoundary => {
                        // The controller already blocked; proceed to next window.
                        window = next_window;
                    }
                    ControlDecision::Restart { run_until_ns, source } => {
                        restart_request = Some((run_until_ns, source));
                        window = None;
                    }
                    ControlDecision::CheckpointNow { label } => {
                        log::info!("Checkpoint requested: label={}", label);
                        if let Err(e) = self.perform_checkpoint(
                            &label,
                            &mut scheduler,
                            current_sim_time_ns,
                            window_start,
                            window_end,
                        ) {
                            log::error!("Checkpoint failed: {:?}", e);
                        } else {
                            log::info!("Checkpoint '{}' completed successfully", label);
                        }
                        window = next_window;
                    }
                    ControlDecision::RestoreCheckpoint { label } => {
                        log::info!("Restore requested: label={}", label);
                        return Ok(SimulationRunResult::RestoreRequested { label });
                    }
                }
            }

            if restart_request.is_some() {
                worker::RESTART_TEARDOWN.store(true, Ordering::Relaxed);
            }
            scheduler.scope(|s| {
                s.run_with_hosts(move |_, hosts| {
                    for_each_host(hosts, |host| {
                        worker::Worker::set_current_time(self.end_time);
                        host.free_all_applications();
                        host.shutdown();
                        worker::Worker::clear_current_time();
                    });
                });
            });
            if restart_request.is_some() {
                worker::RESTART_TEARDOWN.store(false, Ordering::Relaxed);
            }

            // add each thread's local sim statistics to the global sim statistics.
            scheduler.scope(|s| {
                s.run(|_| {
                    worker::Worker::add_to_global_sim_stats();
                });
            });

            scheduler.join();
        }

        self.time_controller.on_simulation_end();

        // simulation is finished, so update the status logger
        worker::WORKER_SHARED
            .borrow()
            .as_ref()
            .unwrap()
            .update_status_logger(|state| {
                state.current = self.end_time;
            });

        let num_plugin_errors = worker::WORKER_SHARED
            .borrow()
            .as_ref()
            .unwrap()
            .plugin_error_count();

        // drop the simulation's global state
        worker::WORKER_SHARED.borrow_mut().take();

        worker::with_global_sim_stats(|stats| {
            if self.config.experimental.use_syscall_counters.unwrap() {
                log::info!(
                    "Global syscall counts: {}",
                    stats.syscall_counts.lock().unwrap()
                );
            }
            if self.config.experimental.use_object_counters.unwrap() {
                let alloc_counts = stats.alloc_counts.lock().unwrap();
                let dealloc_counts = stats.dealloc_counts.lock().unwrap();
                log::info!("Global allocated object counts: {alloc_counts}");
                log::info!("Global deallocated object counts: {dealloc_counts}");

                if *alloc_counts == *dealloc_counts {
                    log::info!("We allocated and deallocated the same number of objects :)");
                } else {
                    log::warn!("Memory leak detected");
                }
            }

            let stats_filename = self.data_path.clone().join("sim-stats.json");
            sim_stats::write_stats_to_file(&stats_filename, stats)
        })?;

        if let Some((run_until_ns, source)) = restart_request {
            return Ok(SimulationRunResult::RestartRequested { run_until_ns, source });
        }

        Ok(SimulationRunResult::Completed { num_plugin_errors })
    }

    fn build_host(&self, host_id: HostId, host_info: &HostInfo) -> anyhow::Result<Box<Host>> {
        let hostname = CString::new(&*host_info.name).unwrap();

        // scope used to enforce drop order for pointers
        let host = {
            let params = HostParameters {
                // the manager sets this ID
                id: host_id,
                // the manager sets this CPU frequency
                cpu_frequency: self.raw_frequency,
                node_seed: host_info.seed,
                hostname,
                node_id: host_info.network_node_id,
                ip_addr: match host_info.ip_addr.unwrap() {
                    std::net::IpAddr::V4(ip) => u32::to_be(ip.into()),
                    // the config only allows ipv4 addresses, so this shouldn't happen
                    std::net::IpAddr::V6(_) => unreachable!("IPv6 not supported"),
                },
                sim_end_time: self.end_time,
                requested_bw_down_bits: host_info.bandwidth_down_bits.unwrap(),
                requested_bw_up_bits: host_info.bandwidth_up_bits.unwrap(),
                cpu_threshold: host_info.cpu_threshold,
                cpu_precision: host_info.cpu_precision,
                log_level: host_info
                    .log_level
                    .map(|x| x.to_c_loglevel())
                    .unwrap_or(logger::_LogLevel_LOGLEVEL_UNSET),
                pcap_config: host_info.pcap_config,
                qdisc: host_info.qdisc,
                init_sock_recv_buf_size: host_info.recv_buf_size,
                autotune_recv_buf: host_info.autotune_recv_buf,
                init_sock_send_buf_size: host_info.send_buf_size,
                autotune_send_buf: host_info.autotune_send_buf,
                native_tsc_frequency: self.native_tsc_frequency,
                model_unblocked_syscall_latency: self.config.model_unblocked_syscall_latency(),
                max_unapplied_cpu_latency: self.config.max_unapplied_cpu_latency(),
                unblocked_syscall_latency: self.config.unblocked_syscall_latency(),
                unblocked_vdso_latency: self.config.unblocked_vdso_latency(),
                strace_logging_options: self.config.strace_logging_mode(),
                shim_log_level: host_info
                    .log_level
                    .unwrap_or_else(|| self.config.general.log_level.unwrap())
                    .to_c_loglevel(),
                use_new_tcp: self.config.experimental.use_new_tcp.unwrap(),
                use_mem_mapper: self.config.experimental.use_memory_manager.unwrap(),
                use_syscall_counters: self.config.experimental.use_syscall_counters.unwrap(),
            };

            Box::new(Host::new(
                params,
                &self.hosts_path,
                self.raw_frequency,
                self.shmem(),
                self.preload_paths.clone(),
            ))
        };

        host.lock_shmem();

        let build_res: anyhow::Result<()> = if let Some(checkpoint) = self.host_checkpoint(host_info.name.as_str()) {
            validate_host_checkpoint_native_state(checkpoint)
                .with_context(|| format!("Host '{}' native restore pre-check failed", host_info.name))?;
            // Replay in a separate phase after worker TLS is initialized.
            Ok(())
        } else {
            for proc in &host_info.processes {
                let plugin_path =
                    CString::new(proc.plugin.clone().into_os_string().as_bytes()).unwrap();
                let plugin_name = CString::new(proc.plugin.file_name().unwrap().as_bytes()).unwrap();
                let pause_for_debugging = host_info.pause_for_debugging;

                let argv: Vec<CString> = proc
                    .args
                    .iter()
                    .map(|x| CString::new(x.as_bytes()).unwrap())
                    .collect();

                let envv: Vec<CString> = proc
                    .env
                    .clone()
                    .into_iter()
                    .map(|(x, y)| {
                        let mut x: OsString = String::from(x).into();
                        x.push("=");
                        x.push(y);
                        CString::new(x.as_bytes()).unwrap()
                    })
                    .collect();

                host.continue_execution_timer();

                host.add_application(
                    proc.start_time,
                    proc.shutdown_time,
                    proc.shutdown_signal,
                    plugin_name,
                    plugin_path,
                    argv,
                    envv,
                    pause_for_debugging,
                    proc.expected_final_state,
                );

                host.stop_execution_timer();
            }
            Ok(())
        };

        // Always release shmem lock before returning, even on restore failure.
        host.unlock_shmem();
        build_res?;

        Ok(host)
    }

    fn host_checkpoint(&self, hostname: &str) -> Option<&HostCheckpoint> {
        self.restore_checkpoint
            .as_ref()?
            .hosts
            .iter()
            .find(|host| host.hostname == hostname)
    }

    fn log_heartbeat(&mut self, now: EmulatedTime) {
        let mut resources: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut resources) } != 0 {
            let err = nix::errno::Errno::last();
            log::warn!("Unable to get shadow's resource usage: {err}");
            return;
        }

        // the sysinfo syscall also would give memory usage info, but it's less detailed
        let mem_info = resource_usage::meminfo(&mut self.meminfo_file).unwrap();

        // the linux man page says this is in kilobytes, but it seems to be in kibibytes
        let max_memory = (resources.ru_maxrss as f64) / 1048576.0; // KiB->GiB
        let user_time_minutes = (resources.ru_utime.tv_sec as f64) / 60.0;
        let system_time_minutes = (resources.ru_stime.tv_sec as f64) / 60.0;

        // tornettools assumes a specific log format for this message, so don't change it without
        // testing that tornettools can parse resource usage information from the shadow log
        // https://github.com/shadow/tornettools/blob/6c00856c3f08899da30bfc452b6a055572cc4536/tornettools/parse_rusage.py#L58-L86
        log::info!(
            "Process resource usage at simtime {} reported by getrusage(): \
            ru_maxrss={:.03} GiB, \
            ru_utime={:.03} minutes, \
            ru_stime={:.03} minutes, \
            ru_nvcsw={}, \
            ru_nivcsw={}",
            (now - EmulatedTime::SIMULATION_START).as_nanos(),
            max_memory,
            user_time_minutes,
            system_time_minutes,
            resources.ru_nvcsw,
            resources.ru_nivcsw,
        );

        // there are different ways of calculating system memory usage (for example 'free' will
        // calculate used memory differently than 'htop'), so we'll log the values we think are
        // useful, and something parsing the log can calculate whatever it wants
        log::info!(
            "System memory usage in bytes at simtime {} ns reported by /proc/meminfo: {}",
            (now - EmulatedTime::SIMULATION_START).as_nanos(),
            serde_json::to_string(&mem_info).unwrap(),
        );
    }

    fn check_resource_usage(&mut self) {
        if self.check_fd_usage {
            match self.fd_usage() {
                // if more than 90% in use
                Ok((usage, limit)) if usage > limit * 90 / 100 => {
                    log::warn!(
                        "Using more than 90% ({usage}/{limit}) of available file descriptors"
                    );
                    self.check_fd_usage = false;
                }
                Err(e) => {
                    log::warn!("Unable to check fd usage: {e}");
                    self.check_fd_usage = false;
                }
                Ok(_) => {}
            }
        }

        if self.check_mem_usage {
            match self.memory_remaining() {
                // if less than 500 MiB available
                Ok(remaining) if remaining < 500 * 1024 * 1024 => {
                    log::warn!("Only {} MiB of memory available", remaining / 1024 / 1024);
                    self.check_mem_usage = false;
                }
                Err(e) => {
                    log::warn!("Unable to check memory usage: {e}");
                    self.check_mem_usage = false;
                }
                Ok(_) => {}
            }
        }
    }

    /// Returns a tuple of (usage, limit).
    fn fd_usage(&mut self) -> anyhow::Result<(u64, u64)> {
        let dir = std::fs::read_dir("/proc/self/fd").context("Failed to open '/proc/self/fd'")?;

        let mut fd_count: u64 = 0;
        for entry in dir {
            // short-circuit and return on error
            entry.context("Failed to read entry in '/proc/self/fd'")?;
            fd_count += 1;
        }

        let (soft_limit, _) =
            nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)
                .context("Failed to get the fd limit")?;

        Ok((fd_count, soft_limit))
    }

    /// Returns the number of bytes remaining.
    fn memory_remaining(&mut self) -> anyhow::Result<u64> {
        let page_size = nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)
            .context("Failed to get the page size")?
            .ok_or_else(|| anyhow::anyhow!("Failed to get the page size (no errno)"))?;

        let avl_pages = nix::unistd::sysconf(nix::unistd::SysconfVar::_AVPHYS_PAGES)
            .context("Failed to get the number of available pages of physical memory")?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Failed to get the number of available pages of physical memory (no errno)"
                )
            })?;

        let page_size: u64 = page_size.try_into().unwrap();
        let avl_pages: u64 = avl_pages.try_into().unwrap();

        Ok(page_size * avl_pages)
    }

    pub fn shmem(&self) -> &ShMemBlock<'_, ManagerShmem> {
        &self.shmem
    }

    /// Perform a full checkpoint at the current window boundary.
    ///
    /// This method:
    /// 1. Collects all `/dev/shm` files used by Shadow
    /// 2. Backs them up to a checkpoint directory
    /// 3. CRIU-dumps all running managed processes (with `--leave-running`)
    /// 4. Serializes simulation metadata to JSON
    fn perform_checkpoint(
        &self,
        label: &str,
        scheduler: &mut Scheduler<Box<Host>>,
        current_sim_time_ns: u64,
        window_start: EmulatedTime,
        window_end: EmulatedTime,
    ) -> anyhow::Result<()> {
        let checkpoint_base = self.data_path.join("checkpoints").join(label);
        let shmem_backup_dir = checkpoint_base.join("shmem");
        let criu_base_dir = checkpoint_base.join("criu");

        std::fs::create_dir_all(&checkpoint_base)
            .context("Failed to create checkpoint directory")?;

        // 1. Collect and backup shmem files
        let shmem_paths = criu::collect_shadow_shmem_paths()
            .unwrap_or_else(|e| {
                log::warn!("Failed to collect shmem from /proc/self/maps: {e}; falling back");
                shmem_backup::collect_all_shadow_shmem_files().unwrap_or_default()
            });
        shmem_backup::backup_shmem_files(&shmem_paths, &shmem_backup_dir)?;

        let host_snapshots = Arc::new(Mutex::new(Vec::<HostCheckpoint>::new()));
        scheduler.scope(|s| {
            let host_snapshots = Arc::clone(&host_snapshots);
            s.run_with_hosts(move |_, hosts| {
                for_each_host(hosts, |host| {
                    host.lock_shmem();
                    let snapshot = snapshot_host(host);
                    host.unlock_shmem();
                    host_snapshots.lock().unwrap().push(snapshot);
                });
            });
        });
        let mut host_snapshots = host_snapshots.lock().unwrap().clone();

        for host_cp in &mut host_snapshots {
            for proc_cp in &mut host_cp.processes {
                if !proc_cp.is_running {
                    continue;
                }
                let images_dir = criu_base_dir.join(format!(
                    "host_{}_proc_{}",
                    host_cp.host_id, proc_cp.process_id
                ));
                // Leave managed processes running after dump so the simulation can
                // continue until the explicit restore request.
                criu::checkpoint_process(proc_cp.native_pid, &images_dir, true)?;
                proc_cp.criu_image_dir = Some(images_dir);
            }
        }

        let worker_shared = worker::WORKER_SHARED.borrow();
        let worker_shared = worker_shared.as_ref().unwrap();
        let checkpoint = SimulationCheckpoint {
            version: SimulationCheckpoint::CURRENT_VERSION,
            sim_time_ns: current_sim_time_ns,
            window_start_ns: window_start.duration_since(&EmulatedTime::SIMULATION_START).as_nanos()
                as u64,
            window_end_ns: window_end.duration_since(&EmulatedTime::SIMULATION_START).as_nanos()
                as u64,
            prng_state: PrngSnapshot { s: [0; 4] },
            runahead: RunaheadSnapshot {
                is_dynamic: worker_shared.runahead.is_dynamic(),
                min_possible_latency_ns: worker_shared.runahead.min_possible_latency().as_nanos()
                    as u64,
                min_used_latency_ns: worker_shared
                    .runahead
                    .min_used_latency()
                    .map(|x| x.as_nanos() as u64),
                min_runahead_config_ns: worker_shared
                    .runahead
                    .min_runahead_config()
                    .map(|x| x.as_nanos() as u64),
            },
            hosts: host_snapshots,
            manager_shmem_handle: self.shmem().serialize().to_string(),
            shmem_backup_dir: shmem_backup_dir.clone(),
            criu_base_dir: criu_base_dir.clone(),
        };

        // 4. Save checkpoint to filesystem
        let store = FilesystemStore::new(self.data_path.join("checkpoints"))?;
        store.save(label, &checkpoint)?;

        log::info!(
            "Checkpoint '{}' saved to {}",
            label,
            checkpoint_base.display()
        );

        Ok(())
    }
}

pub struct ManagerConfig {
    // deterministic source of randomness for this manager
    pub random: Xoshiro256PlusPlus,

    // map of ip addresses to graph nodes
    pub ip_assignment: IpAssignment<u32>,

    // routing information for paths between graph nodes
    pub routing_info: RoutingInfo<u32>,

    // bandwidths of hosts at ip addresses
    pub host_bandwidths: HashMap<std::net::IpAddr, Bandwidth>,

    // a list of hosts and their processes
    pub hosts: Vec<HostInfo>,
}

/// Helper function to initialize the global [`Host`] before running the closure.
fn for_each_host(host_iter: &mut HostIter<Box<Host>>, mut f: impl FnMut(&Host)) {
    host_iter.for_each(|host| {
        worker::Worker::set_active_host(host);
        worker::Worker::with_active_host(|host| {
            f(host);
        })
        .unwrap();
        worker::Worker::take_active_host()
    });
}

fn snapshot_host(host: &Host) -> HostCheckpoint {
    let queue = host.event_queue().lock().unwrap();
    let event_queue = queue
        .cloned_events()
        .iter()
        .map(event_to_snapshot)
        .collect();
    let last_popped_event_time_ns = queue
        .last_popped_event_time()
        .saturating_duration_since(&EmulatedTime::SIMULATION_START)
        .as_nanos() as u64;
    drop(queue);

    HostCheckpoint {
        host_id: u32::from(host.id()),
        hostname: host.name().to_string(),
        event_queue,
        last_popped_event_time_ns,
        next_event_id: host.next_event_id_counter(),
        next_thread_id: host.next_thread_id_counter(),
        next_packet_id: host.next_packet_id_counter(),
        determinism_sequence_counter: host.determinism_sequence_counter(),
        packet_priority_counter: host.packet_priority_counter(),
        cpu_now_ns: host
            .cpu_borrow()
            .snapshot_times()
            .0
            .saturating_duration_since(&EmulatedTime::SIMULATION_START)
            .as_nanos() as u64,
        cpu_available_ns: host
            .cpu_borrow()
            .snapshot_times()
            .1
            .saturating_duration_since(&EmulatedTime::SIMULATION_START)
            .as_nanos() as u64,
        random_state: snapshot_rng(&host.random_borrow()),
        processes: host
            .processes_borrow()
            .values()
            .map(|proc_rc| {
                let proc = proc_rc.borrow(host.root());
                snapshot_process(host, &proc)
            })
            .collect(),
        host_shmem_handle: host.shim_shmem().serialize().to_string(),
    }
}

fn snapshot_process(host: &Host, process: &crate::host::process::Process) -> ProcessCheckpoint {
    let runnable = process.borrow_as_runnable();
    let native_pid = runnable
        .as_ref()
        .map(|r| r.native_pid().as_raw_nonzero().get())
        .unwrap_or(0);
    let threads = runnable
        .as_ref()
        .map(|runnable| {
            runnable
                .threads_borrow()
                .values()
                .map(|thread_rc| {
                    let thread = thread_rc.borrow(host.root());
                    ThreadCheckpoint {
                        thread_id: u32::try_from(libc::pid_t::from(thread.id())).unwrap(),
                        native_tid: thread.native_tid().as_raw_nonzero().get(),
                        ipc_shmem_handle: thread.mthread().ipc_shmem_handle(),
                        thread_shmem_handle: thread.shmem().serialize().to_string(),
                        current_event_bytes: thread.mthread().current_event_bytes(),
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let dumpable = runnable
        .as_ref()
        .map(|_| process.dumpable().val())
        .unwrap_or(linux_api::sched::SuidDump::SUID_DUMP_USER.val());

    ProcessCheckpoint {
        process_id: u32::from(process.id()),
        criu_image_dir: None,
        native_pid,
        is_running: process.is_running(),
        parent_pid: u32::from(process.parent_id()),
        group_id: u32::from(process.group_id()),
        session_id: u32::from(process.session_id()),
        dumpable,
        threads,
        process_shmem_handle: runnable
            .as_ref()
            .map(|_| process.shmem().serialize().to_string())
            .unwrap_or_default(),
    }
}

fn snapshot_rng(rng: &Xoshiro256PlusPlus) -> PrngSnapshot {
    PrngSnapshot {
        // `rand_xoshiro` stores Xoshiro256++ as 4 x u64; use a checked
        // transmute so checkpoints capture the live PRNG state instead of only
        // the initial seed.
        s: unsafe { std::mem::transmute_copy(rng) },
    }
}

fn restore_rng(snapshot: &PrngSnapshot) -> Xoshiro256PlusPlus {
    unsafe { std::mem::transmute_copy(&snapshot.s) }
}

fn apply_host_checkpoint(host: &Host, checkpoint: &HostCheckpoint) -> anyhow::Result<()> {
    {
        let mut processes = host.processes_borrow_mut();
        for process_cp in &checkpoint.processes {
            if !process_cp.is_running {
                continue;
            }
            let process = Process::from_checkpoint(host, process_cp)
                .map_err(|e| anyhow::anyhow!("Process::from_checkpoint failed for pid {}: {:?}", process_cp.process_id, e))?;
            let process_id = process.borrow(host.root()).id();
            processes.insert(process_id, process);
        }
    }
    let queue = rebuild_event_queue(&checkpoint.event_queue, host);
    let last_popped =
        EmulatedTime::SIMULATION_START + SimulationTime::from_nanos(checkpoint.last_popped_event_time_ns);
    host.replace_event_queue(queue, last_popped);
    host.set_next_event_id_counter(checkpoint.next_event_id);
    host.set_next_thread_id_counter(checkpoint.next_thread_id);
    host.set_next_packet_id_counter(checkpoint.next_packet_id);
    host.set_determinism_sequence_counter(checkpoint.determinism_sequence_counter);
    host.set_packet_priority_counter(checkpoint.packet_priority_counter);
    host.set_random_state(restore_rng(&checkpoint.random_state));
    host.cpu_borrow_mut().restore_times(
        EmulatedTime::SIMULATION_START + SimulationTime::from_nanos(checkpoint.cpu_now_ns),
        EmulatedTime::SIMULATION_START + SimulationTime::from_nanos(checkpoint.cpu_available_ns),
    );
    // Kick restored runnable processes once after replay. The serialized event
    // queue may not include resume tasks for already-running workloads.
    let replay_time = EmulatedTime::SIMULATION_START + SimulationTime::from_nanos(checkpoint.cpu_now_ns);
    {
        let processes = host.processes_borrow();
        for (process_id, process_rc) in processes.iter() {
            let process = process_rc.borrow(host.root());
            if !process.is_running() {
                continue;
            }
            let thread_id = process.thread_group_leader_id();
            let pid_u32 = u32::from(*process_id);
            let tid_u32 = libc::pid_t::from(thread_id) as u32;
            let task = TaskRef::new_with_descriptor(
                {
                    let process_id = *process_id;
                    move |host| {
                        host.resume(process_id, thread_id);
                    }
                },
                TaskDescriptor::ResumeProcess {
                    process_id: pid_u32,
                    thread_id: tid_u32,
                },
            );
            host.schedule_task_at_emulated_time(task, replay_time);
        }
    }
    let running_processes = host
        .processes_borrow()
        .values()
        .filter(|p| p.borrow(host.root()).is_running())
        .count();
    let queue_len = host.event_queue().lock().unwrap().cloned_events().len();
    log::info!(
        "Replayed host checkpoint for '{}': running_processes={}, queue_len={}, next_event={:?}",
        host.name(),
        running_processes,
        queue_len,
        host.next_event_time()
    );
    Ok(())
}

fn validate_host_checkpoint_native_state(checkpoint: &HostCheckpoint) -> anyhow::Result<()> {
    for process_cp in &checkpoint.processes {
        if !process_cp.is_running {
            continue;
        }

        let native_pid = process_cp.native_pid;
        if native_pid <= 1 {
            anyhow::bail!(
                "invalid native pid {} for process {}",
                native_pid,
                process_cp.process_id
            );
        }

        // PID isn't a stable identity across CRIU restore. At this point this
        // should already be the post-restore pid; require it to be enumerable.
        let observed_tids = observed_thread_tids(native_pid);
        if observed_tids.is_empty() {
            anyhow::bail!(
                "native pid {} not accessible/enumerable before object restore (process {}); checkpoint_tids={:?}",
                native_pid,
                process_cp.process_id,
                process_cp
                    .threads
                    .iter()
                    .map(|t| t.native_tid)
                    .collect::<Vec<_>>()
            );
        }
        log::info!(
            "Phase1 native validation: process {} pid {} checkpoint_tids={:?} observed_tids={:?}",
            process_cp.process_id,
            native_pid,
            process_cp
                .threads
                .iter()
                .map(|t| t.native_tid)
                .collect::<Vec<_>>(),
            observed_tids
        );

        // Validate process-level shmem handle is parseable.
        let _ = shadow_shmem::allocator::ShMemBlockSerialized::from_str(&process_cp.process_shmem_handle)
            .with_context(|| {
                format!(
                    "invalid process shmem handle for process {}",
                    process_cp.process_id
                )
            })?;

        // Conservative thread checks: each thread has parseable handles and
        // a correctly sized serialized current event buffer.
        for thread_cp in &process_cp.threads {
            if thread_cp.native_tid <= 0 {
                anyhow::bail!(
                    "invalid native tid {} for process {} thread {}",
                    thread_cp.native_tid,
                    process_cp.process_id,
                    thread_cp.thread_id
                );
            }

            let _ = shadow_shmem::allocator::ShMemBlockSerialized::from_str(&thread_cp.ipc_shmem_handle)
                .with_context(|| {
                    format!(
                        "invalid ipc shmem handle for process {} thread {}",
                        process_cp.process_id,
                        thread_cp.thread_id
                    )
                })?;
            let _ = shadow_shmem::allocator::ShMemBlockSerialized::from_str(&thread_cp.thread_shmem_handle)
                .with_context(|| {
                    format!(
                        "invalid thread shmem handle for process {} thread {}",
                        process_cp.process_id,
                        thread_cp.thread_id
                    )
                })?;

            if thread_cp.current_event_bytes.len()
                != std::mem::size_of::<shadow_shim_helper_rs::shim_event::ShimEventToShadow>()
            {
                anyhow::bail!(
                    "unexpected current_event_bytes length {} for process {} thread {}",
                    thread_cp.current_event_bytes.len(),
                    process_cp.process_id,
                    thread_cp.thread_id
                );
            }
        }
    }
    Ok(())
}

fn observed_thread_tids(native_pid: i32) -> Vec<i32> {
    let task_dir = format!("/proc/{native_pid}/task");
    let Ok(entries) = std::fs::read_dir(task_dir) else {
        return Vec::new();
    };
    let mut tids: Vec<i32> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|name| name.parse::<i32>().ok())
        .collect();
    tids.sort_unstable();
    tids
}

/// Get the raw speed of the experiment machine.
fn get_raw_cpu_frequency_hz() -> anyhow::Result<u64> {
    // Original scheme: prefer cpufreq's cpuinfo_max_freq (kHz) when available.
    const CONFIG_CPU_MAX_FREQ_FILE: &str =
        "/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq";
    if let Ok(khz_s) = std::fs::read_to_string(CONFIG_CPU_MAX_FREQ_FILE) {
        if let Ok(khz) = khz_s.trim().parse::<u64>() {
            if khz > 0 {
                return Ok(khz * 1000);
            }
        }
    }

    // Fallback: parse /proc/cpuinfo and use the maximum "cpu MHz".
    // This is more likely to work in containers/VMs/WSL where cpufreq is missing.
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo")
        .context("Could not read /proc/cpuinfo")?;

    let mut max_mhz: Option<f64> = None;
    for line in cpuinfo.lines() {
        let (k, v) = line.split_once(':').unwrap_or(("", ""));
        if k.trim() != "cpu MHz" {
            continue;
        }
        let mhz = v.trim().parse::<f64>().context("Failed to parse cpu MHz")?;
        if mhz.is_finite() && mhz > 0.0 {
            max_mhz = Some(max_mhz.map_or(mhz, |cur| cur.max(mhz)));
        }
    }

    let max_mhz = max_mhz.context("Could not find cpu MHz in /proc/cpuinfo")?;
    Ok((max_mhz * 1_000_000.0) as u64)
}

fn get_required_preload_path(libname: &str) -> anyhow::Result<PathBuf> {
    let libname_c = CString::new(libname).unwrap();
    let libpath_c = unsafe { c::scanRpathForLib(libname_c.as_ptr()) };

    // scope needed to make sure the CStr is dropped before we free libpath_c
    let libpath = if !libpath_c.is_null() {
        let libpath = unsafe { CStr::from_ptr(libpath_c) };
        let libpath = OsStr::from_bytes(libpath.to_bytes());
        Some(PathBuf::from(libpath.to_os_string()))
    } else {
        None
    };

    unsafe { libc::free(libpath_c as *mut libc::c_void) };

    let libpath = libpath.ok_or_else(|| anyhow::anyhow!(format!("Could not library in rpath")))?;

    let bytes = libpath.as_os_str().as_bytes();
    if bytes.iter().any(|c| *c == b' ' || *c == b':') {
        // These are unescapable separators in LD_PRELOAD.
        anyhow::bail!("Preload path contains LD_PRELOAD-incompatible characters: {libpath:?}");
    }

    log::debug!(
        "Found required preload library {} at path {}",
        libname,
        libpath.display(),
    );

    Ok(libpath)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use shadow_shim_helper_rs::shim_shmem::ManagerShmem;
    use shadow_shim_helper_rs::simulation_time::SimulationTime;
    use shadow_shmem::allocator::shmalloc;

    use super::*;
    use crate::core::work::task::TaskRef;

    fn test_host(name: &str) -> Host {
        let temp_root = std::env::temp_dir().join(format!(
            "shadow-checkpoint-test-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(&temp_root).unwrap();
        let manager_shmem = shmalloc(ManagerShmem {
            log_start_time_micros: 0,
            native_preemption_config: None.into(),
            emulate_cpuid: false,
        });

        Host::new(
            HostParameters {
                id: HostId::from(0),
                node_seed: 0x1234_5678,
                hostname: CString::new(name).unwrap(),
                node_id: 0,
                ip_addr: u32::to_be(std::net::Ipv4Addr::new(11, 0, 0, 1).into()),
                sim_end_time: EmulatedTime::SIMULATION_START + SimulationTime::from_secs(60),
                requested_bw_down_bits: 1_000_000_000,
                requested_bw_up_bits: 1_000_000_000,
                cpu_frequency: 2_500_000_000,
                cpu_threshold: Some(SimulationTime::NANOSECOND),
                cpu_precision: Some(SimulationTime::NANOSECOND),
                log_level: logger::_LogLevel_LOGLEVEL_UNSET,
                pcap_config: None,
                qdisc: configuration::QDiscMode::Fifo,
                init_sock_recv_buf_size: 0,
                autotune_recv_buf: false,
                init_sock_send_buf_size: 0,
                autotune_send_buf: false,
                native_tsc_frequency: 2_500_000_000,
                model_unblocked_syscall_latency: false,
                max_unapplied_cpu_latency: SimulationTime::ZERO,
                unblocked_syscall_latency: SimulationTime::ZERO,
                unblocked_vdso_latency: SimulationTime::ZERO,
                strace_logging_options: None,
                shim_log_level: logger::_LogLevel_LOGLEVEL_UNSET,
                use_new_tcp: true,
                use_mem_mapper: true,
                use_syscall_counters: false,
            },
            &temp_root,
            2_500_000_000,
            &manager_shmem,
            Arc::new(Vec::new()),
        )
    }

    fn seed_host_state(host: &Host) {
        host.set_next_event_id_counter(17);
        host.set_next_thread_id_counter(1017);
        host.set_next_packet_id_counter(23);
        host.set_determinism_sequence_counter(29);
        host.set_packet_priority_counter(31);
        host.cpu_borrow_mut().restore_times(
            EmulatedTime::SIMULATION_START + SimulationTime::from_secs(4),
            EmulatedTime::SIMULATION_START + SimulationTime::from_secs(9),
        );

        host.schedule_task_at_emulated_time(
            TaskRef::new_with_descriptor(
                |_host| {},
                TaskDescriptor::TimerExpire {
                    timer_id: 1,
                    expire_id: 7,
                },
            ),
            EmulatedTime::SIMULATION_START + SimulationTime::from_secs(5),
        );
        host.schedule_task_at_emulated_time(
            TaskRef::new_with_descriptor(
                |_host| {},
                TaskDescriptor::RelayForward { relay_id: 0 },
            ),
            EmulatedTime::SIMULATION_START + SimulationTime::from_secs(8),
        );
    }

    #[test]
    fn host_checkpoint_round_trips_queue_and_counters() {
        let source = test_host("src");
        seed_host_state(&source);
        let snapshot = snapshot_host(&source);

        let restored = test_host("dst");
        apply_host_checkpoint(&restored, &snapshot).unwrap();
        let restored_snapshot = snapshot_host(&restored);

        assert_eq!(snapshot.event_queue, restored_snapshot.event_queue);
        assert_eq!(snapshot.last_popped_event_time_ns, restored_snapshot.last_popped_event_time_ns);
        assert_eq!(snapshot.next_event_id, restored_snapshot.next_event_id);
        assert_eq!(snapshot.next_thread_id, restored_snapshot.next_thread_id);
        assert_eq!(snapshot.next_packet_id, restored_snapshot.next_packet_id);
        assert_eq!(
            snapshot.determinism_sequence_counter,
            restored_snapshot.determinism_sequence_counter
        );
        assert_eq!(
            snapshot.packet_priority_counter,
            restored_snapshot.packet_priority_counter
        );
        assert_eq!(snapshot.cpu_now_ns, restored_snapshot.cpu_now_ns);
        assert_eq!(snapshot.cpu_available_ns, restored_snapshot.cpu_available_ns);

        source.shutdown();
        restored.shutdown();
    }

    #[test]
    fn restored_host_replays_new_scheduling_like_original() {
        let source = test_host("src-replay");
        seed_host_state(&source);
        let snapshot = snapshot_host(&source);

        let restored = test_host("dst-replay");
        apply_host_checkpoint(&restored, &snapshot).unwrap();

        let extra_time = EmulatedTime::SIMULATION_START + SimulationTime::from_secs(11);
        let extra_task = || {
            TaskRef::new_with_descriptor(
                |_host| {},
                TaskDescriptor::TimerExpire {
                    timer_id: 9,
                    expire_id: 99,
                },
            )
        };
        source.schedule_task_at_emulated_time(extra_task(), extra_time);
        restored.schedule_task_at_emulated_time(extra_task(), extra_time);

        assert_eq!(snapshot_host(&source).event_queue, snapshot_host(&restored).event_queue);
        assert_eq!(snapshot_host(&source).next_event_id, snapshot_host(&restored).next_event_id);

        source.shutdown();
        restored.shutdown();
    }
}
