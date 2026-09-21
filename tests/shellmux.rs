#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::tests_outside_test_module,
    reason = "an integration test file is the test module"
)]
//! Jobs as a front-end drives them: `spawn`, `start_in`, `stop` and `on_finish`, over the
//! pseudoterminal every job owns, with the frontend the mux delivers all of it to.
//!
//! The claim under test is that owning the wait changes nothing about the transaction: each job's
//! line runs through the job's own [`marsh::Shell::run`] exactly as an interactive line would, with
//! the same verdicts — including losing a race — while the caller only ever observes. The terminal
//! tests pin the other half of the contract: one default geometry every job opens at and
//! [`ShellMux::resize_all`] moves, output that survives byte for byte, and delivery that keeps
//! running for a job nobody is draining.
//!
//! Every fixture drives a fake btrfs ([`marsh_btrfs::fake::CopyTree`]), the same one
//! `tests/shell.rs` uses, so the suite runs anywhere. Every test is `#[serial]` for the reason
//! `tests/shell.rs` already documents: builtin instrumentation is process-global.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use brush_core::env::ShellEnvironment;
use marsh::policy::{Action, Event, Resource};
use marsh::shellmux::{
    CommandCompletion, CommandOptions, FrontendEvent, JobView, MuxError, MuxProfile, OnFinish,
    OutputChannel, ShellFrontend, ShellId, ShellMux, SnapshotUid, SpawnOptions, Spawned,
    TerminalGeometry,
};
use marsh::{MarshError, MarshExecutor, Outcome, PolicyValidator, Publication};
use marsh_btrfs::fake::CopyTree;
use serial_test::serial;
use tempfile::TempDir;
use tokio::sync::Notify;

/// Rows the fixture's mux gives every job.
const ROWS: u16 = 24;
/// Columns the fixture's mux gives every job.
const COLS: u16 = 80;
/// The default job's name, and the principal its commands run as.
const MAIN: &str = "main";
/// How long a wait may take before a test declares the claim it is waiting for unmet.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Holds a job open long enough for a "force while running" test to act on it, and marks the
/// moment it is safe to: `echo` runs inside the external, so once `started` is on the terminal the
/// external's pid is in the job's spawn records and its refresh and earlier writes are done.
///
/// The sleep is as long as [`TIMEOUT`] on purpose: every test that holds a job open this way goes
/// on to force-stop it, and how quickly the kill reaches the external — not how long it could
/// wait — is the claim under test. A kill that missed needs the whole duration to notice.
const HELD: &str = "sh -c 'echo started; exec sleep 30'";

/// The same marker as [`HELD`], held only briefly.
///
/// The two race tests below let their held job conclude on its own rather than force-stopping it,
/// which needs the hold to actually finish. Reusing [`HELD`]'s duration for that would make every
/// passing run of those two tests take the full 30 seconds, tripping this workspace's own
/// `nextest` slow-test timeout (`.config/nextest.toml`) well before the sleep is over.
const RACE_HOLD: &str = "sh -c 'echo started; sleep 2'";

/// A command producing [`PAYLOAD`] on the job's terminal: a full-screen program's alternate-screen
/// escape sequences around one byte that is not valid UTF-8 at all.
const PAYLOAD_CMD: &str = "printf '\\033[?1049h\\377ab\\033[?1049l'";
/// The bytes [`PAYLOAD_CMD`] writes, exactly as `\033`/`\377` (octal) spell them.
const PAYLOAD: &[u8] = b"\x1b[?1049h\xffab\x1b[?1049l";

/// One completion's verdict, shared because neither [`Outcome`] nor [`MuxError`] is cloneable.
type Verdict = Arc<Result<Outcome, MuxError>>;

/// The test suite's frontend: everything a mux delivers, kept until a test consumes it.
///
/// The only [`ShellFrontend`] implementor here, and test-local: a job's bytes and results only
/// exist through it, so a test that wants them has to be the thing the mux delivered them to.
/// Every buffer is keyed by sandbox uid, never by name, so a reused job name never mixes two jobs'
/// contents, and output is keyed by [`OutputChannel`] as well, so a stream is never read as
/// another's.
struct Recorder {
    /// The default terminal geometry this frontend reports: what the mux opens a job at, and what
    /// [`FrontendEvent::DefaultResized`] last moved it to.
    size: (u16, u16),
    /// The mux this recorder is bound to; empty before binding and after detaching.
    mux: Weak<ShellMux>,
    /// The fixture's `state/snap` directory, for [`Self::closed_with_storage`].
    snap: PathBuf,
    /// Live job handles, by identity; removed once [`FrontendEvent::Closed`] names the same uid.
    handles: HashMap<ShellId, Spawned>,
    /// The job table as of the last [`FrontendEvent::Changed`].
    observed_jobs: Vec<JobView>,
    /// The selected job as of the last [`FrontendEvent::Changed`].
    observed_current: Option<ShellId>,
    /// Unconsumed output bytes, by sandbox uid and then by the stream that carried them.
    output: HashMap<SnapshotUid, HashMap<OutputChannel, Vec<u8>>>,
    /// Every completion delivered, in delivery order.
    finished: Vec<Arc<CommandCompletion>>,
    /// [`Self::take_result`]'s read cursor into [`Self::finished`], by sandbox uid.
    consumed: HashMap<SnapshotUid, usize>,
    /// Sandbox uids [`FrontendEvent::Closed`] has named.
    closed: HashSet<SnapshotUid>,
    /// Sandbox uids whose storage still existed on disk at the moment they closed.
    closed_with_storage: HashSet<SnapshotUid>,
    /// The first [`FrontendEvent::IoError`] message reported for each sandbox uid.
    errors: HashMap<SnapshotUid, String>,
    /// The geometry each job's own terminal was last [`FrontendEvent::Resized`] to.
    resized: HashMap<SnapshotUid, TerminalGeometry>,
    /// Notified after every [`ShellFrontend::bind`] and [`ShellFrontend::update`] call.
    signal: Arc<Notify>,
}

impl Recorder {
    /// A recorder reporting `rows` × `cols`, checking closed storage against `snap`.
    fn at(rows: u16, cols: u16, snap: PathBuf) -> Self {
        Self {
            size: (rows, cols),
            mux: Weak::new(),
            snap,
            handles: HashMap::new(),
            observed_jobs: Vec::new(),
            observed_current: None,
            output: HashMap::new(),
            finished: Vec::new(),
            consumed: HashMap::new(),
            closed: HashSet::new(),
            closed_with_storage: HashSet::new(),
            errors: HashMap::new(),
            resized: HashMap::new(),
            signal: Arc::new(Notify::new()),
        }
    }

    /// The mux this recorder is bound to, when it is bound to one.
    fn mux(&self) -> Option<Arc<ShellMux>> {
        self.mux.upgrade()
    }

    /// The live handle for job `id`, when one is open under that name.
    fn handle(&self, id: &ShellId) -> Option<Spawned> {
        self.handles.get(id).cloned()
    }

    /// The job table as this recorder last observed it.
    fn observed_jobs(&self) -> &[JobView] {
        &self.observed_jobs
    }

    /// The selected job as this recorder last observed it.
    const fn observed_current(&self) -> Option<&ShellId> {
        self.observed_current.as_ref()
    }

    /// Every byte buffered for `uid`'s terminal stream, removing it from this recorder.
    fn take_terminal(&mut self, uid: &SnapshotUid) -> Vec<u8> {
        self.output
            .get_mut(uid)
            .and_then(|streams| streams.remove(&OutputChannel::Terminal))
            .unwrap_or_default()
    }

    /// The bytes buffered for `uid`'s terminal stream so far, without draining them.
    fn terminal_bytes(&self, uid: &SnapshotUid) -> &[u8] {
        self.output
            .get(uid)
            .and_then(|streams| streams.get(&OutputChannel::Terminal))
            .map_or(&[][..], Vec::as_slice)
    }

