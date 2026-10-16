//! Privileged enforcement inside the tracing service: the kernel program's maps, kept in step with
//! ptrace stops, and its event stream delivered to each root's policy engine.
//!
//! Without the `privileged` feature [`Kernel`] is uninhabited, so every privileged path below is
//! statically dead and the ptrace gate is the only enforcement.

use super::{RootId, State, Target, Tracing, lock};
use crate::enforcement::{Enforcement, Enforcer, KernelPolicy};
use crate::{FileTarget, local};
use marsh_peer_class::{AF_INET, AF_INET6, Endpoint, Event};
use nix::unistd::Pid;
use std::{io, sync::Arc, sync::Mutex};
use syscalls::Sysno;

/// Process-wide: the mode the first shell chose, and why an `Auto` request fell back.
static SELECTION: Mutex<Option<(Enforcement, Option<String>)>> = Mutex::new(None);

const ALREADY_UNPRIVILEGED: &str =
    "process already selected unprivileged socket enforcement before Auto was requested";

/// The mode this process selected, unprivileged before any selection.
pub(super) fn selected() -> Enforcement {
    lock(&SELECTION)
        .as_ref()
        .map_or(Enforcement::Unprivileged, |(mode, _)| *mode)
}

/// Resolves `requested` against the process's selection, loading the kernel programs when the
/// result is privileged: the mode in force, its kernel, and why `Auto` fell back, if it did.
pub(super) fn select(
    requested: Enforcement,
) -> io::Result<(Enforcement, Option<Kernel>, Option<String>)> {
    let mut selection = lock(&SELECTION);
    let (desired, reason) = match selection.clone() {
        Some((mode, _)) if requested != Enforcement::Auto && requested != mode => {
            return Err(io::Error::other(
                "socket enforcement already selected differently for this process",
            ));
        }
        Some((mode, reason)) => (mode, reason),
        None => (requested, None),
    };
    let (mode, kernel, reason) = match desired {
        Enforcement::Unprivileged => (
            Enforcement::Unprivileged,
            None,
            reason.or_else(|| {
                (requested == Enforcement::Auto).then(|| ALREADY_UNPRIVILEGED.to_owned())
            }),
        ),
        Enforcement::Privileged | Enforcement::Auto => match Kernel::load() {
            Ok(kernel) => (Enforcement::Privileged, Some(kernel), None),
            Err(error) if desired == Enforcement::Auto => {
                (Enforcement::Unprivileged, None, Some(error.to_string()))
            }
            Err(error) => return Err(error),
        },
    };
    *selection = Some((mode, reason.clone()));
    drop(selection);
    Ok((mode, kernel, reason))
}

impl Tracing {
    /// Admits a later shell's request into this generation's mode.
    pub(super) fn admit_enforcement(&self, requested: Enforcement) -> io::Result<()> {
        match requested {
            Enforcement::Auto => {
                if self.enforcement == Enforcement::Unprivileged {
                    self.fallback
                        .get_or_init(|| ALREADY_UNPRIVILEGED.to_owned());
                }
                Ok(())
            }
            mode if mode == self.enforcement => Ok(()),
            _ => Err(io::Error::other(
                "socket enforcement already selected differently for this process",
            )),
        }
    }

