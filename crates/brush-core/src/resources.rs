//! Per-pipeline resource limits and monitoring, enforced with Linux cgroup v2.
//!
//! Configure each outermost pipeline through [`crate::ExecutionParameters::pipeline_options`].
//! Cgroups are lazy: builtin-only pipelines do not need delegation. External spawns fail
//! closed with [`crate::ErrorKind::CgroupUnavailable`] if resource control is unavailable.
//! On non-Linux platforms, requesting these options fails with the same error kind.
//!
//! ```no_run
//! use std::time::Duration;
//! use brush_core::{
//!     CpuLimit, MIB, MemoryLimit, PipelineOptions, ResourceLimits, ResourceMonitorConfig,
//!     ResourceEvent, SourceInfo,
//! };
//! # async fn example() -> Result<(), brush_core::Error> {
//! # let mut shell = brush_core::Shell::builder().build().await?;
//! let limits = ResourceLimits {
//!     memory: Some(MemoryLimit::new(512 * MIB)),
//!     cpu: Some(CpuLimit::new(0.5)),
//!     pids: Some(64),
//! };
//! let monitor = ResourceMonitorConfig::new(Duration::from_millis(100));
//! let mut events: tokio::sync::mpsc::Receiver<ResourceEvent> =
//!     monitor.events().expect("receiver taken once");
//! let consumer = tokio::spawn(async move {
//!     while let Some(event) = events.recv().await {
//!         println!("{event:?}");
//!     }
//! });
//! let options = PipelineOptions::default()
//!     .with_resource_limits(limits)
//!     .with_resource_monitor(monitor);
//! let mut params = shell.default_exec_params();
//! params.pipeline_options = options;
//! shell.run_string("make -j8 | tee log", &SourceInfo::default(), &params).await?;
//! drop(params); // Close the last sender before waiting for the consumer.
//! consumer.await.expect("event consumer");
//! # Ok(())
//! # }
//! ```
//!
//! Run delegated integration checks with:
//! `systemd-run --user --scope -p Delegate=yes cargo test -p brush-core --test resource_limits_tests`.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::{error, extensions::ExecutionObserver};

/// One mebibyte, in bytes.
pub const MIB: u64 = 1024 * 1024;

/// A pipeline's memory ceiling, in bytes (swap disabled).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryLimit(u64);

impl MemoryLimit {
    /// Creates a memory ceiling in bytes.
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    /// Returns the memory ceiling in bytes.
    pub const fn bytes(self) -> u64 {
        self.0
    }
}

/// A pipeline's CPU quota, measured in CPUs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CpuLimit(f64);

impl CpuLimit {
    /// Creates a CPU quota (for example, `0.5` is half a CPU).
    ///
    /// # Panics
    /// Panics if `cpus` is not finite and strictly positive.
    pub fn new(cpus: f64) -> Self {
        assert!(
            cpus.is_finite() && cpus > 0.0,
            "cpu limit must be a positive number of CPUs"
        );
        Self(cpus)
    }

    /// Returns the quota measured in CPUs.
    pub const fn cpus(self) -> f64 {
        self.0
    }
}

/// Limits shared by every external process and descendant of a pipeline.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResourceLimits {
    /// Memory ceiling, with swap disabled and group OOM killing enabled when supported.
    pub memory: Option<MemoryLimit>,
    /// CPU quota over a 100 ms period, floored at the kernel's 1 ms minimum quota.
    pub cpu: Option<CpuLimit>,
    /// Maximum number of tasks; enforced on forks, not migration of pipeline stages.
    pub pids: Option<u32>,
}

/// Cumulative resource accounting for a pipeline's cgroup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceSample {
    /// Time since the cgroup was created.
    pub elapsed: Duration,
    /// Current memory usage (`memory.current`).
    pub memory_bytes: u64,
    /// Peak memory usage (`memory.peak`), or zero on kernels before 5.19.
    pub memory_peak_bytes: u64,
    /// Total CPU time consumed.
    pub cpu_usage: Duration,
    /// CPU time consumed in user mode.
    pub cpu_user: Duration,
    /// CPU time consumed in kernel mode.
    pub cpu_system: Duration,
    /// Time throttled by the CPU quota.
    pub cpu_throttled: Duration,
    /// Current task count.
    pub pids: u32,
    /// Number of processes killed by the OOM killer.
    pub oom_kills: u64,
}

