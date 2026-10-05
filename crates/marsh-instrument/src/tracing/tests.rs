//! In-process tracing of spawned commands and host records, against real processes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::future::poll_fn;
use std::panic::AssertUnwindSafe;
use std::pin::pin;
use std::process::Command;
use std::sync::mpsc;
use std::task::{Context, Poll, Waker};

/// Whether evidence names a file ending in `name` by path, descriptor, returned FD or cwd.
fn mentions(evidence: &[Syscall], name: &[u8]) -> bool {
    evidence.iter().any(|info| {
        info.paths.iter().any(|(_, bytes)| bytes.ends_with(name))
            || info
                .descriptors
                .iter()
                .filter_map(|(_, target)| target.as_ref())
                .chain(info.return_fd.as_ref())
                .chain(info.cwd.as_ref())
                .any(|target| target.path.ends_with(name))
    })
}

/// One registered root collecting everything classified against it.
struct Fixture {
    service: Arc<Tracing>,
    directory: tempfile::TempDir,
    root: RootId,
    seen: Arc<Mutex<Vec<Syscall>>>,
    /// The invocation each classified record was attributed to, by entry order.
    owners: Arc<Mutex<HashMap<u64, Option<InvocationId>>>>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_exec(|_| None)
    }

    /// A fixture whose root tracks the execs `hooks` selects; it is given the service weakly.
    fn with_exec(hooks: impl FnOnce(Weak<Tracing>) -> Option<ExecHooks>) -> Self {
        let service = Tracing::shared().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let owners = Arc::new(Mutex::new(HashMap::new()));
        let (sink, owned) = (Arc::clone(&seen), Arc::clone(&owners));
        let root = service
            .register_root(
                &directory.path().canonicalize().unwrap(),
                Arc::new(move |_, owner, info: Syscall| {
                    lock(&owned).insert(info.entry_order, owner);
                    lock(&sink).push(info);
                    Ok(())
                }),
                hooks(Arc::downgrade(&service)),
            )
            .unwrap();
        Self {
            service,
            directory,
            root,
            seen,
            owners,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().canonicalize().unwrap().join(name)
    }

    /// Spawns `sh -c script` inside `scope`, returning the child and its event stream.
    fn spawn(
        &self,
        scope: &TraceScope,
        script: &str,
    ) -> io::Result<(TracedChild, mpsc::Receiver<ChildEvent>)> {
        let (sender, events) = mpsc::channel();
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        let _guard = scope.enter();
        let child = self.service.spawn(
            command,
            Box::new(move |event| {
                let _ = sender.send(event);
            }),
        )?;
        Ok((child, events))
    }

    fn finish(self, run: TraceRun) {
        self.service.end_run(run).unwrap();
        self.service.unregister_root(self.root).unwrap();
    }
}

fn exited(events: &mpsc::Receiver<ChildEvent>) -> std::process::ExitStatus {
    loop {
        match events.recv().expect("the tracer reports its command's end") {
            ChildEvent::Exited(status) => return status,
            ChildEvent::Stopped => {}
        }
    }
}

#[test]
fn spawned_commands_are_traced_attributed_and_reaped_in_process() {
    let fixture = Fixture::new();
    let run = fixture.service.begin_run(fixture.root).unwrap();
    let scope = fixture.service.scope(run, None).unwrap();
    let file = fixture.path("traced.txt");
    let (child, events) = fixture
        .spawn(&scope, &format!("printf traced > '{}'; /bin/true", file.display()))
        .unwrap();
    assert!(exited(&events).success());
    fixture.service.quiesce(run).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"traced");
    let seen = std::mem::take(&mut *lock(&fixture.seen));
    // The command's own write, and its child's exec, are evidence of the run.
    assert!(mentions(&seen, b"traced.txt"));
    assert!(seen.iter().any(|call| call.info.syscall == Sysno::execve
        && call.path(0).is_some_and(|path| path.ends_with(b"/true"))));
    // Its creation is stated first, so its descriptors are inherited from the host.
    let first = seen.first().expect("records");
    assert_eq!(first.info.syscall, Sysno::fork);
    assert!(matches!(first.info.result, RetCode::Ok(pid) if pid.cast_unsigned() == child.pid));
    // The host itself is never traced.
    assert_eq!(
        proc_field::<i32>("/proc/self/status", "TracerPid:").unwrap(),
        Some(0)
    );
    fixture.finish(run);
}

fn probe(path: &Path) -> HostCall<'_> {
    HostCall::Metadata {
        path,
        follow: true,
        errno: None,
    }
}