    /// Every completion delivered for `uid`, in delivery order.
    fn results(&self, uid: &SnapshotUid) -> Vec<&Arc<CommandCompletion>> {
        self.finished
            .iter()
            .filter(|completion| &completion.shell.uid == uid)
            .collect()
    }

    /// The exit status of every completion delivered for `uid`, in delivery order, in the legacy
    /// callback convention: the real code, or `-1` for a command that produced none.
    fn exit_codes(&self, uid: &SnapshotUid) -> Vec<i32> {
        self.results(uid)
            .into_iter()
            .map(|completion| completion.legacy_status())
            .collect()
    }

    /// The next completion for `uid` this recorder has not yet handed out, if one has arrived.
    fn take_result(&mut self, uid: &SnapshotUid) -> Option<Arc<CommandCompletion>> {
        let cursor = *self.consumed.get(uid).unwrap_or(&0);
        let next = self
            .finished
            .iter()
            .filter(|completion| &completion.shell.uid == uid)
            .nth(cursor)
            .map(Arc::clone)?;
        self.consumed.insert(uid.clone(), cursor + 1);
        Some(next)
    }

    /// Whether `uid`'s streams have ended and its snapshot is reclaimed.
    fn is_closed(&self, uid: &SnapshotUid) -> bool {
        self.closed.contains(uid)
    }

    /// Whether `uid`'s storage still existed on disk at the moment it closed.
    fn closed_with_storage(&self, uid: &SnapshotUid) -> bool {
        self.closed_with_storage.contains(uid)
    }

    /// The first I/O error reported for `uid`, if any.
    fn error(&self, uid: &SnapshotUid) -> Option<&str> {
        self.errors.get(uid).map(String::as_str)
    }

    /// The size `uid`'s own terminal was last announced at.
    fn resized(&self, uid: &SnapshotUid) -> Option<TerminalGeometry> {
        self.resized.get(uid).copied()
    }

    /// The default terminal geometry this recorder currently reports.
    const fn dimensions(&self) -> (u16, u16) {
        self.size
    }
}

impl ShellFrontend for Recorder {
    fn new(rows: u16, cols: u16) -> Self {
        Self::at(rows, cols, PathBuf::new())
    }

    fn size(&self) -> (u16, u16) {
        self.size
    }

    fn bind(&mut self, mux: Weak<ShellMux>) {
        if mux.upgrade().is_none() {
            // An empty weak reference is a detach: the live handles this recorder was holding are
            // this session's, and the next session starts with none.
            self.handles.clear();
        }
        self.mux = mux;
        self.signal.notify_waiters();
    }

    fn update(&mut self, event: FrontendEvent<'_>) -> Option<tokio::sync::oneshot::Receiver<()>> {
        match event {
            FrontendEvent::Changed => {
                if let Some(mux) = self.mux.upgrade() {
                    self.observed_jobs = mux.jobs();
                    self.observed_current = mux.current_job().map(|job| job.id);
                }
            }
            FrontendEvent::Opened(spawned) => {
                self.handles.insert(spawned.id().clone(), spawned.clone());
            }
            FrontendEvent::CommandAccepted { .. } => {
                // The admission receipt is the caller's: every test that needs it holds the
                // `CommandHandle` `spawn` or `start_in` handed it.
            }
            FrontendEvent::Output {
                shell,
                channel,
                bytes,
            } => {
                self.output
                    .entry(shell.uid.clone())
                    .or_default()
                    .entry(channel)
                    .or_default()
                    .extend_from_slice(bytes);
            }
            FrontendEvent::Finished { completion } => {
                self.finished.push(Arc::clone(completion));
            }
            FrontendEvent::Closed { end } => {
                let shell = &end.shell;
                if self.snap.join(shell.uid.as_str()).exists() {
                    self.closed_with_storage.insert(shell.uid.clone());
                }
                self.closed.insert(shell.uid.clone());
                if self
                    .handles
                    .get(&shell.id)
                    .is_some_and(|spawned| spawned.sandbox().uid == shell.uid)
                {
                    self.handles.remove(&shell.id);
                }
            }
            FrontendEvent::Resized { shell, geometry } => {
                self.resized.insert(shell.uid.clone(), geometry);
            }
            FrontendEvent::DefaultResized { geometry } => {
                self.size = (geometry.rows, geometry.cols);
            }
            FrontendEvent::IoError { shell, error, .. } => {
                self.errors
                    .entry(shell.uid.clone())
                    .or_insert_with(|| error.to_string());
            }
        }
        self.signal.notify_waiters();
        // No receipt, ever: every test here wants the mux to keep draining a job nobody is
        // looking at, which is exactly what withholding one would stop.
        None
    }
}

/// A seed subvolume, the fake btrfs it is registered with, and the mux built over it.
struct Fixture {
    /// Kept alive so the scratch directory outlives the test.
    _scratch: TempDir,
    /// The tree the mux's session publishes into.
    seed: PathBuf,
    /// `<scratch>/.marsh/seed`: where snapshots and the log live.
    state: PathBuf,
    /// The btrfs stand-in the session takes snapshots through.
    fs: Arc<CopyTree>,
    /// The mux under test; taken by [`Self::finish_mux`], which every test not left mid-panic
    /// calls before returning.
    mux: Option<Arc<ShellMux>>,
    /// The frontend the mux was built with.
    frontend: Arc<Mutex<Recorder>>,
}

impl Fixture {
    /// A seed containing `src/file0.txt`, `src/file1.txt` and `src/.keep`, with a mux over it.
    fn new() -> Self {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = scratch.path().canonicalize().expect("canonical scratch");
        let seed = root.join("seed");
        std::fs::create_dir_all(seed.join("src")).expect("seed tree");
        std::fs::write(seed.join("src/file0.txt"), b"zero\n").expect("seed file0");
        std::fs::write(seed.join("src/file1.txt"), b"one\n").expect("seed file1");
        std::fs::write(seed.join("src/.keep"), b"").expect("seed .keep");
        let fs = Arc::new(CopyTree::new());
        fs.register(&seed);
        let state = root.join(".marsh/seed");

        let frontend = Arc::new(Mutex::new(Recorder::at(ROWS, COLS, state.join("snap"))));
        let executor = MarshExecutor::open_with(&seed, fs.clone()).expect("open the seed");
        let mux = ShellMux::new(
            executor,
            Arc::new(Mutex::new(PolicyValidator::default())),
            MuxProfile {
                environment: ShellEnvironment::new(),
                ..MuxProfile::default()
            },
            Arc::clone(&frontend),
        )
        .expect("build the mux");

        Self {
            _scratch: scratch,
            seed,
            state,
            fs,
            mux: Some(mux),
            frontend,
        }
    }

    /// The mux under test.
    ///
    /// # Panics
    ///
    /// Panics once [`Self::finish_mux`] has taken it.
    const fn mux(&self) -> &Arc<ShellMux> {
        self.mux.as_ref().expect("the mux was already finished")
    }

    /// This fixture's recorder, recovering a poisoned lock like the crate under test does.
    fn recorder(&self) -> std::sync::MutexGuard<'_, Recorder> {
        self.frontend
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// This recorder's notification handle, for a test helper waiting on the next observation.
    fn signal(&self) -> Arc<Notify> {
        Arc::clone(&self.recorder().signal)
    }

    /// The seed-relative `path`, resolved under this fixture's seed.
    fn seed(&self, path: &str) -> PathBuf {
        self.seed.join(path)
    }

    /// Where every job's snapshot lives.
    fn snap(&self) -> PathBuf {
        self.state.join("snap")
    }

    /// A fresh executor over this fixture's seed, exactly as reopening the session would build.
    fn open_executor(&self) -> Result<MarshExecutor, MarshError> {
        MarshExecutor::open_with(&self.seed, self.fs.clone())
    }