/// Events from the bounded channel shared by a monitor configuration and its clones.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ResourceEvent {
    /// First external spawn was prepared; `command` is the pipeline's source text.
    Started {
        /// Identifier shared by this pipeline's events.
        pipeline_id: u64,
        /// Pipeline source text.
        command: String,
    },
    /// Periodic sample; dropped when the channel is full.
    Sample {
        /// Identifier shared by this pipeline's events.
        pipeline_id: u64,
        /// Cumulative resource accounting.
        sample: ResourceSample,
    },
    /// All stages were waited; always delivered, awaiting channel space if necessary.
    Finished {
        /// Identifier shared by this pipeline's events.
        pipeline_id: u64,
        /// Final cumulative resource accounting.
        sample: ResourceSample,
    },
}

/// Periodic monitoring with a shared, bounded event channel.
///
/// Drain the receiver concurrently with execution: `Started` and `Finished` await channel
/// space, while periodic samples are lossy. Dropping the receiver never blocks execution.
#[derive(Clone, Debug)]
pub struct ResourceMonitorConfig {
    interval: Duration,
    #[cfg_attr(
        not(target_os = "linux"),
        expect(
            dead_code,
            reason = "keeps the channel open; event production requires Linux"
        )
    )]
    events: tokio::sync::mpsc::Sender<ResourceEvent>,
    receiver: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<ResourceEvent>>>>,
}

impl ResourceMonitorConfig {
    const CHANNEL_CAPACITY: usize = 64;

    /// Creates a monitor with a 64-event channel.
    ///
    /// # Panics
    /// Panics if `interval` is zero.
    pub fn new(interval: Duration) -> Self {
        assert!(!interval.is_zero(), "monitor interval must be non-zero");
        let (events, receiver) = tokio::sync::mpsc::channel(Self::CHANNEL_CAPACITY);
        Self {
            interval,
            events,
            receiver: Arc::new(Mutex::new(Some(receiver))),
        }
    }

    /// Takes the event receiver once, shared across this config and its clones.
    pub fn events(&self) -> Option<tokio::sync::mpsc::Receiver<ResourceEvent>> {
        self.receiver
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Returns the sampling interval.
    pub const fn interval(&self) -> Duration {
        self.interval
    }
}

/// Resource policy for each outermost pipeline executed with these options.
///
/// Nested functions, groups, subshells and substitutions share their enclosing pipeline's
/// cgroup. Builtin-only pipelines create no cgroup and emit no events. Setup failures never
/// fall back to unenforced execution. Linux cgroup v2 and delegated cpu/memory/pids controllers
/// are required. Processes in the delegated parent are moved into its `host` leaf to enable
/// controllers; lingering children retain their limits after the pipeline finishes.
#[derive(Clone, Debug, Default)]
pub struct PipelineOptions {
    limits: Option<ResourceLimits>,
    monitor: Option<ResourceMonitorConfig>,
    #[cfg(target_os = "linux")]
    active: Option<Arc<PipelineResources>>,
}

impl PipelineOptions {
    /// Applies limits to each outermost pipeline.
    #[must_use]
    pub const fn with_resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = Some(limits);
        self
    }

    /// Monitors each outermost pipeline that spawns an external command.
    #[must_use]
    pub fn with_resource_monitor(mut self, monitor: ResourceMonitorConfig) -> Self {
        self.monitor = Some(monitor);
        self
    }

