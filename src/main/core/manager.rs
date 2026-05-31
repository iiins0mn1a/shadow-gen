use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::net::Ipv4Addr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use atomic_refcell::AtomicRefCell;
use linux_api::prctl::ArchPrctlOp;
use log::{debug, warn};
use rand::seq::SliceRandom;
use rand_xoshiro::Xoshiro256PlusPlus;
use scheduler::thread_per_core::ThreadPerCoreSched;
use scheduler::thread_per_host::ThreadPerHostSched;
use scheduler::{HostIter, Scheduler};
use shadow_shim_helper_rs::emulated_time::EmulatedTime;
use shadow_shim_helper_rs::option::FfiOption;
use shadow_shim_helper_rs::shim_shmem::{ManagerShmem, NativePreemptionConfig};
use shadow_shim_helper_rs::simulation_time::SimulationTime;
use shadow_shim_helper_rs::syscall_types::{SyscallArgs, UntypedForeignPtr};
use shadow_shim_helper_rs::HostId;
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
use crate::core::worker::Worker;
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

        let route_endpoints: HashMap<Ipv4Addr, worker::RouteEndpoint> = host_init
            .iter()
            .map(|(info, id)| {
                let std::net::IpAddr::V4(addr) = info.ip_addr.unwrap() else {
                    unreachable!("IPv6 not supported");
                };
                let node_id = manager_config.ip_assignment.get_node(addr.into()).unwrap();
                (
                    addr,
                    worker::RouteEndpoint {
                        host_id: *id,
                        node_id,
                    },
                )
            })
            .collect();
        let packet_route_cache = packet_route_cache_enabled()
            .then(|| build_packet_route_cache(&route_endpoints, &manager_config.routing_info));

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
        let network_perf_stats = scheduler_perf_counters_enabled()
            .then(|| Arc::new(worker::NetworkPerfStats::default()));

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
        let old_worker_shared = worker::WORKER_SHARED
            .borrow_mut()
            .replace(worker::WorkerShared {
                ip_assignment: manager_config.ip_assignment,
                routing_info: manager_config.routing_info,
                host_bandwidths: manager_config.host_bandwidths,
                dns,
                route_endpoints,
                packet_route_cache,
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
                network_perf_stats: network_perf_stats.clone(),
                packet_counters_enabled: packet_counters_enabled(),
                bootstrap_end_time,
                sim_end_time: self.end_time,
            });
        if old_worker_shared.is_some() {
            // In restart/restore flows, the previous simulation's global queues may still hold
            // host-bound tasks whose Drop paths access HostTreePointer-backed state. They are
            // dropped here on the main thread before any host is active.
            worker::RESTART_TEARDOWN.store(true, Ordering::Relaxed);
            drop(old_worker_shared);
            worker::RESTART_TEARDOWN.store(false, Ordering::Relaxed);
        }

        let mut restart_request: Option<(
            Option<u64>,
            crate::core::run_control::commands::RestartSource,
        )> = None;

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
                let restore_protocol_mode = sim_checkpoint.restore_protocol.mode;
                let checkpoint_time = EmulatedTime::SIMULATION_START
                    + SimulationTime::from_nanos(sim_checkpoint.sim_time_ns);
                let replay_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

                scheduler.scope(|s| {
                    let checkpoints = Arc::clone(&checkpoints);
                    let replay_err = Arc::clone(&replay_err);
                    let restore_protocol_mode = restore_protocol_mode;
                    let checkpoint_time = checkpoint_time;
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
                            let apply_res = apply_host_checkpoint(
                                host,
                                checkpoint,
                                restore_protocol_mode,
                                checkpoint_time,
                            );
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

                for process_cp in sim_checkpoint
                    .hosts
                    .iter()
                    .flat_map(|host| host.processes.iter())
                    .filter(|process| process.is_running && process.native_pid > 1)
                {
                    let rc = unsafe { libc::kill(process_cp.native_pid, libc::SIGCONT) };
                    if rc != 0 {
                        anyhow::bail!(
                            "failed to resume restored process pid {} with SIGCONT: {}",
                            process_cp.native_pid,
                            std::io::Error::last_os_error()
                        );
                    }
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

            let scheduler_thread_data: Vec<SchedulerThreadData> = (0..scheduler.parallelism())
                .map(|_| SchedulerThreadData::default())
                .collect();

            let heartbeat_interval = self
                .config
                .general
                .heartbeat_interval
                .flatten()
                .map(|x| Duration::from(x).try_into().unwrap());

            let mut last_heartbeat = EmulatedTime::SIMULATION_START;
            let mut time_of_last_usage_check = std::time::Instant::now();
            let scheduler_perf_stats = scheduler_perf_counters_enabled().then(|| {
                Arc::new(SchedulerPerfStats {
                    parallelism: scheduler.parallelism(),
                    ..SchedulerPerfStats::default()
                })
            });

            // Notify the time controller that the simulation is starting.
            self.time_controller.on_simulation_start();

            // the scheduling loop
            while let Some((window_start, window_end)) = window {
                let run_control_trace = run_control_trace_enabled();
                if run_control_trace {
                    log::info!(
                        "run-control manager window start: window_start_ns={} window_end_ns={}",
                        (window_start - EmulatedTime::SIMULATION_START).as_nanos(),
                        (window_end - EmulatedTime::SIMULATION_START).as_nanos(),
                    );
                }

                let scheduler_window_index = scheduler_perf_stats
                    .as_ref()
                    .map(|stats| stats.windows.fetch_add(1, Ordering::Relaxed) + 1)
                    .unwrap_or_default();
                for thread_data in &scheduler_thread_data {
                    *thread_data.next_event_time.borrow_mut() = None;
                    thread_data.worker_body_wall_ns.store(0, Ordering::Relaxed);
                    thread_data
                        .worker_body_continue_receive_wall_ns
                        .store(0, Ordering::Relaxed);
                }

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
                let scheduler_scope_started = scheduler_perf_stats
                    .as_ref()
                    .map(|_| std::time::Instant::now());
                scheduler.scope(|s| {
                    let scheduler_perf_stats = scheduler_perf_stats.clone();
                    s.run_with_data(
                        &scheduler_thread_data,
                        move |thread_index, hosts, thread_data| {
                            let worker_body_started = scheduler_perf_stats
                                .as_ref()
                                .map(|_| std::time::Instant::now());
                            if scheduler_perf_stats.is_some() {
                                crate::host::managed_thread::tdt_perf_reset_worker_body_continue_receive_wall_ns();
                            }
                            let mut next_event_time = thread_data.next_event_time.borrow_mut();
                            let mut host_scans = 0u64;
                            let mut host_executes = 0u64;
                            let mut host_execute_wall_ns = 0u64;
                            let mut host_execute_by_host = scheduler_perf_stats
                                .as_ref()
                                .map(|_| HashMap::<HostId, HostExecutePerf>::new());

                            worker::Worker::reset_next_event_time();
                            worker::Worker::set_round_end_time(window_end);

                            for_each_host(hosts, |host| {
                                host_scans += 1;
                                let host_next_event_time = match host.next_event_time() {
                                    Some(t) if t < window_end => {
                                        host_executes += 1;
                                        host.lock_shmem();
                                        let host_execute_started = scheduler_perf_stats
                                            .as_ref()
                                            .map(|_| std::time::Instant::now());
                                        let collect_scheduler_stats =
                                            scheduler_perf_stats.is_some();
                                        let execution_stats =
                                            host.execute(window_end, collect_scheduler_stats);
                                        if let Some(stats) = scheduler_perf_stats.as_ref() {
                                            stats
                                                .packet_events
                                                .fetch_add(
                                                    execution_stats.packet_events,
                                                    Ordering::Relaxed,
                                                );
                                            stats.local_events.fetch_add(
                                                execution_stats.local_events,
                                                Ordering::Relaxed,
                                            );
                                            stats.cpu_delayed_events.fetch_add(
                                                execution_stats.cpu_delayed_events,
                                                Ordering::Relaxed,
                                            );
                                            stats.packet_event_wall_ns.fetch_add(
                                                execution_stats.packet_event_wall_ns,
                                                Ordering::Relaxed,
                                            );
                                            stats.local_event_wall_ns.fetch_add(
                                                execution_stats.local_event_wall_ns,
                                                Ordering::Relaxed,
                                            );
                                            stats.resume_process_events.fetch_add(
                                                execution_stats.resume_process_events,
                                                Ordering::Relaxed,
                                            );
                                            stats.resume_process_wall_ns.fetch_add(
                                                execution_stats.resume_process_wall_ns,
                                                Ordering::Relaxed,
                                            );
                                            stats.start_application_events.fetch_add(
                                                execution_stats.start_application_events,
                                                Ordering::Relaxed,
                                            );
                                            stats.start_application_wall_ns.fetch_add(
                                                execution_stats.start_application_wall_ns,
                                                Ordering::Relaxed,
                                            );
                                            stats.shutdown_process_events.fetch_add(
                                                execution_stats.shutdown_process_events,
                                                Ordering::Relaxed,
                                            );
                                            stats.shutdown_process_wall_ns.fetch_add(
                                                execution_stats.shutdown_process_wall_ns,
                                                Ordering::Relaxed,
                                            );
                                            stats.relay_forward_events.fetch_add(
                                                execution_stats.relay_forward_events,
                                                Ordering::Relaxed,
                                            );
                                            stats.relay_forward_wall_ns.fetch_add(
                                                execution_stats.relay_forward_wall_ns,
                                                Ordering::Relaxed,
                                            );
                                            stats.syscall_condition_wake_events.fetch_add(
                                                execution_stats.syscall_condition_wake_events,
                                                Ordering::Relaxed,
                                            );
                                            stats.syscall_condition_wake_wall_ns.fetch_add(
                                                execution_stats.syscall_condition_wake_wall_ns,
                                                Ordering::Relaxed,
                                            );
                                            stats
                                                .prepare_poll_timeout_completion_events
                                                .fetch_add(
                                                    execution_stats
                                                        .prepare_poll_timeout_completion_events,
                                                    Ordering::Relaxed,
                                                );
                                            stats
                                                .prepare_poll_timeout_completion_wall_ns
                                                .fetch_add(
                                                    execution_stats
                                                        .prepare_poll_timeout_completion_wall_ns,
                                                    Ordering::Relaxed,
                                                );
                                            stats
                                                .restore_blocked_syscall_condition_events
                                                .fetch_add(
                                                    execution_stats
                                                        .restore_blocked_syscall_condition_events,
                                                    Ordering::Relaxed,
                                                );
                                            stats
                                                .restore_blocked_syscall_condition_wall_ns
                                                .fetch_add(
                                                    execution_stats
                                                        .restore_blocked_syscall_condition_wall_ns,
                                                    Ordering::Relaxed,
                                                );
                                            stats.timer_expire_events.fetch_add(
                                                execution_stats.timer_expire_events,
                                                Ordering::Relaxed,
                                            );
                                            stats.timer_expire_wall_ns.fetch_add(
                                                execution_stats.timer_expire_wall_ns,
                                                Ordering::Relaxed,
                                            );
                                            stats.legacy_tcp_deferred_events.fetch_add(
                                                execution_stats.legacy_tcp_deferred_events,
                                                Ordering::Relaxed,
                                            );
                                            stats.legacy_tcp_deferred_wall_ns.fetch_add(
                                                execution_stats.legacy_tcp_deferred_wall_ns,
                                                Ordering::Relaxed,
                                            );
                                            stats.exec_continuation_events.fetch_add(
                                                execution_stats.exec_continuation_events,
                                                Ordering::Relaxed,
                                            );
                                            stats.exec_continuation_wall_ns.fetch_add(
                                                execution_stats.exec_continuation_wall_ns,
                                                Ordering::Relaxed,
                                            );
                                            stats.opaque_events.fetch_add(
                                                execution_stats.opaque_events,
                                                Ordering::Relaxed,
                                            );
                                            stats.opaque_wall_ns.fetch_add(
                                                execution_stats.opaque_wall_ns,
                                                Ordering::Relaxed,
                                            );
                                            stats.undescribed_events.fetch_add(
                                                execution_stats.undescribed_events,
                                                Ordering::Relaxed,
                                            );
                                            stats.undescribed_wall_ns.fetch_add(
                                                execution_stats.undescribed_wall_ns,
                                                Ordering::Relaxed,
                                            );
                                        }
                                        if let Some(started) = host_execute_started {
                                            let elapsed_ns =
                                                started.elapsed().as_nanos() as u64;
                                            host_execute_wall_ns += elapsed_ns;
                                            if let Some(host_stats) = host_execute_by_host.as_mut()
                                            {
                                                let entry =
                                                    host_stats.entry(host.id()).or_default();
                                                if entry.name.is_empty() {
                                                    entry.name = host.name().to_string();
                                                }
                                                entry.count += 1;
                                                entry.wall_ns += elapsed_ns;
                                                entry.syscall_condition_wake_wall_ns +=
                                                    execution_stats
                                                        .syscall_condition_wake_wall_ns;
                                            }
                                        }
                                        let host_next_event_time = host.next_event_time();
                                        if !execution_stats.host_shmem_unlocked_on_return {
                                            host.unlock_shmem();
                                        }
                                        host_next_event_time
                                    }
                                    host_next_event_time => host_next_event_time,
                                };
                                *next_event_time = min_emulated_time_option(
                                    *next_event_time,
                                    host_next_event_time,
                                );
                            });

                            let packet_next_event_time = worker::Worker::get_next_event_time();

                            *next_event_time =
                                min_emulated_time_option(*next_event_time, packet_next_event_time);

                            if let Some(stats) = scheduler_perf_stats.as_ref() {
                                stats.host_scans.fetch_add(host_scans, Ordering::Relaxed);
                                stats
                                    .host_executes
                                    .fetch_add(host_executes, Ordering::Relaxed);
                                stats
                                    .host_execute_wall_ns
                                    .fetch_add(host_execute_wall_ns, Ordering::Relaxed);
                                if let Some(started) = worker_body_started {
                                    let worker_body_wall_ns =
                                        started.elapsed().as_nanos() as u64;
                                    thread_data
                                        .worker_body_wall_ns
                                        .store(worker_body_wall_ns, Ordering::Relaxed);
                                    stats
                                        .worker_bodies
                                        .fetch_add(1, Ordering::Relaxed);
                                    stats
                                        .worker_body_wall_ns
                                        .fetch_add(worker_body_wall_ns, Ordering::Relaxed);
                                    stats
                                        .max_worker_body_wall_ns
                                        .fetch_max(worker_body_wall_ns, Ordering::Relaxed);
                                    if let Some(host_stats) = host_execute_by_host {
                                        let worker_body_continue_receive_wall_ns =
                                            crate::host::managed_thread::tdt_perf_take_worker_body_continue_receive_wall_ns();
                                        thread_data
                                            .worker_body_continue_receive_wall_ns
                                            .store(
                                                worker_body_continue_receive_wall_ns,
                                                Ordering::Relaxed,
                                            );
                                        stats
                                            .worker_body_continue_receive_wall_ns
                                            .fetch_add(
                                                worker_body_continue_receive_wall_ns,
                                                Ordering::Relaxed,
                                            );
                                        stats
                                            .max_worker_body_continue_receive_wall_ns
                                            .fetch_max(
                                                worker_body_continue_receive_wall_ns,
                                                Ordering::Relaxed,
                                            );
                                        let top_host = host_stats
                                            .iter()
                                            .max_by_key(|(_, perf)| perf.wall_ns)
                                            .map(|(host_id, perf)| WorkerBodyTopHost {
                                                host_id: *host_id,
                                                name: perf.name.clone(),
                                                count: perf.count,
                                                wall_ns: perf.wall_ns,
                                                syscall_condition_wake_wall_ns: perf
                                                    .syscall_condition_wake_wall_ns,
                                            });
                                        stats.worker_body_details.lock().unwrap().push(
                                            WorkerBodyPerf {
                                                window_index: scheduler_window_index,
                                                thread_index,
                                                host_scans,
                                                host_executes,
                                                body_wall_ns: worker_body_wall_ns,
                                                continue_receive_wall_ns:
                                                    worker_body_continue_receive_wall_ns,
                                                top_host,
                                            },
                                        );
                                        let mut aggregate =
                                            stats.host_execute_by_host.lock().unwrap();
                                        for (host_id, local) in host_stats {
                                            let entry = aggregate.entry(host_id).or_default();
                                            if entry.name.is_empty() {
                                                entry.name = local.name;
                                            }
                                            entry.count += local.count;
                                            entry.wall_ns += local.wall_ns;
                                            entry.syscall_condition_wake_wall_ns +=
                                                local.syscall_condition_wake_wall_ns;
                                        }
                                    }
                                }
                            }
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
                if run_control_trace {
                    log::info!(
                        "run-control manager scheduler returned: window_start_ns={} window_end_ns={}",
                        (window_start - EmulatedTime::SIMULATION_START).as_nanos(),
                        (window_end - EmulatedTime::SIMULATION_START).as_nanos(),
                    );
                }
                if let Some(started) = scheduler_scope_started
                    && let Some(stats) = scheduler_perf_stats.as_ref()
                {
                    let mut max_worker_body_wall_ns = 0;
                    let mut max_worker_body_continue_receive_wall_ns = 0;
                    let mut second_worker_body_wall_ns = 0;
                    for thread_data in &scheduler_thread_data {
                        let worker_body_wall_ns =
                            thread_data.worker_body_wall_ns.load(Ordering::Relaxed);
                        let continue_receive_wall_ns = thread_data
                            .worker_body_continue_receive_wall_ns
                            .load(Ordering::Relaxed);
                        if worker_body_wall_ns > max_worker_body_wall_ns {
                            second_worker_body_wall_ns = max_worker_body_wall_ns;
                            max_worker_body_wall_ns = worker_body_wall_ns;
                            max_worker_body_continue_receive_wall_ns = continue_receive_wall_ns;
                        } else if worker_body_wall_ns > second_worker_body_wall_ns {
                            second_worker_body_wall_ns = worker_body_wall_ns;
                        }
                    }
                    let projected_async_body_wall_ns = max_worker_body_wall_ns
                        .saturating_sub(max_worker_body_continue_receive_wall_ns);
                    let projected_async_scope_wall_ns =
                        std::cmp::max(projected_async_body_wall_ns, second_worker_body_wall_ns);
                    let estimated_async_continue_overlap_savings_ns =
                        max_worker_body_wall_ns.saturating_sub(projected_async_scope_wall_ns);
                    stats
                        .scheduler_scope_wall_ns
                        .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    stats
                        .window_max_worker_body_wall_ns
                        .fetch_add(max_worker_body_wall_ns, Ordering::Relaxed);
                    stats
                        .window_max_worker_body_continue_receive_wall_ns
                        .fetch_add(max_worker_body_continue_receive_wall_ns, Ordering::Relaxed);
                    stats.estimated_async_continue_overlap_savings_ns.fetch_add(
                        estimated_async_continue_overlap_savings_ns,
                        Ordering::Relaxed,
                    );
                }

                let min_next_event_time = scheduler_thread_data
                    .iter()
                    .filter_map(|x| x.next_event_time.borrow_mut().take())
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

                let next_window = self
                    .controller
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

                let mut print_next_window_info = || -> String {
                    let Some((next_window_start, next_window_end)) = next_window else {
                        return "** No next window (simulation ending)\n".to_string();
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
                    let mut output = String::new();
                    if info.is_empty() {
                        output.push_str("** No hosts scheduled in next window\n");
                        return output;
                    }
                    info.sort_by_key(|(id, _, _, _)| *id);

                    output.push_str("**\n");
                    output.push_str(&format!(
                        "** Next window: t=[{}, {}]\n",
                        fmt_s(
                            (next_window_start - EmulatedTime::SIMULATION_START).as_nanos() as u64
                        ),
                        fmt_s((next_window_end - EmulatedTime::SIMULATION_START).as_nanos() as u64)
                    ));
                    output.push_str("** Hosts scheduled for next window:\n");
                    for (host_id, hostname, next_time, pids) in info.iter() {
                        output.push_str(&format!(
                            "**   Host {:?} ({}) - next event at t={}\n",
                            host_id,
                            hostname,
                            fmt_s((*next_time - EmulatedTime::SIMULATION_START).as_nanos() as u64)
                        ));
                        if pids.is_empty() {
                            output.push_str("**     <no running processes>\n");
                        } else {
                            for pid in pids {
                                output.push_str(&format!(
                                    "**     pid={} (attach: s:{})\n",
                                    pid.as_raw_nonzero().get(),
                                    pid.as_raw_nonzero().get()
                                ));
                            }
                        }
                    }
                    output
                };

                let boundary_ctx = WindowBoundaryContext {
                    current_sim_time_ns,
                    min_next_event_time,
                    window_start,
                    window_end,
                };

                let decision = self
                    .time_controller
                    .on_window_boundary(&boundary_ctx, &mut print_next_window_info);

                match decision {
                    ControlDecision::Continue => {
                        window = next_window;
                    }
                    ControlDecision::PauseAtBoundary => {
                        // The controller already blocked; proceed to next window.
                        window = next_window;
                    }
                    ControlDecision::Restart {
                        run_until_ns,
                        source,
                    } => {
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
                        log_scheduler_perf_stats(&scheduler_perf_stats);
                        log_network_perf_stats(&network_perf_stats);
                        log_syscall_condition_perf_stats();
                        crate::host::managed_thread::log_tdt_managed_thread_perf_stats();
                        // The existing scheduler and its Hosts are about to be dropped on the
                        // main thread while returning from `run()`. Allow HostTreePointer-backed
                        // objects in the old simulation graph to tear down without an active host.
                        worker::RESTART_TEARDOWN.store(true, Ordering::Relaxed);
                        return Ok(SimulationRunResult::RestoreRequested { label });
                    }
                }
            }

            log_scheduler_perf_stats(&scheduler_perf_stats);
            log_network_perf_stats(&network_perf_stats);
            log_syscall_condition_perf_stats();
            crate::host::managed_thread::log_tdt_managed_thread_perf_stats();

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
            return Ok(SimulationRunResult::RestartRequested {
                run_until_ns,
                source,
            });
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

        let build_res: anyhow::Result<()> =
            if let Some(checkpoint) = self.host_checkpoint(host_info.name.as_str()) {
                validate_host_checkpoint_native_state(checkpoint).with_context(|| {
                    format!("Host '{}' native restore pre-check failed", host_info.name)
                })?;
                // Replay in a separate phase after worker TLS is initialized.
                Ok(())
            } else {
                for proc in &host_info.processes {
                    let plugin_path =
                        CString::new(proc.plugin.clone().into_os_string().as_bytes()).unwrap();
                    let plugin_name =
                        CString::new(proc.plugin.file_name().unwrap().as_bytes()).unwrap();
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

    /// Full checkpoint: persist everything needed to restore later as one labeled bundle.
    ///
    /// ## Why three artifacts?
    /// Simulation state is split across:
    /// - **Shadow (Rust)**: hosts, descriptors, syscall handlers, event queues, thread runtime
    ///   snapshots — serialized to `{label}.checkpoint.json` via [`snapshot_host`].
    /// - **Managed plugins (native processes)**: memory, registers, FD tables — captured by
    ///   **CRIU** under `criu/host_*_proc_*` (Shadow does not duplicate that in JSON).
    /// - **`/dev/shm` backing files**: MAP_SHARED regions are not fully inlined in CRIU
    ///   images; we copy `shadow_shmemfile_*` into `shmem/` so restore can put them back
    ///   before CRIU restore maps them again.
    ///
    /// ## Call site
    /// Invoked from the main loop when run-control returns [`ControlDecision::CheckpointNow`],
    /// i.e. after the current scheduling window has finished executing (see `on_window_boundary`).
    ///
    /// ## Order (high level)
    /// 1. Backup shmem files.
    /// 2. Snapshot every host (while holding per-host shmem locks) into `HostCheckpoint` trees.
    /// 3. CRIU-dump each running managed process (`--leave-running` so the sim can continue).
    /// 4. Build [`SimulationCheckpoint`] (JSON paths + embedded host snapshots + manager knobs)
    ///    and write JSON next to the `checkpoints/<label>/` directory tree.
    fn perform_checkpoint(
        &self,
        label: &str,
        scheduler: &mut Scheduler<Box<Host>>,
        current_sim_time_ns: u64,
        window_start: EmulatedTime,
        window_end: EmulatedTime,
    ) -> anyhow::Result<()> {
        // Layout: <data>/checkpoints/<label>/{shmem,criu}/ plus sibling <label>.checkpoint.json
        let checkpoint_base = self.data_path.join("checkpoints").join(label);
        let shmem_backup_dir = checkpoint_base.join("shmem");
        let criu_base_dir = checkpoint_base.join("criu");
        let checkpoint_time =
            EmulatedTime::SIMULATION_START + SimulationTime::from_nanos(current_sim_time_ns);
        let restore_mode = restore_protocol_mode_from_env();
        let checkpoint_started = Instant::now();

        std::fs::create_dir_all(&checkpoint_base)
            .context("Failed to create checkpoint directory")?;

        let phase_started = Instant::now();
        // Freeze the app-visible shim clock to the checkpoint cut before
        // copying the /dev/shm files. Otherwise restore may resurrect a stale
        // timestamp from an older execution slice.
        scheduler.scope(|s| {
            s.run_with_hosts(move |_, hosts| {
                for_each_host(hosts, |host| {
                    host.lock_shmem();
                    let max_runahead_time = worker::Worker::max_event_runahead_time(host);
                    host.set_shim_clock_state(checkpoint_time, max_runahead_time);
                    host.unlock_shmem();
                });
            });
        });
        let freeze_ms = phase_started.elapsed().as_secs_f64() * 1000.0;

        let phase_started = Instant::now();
        // (1) SHMEM: copy MAP_SHARED backing files Shadow maps from /dev/shm into this checkpoint.
        // CRIU restore expects those files to exist; primary list from /proc/self/maps, else scan /dev/shm.
        let shmem_paths = collect_checkpoint_shmem_paths();
        shmem_backup::backup_shmem_files(&shmem_paths, &shmem_backup_dir)?;
        let shmem_ms = phase_started.elapsed().as_secs_f64() * 1000.0;

        let phase_started = Instant::now();
        // (2) SHADOW JSON: walk all hosts under the scheduler and capture in-memory emulator state.
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
        let snapshot_ms = phase_started.elapsed().as_secs_f64() * 1000.0;

        let phase_started = Instant::now();
        audit_checkpoint_event_queue(&host_snapshots, restore_mode)?;
        let audit_ms = phase_started.elapsed().as_secs_f64() * 1000.0;

        let phase_started = Instant::now();
        // (3) CRIU: freeze+dump each native plugin process tree; record image dir in the snapshot
        // so JSON and CRIU dirs stay linked. leave_running=true: dump then resume — simulation
        // keeps going until an explicit restore run.
        checkpoint_running_process_images(&mut host_snapshots, &criu_base_dir)?;
        let criu_ms = phase_started.elapsed().as_secs_f64() * 1000.0;

        let phase_started = Instant::now();
        // (4) ASSEMBLE + SAVE: one SimulationCheckpoint struct (hosts + paths + window/sim time + runahead).
        let worker_shared = worker::WORKER_SHARED.borrow();
        let worker_shared = worker_shared.as_ref().unwrap();
        let restore_protocol =
            build_restore_protocol_snapshot(&host_snapshots, current_sim_time_ns, restore_mode);
        let checkpoint = SimulationCheckpoint {
            version: SimulationCheckpoint::CURRENT_VERSION,
            sim_time_ns: current_sim_time_ns,
            window_start_ns: window_start
                .duration_since(&EmulatedTime::SIMULATION_START)
                .as_nanos() as u64,
            window_end_ns: window_end
                .duration_since(&EmulatedTime::SIMULATION_START)
                .as_nanos() as u64,
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
            restore_protocol,
        };

        // Persist JSON (paths inside point at shmem/ and criu/ under checkpoint_base).
        let store = FilesystemStore::new(self.data_path.join("checkpoints"))?;
        store.save(label, &checkpoint)?;
        let json_ms = phase_started.elapsed().as_secs_f64() * 1000.0;
        let total_ms = checkpoint_started.elapsed().as_secs_f64() * 1000.0;

        log::info!(
            "Checkpoint '{}' saved to {}",
            label,
            checkpoint_base.display()
        );
        log::info!(
            "Checkpoint '{}' phase timings: freeze_ms={:.3} shmem_ms={:.3} snapshot_ms={:.3} audit_ms={:.3} criu_ms={:.3} json_ms={:.3} total_ms={:.3}",
            label,
            freeze_ms,
            shmem_ms,
            snapshot_ms,
            audit_ms,
            criu_ms,
            json_ms,
            total_ms,
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

fn min_emulated_time_option(
    lhs: Option<EmulatedTime>,
    rhs: Option<EmulatedTime>,
) -> Option<EmulatedTime> {
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => Some(std::cmp::min(lhs, rhs)),
        (Some(lhs), None) => Some(lhs),
        (None, Some(rhs)) => Some(rhs),
        (None, None) => None,
    }
}

#[derive(Default)]
struct SchedulerThreadData {
    next_event_time: AtomicRefCell<Option<EmulatedTime>>,
    worker_body_wall_ns: AtomicU64,
    worker_body_continue_receive_wall_ns: AtomicU64,
}

#[derive(Default)]
struct SchedulerPerfStats {
    parallelism: usize,
    windows: AtomicU64,
    host_scans: AtomicU64,
    host_executes: AtomicU64,
    scheduler_scope_wall_ns: AtomicU64,
    host_execute_wall_ns: AtomicU64,
    window_max_worker_body_wall_ns: AtomicU64,
    window_max_worker_body_continue_receive_wall_ns: AtomicU64,
    estimated_async_continue_overlap_savings_ns: AtomicU64,
    packet_events: AtomicU64,
    local_events: AtomicU64,
    cpu_delayed_events: AtomicU64,
    packet_event_wall_ns: AtomicU64,
    local_event_wall_ns: AtomicU64,
    resume_process_events: AtomicU64,
    resume_process_wall_ns: AtomicU64,
    start_application_events: AtomicU64,
    start_application_wall_ns: AtomicU64,
    shutdown_process_events: AtomicU64,
    shutdown_process_wall_ns: AtomicU64,
    relay_forward_events: AtomicU64,
    relay_forward_wall_ns: AtomicU64,
    syscall_condition_wake_events: AtomicU64,
    syscall_condition_wake_wall_ns: AtomicU64,
    prepare_poll_timeout_completion_events: AtomicU64,
    prepare_poll_timeout_completion_wall_ns: AtomicU64,
    restore_blocked_syscall_condition_events: AtomicU64,
    restore_blocked_syscall_condition_wall_ns: AtomicU64,
    timer_expire_events: AtomicU64,
    timer_expire_wall_ns: AtomicU64,
    legacy_tcp_deferred_events: AtomicU64,
    legacy_tcp_deferred_wall_ns: AtomicU64,
    exec_continuation_events: AtomicU64,
    exec_continuation_wall_ns: AtomicU64,
    opaque_events: AtomicU64,
    opaque_wall_ns: AtomicU64,
    undescribed_events: AtomicU64,
    undescribed_wall_ns: AtomicU64,
    worker_bodies: AtomicU64,
    worker_body_wall_ns: AtomicU64,
    max_worker_body_wall_ns: AtomicU64,
    worker_body_continue_receive_wall_ns: AtomicU64,
    max_worker_body_continue_receive_wall_ns: AtomicU64,
    host_execute_by_host: Mutex<HashMap<HostId, HostExecutePerf>>,
    worker_body_details: Mutex<Vec<WorkerBodyPerf>>,
}

#[derive(Clone, Debug, Default)]
struct HostExecutePerf {
    name: String,
    count: u64,
    wall_ns: u64,
    syscall_condition_wake_wall_ns: u64,
}

#[derive(Clone, Debug)]
struct WorkerBodyTopHost {
    host_id: HostId,
    name: String,
    count: u64,
    wall_ns: u64,
    syscall_condition_wake_wall_ns: u64,
}

#[derive(Clone, Debug)]
struct WorkerBodyPerf {
    window_index: u64,
    thread_index: usize,
    host_scans: u64,
    host_executes: u64,
    body_wall_ns: u64,
    continue_receive_wall_ns: u64,
    top_host: Option<WorkerBodyTopHost>,
}

fn scheduler_perf_counters_enabled() -> bool {
    tdt_perf_counters_enabled()
}

fn packet_counters_enabled() -> bool {
    std::env::var_os("SHADOW_PACKET_COUNTERS").is_some()
}

fn packet_route_cache_enabled() -> bool {
    std::env::var_os("SHADOW_PACKET_ROUTE_CACHE").is_some()
}

fn build_packet_route_cache(
    route_endpoints: &HashMap<Ipv4Addr, worker::RouteEndpoint>,
    routing_info: &RoutingInfo<u32>,
) -> HashMap<(Ipv4Addr, Ipv4Addr), worker::PacketRoute> {
    let mut cache = HashMap::with_capacity(route_endpoints.len().pow(2));
    for (src_ip, src_endpoint) in route_endpoints {
        for (dst_ip, dst_endpoint) in route_endpoints {
            let path = routing_info
                .path(src_endpoint.node_id, dst_endpoint.node_id)
                .unwrap();
            cache.insert(
                (*src_ip, *dst_ip),
                worker::PacketRoute::new(
                    dst_endpoint.host_id,
                    src_endpoint.node_id,
                    dst_endpoint.node_id,
                    path,
                ),
            );
        }
    }
    log::info!("Built packet route cache with {} entries", cache.len());
    cache
}

fn log_scheduler_perf_stats(stats: &Option<Arc<SchedulerPerfStats>>) {
    let Some(stats) = stats.as_ref() else {
        return;
    };

    let windows = stats.windows.load(Ordering::Relaxed);
    let host_scans = stats.host_scans.load(Ordering::Relaxed);
    let host_executes = stats.host_executes.load(Ordering::Relaxed);
    let scheduler_scope_wall_ns = stats.scheduler_scope_wall_ns.load(Ordering::Relaxed);
    let host_execute_wall_ns = stats.host_execute_wall_ns.load(Ordering::Relaxed);
    let window_max_worker_body_wall_ns =
        stats.window_max_worker_body_wall_ns.load(Ordering::Relaxed);
    let window_max_worker_body_continue_receive_wall_ns = stats
        .window_max_worker_body_continue_receive_wall_ns
        .load(Ordering::Relaxed);
    let estimated_async_continue_overlap_savings_ns = stats
        .estimated_async_continue_overlap_savings_ns
        .load(Ordering::Relaxed);
    let packet_events = stats.packet_events.load(Ordering::Relaxed);
    let local_events = stats.local_events.load(Ordering::Relaxed);
    let cpu_delayed_events = stats.cpu_delayed_events.load(Ordering::Relaxed);
    let packet_event_wall_ns = stats.packet_event_wall_ns.load(Ordering::Relaxed);
    let local_event_wall_ns = stats.local_event_wall_ns.load(Ordering::Relaxed);
    let resume_process_events = stats.resume_process_events.load(Ordering::Relaxed);
    let resume_process_wall_ns = stats.resume_process_wall_ns.load(Ordering::Relaxed);
    let start_application_events = stats.start_application_events.load(Ordering::Relaxed);
    let start_application_wall_ns = stats.start_application_wall_ns.load(Ordering::Relaxed);
    let shutdown_process_events = stats.shutdown_process_events.load(Ordering::Relaxed);
    let shutdown_process_wall_ns = stats.shutdown_process_wall_ns.load(Ordering::Relaxed);
    let relay_forward_events = stats.relay_forward_events.load(Ordering::Relaxed);
    let relay_forward_wall_ns = stats.relay_forward_wall_ns.load(Ordering::Relaxed);
    let syscall_condition_wake_events = stats.syscall_condition_wake_events.load(Ordering::Relaxed);
    let syscall_condition_wake_wall_ns =
        stats.syscall_condition_wake_wall_ns.load(Ordering::Relaxed);
    let prepare_poll_timeout_completion_events = stats
        .prepare_poll_timeout_completion_events
        .load(Ordering::Relaxed);
    let prepare_poll_timeout_completion_wall_ns = stats
        .prepare_poll_timeout_completion_wall_ns
        .load(Ordering::Relaxed);
    let restore_blocked_syscall_condition_events = stats
        .restore_blocked_syscall_condition_events
        .load(Ordering::Relaxed);
    let restore_blocked_syscall_condition_wall_ns = stats
        .restore_blocked_syscall_condition_wall_ns
        .load(Ordering::Relaxed);
    let timer_expire_events = stats.timer_expire_events.load(Ordering::Relaxed);
    let timer_expire_wall_ns = stats.timer_expire_wall_ns.load(Ordering::Relaxed);
    let legacy_tcp_deferred_events = stats.legacy_tcp_deferred_events.load(Ordering::Relaxed);
    let legacy_tcp_deferred_wall_ns = stats.legacy_tcp_deferred_wall_ns.load(Ordering::Relaxed);
    let exec_continuation_events = stats.exec_continuation_events.load(Ordering::Relaxed);
    let exec_continuation_wall_ns = stats.exec_continuation_wall_ns.load(Ordering::Relaxed);
    let opaque_events = stats.opaque_events.load(Ordering::Relaxed);
    let opaque_wall_ns = stats.opaque_wall_ns.load(Ordering::Relaxed);
    let undescribed_events = stats.undescribed_events.load(Ordering::Relaxed);
    let undescribed_wall_ns = stats.undescribed_wall_ns.load(Ordering::Relaxed);
    let worker_bodies = stats.worker_bodies.load(Ordering::Relaxed);
    let worker_body_wall_ns = stats.worker_body_wall_ns.load(Ordering::Relaxed);
    let max_worker_body_wall_ns = stats.max_worker_body_wall_ns.load(Ordering::Relaxed);
    let worker_body_continue_receive_wall_ns = stats
        .worker_body_continue_receive_wall_ns
        .load(Ordering::Relaxed);
    let max_worker_body_continue_receive_wall_ns = stats
        .max_worker_body_continue_receive_wall_ns
        .load(Ordering::Relaxed);
    let host_scans_per_execute = if host_executes == 0 {
        0.0
    } else {
        host_scans as f64 / host_executes as f64
    };
    let worker_busy_percent = if scheduler_scope_wall_ns == 0 || stats.parallelism == 0 {
        0.0
    } else {
        (host_execute_wall_ns as f64 / (scheduler_scope_wall_ns as f64 * stats.parallelism as f64))
            * 100.0
    };
    let scheduler_scope_over_window_max_percent = if scheduler_scope_wall_ns == 0 {
        0.0
    } else {
        (window_max_worker_body_wall_ns as f64 / scheduler_scope_wall_ns as f64) * 100.0
    };
    let estimated_async_continue_overlap_savings_percent = if scheduler_scope_wall_ns == 0 {
        0.0
    } else {
        (estimated_async_continue_overlap_savings_ns as f64 / scheduler_scope_wall_ns as f64)
            * 100.0
    };
    let avg_worker_body_wall_ns = if worker_bodies == 0 {
        0.0
    } else {
        worker_body_wall_ns as f64 / worker_bodies as f64
    };
    let worker_body_max_to_avg = if avg_worker_body_wall_ns == 0.0 {
        0.0
    } else {
        max_worker_body_wall_ns as f64 / avg_worker_body_wall_ns
    };
    let top_hosts = {
        let mut host_stats: Vec<_> = stats
            .host_execute_by_host
            .lock()
            .unwrap()
            .iter()
            .map(|(host_id, perf)| (*host_id, perf.clone()))
            .collect();
        host_stats.sort_by_key(|(_, perf)| std::cmp::Reverse(perf.wall_ns));
        host_stats
            .into_iter()
            .take(5)
            .map(|(host_id, perf)| {
                format!(
                    "{:?}:name={}:count={}:wall_ms={:.3}:syscall_wake_ms={:.3}",
                    host_id,
                    perf.name,
                    perf.count,
                    perf.wall_ns as f64 / 1_000_000.0,
                    perf.syscall_condition_wake_wall_ns as f64 / 1_000_000.0
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let top_worker_bodies = {
        let mut worker_bodies = stats.worker_body_details.lock().unwrap().clone();
        worker_bodies.sort_by_key(|perf| std::cmp::Reverse(perf.body_wall_ns));
        worker_bodies
            .into_iter()
            .take(8)
            .map(|perf| {
                let continue_receive_pct = if perf.body_wall_ns == 0 {
                    0.0
                } else {
                    (perf.continue_receive_wall_ns as f64 / perf.body_wall_ns as f64) * 100.0
                };
                let host = perf.top_host.map_or_else(
                    || "host=none".to_string(),
                    |host| {
                        format!(
                            "host={:?}:name={}:host_count={}:host_wall_ms={:.3}:host_syscall_wake_ms={:.3}",
                            host.host_id,
                            host.name,
                            host.count,
                            host.wall_ns as f64 / 1_000_000.0,
                            host.syscall_condition_wake_wall_ns as f64 / 1_000_000.0
                        )
                    },
                );
                format!(
                    "window={}:thread={}:body_ms={:.3}:continue_receive_ms={:.3}:continue_receive_pct={:.3}:host_scans={}:host_executes={}:{}",
                    perf.window_index,
                    perf.thread_index,
                    perf.body_wall_ns as f64 / 1_000_000.0,
                    perf.continue_receive_wall_ns as f64 / 1_000_000.0,
                    continue_receive_pct,
                    perf.host_scans,
                    perf.host_executes,
                    host
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    log::info!(
        "TDT scheduler counters: parallelism={} windows={} host_scans={} host_executes={} host_scans_per_execute={:.3} scheduler_scope_wall_ns={} host_execute_wall_ns={} worker_busy_percent={:.3} window_max_worker_body_wall_ns={} scheduler_scope_over_window_max_percent={:.3} window_max_worker_body_continue_receive_wall_ns={} estimated_async_continue_overlap_savings_ns={} estimated_async_continue_overlap_savings_percent={:.3} packet_events={} local_events={} cpu_delayed_events={} packet_event_wall_ns={} local_event_wall_ns={} resume_process_events={} resume_process_wall_ns={} start_application_events={} start_application_wall_ns={} shutdown_process_events={} shutdown_process_wall_ns={} relay_forward_events={} relay_forward_wall_ns={} syscall_condition_wake_events={} syscall_condition_wake_wall_ns={} prepare_poll_timeout_completion_events={} prepare_poll_timeout_completion_wall_ns={} restore_blocked_syscall_condition_events={} restore_blocked_syscall_condition_wall_ns={} timer_expire_events={} timer_expire_wall_ns={} legacy_tcp_deferred_events={} legacy_tcp_deferred_wall_ns={} exec_continuation_events={} exec_continuation_wall_ns={} opaque_events={} opaque_wall_ns={} undescribed_events={} undescribed_wall_ns={} worker_bodies={} worker_body_wall_ns={} avg_worker_body_wall_ns={:.3} max_worker_body_wall_ns={} worker_body_max_to_avg={:.3} worker_body_continue_receive_wall_ns={} max_worker_body_continue_receive_wall_ns={} top_hosts={} top_worker_bodies={}",
        stats.parallelism,
        windows,
        host_scans,
        host_executes,
        host_scans_per_execute,
        scheduler_scope_wall_ns,
        host_execute_wall_ns,
        worker_busy_percent,
        window_max_worker_body_wall_ns,
        scheduler_scope_over_window_max_percent,
        window_max_worker_body_continue_receive_wall_ns,
        estimated_async_continue_overlap_savings_ns,
        estimated_async_continue_overlap_savings_percent,
        packet_events,
        local_events,
        cpu_delayed_events,
        packet_event_wall_ns,
        local_event_wall_ns,
        resume_process_events,
        resume_process_wall_ns,
        start_application_events,
        start_application_wall_ns,
        shutdown_process_events,
        shutdown_process_wall_ns,
        relay_forward_events,
        relay_forward_wall_ns,
        syscall_condition_wake_events,
        syscall_condition_wake_wall_ns,
        prepare_poll_timeout_completion_events,
        prepare_poll_timeout_completion_wall_ns,
        restore_blocked_syscall_condition_events,
        restore_blocked_syscall_condition_wall_ns,
        timer_expire_events,
        timer_expire_wall_ns,
        legacy_tcp_deferred_events,
        legacy_tcp_deferred_wall_ns,
        exec_continuation_events,
        exec_continuation_wall_ns,
        opaque_events,
        opaque_wall_ns,
        undescribed_events,
        undescribed_wall_ns,
        worker_bodies,
        worker_body_wall_ns,
        avg_worker_body_wall_ns,
        max_worker_body_wall_ns,
        worker_body_max_to_avg,
        worker_body_continue_receive_wall_ns,
        max_worker_body_continue_receive_wall_ns,
        top_hosts,
        top_worker_bodies,
    );
}

fn log_network_perf_stats(stats: &Option<Arc<worker::NetworkPerfStats>>) {
    let Some(stats) = stats.as_ref() else {
        return;
    };

    let packet_pushes = stats.packet_pushes.load(Ordering::Relaxed);
    let cross_host_packet_pushes = stats.cross_host_packet_pushes.load(Ordering::Relaxed);
    let event_queue_lock_wait_ns = stats.event_queue_lock_wait_ns.load(Ordering::Relaxed);
    let max_event_queue_lock_wait_ns = stats.max_event_queue_lock_wait_ns.load(Ordering::Relaxed);
    let event_queue_len_after_sum = stats.event_queue_len_after_sum.load(Ordering::Relaxed);
    let max_event_queue_len_after = stats.max_event_queue_len_after.load(Ordering::Relaxed);
    let avg_event_queue_lock_wait_ns = if packet_pushes == 0 {
        0.0
    } else {
        event_queue_lock_wait_ns as f64 / packet_pushes as f64
    };
    let avg_event_queue_len_after = if packet_pushes == 0 {
        0.0
    } else {
        event_queue_len_after_sum as f64 / packet_pushes as f64
    };
    log::info!(
        "TDT network counters: packet_pushes={} cross_host_packet_pushes={} event_queue_lock_wait_ns={} avg_event_queue_lock_wait_ns={:.3} max_event_queue_lock_wait_ns={} avg_event_queue_len_after={:.3} max_event_queue_len_after={}",
        packet_pushes,
        cross_host_packet_pushes,
        event_queue_lock_wait_ns,
        avg_event_queue_lock_wait_ns,
        max_event_queue_lock_wait_ns,
        avg_event_queue_len_after,
        max_event_queue_len_after,
    );
}

fn log_syscall_condition_perf_stats() {
    if !tdt_perf_counters_enabled() {
        return;
    }

    let schedule_attempts = unsafe { c::syscallcondition_perfScheduleAttempts() };
    let scheduled_wakeups = unsafe { c::syscallcondition_perfScheduledWakeups() };
    let skipped_already_scheduled = unsafe { c::syscallcondition_perfSkippedAlreadyScheduled() };
    let trigger_enters = unsafe { c::syscallcondition_perfTriggerEnters() };
    let trigger_continues = unsafe { c::syscallcondition_perfTriggerContinues() };
    let trigger_reblocks = unsafe { c::syscallcondition_perfTriggerReblocks() };
    let trigger_missing_process = unsafe { c::syscallcondition_perfTriggerMissingProcess() };
    let trigger_stopped_process = unsafe { c::syscallcondition_perfTriggerStoppedProcess() };
    let trigger_missing_thread = unsafe { c::syscallcondition_perfTriggerMissingThread() };
    let notify_status_changed = unsafe { c::syscallcondition_perfNotifyStatusChanged() };
    let notify_timeout_expired = unsafe { c::syscallcondition_perfNotifyTimeoutExpired() };
    let signal_wakeups_scheduled = unsafe { c::syscallcondition_perfSignalWakeupsScheduled() };
    let signal_wakeups_blocked = unsafe { c::syscallcondition_perfSignalWakeupsBlocked() };

    log::info!(
        "TDT syscall-condition counters: schedule_attempts={} scheduled_wakeups={} skipped_already_scheduled={} trigger_enters={} trigger_continues={} trigger_reblocks={} trigger_missing_process={} trigger_stopped_process={} trigger_missing_thread={} notify_status_changed={} notify_timeout_expired={} signal_wakeups_scheduled={} signal_wakeups_blocked={}",
        schedule_attempts,
        scheduled_wakeups,
        skipped_already_scheduled,
        trigger_enters,
        trigger_continues,
        trigger_reblocks,
        trigger_missing_process,
        trigger_stopped_process,
        trigger_missing_thread,
        notify_status_changed,
        notify_timeout_expired,
        signal_wakeups_scheduled,
        signal_wakeups_blocked,
    );
}

fn tdt_perf_counters_enabled() -> bool {
    std::env::var("SHADOW_TDT_PERF_COUNTERS")
        .map(|raw| !(raw.trim().is_empty() || raw.trim() == "0"))
        .unwrap_or(false)
}

fn run_control_trace_enabled() -> bool {
    std::env::var("SHADOW_RUN_CONTROL_TRACE")
        .map(|raw| !(raw.trim().is_empty() || raw.trim() == "0"))
        .unwrap_or(false)
}

fn collect_checkpoint_shmem_paths() -> Vec<PathBuf> {
    criu::collect_shadow_shmem_paths().unwrap_or_else(|e| {
        log::warn!("Failed to collect shmem from /proc/self/maps: {e}; falling back");
        shmem_backup::collect_all_shadow_shmem_files().unwrap_or_default()
    })
}

fn checkpoint_running_process_images(
    host_snapshots: &mut [HostCheckpoint],
    criu_base_dir: &PathBuf,
) -> anyhow::Result<()> {
    let mut jobs = Vec::new();
    for (host_index, host_cp) in host_snapshots.iter().enumerate() {
        for (process_index, proc_cp) in host_cp.processes.iter().enumerate() {
            if !proc_cp.is_running {
                continue;
            }
            let images_dir = criu_base_dir.join(format!(
                "host_{}_proc_{}",
                host_cp.host_id, proc_cp.process_id
            ));
            jobs.push(CriuCheckpointJob {
                host_index,
                process_index,
                host_id: host_cp.host_id,
                process_id: proc_cp.process_id,
                native_pid: proc_cp.native_pid,
                images_dir,
            });
        }
    }

    let jobs_per_batch = checkpoint_criu_jobs().min(jobs.len().max(1));
    log::info!(
        "Checkpointing {} running process image(s) with criu_jobs={}",
        jobs.len(),
        jobs_per_batch
    );

    for batch in jobs.chunks(jobs_per_batch) {
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .cloned()
                .map(|job| {
                    scope.spawn(move || {
                        let started = Instant::now();
                        let result =
                            criu::checkpoint_process(job.native_pid, &job.images_dir, true);
                        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                        let image_bytes = result
                            .as_ref()
                            .ok()
                            .and_then(|_| directory_size_bytes(&job.images_dir).ok())
                            .unwrap_or(0);
                        CriuCheckpointResult {
                            job,
                            result,
                            elapsed_ms,
                            image_bytes,
                        }
                    })
                })
                .collect();

            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });

        for result in results {
            log::info!(
                "CRIU dump completed: host_id={} process_id={} pid={} images_dir={} elapsed_ms={:.3} image_bytes={}",
                result.job.host_id,
                result.job.process_id,
                result.job.native_pid,
                result.job.images_dir.display(),
                result.elapsed_ms,
                result.image_bytes,
            );

            result.result?;
            host_snapshots[result.job.host_index].processes[result.job.process_index]
                .criu_image_dir = Some(result.job.images_dir);
        }
    }

    Ok(())
}

#[derive(Clone, Debug)]
struct CriuCheckpointJob {
    host_index: usize,
    process_index: usize,
    host_id: u32,
    process_id: u32,
    native_pid: i32,
    images_dir: PathBuf,
}

#[derive(Debug)]
struct CriuCheckpointResult {
    job: CriuCheckpointJob,
    result: anyhow::Result<()>,
    elapsed_ms: f64,
    image_bytes: u64,
}

fn checkpoint_criu_jobs() -> usize {
    std::env::var("SHADOW_CHECKPOINT_CRIU_JOBS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1)
}

fn directory_size_bytes(path: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            total += directory_size_bytes(&entry.path())?;
        } else if metadata.is_file() {
            total += metadata.len();
        }
    }
    Ok(total)
}

fn task_descriptor_kind_name(desc: &TaskDescriptor) -> &'static str {
    match desc {
        TaskDescriptor::ResumeProcess { .. } => "ResumeProcess",
        TaskDescriptor::StartApplication { .. } => "StartApplication",
        TaskDescriptor::ShutdownProcess { .. } => "ShutdownProcess",
        TaskDescriptor::RelayForward { .. } => "RelayForward",
        TaskDescriptor::SyscallConditionWake { .. } => "SyscallConditionWake",
        TaskDescriptor::PreparePollTimeoutCompletion { .. } => "PreparePollTimeoutCompletion",
        TaskDescriptor::RestoreBlockedSyscallCondition { .. } => "RestoreBlockedSyscallCondition",
        TaskDescriptor::LegacyTcpDeferredAction { .. } => "LegacyTcpDeferredAction",
        TaskDescriptor::TimerExpire { .. } => "TimerExpire",
        TaskDescriptor::ExecContinuation { .. } => "ExecContinuation",
        TaskDescriptor::Opaque { .. } => "Opaque",
    }
}

fn audit_checkpoint_event_queue(
    host_snapshots: &[HostCheckpoint],
    restore_mode: RestoreProtocolModeSnapshot,
) -> anyhow::Result<()> {
    let mut counts = BTreeMap::<&'static str, usize>::new();
    let mut opaque_examples = Vec::new();

    for host in host_snapshots {
        for event in &host.event_queue {
            let EventDataSnapshot::Local(local) = &event.data else {
                continue;
            };
            let kind = task_descriptor_kind_name(&local.task);
            *counts.entry(kind).or_default() += 1;
            if let TaskDescriptor::Opaque { description } = &local.task {
                opaque_examples.push(format!(
                    "host='{}' event_id={} time_ns={} desc={}",
                    host.hostname, local.event_id, event.time_ns, description
                ));
            }
        }
    }

    if !counts.is_empty() {
        let summary = counts
            .iter()
            .map(|(kind, count)| format!("{kind}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        log::info!(
            "checkpoint event audit (mode={:?}): local-task counts: {}",
            restore_mode,
            summary
        );
    }

    if restore_mode == RestoreProtocolModeSnapshot::DeterministicV2 && !opaque_examples.is_empty() {
        let preview = opaque_examples
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .join(" | ");
        anyhow::bail!(
            "deterministic_v2 checkpoint rejected: {} local tasks fell back to Opaque; examples: {}",
            opaque_examples.len(),
            preview
        );
    }

    Ok(())
}

fn snapshot_host(host: &Host) -> HostCheckpoint {
    if restore_protocol_mode_from_env() == RestoreProtocolModeSnapshot::DeterministicV2 {
        log::info!(
            "checkpoint network-container audit host='{}': router_pending={} relay_inet_out_pending={} relay_inet_in_pending={} relay_loopback_pending={}",
            host.name(),
            host.router_pending_packet_count(),
            host.relay_inet_out_pending_packet_count(),
            host.relay_inet_in_pending_packet_count(),
            host.relay_loopback_pending_packet_count(),
        );
    }
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

    let localhost_send_queue = host
        .interface_borrow(Ipv4Addr::LOCALHOST)
        .map(|iface| {
            let state = iface.snapshot_send_socket_queue();
            InterfaceSendQueueCheckpoint {
                next_push_order: state.next_push_order,
                entries: state
                    .entries
                    .into_iter()
                    .map(|entry| InterfaceSendQueueEntryCheckpoint {
                        canonical_handle: entry.canonical_handle,
                        priority: entry.priority,
                        push_order: entry.push_order,
                    })
                    .collect(),
            }
        })
        .unwrap_or_default();
    let internet_send_queue = host
        .interface_borrow(host.default_ip())
        .map(|iface| {
            let state = iface.snapshot_send_socket_queue();
            InterfaceSendQueueCheckpoint {
                next_push_order: state.next_push_order,
                entries: state
                    .entries
                    .into_iter()
                    .map(|entry| InterfaceSendQueueEntryCheckpoint {
                        canonical_handle: entry.canonical_handle,
                        priority: entry.priority,
                        push_order: entry.push_order,
                    })
                    .collect(),
            }
        })
        .unwrap_or_default();

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
                Worker::set_active_process(proc_rc);
                let snapshot = snapshot_process(host, &proc);
                Worker::clear_active_process();
                snapshot
            })
            .collect(),
        host_shmem_handle: host.shim_shmem().serialize().to_string(),
        localhost_send_queue,
        internet_send_queue,
    }
}

fn snapshot_process(host: &Host, process: &crate::host::process::Process) -> ProcessCheckpoint {
    let restore_mode = restore_protocol_mode_from_env();
    let restore_epoch = host
        .cpu_borrow()
        .snapshot_times()
        .0
        .saturating_duration_since(&EmulatedTime::SIMULATION_START)
        .as_nanos() as u64;
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
                    let mut runtime = thread.mthread().runtime_snapshot();
                    runtime.restore_policy = thread_restore_policy_from_mode(restore_mode);
                    runtime.restore_epoch = restore_epoch;
                    runtime.blocked_timeout_ns = thread
                        .syscall_condition()
                        .and_then(|cond| cond.timeout())
                        .map(|timeout| {
                            timeout
                                .saturating_duration_since(
                                    &shadow_shim_helper_rs::emulated_time::EmulatedTime::SIMULATION_START,
                                )
                                .as_nanos() as u64
                        });
                    if let Some(cond) = thread.syscall_condition() {
                        let descriptor_table = thread.descriptor_table_borrow(host);
                        let resolve_handle_to_fd = |target_handle: usize| {
                            descriptor_table.iter().find_map(|(fd, desc)| {
                                let crate::host::descriptor::CompatFile::New(open_file) = desc.file() else {
                                    return None;
                                };
                                (open_file.inner_file().canonical_handle() == target_handle)
                                    .then_some(u32::from(*fd))
                            })
                        };
                        runtime.blocked_trigger_kind = Some(cond.trigger_kind());
                        runtime.blocked_trigger_fd = cond
                            .trigger_file_canonical_handle()
                            .and_then(resolve_handle_to_fd);
                        runtime.blocked_trigger_state_bits =
                            cond.trigger_state().map(|state| state.bits());
                        runtime.blocked_active_file_fd = cond
                            .active_file_canonical_handle()
                            .and_then(resolve_handle_to_fd);
                        runtime.blocked_listener_sequence_value =
                            cond.trigger_listener_sequence_value();
                    }
                    if runtime.blocked_syscall_active
                        && let Some(syscall_args) = thread.mthread().current_syscall_args()
                    {
                        if syscall_args.number == libc::SYS_futex {
                            runtime.blocked_futex_word =
                                Some(u64::try_from(usize::from(syscall_args.args[0])).unwrap());
                        }
                        runtime.poll_watches = snapshot_poll_watches(process, syscall_args);
                        runtime.blocked_syscall_phase = BlockedSyscallPhaseSnapshot::Waiting;
                        runtime.blocked_syscall_instance_id = Some(make_blocked_syscall_instance_id(
                            u32::from(process.id()),
                            u32::try_from(libc::pid_t::from(thread.id())).unwrap_or_default(),
                            runtime.blocked_syscall_nr,
                            restore_epoch,
                        ));
                    }
                    if runtime.blocked_syscall_active && runtime.blocked_syscall_instance_id.is_none() {
                        runtime.blocked_syscall_instance_id = Some(make_blocked_syscall_instance_id(
                            u32::from(process.id()),
                            u32::try_from(libc::pid_t::from(thread.id())).unwrap_or_default(),
                            runtime.blocked_syscall_nr,
                            restore_epoch,
                        ));
                    }
                    runtime.pending_result = thread
                        .syscallhandler_borrow(host)
                        .pending_result_snapshot();
                    if runtime.pending_result.is_none()
                        && runtime.blocked_trigger_kind
                            == Some(crate::core::checkpoint::snapshot_types::BlockedTriggerKindSnapshot::Futex)
                        && restore_mode != RestoreProtocolModeSnapshot::DeterministicV2
                    {
                        runtime.pending_result = Some(
                            crate::core::checkpoint::snapshot_types::PendingSyscallResultSnapshot::Done {
                                retval_raw: 0,
                            },
                        );
                    }
                    runtime.blocked_restore_action =
                        blocked_restore_action_for_runtime(&runtime);
                    ThreadCheckpoint {
                        thread_id: u32::try_from(libc::pid_t::from(thread.id())).unwrap(),
                        native_tid: thread.native_tid().as_raw_nonzero().get(),
                        ipc_shmem_handle: thread.mthread().ipc_shmem_handle(),
                        thread_shmem_handle: thread.shmem().serialize().to_string(),
                        current_event_bytes: thread.mthread().current_event_bytes(),
                        runtime: Some(runtime),
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let dumpable = runnable
        .as_ref()
        .map(|_| process.dumpable().val())
        .unwrap_or(linux_api::sched::SuidDump::SUID_DUMP_USER.val());

    let descriptor_count_hint = runnable
        .as_ref()
        .map(|_| process.descriptor_count_hint(host) as u32)
        .unwrap_or(0);
    let descriptors = runnable
        .as_ref()
        .map(|_| process.snapshot_descriptor_entries(host))
        .unwrap_or_default();
    log_descriptor_census(
        "checkpoint",
        host.name(),
        u32::from(process.id()),
        &descriptors,
    );

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
        descriptor_count_hint,
        descriptors,
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

fn restore_host_globals(host: &Host, checkpoint: &HostCheckpoint) {
    let queue = rebuild_event_queue(&checkpoint.event_queue, host);
    let last_popped = EmulatedTime::SIMULATION_START
        + SimulationTime::from_nanos(checkpoint.last_popped_event_time_ns);

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
}

fn build_live_inet_socket_map(
    host: &Host,
) -> HashMap<u64, crate::host::descriptor::socket::inet::InetSocket> {
    let mut sockets = HashMap::new();
    let processes = host.processes_borrow();
    for process_rc in processes.values() {
        let process = process_rc.borrow(host.root());
        let Some(thread_rc) = process.first_live_thread_borrow(host.root()) else {
            continue;
        };
        let thread = thread_rc.borrow(host.root());
        let table = thread.descriptor_table_borrow(host);
        for (_, descriptor) in table.iter() {
            let crate::host::descriptor::CompatFile::New(open_file) = descriptor.file() else {
                continue;
            };
            let crate::host::descriptor::File::Socket(
                crate::host::descriptor::socket::Socket::Inet(inet_socket),
            ) = open_file.inner_file()
            else {
                continue;
            };
            sockets
                .entry(inet_socket.canonical_handle() as u64)
                .or_insert_with(|| inet_socket.clone());
        }
    }
    sockets
}

fn restore_interface_send_queues(host: &Host, checkpoint: &HostCheckpoint) -> anyhow::Result<()> {
    let mut sockets_by_handle = build_live_inet_socket_map(host);
    let translated: Vec<_> = sockets_by_handle
        .iter()
        .map(|(live_handle, socket)| (*live_handle, socket.clone()))
        .collect();
    for (live_handle, socket) in translated {
        sockets_by_handle.entry(live_handle).or_insert(socket);
    }

    let restore_one = |iface: &crate::host::network::interface::NetworkInterface,
                       snapshot: &InterfaceSendQueueCheckpoint|
     -> anyhow::Result<()> {
        let translated_entries = crate::host::network::interface::SendSocketQueueState {
            kind: iface.snapshot_send_socket_queue().kind,
            next_push_order: snapshot.next_push_order,
            entries: snapshot
                .entries
                .iter()
                .map(
                    |entry| crate::host::network::interface::SendSocketQueueEntry {
                        canonical_handle: host
                            .translate_restored_canonical_handle(entry.canonical_handle)
                            .unwrap_or(entry.canonical_handle),
                        priority: entry.priority,
                        push_order: entry.push_order,
                    },
                )
                .collect(),
        };
        iface.restore_send_socket_queue(&translated_entries, &sockets_by_handle)
            .map_err(|missing| {
                anyhow::anyhow!(
                    "restore: missing inet sockets for interface send queue on host '{}' handles={missing:?}",
                    host.name()
                )
            })
    };

    if let Some(localhost_iface) = host.interface_borrow(Ipv4Addr::LOCALHOST) {
        restore_one(&localhost_iface, &checkpoint.localhost_send_queue)?;
    }
    if let Some(internet_iface) = host.interface_borrow(host.default_ip()) {
        restore_one(&internet_iface, &checkpoint.internet_send_queue)?;
    }

    Ok(())
}

fn descriptor_census_enabled() -> bool {
    std::env::var("SHADOW_LOG_DESCRIPTOR_CENSUS")
        .map(|x| x == "1")
        .unwrap_or(false)
}

fn log_descriptor_census(
    stage: &str,
    host_name: &str,
    process_id: u32,
    descriptors: &[DescriptorEntrySnapshot],
) {
    if !descriptor_census_enabled() {
        return;
    }

    let mut by_kind: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    for descriptor in descriptors {
        let label = match descriptor.file_kind {
            DescriptorFileKind::Legacy => "legacy",
            DescriptorFileKind::Pipe => "pipe",
            DescriptorFileKind::EventFd => "eventfd",
            DescriptorFileKind::Socket => "socket",
            DescriptorFileKind::TimerFd => "timerfd",
            DescriptorFileKind::Epoll => "epoll",
            DescriptorFileKind::Unknown => "unknown",
        };
        *by_kind.entry(label).or_default() += 1;
    }

    log::info!(
        "descriptor census stage={} host='{}' pid={} total={} by_kind={:?}",
        stage,
        host_name,
        process_id,
        descriptors.len(),
        by_kind
    );
}

fn restore_protocol_mode_from_env() -> RestoreProtocolModeSnapshot {
    let raw = std::env::var("SHADOW_RESTORE_PROTOCOL_MODE")
        .ok()
        .unwrap_or_else(|| "protocol_v1".to_string());
    match raw.trim().to_ascii_lowercase().as_str() {
        "legacy" | "legacy_heuristic" => RestoreProtocolModeSnapshot::LegacyHeuristic,
        "deterministic" | "deterministic_v2" | "strict_deterministic" => {
            RestoreProtocolModeSnapshot::DeterministicV2
        }
        _ => RestoreProtocolModeSnapshot::ProtocolV1,
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).map(|x| x == "1").unwrap_or(false)
}

fn socket_fixup_enabled(restore_protocol_mode: RestoreProtocolModeSnapshot) -> bool {
    let force_disable_socket_fixup = env_flag("SHADOW_DISABLE_POST_RESTORE_SOCKET_FIXUP");
    let force_enable_socket_fixup = env_flag("SHADOW_ENABLE_POST_RESTORE_SOCKET_FIXUP");
    if force_disable_socket_fixup {
        false
    } else if force_enable_socket_fixup {
        true
    } else {
        restore_protocol_mode == RestoreProtocolModeSnapshot::LegacyHeuristic
    }
}

fn deterministic_restore_enabled(restore_protocol_mode: RestoreProtocolModeSnapshot) -> bool {
    restore_protocol_mode == RestoreProtocolModeSnapshot::DeterministicV2
        || env_flag("SHADOW_STRICT_DETERMINISTIC_RESTORE")
}

fn thread_restore_policy_from_mode(
    mode: RestoreProtocolModeSnapshot,
) -> ThreadRestorePolicySnapshot {
    match mode {
        RestoreProtocolModeSnapshot::LegacyHeuristic => {
            ThreadRestorePolicySnapshot::LegacyHeuristic
        }
        RestoreProtocolModeSnapshot::ProtocolV1 => ThreadRestorePolicySnapshot::ProtocolV1,
        RestoreProtocolModeSnapshot::DeterministicV2 => {
            ThreadRestorePolicySnapshot::DeterministicV2
        }
    }
}

fn make_blocked_syscall_instance_id(
    process_id: u32,
    thread_id: u32,
    syscall_nr: Option<i64>,
    restore_epoch: u64,
) -> u64 {
    let nr = syscall_nr.unwrap_or_default() as u64;
    restore_epoch.rotate_left(19)
        ^ (u64::from(process_id) << 32)
        ^ u64::from(thread_id)
        ^ nr.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

fn blocked_restore_action_for_runtime(
    runtime: &ThreadRuntimeSnapshot,
) -> BlockedSyscallRestoreActionSnapshot {
    if runtime.pending_result.is_some() {
        return BlockedSyscallRestoreActionSnapshot::ResumeImmediately;
    }
    if !runtime.blocked_syscall_active {
        return BlockedSyscallRestoreActionSnapshot::None;
    }
    if runtime.blocked_trigger_kind == Some(BlockedTriggerKindSnapshot::Futex)
        && runtime.blocked_futex_word.is_some()
    {
        return BlockedSyscallRestoreActionSnapshot::RearmFutex;
    }
    if !runtime.poll_watches.is_empty() {
        return BlockedSyscallRestoreActionSnapshot::RearmPoll;
    }
    if runtime.blocked_trigger_fd.is_some() || runtime.blocked_active_file_fd.is_some() {
        return BlockedSyscallRestoreActionSnapshot::RearmCondition;
    }
    if runtime.blocked_timeout_ns.is_some() {
        return BlockedSyscallRestoreActionSnapshot::RearmTimeout;
    }
    BlockedSyscallRestoreActionSnapshot::ResumeImmediately
}

fn connection_role_from_descriptor(
    desc: &DescriptorEntrySnapshot,
) -> ConnectionProtocolRoleSnapshot {
    if desc.socket_is_listening {
        ConnectionProtocolRoleSnapshot::Listener
    } else if desc.socket_peer_ip.is_some() && desc.socket_peer_port.is_some() {
        ConnectionProtocolRoleSnapshot::Connected
    } else {
        ConnectionProtocolRoleSnapshot::Unconnected
    }
}

fn make_connection_protocol_id(
    host_id: u32,
    process_id: u32,
    desc: &DescriptorEntrySnapshot,
) -> u64 {
    let key = desc.canonical_handle.unwrap_or(u64::from(desc.fd));
    (u64::from(host_id) << 48) ^ (u64::from(process_id) << 16) ^ key.rotate_left(7)
}

fn build_restore_protocol_snapshot(
    hosts: &[HostCheckpoint],
    restore_epoch: u64,
    mode: RestoreProtocolModeSnapshot,
) -> RestoreProtocolSnapshot {
    let mut connections = Vec::new();
    let mut blocked_syscalls = Vec::new();

    for host in hosts {
        for process in &host.processes {
            for desc in &process.descriptors {
                let Some(transport) = desc.socket_transport.clone() else {
                    continue;
                };
                connections.push(ConnectionProtocolSnapshot {
                    connection_id: make_connection_protocol_id(
                        host.host_id,
                        process.process_id,
                        desc,
                    ),
                    host_id: host.host_id,
                    process_id: process.process_id,
                    fd: desc.fd,
                    canonical_handle: desc.canonical_handle,
                    role: connection_role_from_descriptor(desc),
                    transport,
                    implementation: desc.socket_implementation.clone(),
                    local_ip: desc.socket_local_ip.clone(),
                    local_port: desc.socket_local_port,
                    peer_ip: desc.socket_peer_ip.clone(),
                    peer_port: desc.socket_peer_port,
                    is_listening: desc.socket_is_listening,
                });
            }

            for thread in &process.threads {
                let Some(runtime) = thread.runtime.as_ref() else {
                    continue;
                };
                if !runtime.blocked_syscall_active {
                    continue;
                }
                let Some(syscall_nr) = runtime.blocked_syscall_nr else {
                    continue;
                };
                let instance_id = runtime.blocked_syscall_instance_id.unwrap_or_else(|| {
                    make_blocked_syscall_instance_id(
                        process.process_id,
                        thread.thread_id,
                        runtime.blocked_syscall_nr,
                        restore_epoch,
                    )
                });
                blocked_syscalls.push(BlockedSyscallProtocolSnapshot {
                    host_id: host.host_id,
                    process_id: process.process_id,
                    thread_id: thread.thread_id,
                    syscall_nr,
                    instance_id,
                    phase: runtime.blocked_syscall_phase,
                    action: runtime.blocked_restore_action,
                    timeout_ns: runtime.blocked_timeout_ns,
                    poll_watches: runtime.poll_watches.clone(),
                });
            }
        }
    }

    RestoreProtocolSnapshot {
        mode,
        restore_epoch,
        connections,
        blocked_syscalls,
    }
}

fn read_process_object<T: Copy>(process: &Process, ptr: UntypedForeignPtr) -> Option<T> {
    if ptr.is_null() {
        return None;
    }
    let mut value = unsafe { std::mem::zeroed::<T>() };
    let rv = crate::host::process::export::process_readPtr(
        process,
        std::ptr::from_mut(&mut value).cast(),
        ptr,
        std::mem::size_of::<T>(),
    );
    (rv == 0).then_some(value)
}

fn read_process_array<T: Copy>(
    process: &Process,
    ptr: UntypedForeignPtr,
    len: usize,
) -> Option<Vec<T>> {
    if ptr.is_null() {
        return None;
    }
    let mut values = vec![unsafe { std::mem::zeroed::<T>() }; len];
    let rv = crate::host::process::export::process_readPtr(
        process,
        values.as_mut_ptr().cast(),
        ptr,
        len.saturating_mul(std::mem::size_of::<T>()),
    );
    (rv == 0).then_some(values)
}

fn snapshot_poll_watches(process: &Process, syscall_args: SyscallArgs) -> Vec<PollWatchSnapshot> {
    match syscall_args.number {
        x if x == libc::SYS_poll || x == libc::SYS_ppoll => {
            let nfds = usize::from(syscall_args.args[1]);
            let fds_ptr = UntypedForeignPtr::from(usize::from(syscall_args.args[0]));
            let Some(pfds) = read_process_array::<libc::pollfd>(process, fds_ptr, nfds) else {
                return Vec::new();
            };
            pfds.into_iter()
                .filter_map(|pfd| {
                    if pfd.fd < 0 {
                        return None;
                    }
                    let mut epoll_events = 0u32;
                    if (pfd.events & libc::POLLIN) != 0 {
                        epoll_events |= libc::EPOLLIN as u32;
                    }
                    if (pfd.events & libc::POLLOUT) != 0 {
                        epoll_events |= libc::EPOLLOUT as u32;
                    }
                    (epoll_events != 0).then_some(PollWatchSnapshot {
                        fd: u32::try_from(pfd.fd).ok()?,
                        epoll_events,
                    })
                })
                .collect()
        }
        x if x == libc::SYS_select || x == libc::SYS_pselect6 => {
            let nfds = isize::from(syscall_args.args[0]) as i32;
            let nfds_max = nfds.clamp(0, libc::FD_SETSIZE as i32);
            let readfds = read_process_object::<libc::fd_set>(
                process,
                UntypedForeignPtr::from(usize::from(syscall_args.args[1])),
            );
            let writefds = read_process_object::<libc::fd_set>(
                process,
                UntypedForeignPtr::from(usize::from(syscall_args.args[2])),
            );

            (0..nfds_max)
                .filter_map(|fd| {
                    let mut epoll_events = 0u32;
                    if let Some(readfds) = readfds.as_ref()
                        && unsafe { libc::FD_ISSET(fd, readfds as *const _ as *mut _) }
                    {
                        epoll_events |= libc::EPOLLIN as u32;
                    }
                    if let Some(writefds) = writefds.as_ref()
                        && unsafe { libc::FD_ISSET(fd, writefds as *const _ as *mut _) }
                    {
                        epoll_events |= libc::EPOLLOUT as u32;
                    }
                    (epoll_events != 0).then_some(PollWatchSnapshot {
                        fd: u32::try_from(fd).ok()?,
                        epoll_events,
                    })
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

fn apply_host_checkpoint(
    host: &Host,
    checkpoint: &HostCheckpoint,
    restore_protocol_mode: RestoreProtocolModeSnapshot,
    checkpoint_time: EmulatedTime,
) -> anyhow::Result<()> {
    fn schedule_resume_process_task(
        host: &Host,
        process_id: crate::host::process::ProcessId,
        tid: crate::host::thread::ThreadId,
        when: EmulatedTime,
    ) {
        let task = TaskRef::new_with_result_and_descriptor(
            move |host| host.resume(process_id, tid),
            TaskDescriptor::ResumeProcess {
                process_id: u32::from(process_id),
                thread_id: u32::try_from(libc::pid_t::from(tid)).unwrap_or_default(),
            },
        );
        host.schedule_task_at_emulated_time(task, when);
    }

    #[derive(Copy, Clone, Debug)]
    enum RestoreTcpSocketRole {
        Listener,
        ConnectedPeer,
        Unconnected,
    }

    if !checkpoint.host_shmem_handle.is_empty() {
        let serialized =
            shadow_shmem::allocator::ShMemBlockSerialized::from_str(&checkpoint.host_shmem_handle)
                .context("Failed to parse restored host shmem handle")?;
        let restored_shim_shmem = unsafe {
            shadow_shmem::allocator::shdeserialize::<shadow_shim_helper_rs::shim_shmem::HostShmem>(
                &serialized,
            )
        };
        host.attach_restored_shim_shmem(restored_shim_shmem);
    }
    host.clear_restored_canonical_handle_aliases();
    host.set_shim_clock_state(checkpoint_time, checkpoint_time);

    // Reinstall the serialized queue and host-global counters before rebuilding
    // process-local objects. Descriptor restore can schedule fresh local tasks
    // (for example timerfd expirations), and those tasks must be appended to the
    // restored queue rather than discarded by a later replace_event_queue call.
    restore_host_globals(host, checkpoint);

    {
        let mut processes = host.processes_borrow_mut();
        for process_cp in &checkpoint.processes {
            if !process_cp.is_running {
                continue;
            }
            let process = Process::from_checkpoint(host, process_cp, None).map_err(|e| {
                anyhow::anyhow!(
                    "Process::from_checkpoint failed for pid {}: {:?}",
                    process_cp.process_id,
                    e
                )
            })?;
            let process_id = process.borrow(host.root()).id();
            processes.insert(process_id, process);
        }
    }
    restore_interface_send_queues(host, checkpoint)?;
    let restored_now =
        EmulatedTime::SIMULATION_START + SimulationTime::from_nanos(checkpoint.cpu_now_ns);
    // Kick restored runnable processes once after replay. The serialized event
    // queue may not include resume tasks for already-running workloads.
    let replay_time =
        EmulatedTime::SIMULATION_START + SimulationTime::from_nanos(checkpoint.cpu_now_ns);
    let deterministic_restore = deterministic_restore_enabled(restore_protocol_mode);
    for process_cp in &checkpoint.processes {
        let Ok(process_id) = crate::host::process::ProcessId::try_from(process_cp.process_id)
        else {
            continue;
        };
        for thread_cp in &process_cp.threads {
            let Some(runtime) = thread_cp.runtime.as_ref() else {
                continue;
            };
            if matches!(
                restore_protocol_mode,
                RestoreProtocolModeSnapshot::ProtocolV1
                    | RestoreProtocolModeSnapshot::DeterministicV2
            ) && runtime.blocked_syscall_active
                && runtime.blocked_syscall_instance_id.is_none()
            {
                log::warn!(
                    "restore: skipping blocked-syscall replay without instance_id in protocol_v1 mode (host='{}' pid={} tid={})",
                    host.name(),
                    process_cp.process_id,
                    thread_cp.thread_id
                );
                continue;
            }
            let Ok(tid_raw) = libc::pid_t::try_from(thread_cp.thread_id) else {
                continue;
            };
            let tid = crate::host::thread::ThreadId::try_from(tid_raw).unwrap();
            let abs_timeout = runtime.blocked_timeout_ns.map(|timeout_ns| {
                EmulatedTime::SIMULATION_START + SimulationTime::from_nanos(timeout_ns)
            });
            let trigger_fd = runtime.blocked_trigger_fd;
            let trigger_state_bits = runtime.blocked_trigger_state_bits;
            let active_file_fd = runtime.blocked_active_file_fd;
            let blocked_futex_word = runtime.blocked_futex_word;
            let blocked_listener_sequence_value = runtime.blocked_listener_sequence_value;
            let poll_watches = runtime.poll_watches.clone();
            let blocked_restore_action = runtime.blocked_restore_action;
            let blocked_trigger_kind = runtime.blocked_trigger_kind;
            let epoll_triggered_rearm = trigger_fd.is_some_and(|trigger_fd| {
                process_cp.descriptors.iter().any(|descriptor| {
                    descriptor.fd == trigger_fd
                        && descriptor.file_kind
                            == crate::core::checkpoint::snapshot_types::DescriptorFileKind::Epoll
                })
            });
            if blocked_restore_action == BlockedSyscallRestoreActionSnapshot::None {
                continue;
            }
            if blocked_restore_action != BlockedSyscallRestoreActionSnapshot::ResumeImmediately
                && abs_timeout.is_some_and(|timeout| timeout < restored_now)
            {
                continue;
            }
            if blocked_restore_action == BlockedSyscallRestoreActionSnapshot::ResumeImmediately {
                schedule_resume_process_task(
                    host,
                    process_id,
                    tid,
                    replay_time + SimulationTime::NANOSECOND,
                );
                if !deterministic_restore
                    && blocked_trigger_kind
                    == Some(crate::core::checkpoint::snapshot_types::BlockedTriggerKindSnapshot::Futex)
                {
                    for nudge_i in 0u64..5 {
                        schedule_resume_process_task(
                            host,
                            process_id,
                            tid,
                            replay_time + SimulationTime::from_millis(1 + (25 * nudge_i)),
                        );
                    }
                }
                continue;
            }
            if let (Some(abs_timeout), Some(blocked_syscall_nr)) =
                (abs_timeout, runtime.blocked_syscall_nr)
                && !poll_watches.is_empty()
            {
                let prepare_task = TaskRef::new_with_descriptor(
                    move |host| {
                        let Some(process_rc) = host.process_borrow(process_id) else {
                            return;
                        };
                        let process = process_rc.borrow(host.root());
                        let Some(thread_rc) = process.thread_borrow(tid) else {
                            return;
                        };
                        let thread = thread_rc.borrow(host.root());
                        thread
                            .syscallhandler_borrow_mut(host)
                            .prepare_restored_poll_timeout_completion(blocked_syscall_nr);
                    },
                    TaskDescriptor::PreparePollTimeoutCompletion {
                        process_id: u32::from(process_id),
                        thread_id: u32::try_from(libc::pid_t::from(tid)).unwrap_or_default(),
                        syscall_nr: blocked_syscall_nr,
                    },
                );
                host.schedule_task_at_emulated_time(prepare_task, abs_timeout);
            }
            let task = TaskRef::new_with_descriptor(
                move |host| {
                    let Some(process_rc) = host.process_borrow(process_id) else {
                        return;
                    };
                    Worker::set_active_process(&process_rc);
                    let process = process_rc.borrow(host.root());
                    let Some(thread_rc) = process.thread_borrow(tid) else {
                        Worker::clear_active_process();
                        return;
                    };
                    let thread = thread_rc.borrow(host.root());
                    match blocked_restore_action {
                        BlockedSyscallRestoreActionSnapshot::RearmPoll => {
                            thread.restore_poll_blocked_syscall_condition(
                                host,
                                &process,
                                &poll_watches,
                                abs_timeout,
                                None,
                            );
                            Worker::clear_active_process();
                            return;
                        }
                        BlockedSyscallRestoreActionSnapshot::RearmTimeout => {
                            if let Some(abs_timeout) = abs_timeout {
                                thread.restore_timeout_syscall_condition(
                                    host,
                                    &process,
                                    abs_timeout,
                                );
                            }
                            Worker::clear_active_process();
                            return;
                        }
                        BlockedSyscallRestoreActionSnapshot::RearmFutex => {
                            if let Some(futex_word) = blocked_futex_word {
                                thread.restore_futex_blocked_syscall_condition(
                                    host,
                                    &process,
                                    futex_word,
                                    abs_timeout,
                                    blocked_listener_sequence_value,
                                );
                            }
                            Worker::clear_active_process();
                            return;
                        }
                        BlockedSyscallRestoreActionSnapshot::RearmCondition => {}
                        BlockedSyscallRestoreActionSnapshot::ResumeImmediately
                        | BlockedSyscallRestoreActionSnapshot::None => {
                            Worker::clear_active_process();
                            return;
                        }
                    }
                    let descriptor_table = thread.descriptor_table_borrow(host);
                    let trigger_file = trigger_fd.and_then(|trigger_fd| {
                        let fd =
                            crate::host::descriptor::descriptor_table::DescriptorHandle::try_from(
                                trigger_fd,
                            )
                            .ok()?;
                        let desc = descriptor_table.get(fd)?;
                        let crate::host::descriptor::CompatFile::New(open_file) = desc.file()
                        else {
                            return None;
                        };
                        Some(open_file.inner_file().clone())
                    });
                    let active_file = active_file_fd.and_then(|active_file_fd| {
                        let fd =
                            crate::host::descriptor::descriptor_table::DescriptorHandle::try_from(
                                active_file_fd,
                            )
                            .ok()?;
                        let desc = descriptor_table.get(fd)?;
                        let crate::host::descriptor::CompatFile::New(open_file) = desc.file()
                        else {
                            return None;
                        };
                        Some(open_file.clone())
                    });
                    drop(descriptor_table);
                    let trigger_state = trigger_state_bits
                        .map(crate::host::descriptor::FileState::from_bits_truncate);
                    thread.restore_blocked_syscall_condition(
                        host,
                        &process,
                        trigger_file,
                        trigger_state,
                        active_file,
                        abs_timeout,
                    );
                    Worker::clear_active_process();
                },
                TaskDescriptor::RestoreBlockedSyscallCondition {
                    process_id: u32::from(process_id),
                    thread_id: u32::try_from(libc::pid_t::from(tid)).unwrap_or_default(),
                },
            );
            host.schedule_task_at_emulated_time(task, replay_time + SimulationTime::NANOSECOND);
            if !deterministic_restore
                && blocked_restore_action == BlockedSyscallRestoreActionSnapshot::RearmCondition
                && epoll_triggered_rearm
            {
                for nudge_i in 0u64..5 {
                    schedule_resume_process_task(
                        host,
                        process_id,
                        tid,
                        replay_time + SimulationTime::from_millis(1 + (25 * nudge_i)),
                    );
                }
            }
        }
    }
    let strict_runtime_restore = env_flag("SHADOW_STRICT_RUNTIME_RESTORE");
    let force_disable_socket_fixup = env_flag("SHADOW_DISABLE_POST_RESTORE_SOCKET_FIXUP");
    let force_enable_socket_fixup = env_flag("SHADOW_ENABLE_POST_RESTORE_SOCKET_FIXUP");
    let enable_socket_fixup = socket_fixup_enabled(restore_protocol_mode);
    if !enable_socket_fixup {
        log::debug!(
            "restore: post_restore_socket_fixup disabled (mode={:?}, force_disable={}, force_enable={})",
            restore_protocol_mode,
            force_disable_socket_fixup,
            force_enable_socket_fixup
        );
    }
    // Compatibility fixup path. While object-level runtime snapshots are still evolving,
    // protocol_v1 defaults this path OFF; use env flags to force on/off while migrating.
    if enable_socket_fixup {
        for process_cp in &checkpoint.processes {
            if !process_cp.is_running {
                continue;
            }
            let Ok(process_id) = crate::host::process::ProcessId::try_from(process_cp.process_id)
            else {
                continue;
            };
            for d in &process_cp.descriptors {
                if strict_runtime_restore && d.socket_runtime.is_some() {
                    // In strict runtime-restore mode, skip heuristic fixups when runtime snapshot exists.
                    continue;
                }
                let transport = d.socket_transport.clone();
                let Some(transport) = transport else {
                    continue;
                };
                let Ok(fd) =
                    crate::host::descriptor::descriptor_table::DescriptorHandle::try_from(d.fd)
                else {
                    continue;
                };
                let pid_for_log = process_cp.process_id;
                let fd_for_log = d.fd;
                let socket_impl = d.socket_implementation.clone();
                let socket_peer_ip = d.socket_peer_ip.clone();
                let socket_peer_port = d.socket_peer_port;
                let socket_is_listening = d.socket_is_listening
                    || (d.socket_peer_ip.is_none()
                        && d.socket_peer_port.is_none()
                        && d.socket_local_port.is_some()
                        && matches!(
                            transport,
                            crate::core::checkpoint::snapshot_types::DescriptorSocketTransport::Tcp
                        ));
                let tcp_role = if matches!(
                    transport,
                    crate::core::checkpoint::snapshot_types::DescriptorSocketTransport::Tcp
                ) {
                    if socket_is_listening {
                        Some(RestoreTcpSocketRole::Listener)
                    } else if socket_peer_ip.is_some() && socket_peer_port.is_some() {
                        Some(RestoreTcpSocketRole::ConnectedPeer)
                    } else {
                        Some(RestoreTcpSocketRole::Unconnected)
                    }
                } else {
                    None
                };
                let task = TaskRef::new_with_descriptor(
                    move |host| {
                        log::debug!(
                            "post_restore_socket_fixup start host='{}' pid={} fd={} transport={:?} role={:?}",
                            host.name(),
                            pid_for_log,
                            fd_for_log,
                            transport,
                            tcp_role
                        );
                        let processes = host.processes_borrow();
                        let Some(proc_rc) = processes.get(&process_id) else {
                            log::warn!(
                                "post_restore_socket_fixup missing process pid={}",
                                pid_for_log
                            );
                            return;
                        };
                        Worker::set_active_process(proc_rc);
                        let proc = proc_rc.borrow(host.root());
                        let Some(thread_rc) = proc.first_live_thread_borrow(host.root()) else {
                            Worker::clear_active_process();
                            log::warn!(
                                "post_restore_socket_fixup missing live thread pid={}",
                                pid_for_log
                            );
                            return;
                        };
                        let thread = thread_rc.borrow(host.root());
                        let mut table = thread.descriptor_table_borrow_mut(host);
                        let Some(desc) = table.get_mut(fd) else {
                            Worker::clear_active_process();
                            log::warn!(
                                "post_restore_socket_fixup missing descriptor pid={} fd={}",
                                pid_for_log,
                                fd_for_log
                            );
                            return;
                        };
                        let crate::host::descriptor::CompatFile::New(open_file) = desc.file()
                        else {
                            Worker::clear_active_process();
                            return;
                        };
                        let crate::host::descriptor::File::Socket(socket) = open_file.inner_file()
                        else {
                            Worker::clear_active_process();
                            return;
                        };
                        let socket = socket.clone();
                        let mut rng = host.random_mut();
                        let net_ns = host.network_namespace_borrow();
                        let mut cb = crate::utility::callback_queue::CallbackQueue::new();
                        // Note: socket was already bound in `recreate_socket_descriptor` during process restore.
                        // We skip rebind to avoid EINVAL (socket is already bound).
                        match tcp_role {
                            Some(RestoreTcpSocketRole::Listener) => {
                                let listen_res = socket.listen(16, &net_ns, &mut rng, &mut cb);
                                log::debug!(
                                    "post_restore_socket_fixup listen pid={} fd={} result={:?}",
                                    pid_for_log,
                                    fd_for_log,
                                    listen_res
                                );
                            }
                            Some(RestoreTcpSocketRole::ConnectedPeer) => {
                                if matches!(
                                socket_impl,
                                Some(
                                    crate::core::checkpoint::snapshot_types::DescriptorSocketImplementation::LegacyTcp
                                )
                            ) {
                                log::debug!(
                                    "post_restore_socket_fixup skipping legacy tcp connect pid={} fd={}",
                                    pid_for_log,
                                    fd_for_log
                                );
                                Worker::clear_active_process();
                                return;
                            }
                                if let Some(peer_ip) = socket_peer_ip
                                    .as_ref()
                                    .and_then(|x| x.parse::<std::net::Ipv4Addr>().ok())
                                    && let Some(peer_port) = socket_peer_port
                                {
                                    let sa = nix::sys::socket::SockaddrIn::from(
                                        std::net::SocketAddrV4::new(peer_ip, peer_port),
                                    );
                                    let ss =
                                        crate::utility::sockaddr::SockaddrStorage::from_inet(&sa);
                                    let connect_res =
                                        socket.connect(&ss, &net_ns, &mut rng, &mut cb);
                                    log::debug!(
                                        "post_restore_socket_fixup connect pid={} fd={} peer={:?}:{:?} result={:?}",
                                        pid_for_log,
                                        fd_for_log,
                                        socket_peer_ip,
                                        socket_peer_port,
                                        connect_res
                                    );
                                }
                            }
                            _ => {}
                        }
                        Worker::clear_active_process();
                    },
                    TaskDescriptor::Opaque {
                        description: format!(
                            "post_restore_socket_fixup(pid={},fd={})",
                            pid_for_log, fd_for_log
                        ),
                    },
                );
                host.schedule_task_at_emulated_time(task, replay_time);
            }
        }
    }
    let reduced_nudge_mode = env_flag("SHADOW_REDUCED_RESUME_NUDGE");
    let blocked_condition_by_process: std::collections::HashMap<
        crate::host::process::ProcessId,
        bool,
    > = checkpoint
        .processes
        .iter()
        .filter_map(|p| {
            crate::host::process::ProcessId::try_from(p.process_id)
                .ok()
                .map(|pid| {
                    let has_restorable_blocked_syscall = p.threads.iter().any(|t| {
                        t.runtime.as_ref().is_some_and(|rt| {
                            rt.blocked_timeout_ns.is_some() || rt.blocked_trigger_fd.is_some()
                        })
                    });
                    (pid, has_restorable_blocked_syscall)
                })
        })
        .collect();
    let file_trigger_blocked_by_process: std::collections::HashMap<
        crate::host::process::ProcessId,
        bool,
    > = checkpoint
        .processes
        .iter()
        .filter_map(|p| {
            crate::host::process::ProcessId::try_from(p.process_id)
                .ok()
                .map(|pid| {
                    let has_file_trigger_blocked_syscall = p.threads.iter().any(|t| {
                        t.runtime
                            .as_ref()
                            .is_some_and(|rt| rt.blocked_trigger_fd.is_some())
                    });
                    (pid, has_file_trigger_blocked_syscall)
                })
        })
        .collect();
    let resume_schedule_by_process: std::collections::HashMap<
        crate::host::process::ProcessId,
        (EmulatedTime, EmulatedTime),
    > = checkpoint
        .processes
        .iter()
        .filter_map(|p| {
            crate::host::process::ProcessId::try_from(p.process_id)
                .ok()
                .map(|pid| {
                    let has_blocked_syscall = p.threads.iter().any(|t| {
                        matches!(
                            t.runtime.as_ref().map(|rt| &rt.event_kind),
                            Some(crate::core::checkpoint::snapshot_types::ThreadEventKindSnapshot::Syscall)
                        )
                    });
                    let has_pending_result = p.threads.iter().any(|t| {
                        t.runtime
                            .as_ref()
                            .and_then(|rt| rt.pending_result.as_ref())
                            .is_some()
                    });
                    let poll_timeout_resume = p.threads.iter().find_map(|t| {
                        let rt = t.runtime.as_ref()?;
                        if rt.poll_watches.is_empty() {
                            return None;
                        }
                        let timeout_ns = rt.blocked_timeout_ns?;
                        Some(
                            EmulatedTime::SIMULATION_START
                                + SimulationTime::from_nanos(timeout_ns)
                                + SimulationTime::NANOSECOND,
                        )
                    });
                    let resume_time = if has_pending_result {
                        replay_time + SimulationTime::NANOSECOND
                    } else if let Some(timeout_resume) = poll_timeout_resume {
                        timeout_resume.max(replay_time + SimulationTime::NANOSECOND)
                    } else if has_blocked_syscall {
                        replay_time + SimulationTime::SECOND
                    } else {
                        replay_time + SimulationTime::NANOSECOND
                    };
                    let resume_nudge_start = resume_time + SimulationTime::from_millis(250);
                    (pid, (resume_time, resume_nudge_start))
                })
        })
        .collect();
    let nudge_by_process: std::collections::HashMap<crate::host::process::ProcessId, u64> =
        checkpoint
            .processes
            .iter()
            .filter_map(|p| {
                crate::host::process::ProcessId::try_from(p.process_id)
                    .ok()
                    .map(|pid| {
                        let has_timeout_blocked_syscall =
                            *blocked_condition_by_process.get(&pid).unwrap_or(&false);
                        let has_poll_runtime = p.threads.iter().any(|t| {
                            t.runtime
                                .as_ref()
                                .is_some_and(|rt| !rt.poll_watches.is_empty())
                        });
                        let all_threads_have_runtime =
                            p.threads.iter().all(|t| t.runtime.is_some());
                        let nudge_count = if has_timeout_blocked_syscall && has_poll_runtime {
                            5
                        } else if has_timeout_blocked_syscall {
                            0
                        } else if deterministic_restore {
                            0
                        } else if reduced_nudge_mode && all_threads_have_runtime {
                            5
                        } else {
                            20
                        };
                        (pid, nudge_count)
                    })
            })
            .collect();
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
            let (resume_time, resume_nudge_start) = resume_schedule_by_process
                .get(process_id)
                .copied()
                .unwrap_or((
                    replay_time + SimulationTime::NANOSECOND,
                    replay_time + SimulationTime::from_millis(250),
                ));
            let skip_initial_resume = *file_trigger_blocked_by_process
                .get(process_id)
                .unwrap_or(&false);
            if skip_initial_resume
                || (deterministic_restore
                    && *blocked_condition_by_process
                        .get(process_id)
                        .unwrap_or(&false))
            {
                continue;
            }
            let process_id_for_log = *process_id;
            let task = TaskRef::new_with_result_and_descriptor(
                move |host| {
                    log::debug!(
                        "post_restore_resume host='{}' pid={} tid={}",
                        host.name(),
                        pid_u32,
                        tid_u32
                    );
                    host.resume(process_id_for_log, thread_id)
                },
                TaskDescriptor::ResumeProcess {
                    process_id: pid_u32,
                    thread_id: tid_u32,
                },
            );
            host.schedule_task_at_emulated_time(task, resume_time);
            // Some restored threads can remain parked on stale syscall conditions
            // right after restore. Keep a short compatibility burst, but use fewer retries
            // when checkpoint includes per-thread runtime metadata.
            let nudge_count = *nudge_by_process.get(process_id).unwrap_or(&20);
            for nudge_i in 0u64..nudge_count {
                schedule_resume_process_task(
                    host,
                    *process_id,
                    thread_id,
                    resume_nudge_start + SimulationTime::from_millis(250 * nudge_i),
                );
            }
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
    for process_cp in &checkpoint.processes {
        if !process_cp.is_running {
            continue;
        }
        if let Ok(pid) = crate::host::process::ProcessId::try_from(process_cp.process_id)
            && let Some(proc_rc) = host.processes_borrow().get(&pid)
        {
            let proc = proc_rc.borrow(host.root());
            let restored_desc = proc.descriptor_count_hint(host);
            let restored_entries = proc.snapshot_descriptor_entries(host);
            log_descriptor_census(
                "restore",
                host.name(),
                process_cp.process_id,
                &restored_entries,
            );
            let restored_by_fd: std::collections::HashMap<u32, _> =
                restored_entries.iter().map(|d| (d.fd, d)).collect();
            let cp_socket_fds = process_cp
                .descriptors
                .iter()
                .filter(|d| {
                    matches!(
                        d.file_kind,
                        crate::core::checkpoint::snapshot_types::DescriptorFileKind::Socket
                    )
                })
                .count();
            let restored_socket_fds = restored_entries
                .iter()
                .filter(|d| {
                    matches!(
                        d.file_kind,
                        crate::core::checkpoint::snapshot_types::DescriptorFileKind::Socket
                    )
                })
                .count();
            if restored_desc < process_cp.descriptor_count_hint as usize {
                log::warn!(
                    "restore sanity: process {} descriptor count dropped (cp_hint={}, restored={})",
                    process_cp.process_id,
                    process_cp.descriptor_count_hint,
                    restored_desc
                );
            }
            if restored_socket_fds < cp_socket_fds {
                log::warn!(
                    "restore sanity: process {} socket descriptor count dropped (cp_socket={}, restored_socket={})",
                    process_cp.process_id,
                    cp_socket_fds,
                    restored_socket_fds
                );
            }
            // Structured descriptor consistency checks.
            let mut missing_descriptor = 0usize;
            let mut kind_mismatch = 0usize;
            let mut socket_transport_mismatch = 0usize;
            let mut host_binding_lost = 0usize;
            let mut listen_socket_broken = 0usize;
            let mut socket_runtime_missing = 0usize;
            let mut socket_runtime_mismatch = 0usize;
            for cp_d in &process_cp.descriptors {
                let Some(restored_d) = restored_by_fd.get(&cp_d.fd) else {
                    missing_descriptor += 1;
                    continue;
                };
                if restored_d.file_kind != cp_d.file_kind {
                    kind_mismatch += 1;
                    continue;
                }
                if matches!(
                    cp_d.file_kind,
                    crate::core::checkpoint::snapshot_types::DescriptorFileKind::Socket
                ) {
                    if cp_d.socket_runtime.is_some() && restored_d.socket_runtime.is_none() {
                        socket_runtime_missing += 1;
                    }
                    if let (
                        Some(crate::core::checkpoint::snapshot_types::SocketRuntimeSnapshot::Tcp(
                            cp_rt,
                        )),
                        Some(crate::core::checkpoint::snapshot_types::SocketRuntimeSnapshot::Tcp(
                            restored_rt,
                        )),
                    ) = (&cp_d.socket_runtime, &restored_d.socket_runtime)
                    {
                        let mismatch = cp_rt.tcp_state_kind != restored_rt.tcp_state_kind
                            || cp_rt.tcp_send_buffer_len != restored_rt.tcp_send_buffer_len
                            || cp_rt.tcp_recv_buffer_len != restored_rt.tcp_recv_buffer_len
                            || cp_rt.tcp_send_next_seq != restored_rt.tcp_send_next_seq
                            || cp_rt.tcp_recv_next_seq != restored_rt.tcp_recv_next_seq
                            || cp_rt.tcp_listen_child_count != restored_rt.tcp_listen_child_count
                            || cp_rt.tcp_listen_accept_queue_len
                                != restored_rt.tcp_listen_accept_queue_len;
                        if mismatch {
                            socket_runtime_mismatch += 1;
                        }
                    }
                    if cp_d.socket_transport != restored_d.socket_transport {
                        socket_transport_mismatch += 1;
                    }
                    if cp_d.socket_local_port.is_some() && restored_d.socket_local_port.is_none() {
                        host_binding_lost += 1;
                    }
                    if cp_d.socket_is_listening && !restored_d.socket_is_listening {
                        listen_socket_broken += 1;
                    }
                }
            }
            let total_issues = missing_descriptor
                + kind_mismatch
                + socket_transport_mismatch
                + host_binding_lost
                + listen_socket_broken
                + socket_runtime_missing
                + socket_runtime_mismatch;
            if total_issues > 0 {
                log::error!(
                    "restore descriptor consistency issues host='{}' pid={} total={} missing_descriptor={} kind_mismatch={} socket_transport_mismatch={} host_binding_lost={} listen_socket_broken={} socket_runtime_missing={} socket_runtime_mismatch={}",
                    host.name(),
                    process_cp.process_id,
                    total_issues,
                    missing_descriptor,
                    kind_mismatch,
                    socket_transport_mismatch,
                    host_binding_lost,
                    listen_socket_broken,
                    socket_runtime_missing,
                    socket_runtime_mismatch
                );
            }
        }
    }
    Ok(())
}

fn validate_host_checkpoint_native_state(checkpoint: &HostCheckpoint) -> anyhow::Result<()> {
    validate_host_checkpoint_runtime_coverage(checkpoint)?;
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
        let _ = shadow_shmem::allocator::ShMemBlockSerialized::from_str(
            &process_cp.process_shmem_handle,
        )
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

            let _ = shadow_shmem::allocator::ShMemBlockSerialized::from_str(
                &thread_cp.ipc_shmem_handle,
            )
            .with_context(|| {
                format!(
                    "invalid ipc shmem handle for process {} thread {}",
                    process_cp.process_id, thread_cp.thread_id
                )
            })?;
            let _ = shadow_shmem::allocator::ShMemBlockSerialized::from_str(
                &thread_cp.thread_shmem_handle,
            )
            .with_context(|| {
                format!(
                    "invalid thread shmem handle for process {} thread {}",
                    process_cp.process_id, thread_cp.thread_id
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

fn validate_host_checkpoint_runtime_coverage(checkpoint: &HostCheckpoint) -> anyhow::Result<()> {
    let mut total_network_sockets = 0usize;
    let mut missing_socket_runtime = 0usize;
    let mut total_threads = 0usize;
    let mut missing_thread_runtime = 0usize;

    for process_cp in &checkpoint.processes {
        if !process_cp.is_running {
            continue;
        }
        for d in &process_cp.descriptors {
            if !matches!(
                d.socket_transport,
                Some(
                    crate::core::checkpoint::snapshot_types::DescriptorSocketTransport::Tcp
                        | crate::core::checkpoint::snapshot_types::DescriptorSocketTransport::Udp
                )
            ) {
                continue;
            }
            total_network_sockets += 1;
            if d.socket_runtime.is_none() {
                missing_socket_runtime += 1;
            }
        }

        total_threads += process_cp.threads.len();
        missing_thread_runtime += process_cp
            .threads
            .iter()
            .filter(|t| t.runtime.is_none())
            .count();
    }

    let strict_runtime_snapshot = std::env::var("SHADOW_STRICT_RUNTIME_SNAPSHOT")
        .map(|x| x == "1")
        .unwrap_or(false);
    if missing_socket_runtime > 0 || missing_thread_runtime > 0 {
        let msg = format!(
            "checkpoint runtime coverage is incomplete: missing_socket_runtime={}/{} missing_thread_runtime={}/{}",
            missing_socket_runtime, total_network_sockets, missing_thread_runtime, total_threads
        );
        if strict_runtime_snapshot {
            anyhow::bail!("{msg}");
        }
        log::warn!("{msg}");
    } else {
        log::info!(
            "checkpoint runtime coverage complete: network_sockets={} threads={}",
            total_network_sockets,
            total_threads
        );
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
    const CONFIG_CPU_MAX_FREQ_FILE: &str = "/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq";
    if let Ok(khz_s) = std::fs::read_to_string(CONFIG_CPU_MAX_FREQ_FILE) {
        if let Ok(khz) = khz_s.trim().parse::<u64>() {
            if khz > 0 {
                return Ok(khz * 1000);
            }
        }
    }

    // Fallback: parse /proc/cpuinfo and use the maximum "cpu MHz".
    // This is more likely to work in containers/VMs/WSL where cpufreq is missing.
    let cpuinfo =
        std::fs::read_to_string("/proc/cpuinfo").context("Could not read /proc/cpuinfo")?;

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
            TaskRef::new_with_descriptor(|_host| {}, TaskDescriptor::RelayForward { relay_id: 0 }),
            EmulatedTime::SIMULATION_START + SimulationTime::from_secs(8),
        );
    }

    #[test]
    fn host_checkpoint_round_trips_queue_and_counters() {
        let source = test_host("src");
        seed_host_state(&source);
        let snapshot = snapshot_host(&source);

        let restored = test_host("dst");
        apply_host_checkpoint(
            &restored,
            &snapshot,
            RestoreProtocolModeSnapshot::LegacyHeuristic,
            EmulatedTime::SIMULATION_START + SimulationTime::from_nanos(snapshot.cpu_now_ns),
        )
        .unwrap();
        let restored_snapshot = snapshot_host(&restored);

        assert_eq!(snapshot.event_queue, restored_snapshot.event_queue);
        assert_eq!(
            snapshot.last_popped_event_time_ns,
            restored_snapshot.last_popped_event_time_ns
        );
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
        assert_eq!(
            snapshot.cpu_available_ns,
            restored_snapshot.cpu_available_ns
        );

        source.shutdown();
        restored.shutdown();
    }

    #[test]
    fn restored_host_replays_new_scheduling_like_original() {
        let source = test_host("src-replay");
        seed_host_state(&source);
        let snapshot = snapshot_host(&source);

        let restored = test_host("dst-replay");
        apply_host_checkpoint(
            &restored,
            &snapshot,
            RestoreProtocolModeSnapshot::LegacyHeuristic,
            EmulatedTime::SIMULATION_START + SimulationTime::from_nanos(snapshot.cpu_now_ns),
        )
        .unwrap();

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

        assert_eq!(
            snapshot_host(&source).event_queue,
            snapshot_host(&restored).event_queue
        );
        assert_eq!(
            snapshot_host(&source).next_event_id,
            snapshot_host(&restored).next_event_id
        );

        source.shutdown();
        restored.shutdown();
    }
}
