//! Real mux receipts, native process behavior, terminal bytes and retained-handle safety.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module
)]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use common::{Seed, TIMEOUT, denied, run};
use marsh::shellmux::{
    CommandCompletion, CommandHandle, CommandOptions, FrontendEvent, JobIo, JobView, MuxProfile,
    OutputChannel, Shell, ShellFrontend, ShellId, ShellMux, SpawnOptions, TerminalGeometry,
};
use marsh::{Principal, SandboxPolicy, ShellErrorKind};
use serial_test::serial;

#[derive(Default)]
struct Observation {
    output: HashMap<OutputChannel, Vec<u8>>,
    completed: Vec<Arc<CommandCompletion>>,
    closed: bool,
    error: Option<String>,
    geometry: Option<TerminalGeometry>,
}
/// Byte delivery and terminal lifecycle are one record per opaque instance, not parallel maps.
struct Recorder {
    geometry: (u16, u16),
    mux: Weak<ShellMux>,
    instances: HashMap<Principal, Observation>,
    table: Vec<JobView>,
    changed: Arc<tokio::sync::Notify>,
}
impl ShellFrontend for Recorder {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            geometry: (rows, cols),
            mux: Weak::new(),
            instances: HashMap::new(),
            table: Vec::new(),
            changed: Arc::new(tokio::sync::Notify::new()),
        }
    }
    fn size(&self) -> (u16, u16) {
        self.geometry
    }
    fn bind(&mut self, mux: Weak<ShellMux>) {
        self.mux = mux;
        self.changed.notify_waiters();
    }
    fn update(&mut self, event: FrontendEvent<'_>) -> Option<tokio::sync::oneshot::Receiver<()>> {
        match event {
            FrontendEvent::Changed => {
                if let Some(mux) = self.mux.upgrade() {
                    self.table = mux.jobs();
                }
            }
            FrontendEvent::Opened(shell) => {
                self.instances
                    .entry(shell.sandbox().uid.clone())
                    .or_default();
            }
            FrontendEvent::Output {
                shell,
                channel,
                bytes,
            } => self
                .instances
                .entry(shell.uid.clone())
                .or_default()
                .output
                .entry(channel)
                .or_default()
                .extend_from_slice(bytes),
            FrontendEvent::Finished { completion } => self
                .instances
                .entry(completion.shell.uid.clone())
                .or_default()
                .completed
                .push(Arc::clone(completion)),
            FrontendEvent::Closed { end } => {
                self.instances
                    .entry(end.shell.uid.clone())
                    .or_default()
                    .closed = true;
            }
            FrontendEvent::Resized { shell, geometry } => {
                self.instances
                    .entry(shell.uid.clone())
                    .or_default()
                    .geometry = Some(geometry);
            }
            FrontendEvent::DefaultResized { geometry } => {
                self.geometry = (geometry.rows, geometry.cols);
            }
            FrontendEvent::IoError { shell, error, .. } => {
                self.instances
                    .entry(shell.uid.clone())
                    .or_default()
                    .error
                    .get_or_insert_with(|| error.to_string());
            }
            FrontendEvent::CommandAccepted { .. } => {}
        }
        self.changed.notify_waiters();
        None
    }
}
struct Fixture {
    seed: Seed,
    mux: Arc<ShellMux>,
    recorder: Arc<Mutex<Recorder>>,
}
impl std::ops::Deref for Fixture {
    type Target = Seed;
    fn deref(&self) -> &Seed {
        &self.seed
    }
}
impl Fixture {
    /// A mux whose every command takes the managed route.
    fn new() -> Self {
        Self::profile(MuxProfile {
            sandbox_policy: SandboxPolicy::allow(),
            ..Default::default()
        })
    }
    fn profile(profile: MuxProfile) -> Self {
        let seed = Seed::new("src/file", "original\n");
        let recorder = Arc::new(Mutex::new(Recorder::new(24, 80)));
        let mux =
            marsh_core::test_support::mux(profile, Arc::clone(&recorder), seed.fs.clone()).unwrap();
        Self {
            seed,
            mux,
            recorder,
        }
    }
    async fn open(&self, name: &str, io: JobIo) -> Shell {
        let job = self
            .mux
            .open_shell(
                &self.source,
                Some(ShellId::from(name)),
                SpawnOptions {
                    io,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            self.recorder
                .lock()
                .unwrap()
                .table
                .iter()
                .any(|view| view.id == *job.id() && !view.starting)
        );
        job
    }
    async fn observed(&self, mut predicate: impl FnMut(&Recorder) -> bool) {
        let notify = Arc::clone(&self.recorder.lock().unwrap().changed);
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let changed = notify.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if predicate(&self.recorder.lock().unwrap()) {
                    return;
                }
                changed.await;
            }
        })
        .await
        .expect("frontend observation");
    }
    async fn output(&self, job: &Shell, needle: &[u8]) {
        self.observed(|recorder| {
            recorder
                .instances
                .get(&job.sandbox().uid)
                .is_some_and(|seen| {
                    seen.output
                        .values()
                        .any(|bytes| bytes.windows(needle.len()).any(|part| part == needle))
                })
        })
        .await;
    }
    async fn close(&self, job: &Shell) {
        job.stop(false).await.unwrap();
        tokio::time::timeout(TIMEOUT, job.wait_closed())
            .await
            .unwrap()
            .unwrap();
        self.observed(|recorder| {
            recorder
                .instances
                .get(&job.sandbox().uid)
                .is_some_and(|seen| seen.closed)
        })
        .await;
    }
    async fn shutdown(self) {
        self.mux.shutdown().await.unwrap();
    }
}
async fn schedule(job: &Shell, line: &str) -> CommandHandle {
    let (accepted, receipt) = tokio::sync::oneshot::channel();
    let operation = job.run_command(
        line,
        CommandOptions {
            on_accept: Some(accepted),
            ..CommandOptions::default()
        },
    );
    tokio::pin!(operation);
    tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            receipt=receipt => receipt.expect("admission receipt"),
            result=&mut operation => panic!("command ended before admission: {result:?}"),
        }
    })
    .await
    .unwrap()
}
async fn completed(handle: &CommandHandle) -> Arc<CommandCompletion> {
    tokio::time::timeout(TIMEOUT, handle.wait())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn one_shot_receipts_keep_exact_output_and_native_status() {
    let fixture = Fixture::new();
    let job = fixture.open("pipe", JobIo::Pipes).await;
    let receipt = schedule(
        &job,
        "printf output; printf error >&2; printf published > file; /bin/sh -c 'exit 7'",
    )
    .await;
    let result = completed(&receipt).await;
    assert!(result.is_published());
    assert_eq!(result.exit_code(), Some(7));
    let again = receipt.wait().await.unwrap();
    assert!(Arc::ptr_eq(&result, &again));
    job.wait_closed().await.unwrap();
    let (stdout, stderr, finished) = {
        let recorder = fixture.recorder.lock().unwrap();
        let seen = &recorder.instances[&job.sandbox().uid];
        let observed = (
            seen.output[&OutputChannel::Stdout].clone(),
            seen.output[&OutputChannel::Stderr].clone(),
            seen.completed
                .iter()
                .map(|result| result.id)
                .collect::<Vec<_>>(),
        );
        drop(recorder);
        observed
    };
    assert_eq!(stdout, b"output");
    assert_eq!(stderr, b"error");
    assert_eq!(finished, [receipt.id()]);
    assert_eq!(fixture.bytes("file"), b"published");
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn same_name_never_inherits_a_previous_uid() {
    let fixture = Fixture::new();
    let old = fixture
        .open("build", JobIo::Terminal { geometry: None })
        .await;
    run(&old, "printf owned > src/file").await;
    fixture.close(&old).await;
    let replacement = fixture
        .open("build", JobIo::Terminal { geometry: None })
        .await;
    assert_ne!(old.sandbox().uid, replacement.sandbox().uid);
    for line in ["printf blind > src/file", "release -- src/file"] {
        denied(&replacement, line).await;
    }
    assert!(
        old.run_command("printf lost > src/file", CommandOptions::default())
            .await
            .is_err()
    );
    fixture.close(&replacement).await;
    let impersonator = fixture.open(old.sandbox().uid.as_str(), JobIo::Pipes).await;
    denied(&impersonator, "printf blind > src/file").await;
    impersonator.wait_closed().await.unwrap();
    assert_eq!(fixture.bytes("src/file"), b"owned");
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn terminal_geometry_and_bytes_are_real() {
    let fixture = Fixture::new();
    let first = fixture
        .open("first", JobIo::Terminal { geometry: None })
        .await;
    run(
        &first,
        "stty size; printf '\\033[?1049h\\377ab\\033[?1049l'",
    )
    .await;
    fixture.output(&first, b"24 80").await;
    fixture
        .output(&first, b"\x1b[?1049h\xffab\x1b[?1049l")
        .await;
    fixture
        .mux
        .resize_all(TerminalGeometry {
            rows: 35,
            cols: 110,
        })
        .await
        .unwrap();
    run(&first, "stty size").await;
    fixture.output(&first, b"35 110").await;
    let second = fixture
        .open("second", JobIo::Terminal { geometry: None })
        .await;
    run(&second, "stty size").await;
    fixture.output(&second, b"35 110").await;
    assert!(
        fixture
            .mux
            .resize_all(TerminalGeometry { rows: 0, cols: 10 })
            .await
            .is_err()
    );
    assert_eq!(fixture.recorder.lock().unwrap().geometry, (35, 110));
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn terminal_input_and_ctrl_c_reach_the_native_process() {
    let fixture = Fixture::new();
    let job = fixture
        .open("terminal", JobIo::Terminal { geometry: None })
        .await;
    let receipt = schedule(
        &job,
        "/bin/sh -c 'printf READY; read value; printf \"GOT:%s\\n\" \"$value\"'",
    )
    .await;
    fixture.output(&job, b"READY").await;
    job.write_input(b"answer\n").await.unwrap();
    assert_eq!(completed(&receipt).await.exit_code(), Some(0));
    fixture.output(&job, b"GOT:answer").await;
    let receipt = schedule(&job, "/bin/sh -c 'printf INTERRUPT; exec sleep 30'").await;
    fixture.output(&job, b"INTERRUPT").await;
    job.write_input(b"\x03").await.unwrap();
    let completion = completed(&receipt).await;
    assert_eq!(completion.exit_code(), Some(130));
    assert!(completion.is_published());
    run(&job, "printf usable").await;
    fixture.output(&job, b"usable").await;
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn graceful_and_forced_stops_preserve_their_distinct_boundaries() {
    let fixture = Fixture::new();
    let graceful = fixture
        .open("graceful", JobIo::Terminal { geometry: None })
        .await;
    let receipt = schedule(
        &graceful,
        "/bin/sh -c 'printf GRACEFUL; read value'; printf accepted > accepted",
    )
    .await;
    fixture.output(&graceful, b"GRACEFUL").await;
    graceful.stop(false).await.unwrap();
    assert!(!receipt.is_finished());
    graceful.write_input(b"continue\n").await.unwrap();
    assert!(completed(&receipt).await.is_published());
    graceful.wait_closed().await.unwrap();
    assert_eq!(fixture.bytes("accepted"), b"accepted");
    let forced = fixture
        .open("forced", JobIo::Terminal { geometry: None })
        .await;
    let receipt = schedule(
        &forced,
        "printf private > refused; /bin/sh -c 'printf FORCED; read value'",
    )
    .await;
    fixture.output(&forced, b"FORCED").await;
    tokio::time::timeout(TIMEOUT, forced.stop(true))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        completed(&receipt)
            .await
            .result
            .as_ref()
            .as_ref()
            .err()
            .unwrap()
            .kind(),
        ShellErrorKind::Interrupted
    ));
    forced.wait_closed().await.unwrap();
    assert!(!fixture.source.join("refused").exists());
    let reused = fixture.open("forced", JobIo::Pipes).await;
    assert_ne!(reused.sandbox().uid, forced.sandbox().uid);
    run(&reused, "printf replacement > replacement").await;
    reused.wait_closed().await.unwrap();
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn dropped_answer_and_unselected_output_do_not_stop_owned_work() {
    let fixture = Fixture::new();
    let job = fixture.open("unselected", JobIo::Pipes).await;
    let receipt = schedule(
        &job,
        "/bin/head -c 200000 /dev/zero; printf done > completed",
    )
    .await;
    assert!(completed(&receipt).await.is_published());
    job.wait_closed().await.unwrap();
    let bytes = fixture.recorder.lock().unwrap().instances[&job.sandbox().uid].output
        [&OutputChannel::Stdout]
        .clone();
    assert_eq!(bytes, vec![0; 200_000]);
    assert_eq!(fixture.bytes("completed"), b"done");
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_stale_receipt_never_replays_the_command() {
    let fixture = Fixture::new();
    let a = fixture.open("a", JobIo::Terminal { geometry: None }).await;
    let b = fixture.open("b", JobIo::Terminal { geometry: None }).await;
    let counter = fixture.root.path().join("counter");
    let receipt=schedule(&a,&format!("printf x >> {}; /bin/cat src/file >/dev/null; /bin/sh -c 'printf WAITING; read value'; printf lost > candidate",counter.display())).await;
    fixture.output(&a, b"WAITING").await;
    run(&b, "/bin/cat src/file >/dev/null; printf newer > src/file").await;
    a.write_input(b"continue\n").await.unwrap();
    let result = completed(&receipt).await;
    assert!(matches!(
        result.result.as_ref().as_ref().err().unwrap().kind(),
        ShellErrorKind::Stale { .. }
    ));
    assert_eq!(result.exit_code(), Some(0));
    assert_eq!(std::fs::read(counter).unwrap(), b"x");
    assert!(!fixture.source.join("candidate").exists());
    assert_eq!(fixture.bytes("src/file"), b"newer");
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn disjoint_writers_and_foreign_runtime_callers_progress_independently() {
    let fixture = Fixture::new();
    let a = fixture.open("a", JobIo::Terminal { geometry: None }).await;
    let b = fixture.open("b", JobIo::Terminal { geometry: None }).await;
    let receipt = schedule(&a, "printf A > a; /bin/sh -c 'printf WAIT; read value'").await;
    fixture.output(&a, b"WAIT").await;
    let other = b.clone();
    let (send, done) = tokio::sync::oneshot::channel();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(other.run_command("printf B > b", CommandOptions::default()));
        send.send(result).unwrap();
    });
    assert!(
        tokio::time::timeout(TIMEOUT, done)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_published()
    );
    thread.join().unwrap();
    a.write_input(b"continue\n").await.unwrap();
    assert!(completed(&receipt).await.is_published());
    assert_eq!(fixture.bytes("a"), b"A");
    assert_eq!(fixture.bytes("b"), b"B");
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn idle_terminal_leases_are_reusable_after_each_line() {
    let fixture = Fixture::new();
    let job = fixture
        .open("prompt", JobIo::Terminal { geometry: None })
        .await;
    drop(job.idle_terminal().unwrap());
    for command in ["printf first > first", "printf second > second"] {
        let lease = job.idle_terminal().unwrap();
        job.write_input(b"typed\n").await.unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(TIMEOUT, async {
            while !bytes.ends_with(b"typed\n") {
                let mut buffer = [0; 64];
                let count = lease.read(&mut buffer).await.unwrap().expect("live lease");
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
        })
        .await
        .unwrap();
        drop(lease);
        run(&job, command).await;
    }
    assert_eq!(fixture.bytes("first"), b"first");
    assert_eq!(fixture.bytes("second"), b"second");
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn logical_directories_share_authority_and_distinct_sources_do_not() {
    let fixture = Fixture::new();
    fixture.git();
    let alias = fixture.root.path().join("alias");
    std::os::unix::fs::symlink(&fixture.source, &alias).unwrap();
    let a = fixture
        .mux
        .open_shell(
            &fixture.source.join("src"),
            Some("a".into()),
            SpawnOptions::default(),
        )
        .await
        .unwrap();
    let b = fixture
        .mux
        .open_shell(&alias, Some("b".into()), SpawnOptions::default())
        .await
        .unwrap();
    assert_eq!(
        fixture.mux.job(a.id()).unwrap().working_directory,
        fixture.source.join("src")
    );
    run(&a, "printf owned > file").await;
    denied(&b, "printf blind > src/file").await;
    let other = fixture.root.path().join("other");
    std::fs::create_dir_all(other.join("src")).unwrap();
    fixture.fs.register(&other);
    let c = fixture
        .mux
        .open_shell(&other, Some("c".into()), SpawnOptions::default())
        .await
        .unwrap();
    run(&c, "printf independent > src/file").await;
    assert_eq!(
        std::fs::read(other.join("src/file")).unwrap(),
        b"independent"
    );
    fixture.shutdown().await;
}

/// Holds `line` at its READY barrier, checks host bytes, releases, and checks published bytes.
async fn held(fixture: &Fixture, a: &Shell, line: &str, ready: &[u8], during: &[u8], after: &[u8]) {
    let receipt = schedule(a, line).await;
    fixture.output(a, ready).await;
    assert_eq!(
        fixture.bytes("src/marker"),
        during,
        "before release: {line}"
    );
    a.write_input(b"go\n").await.unwrap();
    let result = completed(&receipt).await;
    assert_eq!(result.exit_code(), Some(0), "{line}");
    assert_eq!(
        fixture.bytes("src/marker"),
        after,
        "after completion: {line}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn shared_source_uses_uid_and_seed_across_muxes_and_standalone_shells() {
    let fixture = Fixture::profile(MuxProfile::default());
    fixture.git();
    let a = fixture
        .mux
        .open_shell(
            &fixture.source.join("src"),
            Some("same".into()),
            SpawnOptions {
                io: JobIo::Terminal { geometry: None },
                ..Default::default()
            },
        )
        .await
        .unwrap();
    held(
        &fixture,
        &a,
        "printf raw > marker; printf 'READY-raw\\n'; read release",
        b"READY-raw",
        b"raw",
        b"raw",
    )
    .await;
    run(&a, "printf before > marker").await;
    assert_eq!(fixture.bytes("src/marker"), b"before");

    let alias = fixture.root.path().join("alias");
    std::os::unix::fs::symlink(&fixture.source, &alias).unwrap();
    let other = Arc::new(Mutex::new(Recorder::new(24, 80)));
    let second =
        marsh_core::test_support::mux(MuxProfile::default(), other, fixture.seed.fs.clone())
            .unwrap();
    let b = second
        .open_shell(&alias, Some("same".into()), SpawnOptions::default())
        .await
        .unwrap();
    assert_eq!(a.sandbox().id, b.sandbox().id);
    assert_eq!(a.sandbox().seed, b.sandbox().seed);
    assert_ne!(a.sandbox().uid, b.sandbox().uid);
    assert_ne!(a.sandbox().dir, b.sandbox().dir);
    held(
        &fixture,
        &a,
        "printf staged > marker; printf 'READY-managed\\n'; read release",
        b"READY-managed",
        b"before",
        b"staged",
    )
    .await;

    b.stop(false).await.unwrap();
    tokio::time::timeout(TIMEOUT, b.wait_closed())
        .await
        .unwrap()
        .unwrap();
    held(
        &fixture,
        &a,
        "printf restored > marker; printf 'READY-restored\\n'; read release",
        b"READY-restored",
        b"restored",
        b"restored",
    )
    .await;

    let standalone = fixture
        .seed
        .builder()
        .working_dir(fixture.source.clone())
        .build()
        .await
        .unwrap();
    held(
        &fixture,
        &a,
        "printf mixed > marker; printf 'READY-standalone\\n'; read release",
        b"READY-standalone",
        b"restored",
        b"mixed",
    )
    .await;
    standalone.close(false).await.unwrap();
    drop(b);
    second.shutdown().await.unwrap();
    fixture.shutdown().await;
}

#[derive(clap::Parser)]
struct StampA;
#[derive(clap::Parser)]
struct StampB;
fn stamp(bytes: &[u8]) -> Result<marsh::ExecutionResult, brush_core::Error> {
    use std::io::Write;
    let context = marsh::builtins::current_context().unwrap();
    let mut file = context.open(
        Path::new("stamp"),
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true),
    )?;
    file.write_all(bytes)?;
    Ok(marsh::ExecutionResult::success())
}
impl marsh::builtins::Command for StampA {
    type Error = brush_core::Error;
    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        _: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<marsh::ExecutionResult, Self::Error> {
        stamp(b"A")
    }
}
impl marsh::builtins::Command for StampB {
    type Error = brush_core::Error;
    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        _: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<marsh::ExecutionResult, Self::Error> {
        stamp(b"B")
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn independent_muxes_keep_their_builtin_implementations() {
    let a = Fixture::profile(MuxProfile {
        builtins: HashMap::from([("stamp".into(), marsh::builtins::builtin::<StampA>())]),
        sandbox_policy: SandboxPolicy::allow(),
        ..Default::default()
    });
    let b = Fixture::profile(MuxProfile {
        builtins: HashMap::from([("stamp".into(), marsh::builtins::builtin::<StampB>())]),
        sandbox_policy: SandboxPolicy::allow(),
        ..Default::default()
    });
    let first = a.open("same", JobIo::Terminal { geometry: None }).await;
    let second = b.open("same", JobIo::Terminal { geometry: None }).await;
    run(&first, "stamp").await;
    run(&second, "stamp").await;
    run(&first, "stamp").await;
    assert_eq!(a.bytes("stamp"), b"A");
    assert_eq!(b.bytes("stamp"), b"B");
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn exit_closes_only_its_pane_and_subshell_exit_does_not() {
    let fixture = Fixture::new();
    let a = fixture.open("a", JobIo::Terminal { geometry: None }).await;
    let b = fixture.open("b", JobIo::Terminal { geometry: None }).await;
    assert_eq!(run(&a, "(exit 23)").await.exit_code(), Some(23));
    assert!(fixture.mux.get_shell(a.id()).is_some());
    assert_eq!(run(&a, "exit").await.exit_code(), Some(23));
    a.wait_closed().await.unwrap();
    assert!(fixture.mux.get_shell(a.id()).is_none());
    run(&b, "printf alive > alive").await;
    assert_eq!(fixture.bytes("alive"), b"alive");
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn shutdown_and_closed_handles_do_not_retain_authority() {
    let fixture = Fixture::new();
    let job = fixture
        .open("retained", JobIo::Terminal { geometry: None })
        .await;
    run(&job, "printf owned > src/file").await;
    fixture.mux.shutdown().await.unwrap();
    assert!(
        job.run_command("printf lost > src/file", CommandOptions::default())
            .await
            .is_err()
    );
    assert!(fixture.recorder.lock().unwrap().mux.upgrade().is_none());
    let reopened = fixture.shell().await;
    denied(&reopened, "printf blind > src/file").await;
    reopened.close(false).await.unwrap();
}
