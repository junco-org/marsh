//! Kernel-mode map lifecycle and ordered delivery, owned by the tracing service.
use super::{RootId, Target, Tracing, lock};
#[cfg(feature = "privileged")]
use crate::{Contact, KernelPolicy};
use crate::{Endpoint, Enforcer, FileTarget, Syscall, local};
#[cfg(feature = "privileged")]
use marsh_lib::RecoverPoison as _;
use nix::unistd::Pid;
use std::{io, sync::Arc};

#[cfg(feature = "privileged")]
struct KernelContact {
    target: Target,
    synchronous: Option<Pid>,
    enforcer: Arc<dyn Enforcer>,
    event: Contact,
}

/// Pins command evidence while a drained batch is delivered without tracing locks.
#[cfg(feature = "privileged")]
pub(super) struct KernelContacts<'a> {
    tracing: &'a Tracing,
    records: Vec<KernelContact>,
    completed: usize,
}

#[cfg(feature = "privileged")]
impl KernelContacts<'_> {
    pub(super) fn deliver(&mut self) -> io::Result<()> {
        for record in &self.records[self.completed..] {
            let KernelContact {
                target,
                synchronous,
                enforcer,
                event,
            } = record;
            enforcer.contact(target.run, target.builtin, *event)?;
            if let Some(tid) = synchronous {
                lock(&self.tracing.contacts)
                    .entry(*tid)
                    .or_default()
                    .push(event.endpoint.sockaddr());
            } else {
                enforcer.asynchronous_contact(
                    target.run,
                    target.builtin,
                    self.tracing.sequence.next()?,
                    *event,
                )?;
            }
            if let Some(run) = lock(&self.tracing.state).runs.get_mut(&target.run) {
                run.contacts -= 1;
            }
            self.completed += 1;
            self.tracing.progress.notify_all();
        }
        Ok(())
    }
}

#[cfg(feature = "privileged")]
impl Drop for KernelContacts<'_> {
    fn drop(&mut self) {
        if self.completed == self.records.len() {
            return;
        }
        if std::thread::panicking() {
            self.tracing.fail("native kernel contact consumer panicked");
        }
        let mut state = lock(&self.tracing.state);
        for record in &self.records[self.completed..] {
            if let Some(run) = state.runs.get_mut(&record.target.run) {
                run.contacts -= 1;
            }
        }
        drop(state);
        self.tracing.progress.notify_all();
    }
}

impl Tracing {
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

    /// Invalidates kernel answers before a policy input changes.
    pub fn stale(&self, root: RootId) -> io::Result<()> {
        #[cfg(feature = "privileged")]
        if let Some(kernel) = &self.kernel {
            let mut kernel = lock(kernel);
            let epoch = kernel.epochs.entry(root.0).or_default();
            *epoch = epoch
                .checked_add(1)
                .ok_or_else(|| io::Error::other("kernel policy epoch exhausted"))?;
            kernel.controller.stale(root.0)?;
            if let Some(answers) = kernel.published.get_mut(&root.0) {
                answers.stale = 1;
            }
        }
        #[cfg(not(feature = "privileged"))]
        let _ = root;
        Ok(())
    }

    pub(super) fn refresh_kernel(
        &self,
        target: Target,
        enforcer: &dyn Enforcer,
        source: Option<&FileTarget>,
    ) -> io::Result<()> {
        #[cfg(feature = "privileged")]
        if let Some(kernel) = &self.kernel {
            let epoch = *lock(kernel).epochs.entry(target.run.root.0).or_default();
            let KernelPolicy {
                mut answers,
                networks,
                local,
            } = enforcer.refresh(target.run, target.builtin, source)?;
            let mut kernel = lock(kernel);
            if kernel
                .epochs
                .get(&target.run.root.0)
                .copied()
                .unwrap_or_default()
                != epoch
            {
                answers.stale = 1;
            }
            if kernel.networks.get(&target.run.root.0) != Some(&networks) {
                kernel.controller.configure(target.run.root.0, &networks)?;
                kernel.networks.insert(target.run.root.0, networks);
                kernel.published.remove(&target.run.root.0);
            }
            if kernel.local.get(&target.run.root.0) != Some(&local) {
                kernel.controller.local(target.run.root.0, &local)?;
                kernel.local.insert(target.run.root.0, local);
                kernel.published.remove(&target.run.root.0);
            }
            if kernel.published.get(&target.run.root.0) != Some(&answers) {
                kernel.controller.answers(target.run.root.0, answers)?;
                kernel.published.insert(target.run.root.0, answers);
            }
        }
        #[cfg(not(feature = "privileged"))]
        let _ = (target, enforcer, source);
        Ok(())
    }

