//! Every host attachment runs in a disposable, freshly exec'd test process.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::cell::RefCell;
use std::future::poll_fn;
use std::os::unix::ffi::OsStrExt;
use std::panic::AssertUnwindSafe;
use std::pin::pin;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;
use std::task::{Context, Poll, Waker};

const OUTSIDE: &[u8] = b"outside-poll-file";

fn host_scenario() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native-file");
    std::fs::write(&path, b"native bytes").unwrap();
    let outside = directory.path().join(std::ffi::OsStr::from_bytes(OUTSIDE));
    std::fs::write(&outside, b"outside bytes").unwrap();
    for _ in 0..2 {
        let service = Tracing::shared();
        let evidence = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&evidence);
        // The shared helper must outlive the thread/runtime that first requested attachment.
        let worker_service = Arc::clone(&service);
        let root_path = directory.path().to_path_buf();
        let root = std::thread::spawn(move || {
            worker_service.register_root(
                &root_path,
                Arc::new(move |_, _, info| {
                    captured.lock().unwrap().push(info);
                    Ok(())
                }),
            )
        })
        .join()
        .unwrap()
        .unwrap();
        let run = service.begin_run(root).unwrap();
        let scope = service.scope(run, None).unwrap();
        let forged = format!("{}{}/leave", service.prefix, scope.id.0);
        {
            // The scope belongs only to the two polls; the read between them is unattributed.
            let mut polls = 0_u8;
            let mut scoped = pin!(Scoped::new(
                poll_fn(|_| {
                    polls += 1;
                    if polls == 1 {
                        assert_eq!(std::fs::read(&path).unwrap(), b"native bytes");
                        return Poll::Pending;
                    }
                    let output = std::process::Command::new("/bin/cat")
                        .arg(&path)
                        .output()
                        .unwrap();
                    assert_eq!(output.stdout, b"native bytes");
                    let output = std::process::Command::new("python3").args(["-c",
                    "import os,sys\ntry: os.readlink(sys.argv[1])\nexcept OSError: pass\nsys.stdout.buffer.write(open(sys.argv[2], 'rb').read())"])
                    .arg(&forged).arg(&path).output().unwrap();
                    assert!(output.status.success());
                    assert_eq!(output.stdout, b"native bytes");
                    Poll::Ready(())
                }),
                scope
            ));
            let mut context = Context::from_waker(Waker::noop());
            assert!(scoped.as_mut().poll(&mut context).is_pending());
            assert_eq!(std::fs::read(&outside).unwrap(), b"outside bytes");
            assert!(scoped.as_mut().poll(&mut context).is_ready());
        }
        service.end_run(run).unwrap();
        let evidence = evidence.lock().unwrap();
        assert!(evidence.iter().any(|info| info.info.syscall == Sysno::read
            && info.descriptors.iter().any(|(_, target)| {
                target.as_ref().is_some_and(|target| {
                    Path::new(std::ffi::OsStr::from_bytes(&target.path)) == path
                })
            })));
        assert!(
            evidence
                .iter()
                .any(|info| info.info.syscall == Sysno::execve)
        );
        assert!(
            evidence
                .iter()
                .any(|info| info.path(0) == Some(forged.as_bytes())),
            "external scope forgery remains ordinary evidence"
        );
        assert!(
            !evidence.iter().any(|info| info
                .paths
                .iter()
                .any(|(_, bytes)| bytes.ends_with(OUTSIDE))
                || info
                    .descriptors
                    .iter()
                    .filter_map(|(_, target)| target.as_ref())
                    .chain(info.return_fd.as_ref())
                    .chain(info.cwd.as_ref())
                    .any(|target| target.path.ends_with(OUTSIDE))),
            "a read between scoped polls is not attributed"
        );
        drop(evidence);
        service.unregister_root(root).unwrap();
    }
}

fn pressure(calls: usize) {
    for _ in 0..calls {
        let mut sink = [0_u8];
        // SAFETY: the constant is NUL terminated and the output is a live one-byte buffer.
        unsafe {
            libc::readlink(
                c"/proc/self/native-pressure".as_ptr(),
                sink.as_mut_ptr().cast(),
                1,
            );
        }
    }
}

fn blocked_scenario(overflow: bool) {
    let directory = tempfile::tempdir().unwrap();
    let tracing = Tracing::shared();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let gate = Mutex::new(Some(gate));
    let seen = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&seen);
    let root = tracing
        .register_root(
            directory.path(),
            Arc::new(move |_, _, info| {
                if info.info.syscall == Sysno::readlink {
                    counted.fetch_add(1, Ordering::Relaxed);
                    let taken = gate.lock().unwrap().take();
                    if let Some(gate) = taken {
                        entered.send(()).unwrap();
                        gate.recv().unwrap();
                    }
                }
                Ok(())
            }),
        )
        .unwrap();
    let run = tracing.begin_run(root).unwrap();
    let scope = tracing.scope(run, None).unwrap();
    {
        let _guard = scope.enter();
        pressure(1);
        ready.recv_timeout(DEADLINE).unwrap();
        pressure(if overflow { 40_000 } else { 4095 });
    }
    drop(scope);
    if overflow {
        let deadline = Instant::now() + DEADLINE;
        while tracing.health(run).is_ok() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            tracing.health(run).is_err(),
            "pidfd monitor reports queue loss while callback is blocked"
        );
        release.send(()).unwrap();
        assert!(tracing.drain(run).is_err());
        // The test owns no workload producers; release this failed test registration explicitly.
        lock(&tracing.state).runs.remove(&run);
    } else {
        assert_eq!(seen.load(Ordering::Relaxed), 1);
        release.send(()).unwrap();
        tracing.end_run(run).unwrap();
        assert_eq!(seen.load(Ordering::Relaxed), 4096);
    }
    tracing.unregister_root(root).unwrap();
    host_scenario();
}

#[test]
fn native_host_lifecycle_and_backpressure() {
    if let Some(scenario) = std::env::var_os("MARSH_NATIVE_SCENARIO") {
        match scenario.to_str().unwrap() {
            "host" => host_scenario(),
            "burst" => blocked_scenario(false),
            "overflow" => blocked_scenario(true),
            _ => panic!("unknown scenario"),
        }
        println!("native scenario completed");
        return;
    }
    for scenario in ["host", "burst", "overflow"] {
        let test = "tracing::tests::native_host_lifecycle_and_backpressure";
        crate::observation::tests::isolated(
            test,
            "MARSH_NATIVE_SCENARIO",
            scenario,
            Duration::from_secs(45),
            "native scenario completed",
        );
    }
}

#[test]
fn transport_rejects_incomplete_invalid_and_gapped_frames() {
    // The concurrent writer lets an oversized frame outgrow the socket buffer.
    for frame in [
        b"{".to_vec(),
        b"not json\n".to_vec(),
        b"[2,1,0,null,null]\n".to_vec(),
        vec![b'x'; FRAME_LIMIT + 1],
    ] {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let thread = std::thread::spawn(move || {
            writer.write_all(PRELUDE).unwrap();
            let _ = writer.write_all(&frame);
        });
        assert!(receive(&Weak::new(), reader).is_err());
        thread.join().unwrap();
    }
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