    /// Installs the shared policy engine for both enforcement modes.
    pub fn enforce_peers(&self, root: RootId, enforcer: Arc<dyn Enforcer>) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.check()?;
        if state.runs.keys().any(|run| run.root == root) {
            return Err(io::Error::other(
                "socket enforcer must be installed before a root begins running",
            ));
        }
        state
            .roots
            .get_mut(&root)
            .ok_or_else(|| io::Error::other("unknown root"))?
            .enforcer = Some(enforcer);
        drop(state);
        Ok(())
    }

    /// Publishes a root's answers to the kernel. Answers of an older generation than those in
    /// force are dropped: concurrent tracers may publish out of order.
    pub fn publish(&self, root: RootId, policy: KernelPolicy) -> io::Result<()> {
        match &self.kernel {
            Some(kernel) => kernel.publish(root.0, policy),
            None => Ok(()),
        }
    }

    /// Makes the kernel refuse every class any rule could forbid until the root's answers are
    /// published again: an input changed and they are not recomputed yet.
    pub fn stale(&self, root: RootId) -> io::Result<()> {
        match &self.kernel {
            Some(kernel) => kernel.stale(root.0),
            None => Ok(()),
        }
    }

    /// Prepares the kernel for a socket call stopped at its entry. Returns whether the ptrace gate
    /// must judge it too: always without the kernel, and for in-kernel copies whose hook
    /// coverage varies by kernel.
    pub(super) fn kernel_entry(
        &self,
        target: Target,
        enforcer: &dyn Enforcer,
        syscall: Sysno,
        source: Option<&FileTarget>,
    ) -> io::Result<bool> {
        let Some(kernel) = &self.kernel else {
            return Ok(true);
        };
        let copies = matches!(syscall, Sysno::sendfile | Sysno::splice);
        let root = target.run.root.0;
        if copies || kernel.needs_refresh(root) {
            // Contacts already made must reach the answers before this one is judged.
            self.deliver_kernel_events()?;
            kernel.publish(root, enforcer.refresh(target.run, source)?)?;
        }
        if kernel.restricts_untraced(root) {
            kernel.allow_local(&self.traced_local_endpoints()?)?;
        }
        Ok(copies)
    }

    pub(super) fn map_task(&self, tid: Pid) -> io::Result<()> {
        let Some(kernel) = &self.kernel else {
            return Ok(());
        };
        let state = lock(&self.state);
        let Some(target) = state.threads.get(&tid).and_then(|thread| thread.inherited) else {
            return Ok(());
        };
        if state
            .roots
            .get(&target.run.root)
            .is_none_or(|root| root.enforcer.is_none())
        {
            return Ok(());
        }
        drop(state);
        kernel.map_task(tid, target.run.root.0)
    }

    pub(super) fn unmap_task(&self, tid: Pid) -> io::Result<()> {
        match &self.kernel {
            Some(kernel) => kernel.unmap_task(tid),
            None => Ok(()),
        }
    }

    pub(super) fn remove_kernel_root(&self, root: RootId) -> io::Result<()> {
        match &self.kernel {
            Some(kernel) => kernel.remove_root(root.0),
            None => Ok(()),
        }
    }

    /// Kills every governed task and detaches the programs after a shared failure.
    pub(super) fn terminate_kernel(&self) -> io::Result<()> {
        match &self.kernel {
            Some(kernel) => kernel.terminate(),
            None => Ok(()),
        }
    }

    /// Drains the kernel's events and delivers each to its root's policy engine, outside every
    /// trace lock. A failure fails the whole service: an undelivered taint receipt or contact
    /// cannot be recovered.
    pub(super) fn deliver_kernel_events(&self) -> io::Result<()> {
        let Some(kernel) = &self.kernel else {
            return Ok(());
        };
        // Attribution is reserved while the drain's state lock is held: another tracer could
        // otherwise retire a root whose events this batch already removed from the ring.
        let mut state = lock(&self.state);
        let events = kernel.drain()?;
        let mut deliveries = Deliveries::reserve(self, &mut state, events)?;
        drop(state);
        deliveries.deliver().inspect_err(|error| {
            // Other owners must see the failure before releasing their undelivered records
            // could make them quiescent enough to publish incomplete evidence.
            self.fail(format!("kernel event delivery: {error}"));
        })
    }

    /// Delivers kernel events as they arrive, so taint receipts reach the policy engine without
    /// waiting for a stop and the ring cannot overflow while tasks only send.
    pub(super) fn start_kernel_events(self: &Arc<Self>) -> io::Result<()> {
        let Some(kernel) = &self.kernel else {
            return Ok(());
        };
        let ring = kernel.events()?;
        let tracing = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("marsh-bpf-events".into())
            .spawn(move || {
                let mut poll = libc::pollfd {
                    fd: std::os::fd::AsRawFd::as_raw_fd(&ring),
                    events: libc::POLLIN,
                    revents: 0,
                };
                loop {
                    // Bounded, so the thread notices the service ending without owning it.
                    // SAFETY: poll reads and writes exactly this one live pollfd.
                    let ready = unsafe { libc::poll(&raw mut poll, 1, 100) } > 0;
                    let Some(tracing) = tracing.upgrade() else {
                        return;
                    };
                    if ready && tracing.deliver_kernel_events().is_err() {
                        return;
                    }
                }
            })?;
        Ok(())
    }

    /// The IP endpoints traced tasks hold, excluding any an untraced process also listens on.
    pub(super) fn traced_local_endpoints(&self) -> io::Result<Vec<Endpoint>> {
        let tasks: Vec<Pid> = lock(&self.state)
            .threads
            .iter()
            .filter(|(_, thread)| thread.inherited.is_some())
            .map(|(tid, _)| *tid)
            .collect();
        Ok(local::endpoints(&tasks)?
            .into_iter()
            .filter(|endpoint| matches!(endpoint.family, AF_INET | AF_INET6))
            .filter(|endpoint| {
                self.receivers(&endpoint.sockaddr()).is_some_and(|sockets| {
                    !sockets.is_empty() && sockets.iter().all(|holders| !holders.is_empty())
                })
            })
            .collect())
    }
}