#[test]
fn host_records_count_only_inside_workload_scopes() {
    let fixture = Fixture::new();
    let run = fixture.service.begin_run(fixture.root).unwrap();
    let scope = fixture.service.scope(run, None).unwrap();
    let internal = fixture.service.internal_scope().unwrap();
    let file = fixture.path("probed.txt");
    std::fs::write(&file, b"x").unwrap();
    let outside = fixture.path("outside.txt");
    let hidden = fixture.path("internal.txt");
    fixture.service.host(probe(&outside)).unwrap();
    {
        let _workload = scope.enter();
        {
            let _internal = internal.enter();
            fixture.service.host(probe(&hidden)).unwrap();
        }
        fixture.service.host(probe(&file)).unwrap();
        let opened = std::fs::File::open(&file).unwrap();
        fixture
            .service
            .host(HostCall::Open {
                path: &file,
                result: Ok(opened.as_raw_fd()),
            })
            .unwrap();
    }
    let seen = std::mem::take(&mut *lock(&fixture.seen));
    assert!(!mentions(&seen, b"outside.txt"));
    assert!(!mentions(&seen, b"internal.txt"));
    assert!(seen.iter().any(|call| call.info.syscall == Sysno::newfstatat
        && call.path(1).is_some_and(|path| path.ends_with(b"probed.txt"))));
    // A read-only open also stands for the reads made through it.
    assert!(seen.iter().any(|call| call.info.syscall == Sysno::read
        && call
            .fd(0)
            .unwrap()
            .is_some_and(|target| target.path.ends_with(b"probed.txt"))));
    fixture.finish(run);
}

#[test]
fn failed_launches_and_unscoped_spawns_leave_nothing_running() {
    let fixture = Fixture::new();
    let run = fixture.service.begin_run(fixture.root).unwrap();
    let scope = fixture.service.scope(run, None).unwrap();
    let missing = {
        let _guard = scope.enter();
        fixture
            .service
            .spawn(Command::new("/nonexistent/program"), Box::new(|_| {}))
    };
    assert_eq!(
        missing.err().map(|error| error.kind()),
        Some(io::ErrorKind::NotFound)
    );
    let unscoped = fixture
        .service
        .spawn(Command::new("/bin/true"), Box::new(|_| {}));
    assert!(unscoped.is_err());
    let unresolved = {
        let _guard = scope.enter();
        fixture.service.spawn(Command::new("true"), Box::new(|_| {}))
    };
    assert_eq!(
        unresolved.err().map(|error| error.kind()),
        Some(io::ErrorKind::InvalidInput)
    );
    // Nothing is left attributed to the run, and it still spawns.
    fixture.service.quiesce(run).unwrap();
    let (_, events) = fixture.spawn(&scope, "exit 3").unwrap();
    assert_eq!(exited(&events).code(), Some(3));
    fixture.finish(run);
}

/// What the exec hooks of a tracking fixture saw: each admitted command, whether its marker
/// existed when it was admitted, and each end.
#[derive(Default)]
struct Tracked {
    begun: Vec<(InvocationId, ExecCommand, bool)>,
    ended: Vec<(InvocationId, Option<std::process::ExitStatus>)>,
}

/// A fixture tracking every exec of a file named `tracked` by a task no invocation owns.
/// `marker` is checked at admission; `cancel` cancels the run from inside the begin hook.
fn tracking(marker: PathBuf, cancel: bool) -> (Fixture, Arc<Mutex<Tracked>>) {
    let tracked = Arc::new(Mutex::new(Tracked::default()));
    let (begun, ended) = (Arc::clone(&tracked), Arc::clone(&tracked));
    let fixture = Fixture::with_exec(move |service| {
        Some(ExecHooks {
            select: Arc::new(|_, owner, path| {
                owner.is_none() && path.file_name().is_some_and(|name| name == "tracked")
            }),
            begin: Arc::new(move |run, _, command| {
                let service = service.upgrade().expect("live service");
                let id = service.invocation(run)?;
                if cancel {
                    service.cancel(run)?;
                }
                let absent = !marker.exists();
                lock(&begun).begun.push((id, command, absent));
                Ok(ExecDecision::Track(id))
            }),
            end: Arc::new(move |_, id, status| {
                lock(&ended).ended.push((id, status));
                Ok(())
            }),
        })
    });
    (fixture, tracked)
}

impl Fixture {
    /// The owner of the first record naming a path that ends in `name`.
    fn owner_of(&self, name: &[u8]) -> (u64, Option<InvocationId>) {
        let entry = lock(&self.seen)
            .iter()
            .find(|call| call.paths.iter().any(|(_, path)| path.ends_with(name)))
            .map_or_else(
                || panic!("no record of {}", String::from_utf8_lossy(name)),
                |call| call.entry_order,
            );
        (entry, lock(&self.owners)[&entry])
    }
}