    pub(super) fn map_task(&self, tid: Pid) -> io::Result<()> {
        #[cfg(feature = "privileged")]
        if let Some(kernel) = &self.kernel {
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
            let group = Self::task_group(tid)?;
            let mut kernel = lock(kernel);
            kernel.controller.task(group, target.run.root.0)?;
            kernel.tasks.insert(tid.as_raw().cast_unsigned(), group);
        }
        #[cfg(not(feature = "privileged"))]
        let _ = tid;
        Ok(())
    }

    #[cfg(feature = "privileged")]
    fn task_group(tid: Pid) -> io::Result<u32> {
        let text = std::fs::read_to_string(format!("/proc/{tid}/status"))?;
        text.lines()
            .find_map(|line| line.strip_prefix("Tgid:"))
            .ok_or_else(|| io::Error::other("missing tracee tgid"))?
            .trim()
            .parse()
            .map_err(io::Error::other)
    }

    pub(super) fn unmap_task(&self, tid: Pid) -> io::Result<()> {
        #[cfg(feature = "privileged")]
        if let Some(kernel) = &self.kernel {
            let mut kernel = lock(kernel);
            if let Some(group) = kernel.tasks.remove(&tid.as_raw().cast_unsigned())
                && !kernel.tasks.values().any(|other| *other == group)
            {
                kernel.controller.remove_task(group)?;
            }
            drop(kernel);
            lock(&self.contacts).remove(&tid);
        }
        #[cfg(not(feature = "privileged"))]
        let _ = tid;
        Ok(())
    }

    pub(super) fn remove_kernel_root(&self, root: RootId) -> io::Result<()> {
        #[cfg(feature = "privileged")]
        if let Some(kernel) = &self.kernel {
            let mut kernel = lock(kernel);
            kernel.controller.remove_root(root.0)?;
            kernel.epochs.remove(&root.0);
            kernel.networks.remove(&root.0);
            kernel.local.remove(&root.0);
            kernel.published.remove(&root.0);
        }
        #[cfg(not(feature = "privileged"))]
        let _ = root;
        Ok(())
    }