    /// Shuts the mux down and asserts nothing else still holds it.
    ///
    /// # Panics
    ///
    /// Panics if the mux was already finished, if shutdown fails, or if a clone of it survived.
    async fn finish_mux(&mut self) {
        let mux = self.mux.take().expect("the mux was already finished");
        mux.shutdown().await.expect("shut the mux down");
        assert_eq!(
            Arc::strong_count(&mux),
            1,
            "a clone of the mux outlived finish_mux"
        );
        drop(mux);
    }
}

/// Whether `haystack` contains `needle` as one contiguous run of bytes.
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|window| window == needle)
}

/// Waits until `observe` yields something from `fixture`'s recorder, or [`TIMEOUT`] passes.
///
/// The notification is registered before the check and awaited with the recorder's lock released,
/// so a change landing between the two is a pending wakeup rather than a lost one.
async fn wait_for<T>(
    fixture: &Fixture,
    mut observe: impl FnMut(&mut Recorder) -> Option<T>,
) -> Option<T> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let signal = fixture.signal();
        let notified = signal.notified();
        let value = {
            let mut guard = fixture.recorder();
            observe(&mut guard)
        };
        if let Some(value) = value {
            return Some(value);
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return None;
        }
    }
}

/// [`wait_for`] for a plain predicate.
async fn wait_until(fixture: &Fixture, mut ready: impl FnMut(&mut Recorder) -> bool) -> bool {
    wait_for(fixture, |recorder| ready(recorder).then_some(())).await.is_some()
}