#[test]
fn tracked_execs_own_their_whole_tree_until_their_end() {
    let probe = tempfile::tempdir().unwrap();
    let sub = probe.path().canonicalize().unwrap();
    let (fixture, tracked) = tracking(sub.join("marker"), false);
    let program = fixture.path("tracked");
    std::fs::copy("/bin/sh", &program).unwrap();
    let run = fixture.service.begin_run(fixture.root).unwrap();
    let scope = fixture.service.scope(run, None).unwrap();

    // execve of an absolute path from a changed cwd, with an environment the host lacks; a
    // forked descendant writes too, and the caller writes after it.
    let inner = "printf x > marker; /bin/sh -c 'printf y > child'; exit 3";
    let (_, events) = fixture
        .spawn(
            &scope,
            &format!(
                "cd {sub}; SELECTOR=fixture {program} -c \"{inner}\" 'a b' ''; printf after > after",
                sub = sub.display(),
                program = program.display()
            ),
        )
        .unwrap();
    assert!(exited(&events).success());

    // A non-leader thread execs; a descriptor-only execveat runs a vforking child.
    let vfork = fixture.path("vfork.py");
    std::fs::write(
        &vfork,
        "import subprocess\nsubprocess.run(['/bin/sh', '-c', 'printf v > vchild'], check=True)\n",
    )
    .unwrap();
    let threaded = fixture.path("threaded.py");
    std::fs::write(
        &threaded,
        format!(
            "import os, threading\nos.chdir({sub:?})\nthreading.Thread(target=lambda: \
             os.execv({program:?}, ['tracked', '-c', 'printf n > nonleader'])).start()\n\
             threading.Event().wait()\n",
            sub = sub.display().to_string(),
            program = program.display().to_string()
        ),
    )
    .unwrap();
    let fexecve = fixture.path("fexecve.py");
    std::fs::write(
        &fexecve,
        format!(
            "import os\nfd = os.open({program:?}, os.O_RDONLY)\nos.chdir({sub:?})\n\
             os.execve(fd, ['tracked', '-c', 'exec python3 {vfork}'], \
             {{'SELECTOR': 'fd', 'PATH': '/usr/bin:/bin'}})\n",
            sub = sub.display().to_string(),
            program = program.display().to_string(),
            vfork = vfork.display()
        ),
    )
    .unwrap();
    for script in [&threaded, &fexecve] {
        let (_, events) = fixture
            .spawn(&scope, &format!("exec python3 {}", script.display()))
            .unwrap();
        assert!(exited(&events).success());
    }
    fixture.service.quiesce(run).unwrap();

    let tracked = std::mem::take(&mut *lock(&tracked));
    assert_eq!(tracked.begun.len(), 3);
    assert_eq!(
        tracked.ended.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        tracked.begun.iter().map(|(id, ..)| *id).collect::<Vec<_>>(),
        "every tracked invocation ends exactly once"
    );

    let (id, command, absent) = &tracked.begun[0];
    assert!(absent, "admitted before its first write");
    assert_eq!(command.program, program);
    assert_eq!(
        command.argv,
        [program.to_str().unwrap(), "-c", inner, "a b", ""]
    );
    assert!(command.environment.iter().any(|entry| entry == "SELECTOR=fixture"));
    assert_eq!(command.cwd, sub);
    assert_eq!(tracked.ended[0].1.and_then(|status| status.code()), Some(3));
    assert_eq!(fixture.owner_of(b"marker").1, Some(*id));
    assert_eq!(fixture.owner_of(b"child").1, Some(*id), "descendants stay owned");
    let (after, owner) = fixture.owner_of(b"after");
    assert_eq!(owner, None);
    let (start, finish) = fixture.service.invocation_orders(run, *id).unwrap();
    assert_eq!(start, command.entry_order);
    assert!(after > finish, "the caller resumes only after the end hook");

    let (id, command, _) = &tracked.begun[1];
    assert_eq!(command.argv, ["tracked", "-c", "printf n > nonleader"]);
    assert_eq!(fixture.owner_of(b"nonleader").1, Some(*id));

    let (id, command, _) = &tracked.begun[2];
    assert_eq!(command.program, program);
    assert_eq!(command.environment, ["SELECTOR=fd", "PATH=/usr/bin:/bin"]);
    assert_eq!(fixture.owner_of(b"vchild").1, Some(*id), "vforked children stay owned");
    assert!(tracked.ended.iter().all(|(_, status)| status.is_some()));
    fixture.finish(run);
}