    #[cfg(feature = "privileged")]
    fn reserve_kernel_contacts<'a>(
        &'a self,
        state: &mut super::State,
        events: Vec<Contact>,
    ) -> io::Result<KernelContacts<'a>> {
        let records = events
            .into_iter()
            .map(|event| {
                let event_tid = Pid::from_raw(i32::try_from(event.tid).map_err(io::Error::other)?);
                let synchronous = state
                    .threads
                    .get(&event_tid)
                    .and_then(|thread| thread.inherited)
                    .filter(|target| target.run.root.0 == event.root);
                let target = synchronous
                    .or_else(|| {
                        state
                            .threads
                            .values()
                            .find(|thread| {
                                thread.process.as_ref().is_some_and(|process| {
                                    process.group.as_raw().cast_unsigned() == event.tgid
                                })
                            })
                            .and_then(|thread| thread.inherited)
                            .filter(|target| target.run.root.0 == event.root)
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
                    .ok_or_else(|| {
                        io::Error::other("kernel socket contact has no live trace root")
                    })?;
                state.run(target.run)?;
                let enforcer = state
                    .roots
                    .get(&target.run.root)
                    .and_then(|root| root.enforcer.clone())
                    .ok_or_else(|| io::Error::other("kernel contact lost its policy engine"))?;
                Ok(KernelContact {
                    target,
                    synchronous: synchronous.map(|_| event_tid),
                    enforcer,
                    event,
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        for record in &records {
            state
                .runs
                .get_mut(&record.target.run)
                .ok_or_else(|| io::Error::other("kernel contact lost its trace run"))?
                .contacts += 1;
        }
        Ok(KernelContacts {
            tracing: self,
            records,
            completed: 0,
        })
    }

    pub(super) fn kernel_contacts(
        &self,
        tid: Pid,
        info: Option<Syscall>,
    ) -> io::Result<Option<Syscall>> {
        #[cfg(feature = "privileged")]
        if let Some(kernel) = &self.kernel {
            let mut info = info;
            // Reserve attribution while the drain's state lock is still held: a different
            // tracer can otherwise retire a root whose records this batch already removed.
            let mut state = lock(&self.state);
            let events = lock(kernel).controller.drain()?;
            let mut contacts = self.reserve_kernel_contacts(&mut state, events)?;
            drop(state);
            if let Err(error) = contacts.deliver() {
                // Other owners must see the failure before releasing their undelivered records
                // can make them quiescent enough to publish incomplete evidence.
                self.fail(format!("native observation: {error}"));
                return Err(error);
            }
            drop(contacts);
            let mut state = lock(&self.state);
            let run = state
                .threads
                .get(&tid)
                .and_then(|thread| thread.inherited)
                .map(|target| target.run);
            while run.is_some_and(|run| state.runs.get(&run).is_some_and(|run| run.contacts != 0)) {
                state.check()?;
                state = self.progress.wait(state).recover();
            }
            drop(state);
            if let Some(call) = &mut info {
                call.peers
                    .extend(lock(&self.contacts).remove(&tid).unwrap_or_default());
            }
            return Ok(info);
        }
        let _ = tid;
        Ok(info)
    }

    /// Local socket endpoints held by traced tasks belonging to these roots.
    pub fn local_endpoints(&self, roots: &[RootId]) -> io::Result<Vec<Endpoint>> {
        let tasks: Vec<Pid> = lock(&self.state)
            .threads
            .iter()
            .filter_map(|(tid, thread)| {
                thread
                    .inherited
                    .filter(|target| roots.contains(&target.run.root))
                    .map(|_| *tid)
            })
            .collect();
        let endpoints = local::endpoints(&tasks)?;
        Ok(endpoints
            .into_iter()
            .filter(|endpoint| {
                self.receivers(&endpoint.sockaddr())
                    .is_some_and(|listeners| {
                        !listeners.is_empty()
                            && listeners
                                .iter()
                                .all(|holders| holders.iter().any(|holder| roots.contains(holder)))
                    })
            })
            .collect())
    }
}

#[cfg(all(test, feature = "privileged"))]
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
            unreachable!("this regression exercises contact delivery, not policy refresh")
        }

        fn refresh(
            &self,
            _: TraceRun,
            _: Option<InvocationId>,
            _: Option<&FileTarget>,
        ) -> io::Result<KernelPolicy> {
            unreachable!("this regression exercises contact delivery, not policy refresh")
        }

        fn contact(&self, run: TraceRun, _: Option<InvocationId>, _: Contact) -> io::Result<()> {
            // These acquire both production locks when present: delivery cannot hold either.
            let tracing = self.tracing.upgrade().unwrap();
            tracing.health(run)?;
            tracing.stale(run.root)?;
            if self.fail.load(Ordering::Acquire) {
                return Err(io::Error::other("consumer failure"));
            }
            Ok(())
        }

        fn asynchronous_contact(
            &self,
            run: TraceRun,
            _: Option<InvocationId>,
            _: u64,
            _: Contact,
        ) -> io::Result<()> {
            self.tracing.upgrade().unwrap().health(run)?;
            self.observed.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    #[test]
    fn drained_contacts_keep_run_live_until_callbacks_finish() {
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
        let event = Contact {
            root: root.0,
            tid: nix::unistd::gettid().as_raw().cast_unsigned(),
            tgid: std::process::id(),
            ..Contact::default()
        };
        // Its producer has already ended: only the drained record keeps this real registered
        // run alive while another thread tries to finish it.
        let mut contacts = tracing
            .reserve_kernel_contacts(&mut lock(&tracing.state), vec![event])
            .unwrap();
        assert_eq!(lock(&tracing.state).runs.get(&run).unwrap().contacts, 1);
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
        contacts.deliver().unwrap();
        drop(contacts);
        close.recv_timeout(Duration::from_secs(5)).unwrap();
        closer.join().unwrap();
        assert_eq!(consumer.observed.load(Ordering::Acquire), 1);

        // A failed callback must release the whole remaining batch too, not strand shutdown.
        let run = tracing.begin_run(root).unwrap();
        consumer.fail.store(true, Ordering::Release);
        let mut contacts = tracing
            .reserve_kernel_contacts(&mut lock(&tracing.state), vec![event, event])
            .unwrap();
        assert_eq!(
            contacts.deliver().unwrap_err().to_string(),
            "consumer failure"
        );
        drop(contacts);
        assert_eq!(lock(&tracing.state).runs.get(&run).unwrap().contacts, 0);
        tracing.end_run(run).unwrap();
        tracing.unregister_root(root).unwrap();
    }
}