/// One drained event and where it goes.
struct Delivery {
    target: Target,
    enforcer: Arc<dyn Enforcer>,
    event: Event,
}

/// A drained batch whose runs stay live until each event is delivered, or the batch dropped.
struct Deliveries<'a> {
    tracing: &'a Tracing,
    pending: Vec<Delivery>,
    completed: usize,
}

impl<'a> Deliveries<'a> {
    fn reserve(tracing: &'a Tracing, state: &mut State, events: Vec<Event>) -> io::Result<Self> {
        let pending = events
            .into_iter()
            .map(|event| {
                let target = attribute(state, &event)?;
                state.run(target.run)?;
                let enforcer = state
                    .roots
                    .get(&target.run.root)
                    .and_then(|root| root.enforcer.clone())
                    .ok_or_else(|| io::Error::other("kernel event lost its policy engine"))?;
                Ok(Delivery {
                    target,
                    enforcer,
                    event,
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        for delivery in &pending {
            state
                .runs
                .get_mut(&delivery.target.run)
                .ok_or_else(|| io::Error::other("kernel event lost its trace run"))?
                .deliveries += 1;
        }
        Ok(Self {
            tracing,
            pending,
            completed: 0,
        })
    }

    fn deliver(&mut self) -> io::Result<()> {
        while let Some(delivery) = self.pending.get(self.completed) {
            let run = delivery.target.run;
            delivery
                .enforcer
                .event(run, delivery.target.builtin, &delivery.event)?;
            self.release(run);
            self.completed += 1;
        }
        Ok(())
    }

    fn release(&self, run: super::TraceRun) {
        if let Some(run) = lock(&self.tracing.state).runs.get_mut(&run) {
            run.deliveries -= 1;
        }
        self.tracing.progress.notify_all();
    }
}

impl Drop for Deliveries<'_> {
    fn drop(&mut self) {
        if self.completed == self.pending.len() {
            return;
        }
        if std::thread::panicking() {
            self.tracing.fail("kernel event delivery panicked");
        }
        for delivery in &self.pending[self.completed..] {
            self.release(delivery.target.run);
        }
    }
}

/// The run an event belongs to: its thread's, else its process's, else any run of its root
/// (an `io_uring` worker thread is never traced itself).
fn attribute(state: &State, event: &Event) -> io::Result<Target> {
    let of_root = |target: Option<Target>| target.filter(|target| target.run.root.0 == event.root);
    let tid = Pid::from_raw(i32::try_from(event.tid).map_err(io::Error::other)?);
    of_root(state.threads.get(&tid).and_then(|thread| thread.inherited))
        .or_else(|| {
            of_root(
                state
                    .threads
                    .values()
                    .find(|thread| {
                        thread.process.as_ref().is_some_and(|process| {
                            process.group.as_raw().cast_unsigned() == event.tgid
                        })
                    })
                    .and_then(|thread| thread.inherited),
            )
        })
        .or_else(|| {
            state
                .runs
                .keys()
                .find(|run| run.root.0 == event.root)
                .map(|run| Target {
                    run: *run,
                    builtin: None,
                })
        })
        .ok_or_else(|| io::Error::other("kernel event has no live trace run"))
}

#[cfg(feature = "privileged")]
pub(crate) use privileged::Kernel;

#[cfg(not(feature = "privileged"))]
pub(crate) use unavailable::Kernel;

#[cfg(feature = "privileged")]
mod privileged {
    use super::{Endpoint, Event, KernelPolicy, Mutex, Pid, io};
    use marsh_lib::RecoverPoison as _;
    use marsh_bpf::Controller;
    use marsh_peer_class::Network;
    use std::collections::HashMap;
    use std::os::fd::{BorrowedFd, OwnedFd};

    /// The loaded programs, and what was last published to them for each root.
    pub(crate) struct Kernel(Mutex<Inner>);

    struct Inner {
        controller: Controller,
        roots: HashMap<u64, Published>,
        /// Governed threads and the process each belongs to.
        tasks: HashMap<Pid, u32>,
    }

    #[derive(Default)]
    struct Published {
        networks: Option<Vec<Network>>,
        policy: Option<KernelPolicy>,
    }

    impl Kernel {
        pub(super) fn load() -> io::Result<Self> {
            Ok(Self(Mutex::new(Inner {
                controller: Controller::load()?,
                roots: HashMap::new(),
                tasks: HashMap::new(),
            })))
        }

        fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
            self.0.lock().recover()
        }

        pub(super) fn publish(&self, root: u64, policy: KernelPolicy) -> io::Result<()> {
            let mut inner = self.inner();
            let Inner {
                controller, roots, ..
            } = &mut *inner;
            let published = roots.entry(root).or_default();
            if let Some(current) = &published.policy
                && (policy.answers.generation < current.answers.generation || *current == policy)
            {
                return Ok(());
            }
            if published.networks.as_ref() != Some(&policy.networks) {
                controller.configure(root, &policy.networks)?;
                published.networks = Some(policy.networks.clone());
            }
            controller.publish(root, policy.answers)?;
            published.policy = Some(policy);
            drop(inner);
            Ok(())
        }

        pub(super) fn stale(&self, root: u64) -> io::Result<()> {
            let mut inner = self.inner();
            inner.controller.stale(root)?;
            if let Some(policy) = inner
                .roots
                .get_mut(&root)
                .and_then(|published| published.policy.as_mut())
            {
                policy.answers.stale = 1;
            }
            drop(inner);
            Ok(())
        }

        /// Whether the kernel's answers for `root` cannot be trusted for one more contact.
        pub(super) fn needs_refresh(&self, root: u64) -> bool {
            self.inner()
                .roots
                .get(&root)
                .and_then(|published| published.policy.as_ref())
                .is_none_or(|policy| policy.answers.stale != 0 || policy.contact_sensitive)
        }

        pub(super) fn restricts_untraced(&self, root: u64) -> bool {
            self.inner()
                .roots
                .get(&root)
                .and_then(|published| published.policy.as_ref())
                .is_some_and(|policy| policy.answers.untraced != 0)
        }

        pub(super) fn allow_local(&self, endpoints: &[Endpoint]) -> io::Result<()> {
            self.inner().controller.local(endpoints)
        }

        pub(super) fn map_task(&self, tid: Pid, root: u64) -> io::Result<()> {
            let group = process_of(tid)?;
            let mut inner = self.inner();
            inner.controller.task(group, root)?;
            inner.tasks.insert(tid, group);
            drop(inner);
            Ok(())
        }

        pub(super) fn unmap_task(&self, tid: Pid) -> io::Result<()> {
            let mut inner = self.inner();
            if let Some(group) = inner.tasks.remove(&tid)
                && !inner.tasks.values().any(|other| *other == group)
            {
                inner.controller.remove_task(group)?;
            }
            drop(inner);
            Ok(())
        }

        pub(super) fn remove_root(&self, root: u64) -> io::Result<()> {
            let mut inner = self.inner();
            inner.controller.remove_root(root)?;
            inner.roots.remove(&root);
            drop(inner);
            Ok(())
        }

        pub(super) fn terminate(&self) -> io::Result<()> {
            self.inner().controller.terminate()
        }

        pub(super) fn drain(&self) -> io::Result<Vec<Event>> {
            self.inner().controller.drain()
        }

        /// A duplicate of the ring's descriptor, which outlives the controller harmlessly.
        pub(super) fn events(&self) -> io::Result<OwnedFd> {
            let inner = self.inner();
            let ring: BorrowedFd<'_> = inner.controller.events();
            let owned = ring.try_clone_to_owned();
            drop(inner);
            owned
        }
    }

    fn process_of(tid: Pid) -> io::Result<u32> {
        crate::capture::proc_field::<u32>(format!("/proc/{tid}/status"), "Tgid:")?
            .ok_or_else(|| io::Error::other("missing tracee tgid"))
    }
}

#[cfg(not(feature = "privileged"))]
mod unavailable {
    use super::*;
    use std::os::fd::OwnedFd;

    /// Never constructed: privileged enforcement was not compiled.
    pub(crate) enum Kernel {}

    impl Kernel {
        pub(super) fn load() -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "privileged socket enforcement requires the privileged cargo feature",
            ))
        }
        pub(super) fn publish(&self, _: u64, _: KernelPolicy) -> io::Result<()> {
            match *self {}
        }
        pub(super) fn stale(&self, _: u64) -> io::Result<()> {
            match *self {}
        }
        pub(super) fn needs_refresh(&self, _: u64) -> bool {
            match *self {}
        }
        pub(super) fn restricts_untraced(&self, _: u64) -> bool {
            match *self {}
        }
        pub(super) fn allow_local(&self, _: &[Endpoint]) -> io::Result<()> {
            match *self {}
        }
        pub(super) fn map_task(&self, _: Pid, _: u64) -> io::Result<()> {
            match *self {}
        }
        pub(super) fn unmap_task(&self, _: Pid) -> io::Result<()> {
            match *self {}
        }
        pub(super) fn remove_root(&self, _: u64) -> io::Result<()> {
            match *self {}
        }
        pub(super) fn terminate(&self) -> io::Result<()> {
            match *self {}
        }
        pub(super) fn drain(&self) -> io::Result<Vec<Event>> {
            match *self {}
        }
        pub(super) fn events(&self) -> io::Result<OwnedFd> {
            match *self {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::{InvocationId, TraceRun};
    use std::sync::{
        Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    };
    use std::time::Duration;

    struct Consumer {
        tracing: Weak<Tracing>,
        fail: AtomicBool,
        observed: AtomicUsize,
    }

    impl Enforcer for Consumer {
        fn permits(
            &self,
            _: TraceRun,
            _: Option<InvocationId>,
            _: &[u8],
            _: Option<&FileTarget>,
        ) -> bool {
            unreachable!("this regression exercises event delivery, not policy")
        }

        fn refresh(&self, _: TraceRun, _: Option<&FileTarget>) -> io::Result<KernelPolicy> {
            unreachable!("this regression exercises event delivery, not policy")
        }

        fn event(&self, run: TraceRun, _: Option<InvocationId>, _: &Event) -> io::Result<()> {
            // Delivery must hold no trace lock: these take the production locks.
            let tracing = self.tracing.upgrade().unwrap();
            tracing.health(run)?;
            tracing.stale(run.root)?;
            if self.fail.load(Ordering::Acquire) {
                return Err(io::Error::other("consumer failure"));
            }
            self.observed.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    #[test]
    fn drained_events_keep_run_live_until_delivered() {
        let tracing = Tracing::shared().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let root = tracing
            .register_root(directory.path(), Arc::new(|_, _, _| Ok(())), None)
            .unwrap();
        let consumer = Arc::new(Consumer {
            tracing: Arc::downgrade(&tracing),
            fail: AtomicBool::new(false),
            observed: AtomicUsize::new(0),
        });
        tracing.enforce_peers(root, consumer.clone()).unwrap();
        let run = tracing.begin_run(root).unwrap();
        let event = Event {
            root: root.0,
            tid: nix::unistd::gettid().as_raw().cast_unsigned(),
            tgid: std::process::id(),
            ..Event::default()
        };
        // Its producer already ended: only the drained record keeps this run alive while
        // another thread tries to finish it.
        let mut deliveries =
            Deliveries::reserve(&tracing, &mut lock(&tracing.state), vec![event]).unwrap();
        assert_eq!(lock(&tracing.state).runs.get(&run).unwrap().deliveries, 1);
        let (started, start) = mpsc::channel();
        let (closed, close) = mpsc::channel();
        let retiring = Arc::clone(&tracing);
        let closer = std::thread::spawn(move || {
            started.send(()).unwrap();
            retiring.end_run(run).unwrap();
            closed.send(()).unwrap();
        });
        start.recv().unwrap();
        assert!(matches!(
            close.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        deliveries.deliver().unwrap();
        drop(deliveries);
        close.recv_timeout(Duration::from_secs(5)).unwrap();
        closer.join().unwrap();
        assert_eq!(consumer.observed.load(Ordering::Acquire), 1);

        // A failed delivery must release the rest of the batch, not strand shutdown.
        let run = tracing.begin_run(root).unwrap();
        consumer.fail.store(true, Ordering::Release);
        let mut deliveries =
            Deliveries::reserve(&tracing, &mut lock(&tracing.state), vec![event, event]).unwrap();
        assert_eq!(
            deliveries.deliver().unwrap_err().to_string(),
            "consumer failure"
        );
        drop(deliveries);
        assert_eq!(lock(&tracing.state).runs.get(&run).unwrap().deliveries, 0);
        tracing.end_run(run).unwrap();
        tracing.unregister_root(root).unwrap();
    }
}