#[test]
fn cancelling_while_an_admitted_exec_is_held_ends_it_incomplete() {
    let probe = tempfile::tempdir().unwrap();
    let (fixture, tracked) = tracking(probe.path().join("marker"), true);
    let program = fixture.path("tracked");
    std::fs::copy("/bin/sh", &program).unwrap();
    let run = fixture.service.begin_run(fixture.root).unwrap();
    let scope = fixture.service.scope(run, None).unwrap();
    let (_, events) = fixture
        .spawn(&scope, &format!("exec {} -c 'exec /bin/sleep 60'", program.display()))
        .unwrap();
    assert_eq!(exited(&events).signal(), Some(libc::SIGKILL));
    fixture.service.quiesce(run).unwrap();
    let tracked = lock(&tracked);
    assert_eq!(tracked.begun.len(), 1);
    assert_eq!(tracked.ended.len(), 1);
    assert_eq!(tracked.ended[0].1, None, "a cancelled invocation never completes");
    drop(tracked);
    fixture.finish(run);
}

#[test]
fn cancellation_kills_traced_commands() {
    let fixture = Fixture::new();
    let run = fixture.service.begin_run(fixture.root).unwrap();
    let scope = fixture.service.scope(run, None).unwrap();
    let (_, events) = fixture.spawn(&scope, "exec /bin/sleep 60").unwrap();
    assert!(fixture.service.cancel(run).unwrap() >= 1);
    assert_eq!(exited(&events).signal(), Some(libc::SIGKILL));
    fixture.service.quiesce(run).unwrap();
    fixture.finish(run);
}

thread_local! { static LABEL: RefCell<Option<&'static str>> = const { RefCell::new(None) }; }

fn label() -> Option<&'static str> {
    LABEL.with(|label| *label.borrow())
}

/// A caller-defined per-poll context: installs one thread-local label.
struct Label(&'static str);

/// Restores the label that was current when its scope was entered.
struct Restore(Option<&'static str>);

impl Drop for Restore {
    fn drop(&mut self) {
        LABEL.with(|label| {
            label.replace(self.0.take());
        });
    }
}

impl PollScope for Label {
    type Guard = Restore;
    fn enter(&self) -> Restore {
        Restore(LABEL.with(|label| label.replace(Some(self.0))))
    }
}

#[test]
fn poll_scope_restores_context_between_polls() {
    let observed = RefCell::new(Vec::new());
    let mut context = Context::from_waker(Waker::noop());
    let outer = Label("outer").enter();
    {
        let mut polls = 0_u8;
        let mut inner = pin!(Scoped::new(
            poll_fn(|_| {
                observed.borrow_mut().push(("inner", label()));
                polls += 1;
                if polls == 1 {
                    Poll::Pending
                } else {
                    Poll::Ready(polls)
                }
            }),
            Label("inner")
        ));
        let mut future = pin!(Scoped::new(
            poll_fn(|cx| {
                let poll = inner.as_mut().poll(cx);
                observed.borrow_mut().push(("middle", label()));
                poll
            }),
            Label("middle")
        ));
        assert!(future.as_mut().poll(&mut context).is_pending());
        assert_eq!(label(), Some("outer"));
        assert_eq!(future.as_mut().poll(&mut context), Poll::Ready(2));
        assert_eq!(label(), Some("outer"));
    }
    drop(outer);
    assert_eq!(label(), None);
    assert_eq!(
        *observed.borrow(),
        [
            ("inner", Some("inner")),
            ("middle", Some("middle")),
            ("inner", Some("inner")),
            ("middle", Some("middle"))
        ]
    );
}

#[test]
fn poll_scope_restores_context_after_unwind() {
    let outer = Label("outer").enter();
    let payload = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut context = Context::from_waker(Waker::noop());
        let mut inner = pin!(Scoped::new(
            poll_fn(|_| -> Poll<()> {
                assert_eq!(label(), Some("inner"));
                panic!("scoped poll unwinds");
            }),
            Label("inner")
        ));
        let mut future = pin!(Scoped::new(
            poll_fn(|cx| -> Poll<()> {
                let payload =
                    std::panic::catch_unwind(AssertUnwindSafe(|| inner.as_mut().poll(cx)))
                        .unwrap_err();
                assert_eq!(label(), Some("middle"));
                std::panic::resume_unwind(payload)
            }),
            Label("middle")
        ));
        let _ = future.as_mut().poll(&mut context);
    }))
    .unwrap_err();
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"scoped poll unwinds"));
    assert_eq!(label(), Some("outer"));
    drop(outer);
    assert_eq!(label(), None);
}
