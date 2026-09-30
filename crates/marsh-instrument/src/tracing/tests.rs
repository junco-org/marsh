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
}

impl Fixture {
    fn new() -> Self {
        let service = Tracing::shared().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let root = service
            .register_root(
                &directory.path().canonicalize().unwrap(),
                Arc::new(move |_, _, info| {
                    lock(&sink).push(info);
                    Ok(())
                }),
            )
            .unwrap();
        Self {
            service,
            directory,
            root,
            seen,
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
