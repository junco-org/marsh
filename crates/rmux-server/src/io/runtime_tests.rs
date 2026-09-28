//! What happens to a job when the caller that asked for it was not this daemon.
//!
//! The facade is reachable from places that are not a daemon task: a status producer on its own
//! thread, a library consumer driving its own runtime, a completion callback, a detached queue.
//! Every one of those can admit a job — and a job is not a value, it is a set of tasks and a set
//! of descriptors registered with a reactor. Created on the caller's runtime, they are the
//! caller's: when that runtime is dropped, the pumps are cancelled and the reactor the job's
//! pipes are registered with disappears, leaving a job that every observation this daemon
//! publishes still calls running and that will never produce another byte.
//!
//! These tests enter through the two shapes that are not this daemon's runtime — another Tokio
//! runtime, and a plain thread with no runtime at all — and then destroy the caller's execution
//! context before asking the job to do anything. Both must survive it.

use std::sync::{Arc, Mutex as StdMutex, PoisonError};

use marsh_core::shellmux::{CommandOptions, JobIo, SpawnOptions, TerminalGeometry};

use crate::io::ShellIo;
use crate::shell_frontend::FrontendMessage;

/// Default geometry for the test host; nothing here opens a terminal, but a mux refuses a zero.
const ROWS: u16 = 24;
/// Default width, as above.
const COLS: u16 = 80;

/// A facade bound to a runtime this test owns, and everything that has to outlive it.
///
/// Field order is the drop order and is load-bearing: the facade releases the core, which
/// reclaims every snapshot, before the runtime its tasks live on goes away, and the scratch tree
/// the seed lives in is last of all.
struct Host {
    /// The facade under test, bound to [`Self::runtime`].
    io: ShellIo,
    /// Everything the frontend queue has delivered, so a test can check that bytes still arrive.
    seen: Arc<StdMutex<Vec<u8>>>,
    /// The daemon runtime: the one every job of this host must end up on.
    runtime: tokio::runtime::Runtime,
    /// The seed's tree. Deleting it early would fail a test for a reason it is not about.
    _scratch: tempfile::TempDir,
}

impl Host {
    /// Builds a host over a private seed, with its frontend queue drained on its own runtime.
    ///
    /// The mux is constructed *inside* that runtime, because a mux binds the runtime it is built
    /// on and this test's whole subject is which runtime that is.
    fn open() -> Self {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = scratch.path().canonicalize().expect("canonical scratch");
        let seed = root.join("seed");
        std::fs::create_dir_all(&seed).expect("seed tree");
        let filesystem = Arc::new(marsh_btrfs::fake::CopyTree::new());
        filesystem.register(&seed);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("the host runtime");

        let seen = Arc::new(StdMutex::new(Vec::new()));
        let io = runtime.block_on({
            let seen = Arc::clone(&seen);
            let socket = root.join("rmux.sock");
            async move {
                let (io, events) = ShellIo::new(
                    &seed,
                    brush_core::env::ShellEnvironment::new(),
                    TerminalGeometry {
                        rows: ROWS,
                        cols: COLS,
                    },
                    tokio::runtime::Handle::current(),
                    socket,
                    |mut profile, frontend| {
                        // Every command takes the managed route these tests were written against.
                        profile.sandbox_policy = marsh_core::SandboxPolicy::allow();
                        marsh_core::test_support::mux(profile, frontend, filesystem)
                    },
                )
                .expect("open the test engine");
                tokio::spawn(drain(events, seen));
                io
            }
        });

        Self {
            io,
            seen,
            runtime,
            _scratch: scratch,
        }
    }

    /// How many tasks are alive on the runtime this host is bound to.
    fn alive_tasks(&self) -> usize {
        self.runtime.metrics().num_alive_tasks()
    }