    /// Installs ownership on the pipeline's private parameters clone, unless nested.
    #[cfg_attr(
        target_os = "linux",
        expect(clippy::unnecessary_wraps, reason = "non-Linux setup can fail")
    )]
    pub(crate) fn begin(
        &mut self,
        command: impl FnOnce() -> String,
    ) -> Result<Option<Arc<PipelineResources>>, error::Error> {
        #[cfg(target_os = "linux")]
        {
            if self.active.is_some() || (self.limits.is_none() && self.monitor.is_none()) {
                return Ok(None);
            }
            let resources = Arc::new(PipelineResources::new(
                self.limits.unwrap_or_default(),
                self.monitor.clone(),
                self.monitor.is_some().then(command),
            ));
            self.active = Some(resources.clone());
            Ok(Some(resources))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = command;
            if self.limits.is_some() || self.monitor.is_some() {
                return Err(error::ErrorKind::CgroupUnavailable(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "pipeline resource control requires Linux cgroup v2",
                ))
                .into());
            }
            Ok(None)
        }
    }

    pub(crate) const fn active(&self) -> Option<&Arc<PipelineResources>> {
        #[cfg(target_os = "linux")]
        {
            self.active.as_ref()
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub(crate) struct PipelineResources;

#[cfg(not(target_os = "linux"))]
impl PipelineResources {
    pub(crate) fn attach(
        &self,
        _cmd: &mut std::process::Command,
        _observer: &impl ExecutionObserver,
    ) -> Result<(), error::Error> {
        Ok(())
    }

    pub(crate) async fn finish(&self) {}
}

#[cfg(target_os = "linux")]
pub(crate) use linux::PipelineResources;

#[cfg(target_os = "linux")]
mod linux {
    use super::{
        Arc, Duration, ExecutionObserver, Mutex, PoisonError, ResourceEvent, ResourceLimits,
        ResourceMonitorConfig, ResourceSample, error,
    };
    use std::ffi::CString;
    use std::fmt::Display;
    use std::fs;
    use std::io;
    use std::os::unix::ffi::OsStringExt as _;
    use std::os::unix::process::CommandExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Instant;

    use nix::fcntl::OFlag;
    use nix::sys::stat::Mode;
    use tokio::sync::Notify;

    const CGROUP_ROOT: &str = "/sys/fs/cgroup";
    const CONTROLLERS: &str = "+cpu +memory +pids";
    const HOST_LEAF: &str = "host";
    const CPU_PERIOD_USEC: u64 = 100_000;
    const MIN_CPU_QUOTA_USEC: u64 = 1_000;
    static PARENT: OnceLock<PathBuf> = OnceLock::new();
    // Discovery and migration must be serialized: another caller must not discover our host leaf.
    static PARENT_INIT: Mutex<()> = Mutex::new(());
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    static LEFTOVERS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    fn unavailable(path: &Path, e: &io::Error) -> error::Error {
        error::ErrorKind::CgroupUnavailable(io::Error::new(
            e.kind(),
            format!("{}: {e}", path.display()),
        ))
        .into()
    }

    fn own_cgroup() -> io::Result<PathBuf> {
        let text = fs::read_to_string("/proc/self/cgroup")?;
        let path = text
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .map(str::trim)
            .filter(|path| path.starts_with('/'))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "cgroup v2 unified hierarchy required",
                )
            })?;
        Ok(Path::new(CGROUP_ROOT).join(path.trim_start_matches('/')))
    }

    fn parent() -> Result<&'static Path, error::Error> {
        if let Some(parent) = PARENT.get() {
            return Ok(parent);
        }
        let _guard = PARENT_INIT.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(parent) = PARENT.get() {
            return Ok(parent);
        }
        let own = own_cgroup().map_err(|e| unavailable(Path::new("/proc/self/cgroup"), &e))?;
        enable_controllers(&own)?;
        Ok(PARENT.get_or_init(|| own))
    }

    fn enable_controllers(own: &Path) -> Result<(), error::Error> {
        let control = own.join("cgroup.subtree_control");
        for _ in 0..3 {
            match fs::write(&control, CONTROLLERS) {
                Ok(()) => return Ok(()),
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => migrate_to_leaf(own)?,
                Err(e) => return Err(unavailable(&control, &e)),
            }
        }
        Err(unavailable(
            &control,
            &io::Error::from_raw_os_error(libc::EBUSY),
        ))
    }

    fn migrate_to_leaf(own: &Path) -> Result<(), error::Error> {
        // ponytail: moves every process of our cgroup (including this one) into a host leaf;
        // embedders needing a prearranged leaf can use systemd's DelegateSubgroup=host.
        let leaf = own.join(HOST_LEAF);
        match fs::create_dir(&leaf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(unavailable(&leaf, &e)),
        }
        let procs = own.join("cgroup.procs");
        let text = fs::read_to_string(&procs).map_err(|e| unavailable(&procs, &e))?;
        let target = leaf.join("cgroup.procs");
        for pid in text.lines() {
            // Exited processes race harmlessly; other failures surface on the next enable attempt.
            let _ = fs::write(&target, pid);
        }
        Ok(())
    }

    fn set(dir: &Path, file: &str, value: impl Display) -> Result<(), error::Error> {
        let path = dir.join(file);
        fs::write(&path, value.to_string()).map_err(|e| unavailable(&path, &e))
    }

    fn set_if_present(dir: &Path, file: &str, value: impl Display) -> Result<(), error::Error> {
        let path = dir.join(file);
        match fs::write(&path, value.to_string()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(unavailable(&path, &e)),
        }
    }

    fn field(text: &str, key: &str) -> u64 {
        text.lines()
            .filter_map(|line| line.split_once(' '))
            .find_map(|(name, value)| (name == key).then(|| value.trim().parse().ok()).flatten())
            .unwrap_or(0)
    }

    fn sweep_leftovers() {
        LEFTOVERS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(
                |dir| matches!(fs::remove_dir(dir), Err(e) if e.kind() != io::ErrorKind::NotFound),
            );
    }

    #[derive(Debug)]
    pub(crate) struct PipelineResources {
        limits: ResourceLimits,
        monitor: Option<ResourceMonitorConfig>,
        command: Option<String>,
        cgroup: Mutex<Option<Arc<PipelineCgroup>>>,
        monitor_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
        finished: AtomicBool,
    }

    impl PipelineResources {
        pub(super) const fn new(
            limits: ResourceLimits,
            monitor: Option<ResourceMonitorConfig>,
            command: Option<String>,
        ) -> Self {
            Self {
                limits,
                monitor,
                command,
                cgroup: Mutex::new(None),
                monitor_task: Mutex::new(None),
                finished: AtomicBool::new(false),
            }
        }

        pub(crate) fn attach(
            &self,
            cmd: &mut std::process::Command,
            observer: &impl ExecutionObserver,
        ) -> Result<(), error::Error> {
            let mut slot = self.cgroup.lock().unwrap_or_else(PoisonError::into_inner);
            // Check under the slot lock so finish cannot race with installing a new monitor.
            if self.finished.load(Ordering::Acquire) {
                let cgroup = PipelineCgroup::create(&self.limits)?;
                cgroup.join(cmd);
                // ponytail: one empty directory may remain if no later pipeline sweeps it.
                LEFTOVERS
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(cgroup.dir);
                return Ok(());
            }
            if slot.is_none() {
                let cgroup = Arc::new(PipelineCgroup::create(&self.limits)?);
                if let Some(monitor) = &self.monitor {
                    match observer.spawn_task(monitor_loop(
                        cgroup.clone(),
                        monitor.clone(),
                        self.command.clone().unwrap_or_default(),
                    )) {
                        Ok(task) => {
                            *self
                                .monitor_task
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner) = Some(task);
                        }
                        Err(e) => {
                            cgroup.remove();
                            return Err(e);
                        }
                    }
                }
                *slot = Some(cgroup);
            }
            if let Some(cgroup) = slot.as_ref() {
                cgroup.join(cmd);
            }
            drop(slot);
            Ok(())
        }

        pub(crate) async fn finish(&self) {
            self.finished.store(true, Ordering::Release);
            let cgroup = self
                .cgroup
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            let task = self
                .monitor_task
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            let Some(cgroup) = cgroup else { return };
            cgroup.stop.notify_one();
            if let Some(task) = task {
                let _ = task.await;
            }
            cgroup.remove();
        }
    }

    impl Drop for PipelineResources {
        fn drop(&mut self) {
            if let Some(cgroup) = self
                .cgroup
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                cgroup.stop.notify_one();
                cgroup.remove();
            }
        }
    }

    #[derive(Debug)]
    struct PipelineCgroup {
        id: u64,
        dir: PathBuf,
        procs: CString,
        started: Instant,
        stop: Notify,
    }

    impl PipelineCgroup {
        fn create(limits: &ResourceLimits) -> Result<Self, error::Error> {
            let parent = parent()?;
            sweep_leftovers();
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let dir = parent.join(format!("brush-{}-{id}", std::process::id()));
            fs::create_dir(&dir).map_err(|e| unavailable(&dir, &e))?;
            let applied = (|| {
                if let Some(memory) = limits.memory {
                    set(&dir, "memory.max", memory.bytes())?;
                    set_if_present(&dir, "memory.swap.max", 0)?;
                    set_if_present(&dir, "memory.oom.group", 1)?;
                }
                if let Some(cpu) = limits.cpu {
                    #[expect(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        reason = "CPUs is finite and positive; saturating conversion caps the quota"
                    )]
                    let quota = ((cpu.cpus() * 100_000.0).round() as u64).max(MIN_CPU_QUOTA_USEC);
                    set(&dir, "cpu.max", format_args!("{quota} {CPU_PERIOD_USEC}"))?;
                }
                if let Some(pids) = limits.pids {
                    set(&dir, "pids.max", pids)?;
                }
                Ok(())
            })();
            if let Err(e) = applied {
                let _ = fs::remove_dir(&dir);
                return Err(e);
            }
            let procs = CString::new(dir.join("cgroup.procs").into_os_string().into_vec())
                .map_err(|e| unavailable(&dir, &io::Error::new(io::ErrorKind::InvalidInput, e)))?;
            Ok(Self {
                id,
                dir,
                procs,
                started: Instant::now(),
                stop: Notify::new(),
            })
        }

        fn join(&self, cmd: &mut std::process::Command) {
            let procs = self.procs.clone();
            // SAFETY: runs in the forked child before exec. Only async-signal-safe open,
            // write and close on a path prepared in the parent; no allocation or locks.
            unsafe {
                cmd.pre_exec(move || {
                    let fd = nix::fcntl::open(
                        procs.as_c_str(),
                        OFlag::O_WRONLY | OFlag::O_CLOEXEC,
                        Mode::empty(),
                    )?;
                    nix::unistd::write(&fd, b"0")?;
                    Ok(())
                });
            }
        }

        fn remove(&self) {
            // ponytail: no cgroup.kill on exit; lingering children keep their limits (shell semantics).
            match fs::remove_dir(&self.dir) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(_) => LEFTOVERS
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(self.dir.clone()),
            }
        }

        fn sample(&self) -> ResourceSample {
            let read_u64 = |file| {
                fs::read_to_string(self.dir.join(file))
                    .ok()
                    .and_then(|text| text.trim().parse().ok())
                    .unwrap_or(0)
            };
            let cpu = fs::read_to_string(self.dir.join("cpu.stat")).unwrap_or_default();
            let events = fs::read_to_string(self.dir.join("memory.events")).unwrap_or_default();
            ResourceSample {
                elapsed: self.started.elapsed(),
                memory_bytes: read_u64("memory.current"),
                memory_peak_bytes: read_u64("memory.peak"),
                cpu_usage: Duration::from_micros(field(&cpu, "usage_usec")),
                cpu_user: Duration::from_micros(field(&cpu, "user_usec")),
                cpu_system: Duration::from_micros(field(&cpu, "system_usec")),
                cpu_throttled: Duration::from_micros(field(&cpu, "throttled_usec")),
                pids: u32::try_from(read_u64("pids.current")).unwrap_or(u32::MAX),
                oom_kills: field(&events, "oom_kill"),
            }
        }
    }

    async fn monitor_loop(
        cgroup: Arc<PipelineCgroup>,
        monitor: ResourceMonitorConfig,
        command: String,
    ) {
        let pipeline_id = cgroup.id;
        if monitor
            .events
            .send(ResourceEvent::Started {
                pipeline_id,
                command,
            })
            .await
            .is_err()
        {
            return;
        }
        let mut ticks = tokio::time::interval(monitor.interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticks.tick() => {
                    let event = ResourceEvent::Sample { pipeline_id, sample: cgroup.sample() };
                    if monitor.events.try_send(event).is_err() && monitor.events.is_closed() {
                        return;
                    }
                }
                () = cgroup.stop.notified() => break,
            }
        }
        let _ = monitor
            .events
            .send(ResourceEvent::Finished {
                pipeline_id,
                sample: cgroup.sample(),
            })
            .await;
    }
}