/// Waits until `predicate` holds, or [`TIMEOUT`] passes.
///
/// What a test uses for a condition the recorder cannot observe: the job table itself, or the
/// filesystem.
///
/// # Panics
///
/// Panics, naming `label`, if the condition never becomes true.
async fn eventually(label: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if predicate() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{label}: condition never became true"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The next completion of a command in the sandbox `uid`: its identity, its process status and
/// what the gate made of it.
///
/// # Panics
///
/// Panics, naming `uid`, when none arrives within [`TIMEOUT`].
async fn concluded(fixture: &Fixture, uid: &SnapshotUid) -> Arc<CommandCompletion> {
    wait_for(fixture, |recorder| recorder.take_result(uid))
        .await
        .unwrap_or_else(|| panic!("no command in {uid} concluded"))
}

/// The [`Outcome`] a completion carries, failing the test when the conclusion itself broke.
fn verdict(result: &Verdict) -> &Outcome {
    match result.as_ref() {
        Ok(outcome) => outcome,
        Err(error) => panic!("expected a verdict, got an infrastructure failure: {error}"),
    }
}

/// Waits for the frontend's end of stream for the sandbox `uid`.
///
/// # Panics
///
/// Panics, naming `uid`, if it never closes within [`TIMEOUT`].
async fn wait_for_close(fixture: &Fixture, uid: &SnapshotUid) {
    assert!(
        wait_until(fixture, |recorder| recorder.is_closed(uid)).await,
        "{uid} never closed"
    );
}

/// Drains `job`'s recorded terminal bytes until `done` accepts everything seen so far.
///
/// # Panics
///
/// Panics if the job's stream ends, or reports an I/O error, before `done` is satisfied, and if
/// [`TIMEOUT`] passes first.
async fn drain_output(
    fixture: &Fixture,
    job: &Spawned,
    label: &str,
    done: impl Fn(&[u8]) -> bool + Send + Sync,
) -> Vec<u8> {
    let uid = job.sandbox().uid.clone();
    wait_for(fixture, |recorder| {
        if done(recorder.terminal_bytes(&uid)) {
            return Some(recorder.take_terminal(&uid));
        }
        if let Some(message) = recorder.error(&uid) {
            panic!("{label}: {uid} reported an error before its output was satisfied: {message}");
        }
        assert!(
            !recorder.is_closed(&uid),
            "{label}: {uid} closed before its output was satisfied"
        );
        None
    })
    .await
    .unwrap_or_else(|| panic!("{label}: timed out waiting for {uid}'s output"))
}

/// Starts `cmd` in `job`, then waits until its terminal has produced `started`.
///
/// The primitive behind [`start_held`]; called directly by the two race tests, which hold their
/// job open only briefly rather than for [`HELD`]'s whole duration.
async fn start_marked(fixture: &Fixture, job: &Spawned, cmd: &str, options: CommandOptions) {
    fixture
        .mux()
        .start_in(job, cmd, options)
        .await
        .expect("start the marked line");
    drain_output(fixture, job, "start_marked", |bytes| {
        contains_subslice(bytes, b"started")
    })
    .await;
}

/// [`start_marked`] with `prefix` in front of [`HELD`].
async fn start_held(fixture: &Fixture, job: &Spawned, prefix: &str, options: CommandOptions) {
    start_marked(fixture, job, &format!("{prefix}{HELD}"), options).await;
}

/// Runs `cmd` in `job` and returns the first line its terminal produced, trimmed.
///
/// # Panics
///
/// Panics if the line does not publish.
async fn run_line(fixture: &Fixture, job: &Spawned, cmd: &str) -> String {
    fixture
        .mux()
        .start_in(job, cmd, CommandOptions::default())
        .await
        .expect("start the line");
    let bytes = drain_output(fixture, job, "run_line", |bytes| bytes.contains(&b'\n')).await;
    let completion = concluded(fixture, &job.sandbox().uid).await;
    assert!(
        matches!(verdict(&completion.outcome), Outcome::Published { .. }),
        "expected a published line, got {:?}",
        verdict(&completion.outcome)
    );
    let first_line = bytes.split(|&byte| byte == b'\n').next().unwrap_or(&[]);
    String::from_utf8_lossy(first_line).trim().to_string()
}

/// The terminal geometry `job` reports, as `"<rows> <cols>"`.
async fn size_of_job(fixture: &Fixture, job: &Spawned) -> String {
    run_line(fixture, job, "stty size").await
}

/// Runs `cmd` in `job`, waits for `marker` on its terminal, and reports the process status it
/// ended with.
///
/// What [`run_line`] cannot do: that one rejects anything but a published line and hands back a
/// trimmed line, while this returns the raw exit code for a command that ends non-zero on purpose.
async fn run_to_marker(
    fixture: &Fixture,
    job: &Spawned,
    cmd: &str,
    marker: &[u8],
) -> Option<i32> {
    fixture
        .mux()
        .start_in(job, cmd, CommandOptions::default())
        .await
        .expect("start the line");
    let marker = marker.to_vec();
    drain_output(fixture, job, "run_to_marker", move |bytes| {
        contains_subslice(bytes, &marker)
    })
    .await;
    concluded(fixture, &job.sandbox().uid).await.exit_code
}

/// Asserts the recorder's own callback-observed table shows `opened` as ready: present, and no
/// longer `starting`.
///
/// The recorder's view, not a fresh [`ShellMux::jobs`] call: a state the mux reached without
/// announcing it fails here instead of passing on a poll a real frontend would never make.
fn observed_ready(fixture: &Fixture, opened: &Spawned) {
    let recorder = fixture.recorder();
    let ready = recorder
        .observed_jobs()
        .iter()
        .any(|job| &job.id == opened.id() && !job.starting);
    let jobs = format!("{:?}", recorder.observed_jobs());
    drop(recorder);
    assert!(ready, "{} not observed ready: {jobs}", opened.id());
}

/// Asserts that the closed sandbox `uid` still answers for exactly `exits`, and that its buffer
/// stays empty: a later job under the same name writes into its own, never back into this one's.
fn history_intact(fixture: &Fixture, uid: &SnapshotUid, exits: &[i32]) {
    let recorder = fixture.recorder();
    let codes = recorder.exit_codes(uid);
    let buffer_empty = recorder.terminal_bytes(uid).is_empty();
    drop(recorder);
    assert_eq!(codes, exits, "{uid}'s history changed");
    assert!(buffer_empty, "{uid}'s buffer was written to after it closed");
}

/// Appends whatever the recorder still holds for `uid` to `terminal`.
///
/// What the job's terminal carried in full: a live drain takes a prefix, the tail arrives between
/// that drain and the end of stream, and only the two together compare equal to the whole payload.
fn complete_tails(fixture: &Fixture, uid: &SnapshotUid, terminal: &mut Vec<u8>) {
    terminal.extend(fixture.recorder().take_terminal(uid));
}

#[tokio::test]
#[serial]
async fn a_job_command_publishes_like_a_shell_line() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .spawn(
            "",
            Some(ShellId::from("1")),
            Some("printf 'one\\n' > src/file0.txt"),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn a job with a command");

    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(completion.exit_code, Some(0));
    assert_eq!(
        verdict(&completion.outcome),
        &Outcome::Published {
            publication: Publication { seq: 1, ops: 1 },
            granted: vec![Event::new(
                "1",
                Action::Edit,
                Resource::from(vec!["src", "file0.txt"])
            )],
        }
    );
    assert_eq!(
        std::fs::read(fixture.seed("src/file0.txt")).expect("read the seed file"),
        b"one\n"
    );

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn a_jobs_snapshot_outlives_its_lines_and_goes_with_the_job() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .spawn("", Some(ShellId::from("1")), None, SpawnOptions::default())
        .await
        .expect("spawn a job");

    fixture
        .mux()
        .start_in(&job, "printf 'x\\n' > src/file0.txt", CommandOptions::default())
        .await
        .expect("start the line");
    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(completion.exit_code, Some(0));
    assert!(matches!(verdict(&completion.outcome), Outcome::Published { .. }));

    assert!(
        fixture.snap().join(job.sandbox().uid.as_str()).is_dir(),
        "the job's snapshot exists"
    );
    let entries: Vec<_> = std::fs::read_dir(fixture.snap())
        .expect("read the snap directory")
        .map(|entry| entry.expect("dir entry").file_name())
        .collect();
    assert_eq!(
        entries,
        vec![std::ffi::OsString::from(job.sandbox().uid.as_str())]
    );

    fixture.mux().stop(&job, false).await.expect("stop the job");
    wait_for_close(&fixture, &job.sandbox().uid).await;

    assert!(
        std::fs::read_dir(fixture.snap())
            .map_or(true, |mut entries| entries.next().is_none()),
        "the snapshot is gone once the job closes"
    );
    assert!(
        !fixture.recorder().closed_with_storage(&job.sandbox().uid),
        "storage was already reclaimed before the frontend was told the job was closed"
    );
    assert!(
        fixture.recorder().handle(job.id()).is_none(),
        "the handle is released at Closed"
    );
    assert_eq!(fixture.recorder().results(&job.sandbox().uid).len(), 1);

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn ctrl_c_at_the_terminal_reaches_the_line_which_is_then_gated() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .spawn("", Some(ShellId::from(MAIN)), None, SpawnOptions::default())
        .await
        .expect("spawn a job");
    start_held(
        &fixture,
        &job,
        "printf 'partial\\n' > src/file0.txt; ",
        CommandOptions::default(),
    )
    .await;

    fixture
        .mux()
        .write_input(&job, b"\x03")
        .await
        .expect("send ctrl-c");

    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(
        completion.exit_code,
        Some(130),
        "the external was killed by SIGINT"
    );
    match verdict(&completion.outcome) {
        Outcome::Published { publication, .. } => assert_eq!(publication.ops, 1),
        other => panic!("expected a publication, got {other:?}"),
    }
    assert_eq!(
        std::fs::read(fixture.seed("src/file0.txt")).expect("read the seed file"),
        b"partial\n"
    );
    assert!(
        fixture.mux().job(job.id()).is_some(),
        "a command ending is not a closure"
    );
    assert!(fixture.snap().join(job.sandbox().uid.as_str()).is_dir());

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn a_job_has_exactly_three_standard_streams() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .spawn("", Some(ShellId::from(MAIN)), None, SpawnOptions::default())
        .await
        .expect("spawn a job");

    let command = fixture
        .mux()
        .start_in(&job, "echo x >&3", CommandOptions::default())
        .await
        .expect("start the line");
    let awaited = tokio::time::timeout(TIMEOUT, command.wait())
        .await
        .expect("the command's receipt timed out")
        .expect("the command concluded");
    assert_eq!(awaited.exit_code, Some(1));

    let bytes = drain_output(&fixture, &job, "fd3", |bytes| {
        contains_subslice(bytes, b"bad file descriptor: 3")
    })
    .await;
    assert!(
        contains_subslice(&bytes, b"bad file descriptor: 3"),
        "stderr carries the redirection failure: {:?}",
        String::from_utf8_lossy(&bytes)
    );

    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(completion.id, awaited.id, "one command, one receipt");
    assert_eq!(completion.exit_code, Some(1));
    match verdict(&completion.outcome) {
        Outcome::Published { publication, .. } => assert_eq!(publication.ops, 0),
        other => panic!("expected a publication with no filesystem effect, got {other:?}"),
    }

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn terminal_output_is_preserved_byte_for_byte() {
    let mut fixture = Fixture::new();

    let job = fixture
        .mux()
        .spawn(
            "",
            Some(ShellId::from(MAIN)),
            Some(PAYLOAD_CMD),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn a job with a command");

    let mut output =
        drain_output(&fixture, &job, "payload", |bytes| bytes.len() >= PAYLOAD.len()).await;

    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(completion.exit_code, Some(0));

    fixture.mux().stop(&job, false).await.expect("stop the job");
    wait_for_close(&fixture, &job.sandbox().uid).await;
    complete_tails(&fixture, &job.sandbox().uid, &mut output);

    assert_eq!(output, PAYLOAD, "the terminal carries the payload byte for byte");

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn concurrent_job_commands_race_like_tabs() {
    let mut fixture = Fixture::new();
    let slow = fixture
        .mux()
        .spawn("", Some(ShellId::from("slow")), None, SpawnOptions::default())
        .await
        .expect("spawn slow");
    let quick = fixture
        .mux()
        .spawn("", Some(ShellId::from("quick")), None, SpawnOptions::default())
        .await
        .expect("spawn quick");

    start_marked(
        &fixture,
        &slow,
        &format!("printf 'slow\\n' > src/file1.txt; {RACE_HOLD}"),
        CommandOptions::default(),
    )
    .await;

    fixture
        .mux()
        .start_in(
            &quick,
            "printf 'quick\\n' > src/file1.txt",
            CommandOptions::default(),
        )
        .await
        .expect("start quick's line");
    let quick_completion = concluded(&fixture, &quick.sandbox().uid).await;
    assert_eq!(quick_completion.exit_code, Some(0));
    assert!(matches!(
        verdict(&quick_completion.outcome),
        Outcome::Published { .. }
    ));

    let slow_completion = concluded(&fixture, &slow.sandbox().uid).await;
    match verdict(&slow_completion.outcome) {
        Outcome::Stale { stale, .. } => assert!(
            stale.iter().any(|path| path.path == Path::new("src/file1.txt")),
            "the loser names the path the winner published: {stale:?}"
        ),
        other => panic!("expected the slow line to lose the race, got {other:?}"),
    }

    assert_eq!(
        std::fs::read(fixture.seed("src/file1.txt")).expect("read the seed file"),
        b"quick\n"
    );

    fixture.mux().stop(&slow, true).await.expect("stop slow");
    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn a_publication_invalidates_every_older_snapshot() {
    let mut fixture = Fixture::new();
    let slow = fixture
        .mux()
        .spawn("", Some(ShellId::from("slow")), None, SpawnOptions::default())
        .await
        .expect("spawn slow");
    let quick = fixture
        .mux()
        .spawn("", Some(ShellId::from("quick")), None, SpawnOptions::default())
        .await
        .expect("spawn quick");

    start_marked(
        &fixture,
        &slow,
        &format!("printf 'b\\n' > src/b.txt; {RACE_HOLD}"),
        CommandOptions::default(),
    )
    .await;

    fixture
        .mux()
        .start_in(&quick, "printf 'a\\n' > src/a.txt", CommandOptions::default())
        .await
        .expect("start quick's line");
    let quick_completion = concluded(&fixture, &quick.sandbox().uid).await;
    assert_eq!(quick_completion.exit_code, Some(0));
    assert!(matches!(
        verdict(&quick_completion.outcome),
        Outcome::Published { .. }
    ));

    let slow_completion = concluded(&fixture, &slow.sandbox().uid).await;
    match verdict(&slow_completion.outcome) {
        Outcome::Stale { stale, .. } => assert!(
            stale.iter().any(|path| path.path == Path::new("src/a.txt")),
            "the loser's write set names the winner's new file: {stale:?}"
        ),
        other => panic!("expected the slow line to lose the race, got {other:?}"),
    }

    assert_eq!(
        std::fs::read(fixture.seed("src/a.txt")).expect("read the seed file"),
        b"a\n"
    );
    assert!(!fixture.seed("src/b.txt").exists());

    fixture.mux().stop(&slow, true).await.expect("stop slow");
    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn one_terminal_size_governs_every_job() {
    let mut fixture = Fixture::new();
    let nested_job = fixture
        .mux()
        .spawn(
            "src",
            Some(ShellId::from("nested")),
            None,
            SpawnOptions::default(),
        )
        .await
        .expect("spawn a nested job");
    observed_ready(&fixture, &nested_job);
    let root_job = fixture
        .mux()
        .spawn("", Some(ShellId::from("root")), None, SpawnOptions::default())
        .await
        .expect("spawn a root job");
    observed_ready(&fixture, &root_job);

    assert_eq!(size_of_job(&fixture, &nested_job).await, "24 80");
    assert_eq!(size_of_job(&fixture, &root_job).await, "24 80");

    let nested_dir = run_line(&fixture, &nested_job, "pwd").await;
    let root_dir = run_line(&fixture, &root_job, "pwd").await;
    assert!(nested_dir.ends_with("/src"), "nested job works in src: {nested_dir}");
    assert!(
        !root_dir.ends_with("/src"),
        "root job works at the seed root: {root_dir}"
    );
    assert_eq!(
        PathBuf::from(&nested_dir),
        fixture
            .snap()
            .join(nested_job.sandbox().uid.as_str())
            .join("src"),
        "the nested job works inside its own snapshot"
    );

    let spawn_task = tokio::spawn({
        let mux = Arc::clone(fixture.mux());
        async move {
            mux.spawn("", Some(ShellId::from("late")), None, SpawnOptions::default())
                .await
        }
    });
    let resize_task = tokio::spawn({
        let mux = Arc::clone(fixture.mux());
        async move {
            mux.resize_all(TerminalGeometry {
                rows: 40,
                cols: 120,
            })
            .await
        }
    });
    let (spawned, resized) = tokio::join!(spawn_task, resize_task);
    let late_job = spawned.expect("join the spawn task").expect("spawn a late job");
    resized.expect("join the resize task").expect("resize the mux");
    observed_ready(&fixture, &late_job);

    // One pass moves the default *and* every terminal that was already open: each job's own
    // terminal is announced in its own right, so a frontend never has to infer which ones moved.
    assert_eq!(
        fixture.recorder().dimensions(),
        (40, 120),
        "the default future jobs open at moved"
    );
    for job in [&nested_job, &root_job] {
        assert_eq!(
            fixture.recorder().resized(&job.sandbox().uid),
            Some(TerminalGeometry {
                rows: 40,
                cols: 120
            }),
            "{} was resized in its own right",
            job.id()
        );
    }

    assert_eq!(size_of_job(&fixture, &nested_job).await, "40 120");
    assert_eq!(size_of_job(&fixture, &late_job).await, "40 120");

    let refused = fixture
        .mux()
        .resize_all(TerminalGeometry { rows: 0, cols: 100 })
        .await;
    assert!(matches!(
        refused,
        Err(MuxError::InvalidTerminalSize { rows: 0, cols: 100 })
    ));
    assert_eq!(
        fixture.recorder().dimensions(),
        (40, 120),
        "a refused resize changes nothing"
    );

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn a_zero_dimension_is_refused_before_the_mux_is_built() {
    let mut fixture = Fixture::new();
    fixture.finish_mux().await;

    let result = ShellMux::new(
        fixture.open_executor().expect("reopen the seed"),
        Arc::new(Mutex::new(PolicyValidator::default())),
        MuxProfile {
            environment: ShellEnvironment::new(),
            ..MuxProfile::default()
        },
        Arc::new(Mutex::new(Recorder::new(0, COLS))),
    );
    assert!(matches!(
        result,
        Err(MuxError::InvalidTerminalSize { rows: 0, cols: COLS })
    ));

    assert!(
        fixture.open_executor().is_ok(),
        "the consumed executor released its lease"
    );
}

#[tokio::test]
#[serial]
async fn a_job_opened_for_a_command_is_in_the_table_before_it_starts() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .spawn(
            "",
            Some(ShellId::from("bg")),
            Some("printf 'done\\n' > src/file0.txt"),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn a job with a command");

    // Inspected before anything is awaited: the receipt is reserved under the same lock that
    // admitted the job, so it cannot be missing merely because the launch task has not run.
    let reserved = job
        .initial_command()
        .expect("the command's receipt exists the instant spawn returns");
    assert_eq!(reserved.text(), "printf 'done\\n' > src/file0.txt");

    let view = fixture
        .mux()
        .job(job.id())
        .expect("the row exists before the launch runs");
    assert!(view.starting, "the row is reserved before anything ran");
    assert!(view.running.is_none());

    assert!(matches!(
        fixture
            .mux()
            .spawn("", Some(job.id().clone()), None, SpawnOptions::default())
            .await,
        Err(MuxError::JobExists(_))
    ));

    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(completion.id, reserved.id(), "the reserved receipt is the one that resolved");
    assert_eq!(completion.exit_code, Some(0));
    assert!(matches!(verdict(&completion.outcome), Outcome::Published { .. }));
    assert_eq!(
        std::fs::read(fixture.seed("src/file0.txt")).expect("read the seed file"),
        b"done\n"
    );

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn graceful_stop_requested_while_starting_finishes_and_closes() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .spawn(
            "",
            Some(ShellId::from("1")),
            Some("printf 'done\\n' > src/file0.txt"),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn a job with a command");

    fixture
        .mux()
        .stop(&job, false)
        .await
        .expect("graceful stop while starting");
    assert!(
        !fixture.mux().keep(&job).expect("the job is still live"),
        "an explicit stop is not a closure keep may quietly revoke"
    );

    assert!(
        fixture.mux().job(job.id()).is_some(),
        "a graceful stop does not retire the row"
    );
    assert!(matches!(
        fixture
            .mux()
            .start_in(&job, "echo too-late", CommandOptions::default())
            .await,
        Err(MuxError::JobClosing(_))
    ));

    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(completion.exit_code, Some(0));
    assert!(matches!(verdict(&completion.outcome), Outcome::Published { .. }));
    assert_eq!(
        std::fs::read(fixture.seed("src/file0.txt")).expect("read the seed file"),
        b"done\n"
    );

    wait_for_close(&fixture, &job.sandbox().uid).await;
    assert!(fixture.mux().job(job.id()).is_none());
    eventually("the sandbox is removed", || {
        !fixture.snap().join(job.sandbox().uid.as_str()).exists()
    })
    .await;

    // The handle outlived its job, so it names an instance that is gone rather than a name that
    // is merely free: it can never reach whatever takes the name next.
    assert!(matches!(
        fixture.mux().stop(&job, false).await,
        Err(MuxError::StaleJob(_))
    ));

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn force_stop_requested_while_starting_discards_the_line() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .spawn(
            "",
            Some(ShellId::from("forced")),
            Some("printf 'partial\\n' > src/file0.txt; sleep 60"),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn a job with a command");

    fixture
        .mux()
        .stop(&job, true)
        .await
        .expect("force stop while starting");

    assert!(
        fixture.mux().job(job.id()).is_none(),
        "the job left public view at once"
    );
    assert!(!fixture.mux().jobs().iter().any(|view| &view.id == job.id()));
    assert!(
        matches!(
            fixture
                .mux()
                .spawn("", Some(job.id().clone()), None, SpawnOptions::default())
                .await,
            Err(MuxError::JobExists(_))
        ),
        "the row still exists until the launch task runs"
    );

    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert!(
        completion.exit_code.is_none(),
        "nothing ran, so there is no process status to report: {:?}",
        completion.exit_code
    );
    assert!(matches!(verdict(&completion.outcome), Outcome::Discarded));

    assert_eq!(
        std::fs::read(fixture.seed("src/file0.txt")).expect("read the seed file"),
        b"zero\n"
    );

    wait_for_close(&fixture, &job.sandbox().uid).await;
    eventually("the sandbox is removed", || {
        !fixture.snap().join(job.sandbox().uid.as_str()).exists()
    })
    .await;

    assert!(
        fixture
            .mux()
            .spawn("", Some(job.id().clone()), None, SpawnOptions::default())
            .await
            .is_ok(),
        "the name is free again"
    );

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn a_forced_stop_kills_the_running_line_and_discards_it() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .spawn("", Some(ShellId::from(MAIN)), None, SpawnOptions::default())
        .await
        .expect("spawn a job");

    let (tx, rx) = tokio::sync::oneshot::channel();
    let done: OnFinish = Box::new(move |code| {
        let _ = tx.send(code);
    });
    start_held(
        &fixture,
        &job,
        "printf 'partial\\n' > src/f.txt; ",
        CommandOptions {
            on_finish: Some(done),
            ..CommandOptions::default()
        },
    )
    .await;

    fixture
        .mux()
        .stop(&job, true)
        .await
        .expect("force stop the running line");

    let killed_code = tokio::time::timeout(TIMEOUT, rx)
        .await
        .expect("the kill arrived well under the timeout")
        .expect("the done callback fired");
    assert_eq!(killed_code, 137, "the external was killed by SIGKILL");

    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(completion.exit_code, Some(137));
    assert!(matches!(verdict(&completion.outcome), Outcome::Discarded));

    assert!(
        !fixture.seed("src/f.txt").exists(),
        "the partial write never reached the seed"
    );

    wait_for_close(&fixture, &job.sandbox().uid).await;
    assert!(
        std::fs::read_dir(fixture.snap())
            .map_or(true, |mut entries| entries.next().is_none()),
        "the snapshot is gone"
    );

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn a_finish_callback_reports_each_command_once() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .spawn("", Some(ShellId::from(MAIN)), None, SpawnOptions::default())
        .await
        .expect("spawn a job");

    let (tx1, rx1) = tokio::sync::oneshot::channel();
    let done1: OnFinish = Box::new(move |code| {
        let _ = tx1.send(code);
    });
    fixture
        .mux()
        .start_in(
            &job,
            "sh -c 'exit 3'",
            CommandOptions {
                on_finish: Some(done1),
                ..CommandOptions::default()
            },
        )
        .await
        .expect("start the first line");
    let code1 = tokio::time::timeout(TIMEOUT, rx1)
        .await
        .expect("the first callback timed out")
        .expect("the callback channel");
    assert_eq!(code1, 3);
    let first = concluded(&fixture, &job.sandbox().uid).await;

    let (tx2, rx2) = tokio::sync::oneshot::channel();
    let done2: OnFinish = Box::new(move |code| {
        let _ = tx2.send(code);
    });
    start_held(
        &fixture,
        &job,
        "",
        CommandOptions {
            on_finish: Some(done2),
            ..CommandOptions::default()
        },
    )
    .await;

    let (tx_extra, _rx_extra) = tokio::sync::oneshot::channel();
    let extra: OnFinish = Box::new(move |code| {
        let _ = tx_extra.send(code);
    });
    assert!(
        !fixture
            .mux()
            .on_finish(&job, extra)
            .expect("the job is still live"),
        "a callback is already registered for the running command"
    );

    fixture
        .mux()
        .stop(&job, true)
        .await
        .expect("force stop the held line");
    let code2 = tokio::time::timeout(TIMEOUT, rx2)
        .await
        .expect("the second callback timed out")
        .expect("the callback channel");
    assert_eq!(code2, 137);
    let second = concluded(&fixture, &job.sandbox().uid).await;

    assert_ne!(
        first.id, second.id,
        "each line has a receipt of its own; no completion is stolen"
    );
    assert_eq!(fixture.recorder().exit_codes(&job.sandbox().uid), vec![3, 137]);

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn a_callback_nobody_can_register_is_dropped_at_once() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .spawn("", Some(ShellId::from(MAIN)), None, SpawnOptions::default())
        .await
        .expect("spawn a job");

    let (tx_idle, rx_idle) = tokio::sync::oneshot::channel();
    let idle: OnFinish = Box::new(move |code| {
        let _ = tx_idle.send(code);
    });
    assert!(
        !fixture
            .mux()
            .on_finish(&job, idle)
            .expect("the job is live"),
        "the job is idle: nothing to report"
    );
    assert!(rx_idle.await.is_err(), "the callback was dropped, not called");

    // A handle whose job has closed names an instance that is gone. Registering through it is
    // refused outright rather than silently attached to whatever holds the name now.
    let ghost = fixture
        .mux()
        .spawn("", Some(ShellId::from("ghost")), None, SpawnOptions::default())
        .await
        .expect("spawn ghost");
    let ghost_uid = ghost.sandbox().uid.clone();
    fixture.mux().stop(&ghost, true).await.expect("stop ghost");
    wait_for_close(&fixture, &ghost_uid).await;

    let (tx_unknown, rx_unknown) = tokio::sync::oneshot::channel();
    let unknown: OnFinish = Box::new(move |code| {
        let _ = tx_unknown.send(code);
    });
    assert!(matches!(
        fixture.mux().on_finish(&ghost, unknown),
        Err(MuxError::StaleJob(_))
    ));
    assert!(rx_unknown.await.is_err());

    let (tx_first, rx_first) = tokio::sync::oneshot::channel();
    let first: OnFinish = Box::new(move |code| {
        let _ = tx_first.send(code);
    });
    start_held(
        &fixture,
        &job,
        "",
        CommandOptions {
            on_finish: Some(first),
            ..CommandOptions::default()
        },
    )
    .await;

    let (tx_refused, rx_refused) = tokio::sync::oneshot::channel();
    let refused: OnFinish = Box::new(move |code| {
        let _ = tx_refused.send(code);
    });
    assert!(matches!(
        fixture
            .mux()
            .start_in(
                &job,
                "echo too-late",
                CommandOptions {
                    on_finish: Some(refused),
                    ..CommandOptions::default()
                },
            )
            .await,
        Err(MuxError::JobBusy(_))
    ));
    assert!(
        rx_refused.await.is_err(),
        "the refused callback was never registered"
    );

    fixture
        .mux()
        .stop(&job, true)
        .await
        .expect("force stop the held line");
    let code = tokio::time::timeout(TIMEOUT, rx_first)
        .await
        .expect("the first callback timed out")
        .expect("the callback channel");
    assert_eq!(code, 137);
    let _ = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(fixture.recorder().exit_codes(&job.sandbox().uid), vec![137]);

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn shutdown_discards_running_lines_and_drops_waiting_callbacks() {
    let mut fixture = Fixture::new();
    let _idle = fixture
        .mux()
        .spawn("", Some(ShellId::from("idle")), None, SpawnOptions::default())
        .await
        .expect("spawn idle");
    let busy = fixture
        .mux()
        .spawn("", Some(ShellId::from("busy")), None, SpawnOptions::default())
        .await
        .expect("spawn busy");

    let (tx, mut rx) = tokio::sync::oneshot::channel();
    let done: OnFinish = Box::new(move |code| {
        let _ = tx.send(code);
    });
    start_held(
        &fixture,
        &busy,
        "printf 'partial\\n' > src/g.txt; ",
        CommandOptions {
            on_finish: Some(done),
            ..CommandOptions::default()
        },
    )
    .await;
    let busy_uid = busy.sandbox().uid.clone();

    tokio::time::timeout(TIMEOUT, fixture.finish_mux())
        .await
        .expect("shutdown timed out");

    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
    assert!(!fixture.seed("src/g.txt").exists());
    assert!(
        std::fs::read_dir(fixture.snap())
            .map_or(true, |mut entries| entries.next().is_none())
    );
    assert!(fixture.recorder().mux().is_none());
    assert!(
        fixture.recorder().results(&busy_uid).is_empty(),
        "shutdown reports nothing"
    );
}

#[tokio::test]
#[serial]
async fn owned_job_futures_progress_while_another_job_is_blocked() {
    let mut fixture = Fixture::new();
    let blocked = fixture
        .mux()
        .spawn(
            "",
            Some(ShellId::from("blocked")),
            None,
            SpawnOptions::default(),
        )
        .await
        .expect("spawn the blocked job");
    fixture
        .mux()
        .start_in(&blocked, "cat", CommandOptions::default())
        .await
        .expect("start cat");

    let worker = fixture
        .mux()
        .spawn(
            "",
            Some(ShellId::from("worker")),
            None,
            SpawnOptions::default(),
        )
        .await
        .expect("spawn the worker job");

    fixture.mux().switch(&worker).await.expect("switch to the worker");
    assert_eq!(
        fixture.mux().current_job().map(|view| view.id),
        Some(worker.id().clone())
    );
    assert_eq!(fixture.mux().jobs().len(), 2);
    assert!(fixture.mux().history().is_empty());

    fixture
        .mux()
        .resize_all(TerminalGeometry { rows: 28, cols: 96 })
        .await
        .expect("resize while a job is blocked");

    fixture
        .mux()
        .start_in(
            &worker,
            "printf 'x\\n' > src/file0.txt",
            CommandOptions::default(),
        )
        .await
        .expect("start the worker's line");
    let worker_completion = concluded(&fixture, &worker.sandbox().uid).await;
    assert_eq!(worker_completion.exit_code, Some(0));
    assert!(matches!(
        verdict(&worker_completion.outcome),
        Outcome::Published { .. }
    ));
    assert_eq!(fixture.mux().history().len(), 1);

    fixture
        .mux()
        .write_input(&blocked, b"\x04")
        .await
        .expect("send eof to cat");
    let blocked_completion = concluded(&fixture, &blocked.sandbox().uid).await;
    assert_eq!(blocked_completion.exit_code, Some(0));

    fixture
        .mux()
        .stop(&blocked, false)
        .await
        .expect("stop the blocked job");
    wait_for_close(&fixture, &blocked.sandbox().uid).await;

    let mux = fixture.mux.take().expect("the mux is still present");
    tokio::spawn(async move { mux.shutdown().await.expect("shut down from a spawned task") })
        .await
        .expect("join the shutdown task");
}

#[tokio::test]
#[serial]
async fn an_unselected_job_needs_no_reader() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .spawn(
            "",
            Some(ShellId::from(MAIN)),
            Some("dd if=/dev/zero bs=65536 count=16 2>/dev/null"),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn a job with a command");

    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(completion.exit_code, Some(0));

    fixture.mux().stop(&job, false).await.expect("stop the job");
    wait_for_close(&fixture, &job.sandbox().uid).await;

    let output = fixture.recorder().take_terminal(&job.sandbox().uid);
    assert_eq!(output.len(), 1_048_576);
    assert!(output.iter().all(|&byte| byte == 0));
    assert!(!fixture.recorder().closed_with_storage(&job.sandbox().uid));

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn a_bound_frontend_drives_the_table_it_observes() {
    let mut fixture = Fixture::new();
    assert_eq!(
        std::ptr::from_ref::<ShellMux>(
            fixture.recorder().mux().expect("the frontend is bound").as_ref()
        ),
        Arc::as_ptr(fixture.mux()),
        "the frontend is bound to this fixture's own mux"
    );

    let alpha = fixture
        .mux()
        .spawn("", Some(ShellId::from("alpha")), None, SpawnOptions::default())
        .await
        .expect("spawn alpha");
    observed_ready(&fixture, &alpha);
    let beta = fixture
        .mux()
        .spawn("", Some(ShellId::from("beta")), None, SpawnOptions::default())
        .await
        .expect("spawn beta");
    observed_ready(&fixture, &beta);

    let ids: Vec<_> = fixture
        .recorder()
        .observed_jobs()
        .iter()
        .map(|job| job.id.clone())
        .collect();
    assert_eq!(ids, vec![alpha.id().clone(), beta.id().clone()]);

    fixture.mux().switch(&beta).await.expect("switch to beta");
    assert_eq!(fixture.recorder().observed_current(), Some(beta.id()));

    assert!(matches!(
        fixture
            .mux()
            .spawn("", Some(alpha.id().clone()), None, SpawnOptions::default())
            .await,
        Err(MuxError::JobExists(_))
    ));
    let escape = ShellId::from("escape");
    assert!(matches!(
        fixture
            .mux()
            .spawn(
                "../outside",
                Some(escape.clone()),
                None,
                SpawnOptions::default()
            )
            .await,
        Err(MuxError::SandboxDir { .. })
    ));

    assert!(fixture.recorder().handle(&escape).is_none());
    assert_eq!(
        fixture
            .recorder()
            .handle(alpha.id())
            .map(|spawned| spawned.sandbox().uid.clone()),
        Some(alpha.sandbox().uid.clone())
    );

    fixture.mux().stop(&beta, true).await.expect("force stop beta");
    let ids: Vec<_> = fixture
        .recorder()
        .observed_jobs()
        .iter()
        .map(|job| job.id.clone())
        .collect();
    assert_eq!(ids, vec![alpha.id().clone()]);
    assert!(fixture.recorder().observed_current().is_none());

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn input_reaches_a_job_and_every_result_is_delivered_once() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .spawn(
            "",
            Some(ShellId::from(MAIN)),
            Some("stty -echo; printf READY; cat"),
            SpawnOptions::default(),
        )
        .await
        .expect("spawn a job with a command");

    drain_output(&fixture, &job, "ready marker", |bytes| {
        contains_subslice(bytes, b"READY")
    })
    .await;

    fixture
        .mux()
        .write_input(&job, b"roundtrip\n")
        .await
        .expect("write to cat");
    drain_output(&fixture, &job, "roundtrip echo", |bytes| {
        contains_subslice(bytes, b"roundtrip\r\n")
    })
    .await;

    fixture
        .mux()
        .write_input(&job, b"\x04")
        .await
        .expect("send eof");
    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(completion.exit_code, Some(0));
    assert_eq!(fixture.recorder().exit_codes(&job.sandbox().uid), vec![0]);

    let second = run_to_marker(
        &fixture,
        &job,
        "printf 'second\\n'; sh -c 'exit 7'",
        b"second\r\n",
    )
    .await;
    assert_eq!(second, Some(7));
    assert_eq!(fixture.recorder().exit_codes(&job.sandbox().uid), vec![0, 7]);

    fixture.mux().stop(&job, false).await.expect("stop the job");
    wait_for_close(&fixture, &job.sandbox().uid).await;
    history_intact(&fixture, &job.sandbox().uid, &[0, 7]);
    assert!(fixture.recorder().handle(job.id()).is_none());

    let reopened = fixture
        .mux()
        .spawn("", Some(ShellId::from(MAIN)), None, SpawnOptions::default())
        .await
        .expect("reopen the name");
    assert_ne!(reopened.sandbox().uid, job.sandbox().uid);

    let again = run_to_marker(&fixture, &reopened, "printf 'again\\n'", b"again\r\n").await;
    assert_eq!(again, Some(0));
    history_intact(&fixture, &job.sandbox().uid, &[0, 7]);
    assert_eq!(
        fixture.recorder().exit_codes(&reopened.sandbox().uid),
        vec![0]
    );
    assert_eq!(
        fixture
            .recorder()
            .handle(reopened.id())
            .map(|spawned| spawned.sandbox().uid.clone()),
        Some(reopened.sandbox().uid.clone())
    );

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn shutdown_detaches_frontend_without_retaining_session() {
    let mut fixture = Fixture::new();
    let idle = fixture
        .mux()
        .spawn("", Some(ShellId::from("idle")), None, SpawnOptions::default())
        .await
        .expect("spawn idle");
    let worked = fixture
        .mux()
        .spawn(
            "",
            Some(ShellId::from("worked")),
            None,
            SpawnOptions::default(),
        )
        .await
        .expect("spawn worked");

    fixture
        .mux()
        .start_in(&worked, "printf 'done\\n'", CommandOptions::default())
        .await
        .expect("start the line");
    let completion = concluded(&fixture, &worked.sandbox().uid).await;
    assert_eq!(completion.exit_code, Some(0));

    tokio::time::timeout(TIMEOUT, fixture.finish_mux())
        .await
        .expect("shutdown timed out");

    assert!(fixture.recorder().mux().is_none());
    assert!(fixture.recorder().handle(idle.id()).is_none());
    assert!(fixture.recorder().handle(worked.id()).is_none());
    assert_eq!(fixture.recorder().exit_codes(&worked.sandbox().uid), vec![0]);

    assert!(
        fixture.open_executor().is_ok(),
        "the session lease was released with the mux"
    );
}

#[tokio::test]
#[serial]
async fn a_denial_is_atomic_and_a_claim_outlives_its_job() {
    let mut fixture = Fixture::new();
    let owner = fixture
        .mux()
        .spawn("", Some(ShellId::from("owner")), None, SpawnOptions::default())
        .await
        .expect("spawn owner");
    let other = fixture
        .mux()
        .spawn("", Some(ShellId::from("other")), None, SpawnOptions::default())
        .await
        .expect("spawn other");

    fixture
        .mux()
        .start_in(
            &owner,
            "printf 'owned\\n' > src/file1.txt",
            CommandOptions::default(),
        )
        .await
        .expect("start owner's line");
    let owner_completion = concluded(&fixture, &owner.sandbox().uid).await;
    assert_eq!(owner_completion.exit_code, Some(0));
    assert!(matches!(
        verdict(&owner_completion.outcome),
        Outcome::Published { .. }
    ));

    fixture
        .mux()
        .start_in(
            &other,
            "printf 'new\\n' > src/new.txt; printf 'bad\\n' > src/file1.txt",
            CommandOptions::default(),
        )
        .await
        .expect("start other's line");
    let other_completion = concluded(&fixture, &other.sandbox().uid).await;
    assert_eq!(
        other_completion.exit_code,
        Some(0),
        "the builtins succeed; the gate is what refuses the line"
    );
    match verdict(&other_completion.outcome) {
        Outcome::Denied { requested, .. } => assert!(
            requested
                .iter()
                .any(|event| event.resource == Resource::from(vec!["src", "new.txt"])),
            "the denied line still names its other-touched path: {requested:?}"
        ),
        other => panic!("expected a denial, got {other:?}"),
    }
    assert!(
        !other_completion.is_published(),
        "a zero exit is not a publication"
    );

    assert_eq!(
        std::fs::read(fixture.seed("src/file1.txt")).expect("read the seed file"),
        b"owned\n"
    );
    assert!(
        !fixture.seed("src/new.txt").exists(),
        "the denied line's other write never reached the seed"
    );

    assert_eq!(
        fixture.mux().history(),
        vec![Event::new(
            "owner",
            Action::Edit,
            Resource::from(vec!["src", "file1.txt"])
        )],
        "the granted first edit of the denied line is not recorded"
    );

    fixture.mux().stop(&owner, true).await.expect("force stop owner");
    assert!(fixture.mux().job(owner.id()).is_none());
    assert_eq!(
        fixture.mux().history().len(),
        1,
        "the claim outlives the job that earned it"
    );

    fixture.finish_mux().await;
}

/// A pane's prompt takes its terminal back after every line, whether or not the line was a command.
///
/// The interactive loop is: lease the idle terminal, read a line, release the lease, run the line —
/// or answer it at the front-end and admit nothing at all — then lease again. Two separate pieces
/// of state used to make the second iteration impossible, and a pane accepted exactly one line
/// before going deaf. The reservation a grant took was reaped only by an admission or a stop, so a
/// prompt that released without admitting anything stayed busy for a lease nobody held; and the
/// run-revocation an admission raised was never lowered, so the lease the prompt took after its
/// command finished answered `None` to its first read.
///
/// Both are load-bearing for a terminal nobody can see from a unit test, so this drives the real
/// loop: it reads through the lease, releases it, publishes a line, and does it all again.
#[tokio::test]
#[serial]
async fn a_prompt_leases_its_terminal_again_after_every_line() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .spawn("", Some(ShellId::from("1")), None, SpawnOptions::default())
        .await
        .expect("spawn a job");

    // A line the front-end answers itself: `jobs`, `fg`, an invalid line. Nothing is admitted, so
    // nothing reaps the reservation on an admission's behalf.
    let lease = fixture
        .mux()
        .idle_terminal(&job)
        .await
        .expect("lease the idle terminal");
    assert!(!lease.is_revoked(), "a fresh lease starts live");
    drop(lease);

    // The prompt asks for its own terminal back. This is the iteration that used to be refused
    // with `TerminalBusy` forever.
    let lease = fixture
        .mux()
        .idle_terminal(&job)
        .await
        .expect("lease again after releasing without admitting a command");
    assert!(read_through(&fixture, &job, &lease, b"typed\n").await);
    drop(lease);

    assert_eq!(
        run_line(&fixture, &job, "printf 'one\\n' > src/file0.txt; printf 'ok1\\n'").await,
        "ok1"
    );

    // And again after a command, which is the iteration the un-lowered run-revocation used to
    // leave granted-but-deaf: the lease was handed over, and every read answered `None`.
    let lease = fixture
        .mux()
        .idle_terminal(&job)
        .await
        .expect("lease again after a command");
    assert!(
        !lease.is_revoked(),
        "the finished command's revocation was lowered"
    );
    assert!(read_through(&fixture, &job, &lease, b"typed again\n").await);
    drop(lease);

    assert_eq!(
        run_line(&fixture, &job, "printf 'two\\n' > src/file1.txt; printf 'ok2\\n'").await,
        "ok2"
    );

    assert!(
        fixture.mux().job(job.id()).is_some(),
        "the pane is still open after both lines"
    );
    assert_eq!(
        std::fs::read(fixture.seed("src/file0.txt")).expect("read the first file"),
        b"one\n"
    );
    assert_eq!(
        std::fs::read(fixture.seed("src/file1.txt")).expect("read the second file"),
        b"two\n"
    );

    fixture.finish_mux().await;
}

/// Whether `bytes` written to `job`'s terminal reach a prompt holding `lease`.
///
/// The whole point of a lease: the keyboard of one pane, read by that pane's prompt and by nothing
/// else. A revoked lease answers `None` instead, which is what a prompt granted a terminal it
/// cannot read looks like.
async fn read_through(
    fixture: &Fixture,
    job: &Spawned,
    lease: &marsh::shellmux::IdleTerminal,
    bytes: &[u8],
) -> bool {
    fixture
        .mux()
        .write_input(job, bytes)
        .await
        .expect("write to the leased terminal");
    let mut seen = Vec::new();
    tokio::time::timeout(TIMEOUT, async {
        while !contains_subslice(&seen, bytes) {
            let mut buffer = [0_u8; 64];
            match lease
                .read(&mut buffer)
                .await
                .expect("read the leased terminal")
            {
                None | Some(0) => return false,
                Some(read) => seen.extend_from_slice(&buffer[..read]),
            }
        }
        true
    })
    .await
    .unwrap_or(false)
}