    /// The bytes the frontend queue has delivered so far.
    fn output(&self) -> Vec<u8> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The delivered bytes, once they are `expected` or the deadline passes.
    ///
    /// A command's verdict and its bytes are separate events: the line concludes when the shell
    /// is done with it, while the last chunk is still on its way through the pipe, its pump and
    /// the frontend queue. So a test asserting on output waits for it — and a pump that is not
    /// running simply never arrives, which is exactly the failure being guarded against.
    fn await_output(&self, expected: &[u8]) -> Vec<u8> {
        self.runtime.block_on(async {
            for _ in 0..200_u32 {
                if self.output() == expected {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });
        self.output()
    }

    /// Checks that the `echo marsh` line which concluded with `exit_code` ran on this host and
    /// that its bytes arrived (`delivered` says why they must have), then releases the core, so
    /// the seed's snapshots are reclaimed before the scratch tree goes.
    fn finish_after_echo(&self, exit_code: Option<i32>, delivered: &str) {
        assert_eq!(
            exit_code,
            Some(0),
            "the command ran to completion on the host's runtime"
        );
        assert_eq!(
            self.await_output(b"marsh\n"),
            b"marsh\n".to_vec(),
            "{delivered}"
        );
        self.runtime
            .block_on(self.io.shutdown())
            .expect("shut the host down");
    }
}

/// Drains the frontend queue the way this daemon's own consumer does, minus the presentation.
///
/// Two things a test needs from it. Output is *receipted*: a chunk whose receipt is never
/// completed stalls that stream's pump for good, so a host with nothing draining it would prove
/// nothing about whether its pumps are alive. And the bytes are kept, because "the job still
/// produces output after the caller's runtime is gone" is the observation these tests are for.
async fn drain(
    mut events: tokio::sync::mpsc::UnboundedReceiver<FrontendMessage>,
    seen: Arc<StdMutex<Vec<u8>>>,
) {
    while let Some(message) = events.recv().await {
        if let FrontendMessage::Output { bytes, receipt, .. } = message {
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(&bytes);
            let _ = receipt.send(());
        }
    }
}

/// Blocks the calling thread on `future` with no Tokio runtime anywhere in sight.
///
/// Deliberately hand-rolled rather than borrowed from a runtime: the caller this stands in for is
/// a plain thread, and building any runtime to wait on the facade would test the opposite of what
/// the test is about. It works because the facade performs the work on the host's runtime and
/// hands back an ordinary future, woken from there.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    /// Wakes the thread parked below.
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut context = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(value) => return value,
            // Spurious unparks are allowed and cost one extra poll; a wake that arrives before
            // the park leaves the token set, so this cannot miss one.
            std::task::Poll::Pending => std::thread::park(),
        }
    }
}

/// An idle pipe shell: two real output streams and nothing running in it yet.
fn pipes() -> SpawnOptions {
    SpawnOptions {
        io: JobIo::Pipes,
        ..SpawnOptions::default()
    }
}

/// One line, admitted normally.
fn line() -> CommandOptions {
    CommandOptions {
        close_on_finish: false,
        on_accept: None,
    }
}

#[test]
fn a_job_admitted_from_another_runtime_belongs_to_this_host() {
    let host = Host::open();
    let idle = host.alive_tasks();

    let caller = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the caller's runtime");
    let job = caller
        .block_on(host.io.open_shell(std::path::Path::new(""), None, pipes()))
        .expect("admit the shell from the caller's runtime");

    assert_eq!(
        caller.metrics().num_alive_tasks(),
        0,
        "the caller's runtime carries none of the shell's tasks"
    );
    assert!(
        host.alive_tasks() > idle,
        "the shell's tasks were created on the host's runtime"
    );

    // The acceptance observation, deliberately: returning here at *completion* would mean the
    // caller's runtime outlived the command, and dropping it afterwards would prove nothing. The
    // receipt arrives at admission, the `run_command` future is abandoned with it, and the
    // caller's whole executor then goes away underneath a line that is still the host's.
    let (accepted, admission) = tokio::sync::oneshot::channel();
    let command = caller.block_on(async {
        let run = job.run_command(
            "echo marsh",
            CommandOptions {
                on_accept: Some(accepted),
                ..line()
            },
        );
        let mut run = std::pin::pin!(run);
        tokio::select! {
            received = admission => received.expect("the line was admitted"),
            _ = &mut run => panic!("the line concluded before it was ever admitted"),
        }
    });

    // Everything the caller had is now gone: its worker, its reactor and its blocking pool. A
    // shell whose pumps or whose pipe registrations had been created there is now a shell that
    // can never report another byte, and a command owned by it would never reach a verdict.
    drop(caller);

    let completion = host
        .runtime
        .block_on(command.wait())
        .expect("the line reached a verdict");
    host.finish_after_echo(
        completion.exit_code(),
        "the shell's pipe pumps survived the caller's runtime and delivered its bytes",
    );
}

#[test]
fn a_job_admitted_from_a_thread_with_no_runtime_belongs_to_this_host() {
    let host = Host::open();
    let idle = host.alive_tasks();

    let io = host.io.unleased();
    let job = std::thread::spawn(move || {
        block_on(io.open_shell(std::path::Path::new(""), None, pipes()))
    })
    .join()
    .expect("the caller thread finished")
    .expect("admit the shell from a thread with no runtime");

    assert!(
        host.alive_tasks() > idle,
        "the shell's tasks were created on the host's runtime, since the caller had none"
    );

    // The same shape from the other direction: the whole exchange — running the line and waiting
    // for its verdict — is driven by a thread that has no executor of its own. `run_command` is
    // an ordinary future woken by the host's runtime, so a plain parked thread can complete it.
    let completion = std::thread::spawn(move || block_on(job.run_command("echo marsh", line())))
        .join()
        .expect("the caller thread finished")
        .expect("the line reached a verdict");

    host.finish_after_echo(
        completion.exit_code(),
        "the shell's pipe pumps delivered its bytes",
    );
}
