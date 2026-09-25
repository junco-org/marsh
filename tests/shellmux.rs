#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::tests_outside_test_module,
    reason = "an integration test file is the test module"
)]
//! Shells as a front-end drives them: `open_shell`, `run_command`, `stop` and `on_finish`, over
//! the pseudoterminal every terminal shell owns, with the frontend the mux delivers all of it to.
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
use std::time::{Duration, Instant};

use brush_core::env::ShellEnvironment;
use marsh::policy::{Action, Event, Principal, Resource};
use marsh::shellmux::{
    CommandCompletion, CommandHandle, CommandOptions, FrontendEvent, JobCloseMode, JobView,
    MuxError, MuxProfile, OnFinish, OutputChannel, RunError, Shell, ShellFrontend, ShellId,
    ShellMux, SnapshotUid, SpawnOptions, TerminalGeometry,
};
use marsh::{MarshError, MarshExecutor, Outcome, Publication};
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
    /// Each admitted job's own snapshot root, by uid, for [`Self::closed_with_storage`].
    ///
    /// Filled from every [`JobView`] a `Changed` carries rather than from one fixed directory: a
    /// mux hosts jobs over several seeds, so there is no single `snap` to check a closing job
    /// against. Recorded at admission — before the row has resources — so a job whose
    /// construction fails is covered too.
    snapshot_roots: HashMap<SnapshotUid, PathBuf>,
    /// Live job handles, by identity; removed once [`FrontendEvent::Closed`] names the same uid.
    handles: HashMap<ShellId, Shell>,
    /// The job table as of the last [`FrontendEvent::Changed`].
    observed_jobs: Vec<JobView>,
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
    /// The mux this recorder is bound to, when it is bound to one.
    fn mux(&self) -> Option<Arc<ShellMux>> {
        self.mux.upgrade()
    }

    /// The live handle for job `id`, when one is open under that name.
    fn handle(&self, id: &ShellId) -> Option<Shell> {
        self.handles.get(id).cloned()
    }

    /// The job table as this recorder last observed it.
    fn observed_jobs(&self) -> &[JobView] {
        &self.observed_jobs
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
        Self {
            size: (rows, cols),
            mux: Weak::new(),
            snapshot_roots: HashMap::new(),
            handles: HashMap::new(),
            observed_jobs: Vec::new(),
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
                    // Every row, admitted ones included: a job whose construction later fails
                    // still has to be checked for a leftover snapshot when it closes, and by then
                    // its row is gone.
                    for view in &self.observed_jobs {
                        if let Some(root) = view.snapshot_root.clone() {
                            self.snapshot_roots
                                .entry(view.sandbox.uid.clone())
                                .or_insert(root);
                        }
                    }
                }
            }
            FrontendEvent::Opened(opened) => {
                self.handles.insert(opened.id().clone(), opened.clone());
            }
            FrontendEvent::CommandAccepted { .. } => {
                // The admission receipt is the caller's: every test that needs it holds the
                // `CommandHandle` its own `on_accept` sender was given.
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
                if let Some(root) = self.snapshot_roots.remove(&shell.uid) {
                    if root.exists() {
                        self.closed_with_storage.insert(shell.uid.clone());
                    }
                }
                self.closed.insert(shell.uid.clone());
                if self
                    .handles
                    .get(&shell.id)
                    .is_some_and(|held| held.sandbox().uid == shell.uid)
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
    /// The canonical scratch root both the seed and its state directory live under.
    root: PathBuf,
    /// The tree this fixture's first shell publishes into.
    seed: PathBuf,
    /// `<scratch>/.marsh/seed`: where snapshots and the log live.
    state: PathBuf,
    /// The btrfs stand-in every seed of this fixture is reached through.
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

        let frontend = Arc::new(Mutex::new(Recorder::new(ROWS, COLS)));
        let mux = ShellMux::new_with(
            MuxProfile {
                environment: ShellEnvironment::new(),
                ..MuxProfile::default()
            },
            Arc::clone(&frontend),
            fs.clone(),
        )
        .expect("build the mux");

        Self {
            _scratch: scratch,
            root,
            seed,
            state,
            fs,
            mux: Some(mux),
            frontend,
        }
    }

    /// Registers a second seed named `name` beside the first, containing `src/`.
    ///
    /// A sibling rather than a nested tree, so the two keep separate `.marsh` state directories —
    /// which is the layout two independent seeds actually have.
    fn sibling_seed(&self, name: &str) -> PathBuf {
        let seed = self.root.join(name);
        std::fs::create_dir_all(seed.join("src")).expect("sibling seed tree");
        self.fs.register(&seed);
        seed
    }

    /// `<state root>/snap` for a seed registered by [`Self::sibling_seed`].
    fn sibling_snap(&self, name: &str) -> PathBuf {
        self.root.join(".marsh").join(name).join("snap")
    }

    /// An existing directory under no subvolume at all.
    fn scratch_dir(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::create_dir_all(&path).expect("an unregistered scratch directory");
        path
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
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
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
    wait_for(fixture, |recorder| ready(recorder).then_some(()))
        .await
        .is_some()
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
    job: &Shell,
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

/// Admits `cmd` into `job` and answers with its receipt, leaving the line running.
///
/// What a test that has to act *while* a line runs uses. [`Shell::run_command`] resolves with the
/// completed verdict, which is far too late to feed standard input, force-stop the line or watch a
/// callback fire; `on_accept` is the admission itself.
///
/// The run future is polled only until that admission and then dropped, which is exactly what the
/// contract says it is for: the collection owns the work from the first poll, so dropping this
/// abandons the *answer* and nothing else. Every test here reads that answer from the frontend's
/// own [`FrontendEvent::Finished`] instead, through [`concluded`].
///
/// # Panics
///
/// Panics if the line is never admitted.
async fn schedule(job: &Shell, cmd: &str, options: CommandOptions) -> CommandHandle {
    let (accepted, receipt) = tokio::sync::oneshot::channel();
    let running = job.run_command(
        cmd,
        CommandOptions {
            on_accept: Some(accepted),
            ..options
        },
    );
    // Biased, so the receipt is always read first. A line fast enough to conclude before this
    // polls would otherwise leave both arms ready at once, and a random choice between them would
    // report an admitted command as one that was never admitted.
    tokio::select! {
        biased;
        admitted = receipt => admitted.expect("the line was admitted"),
        outcome = running => panic!("the line ended without ever being admitted: {outcome:?}"),
    }
}

/// Schedules `cmd` in `job`, then waits until its terminal has produced `started`.
///
/// The primitive behind [`start_held`]; called directly by the two race tests, which hold their
/// job open only briefly rather than for [`HELD`]'s whole duration.
async fn start_marked(fixture: &Fixture, job: &Shell, cmd: &str, options: CommandOptions) {
    schedule(job, cmd, options).await;
    drain_output(fixture, job, "start_marked", |bytes| {
        contains_subslice(bytes, b"started")
    })
    .await;
}

/// [`start_marked`] with `prefix` in front of [`HELD`].
async fn start_held(fixture: &Fixture, job: &Shell, prefix: &str, options: CommandOptions) {
    start_marked(fixture, job, &format!("{prefix}{HELD}"), options).await;
}

/// Runs `cmd` in `job` and returns the first line its terminal produced, trimmed.
///
/// # Panics
///
/// Panics if the line does not publish.
async fn run_line(fixture: &Fixture, job: &Shell, cmd: &str) -> String {
    let published = job
        .run_command(cmd, CommandOptions::default())
        .await
        .expect("the line publishes");
    let bytes = drain_output(fixture, job, "run_line", |bytes| bytes.contains(&b'\n')).await;
    let delivered = concluded(fixture, &job.sandbox().uid).await;
    assert_eq!(
        delivered.id, published.id,
        "the verdict the frontend was given is this line's own"
    );
    let first_line = bytes.split(|&byte| byte == b'\n').next().unwrap_or(&[]);
    String::from_utf8_lossy(first_line).trim().to_string()
}

/// The terminal geometry `job` reports, as `"<rows> <cols>"`.
async fn size_of_job(fixture: &Fixture, job: &Shell) -> String {
    run_line(fixture, job, "stty size").await
}

/// Runs `cmd` in `job`, waits for `marker` on its terminal, and reports the process status it
/// ended with.
///
/// What [`run_line`] cannot do: that one hands back a trimmed line, while this returns the raw
/// exit code for a command that ends non-zero on purpose — and still requires the run itself to
/// have *succeeded*, because a process status is not a publication verdict.
async fn run_to_marker(fixture: &Fixture, job: &Shell, cmd: &str, marker: &[u8]) -> Option<i32> {
    job.run_command(cmd, CommandOptions::default())
        .await
        .expect("the line publishes, whatever its process exited with");
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
fn observed_ready(fixture: &Fixture, opened: &Shell) {
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
    assert!(
        buffer_empty,
        "{uid}'s buffer was written to after it closed"
    );
}

/// Where `job`'s shell currently stands, as the mux reports it.
///
/// # Panics
///
/// Panics when the job is not in the table.
fn job_cwd(fixture: &Fixture, job: &Shell) -> PathBuf {
    fixture
        .mux()
        .job(job.id())
        .expect("the job is open")
        .working_directory
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
        .open_shell(
            &fixture.seed,
            Some(Principal::from("1")),
            SpawnOptions::default(),
        )
        .await
        .expect("open a shell");
    job.run_command("printf 'one\\n' > src/file0.txt", CommandOptions::default())
        .await
        .expect("the line publishes");

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
        .open_shell(
            &fixture.seed,
            Some(Principal::from("1")),
            SpawnOptions::default(),
        )
        .await
        .expect("open a shell");

    job.run_command("printf 'x\\n' > src/file0.txt", CommandOptions::default())
        .await
        .expect("the line publishes");
    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(completion.exit_code, Some(0));
    assert!(matches!(
        verdict(&completion.outcome),
        Outcome::Published { .. }
    ));

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

    job.stop(false).await.expect("stop the shell");
    wait_for_close(&fixture, &job.sandbox().uid).await;

    assert!(
        std::fs::read_dir(fixture.snap()).map_or(true, |mut entries| entries.next().is_none()),
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
        .open_shell(
            &fixture.seed,
            Some(Principal::from(MAIN)),
            SpawnOptions::default(),
        )
        .await
        .expect("open a shell");
    start_held(
        &fixture,
        &job,
        "printf 'partial\\n' > src/file0.txt; ",
        CommandOptions::default(),
    )
    .await;

    job.write_input(b"\x03").await.expect("send ctrl-c");

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
        .open_shell(
            &fixture.seed,
            Some(Principal::from(MAIN)),
            SpawnOptions::default(),
        )
        .await
        .expect("open a shell");

    // A failed redirection is a published line with no filesystem effect, so the run succeeds and
    // answers with the completion whose nonzero status is the *process's*, not a verdict.
    let awaited = tokio::time::timeout(
        TIMEOUT,
        job.run_command("echo x >&3", CommandOptions::default()),
    )
    .await
    .expect("the command timed out")
    .expect("the line publishes");
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
        .open_shell(
            &fixture.seed,
            Some(Principal::from(MAIN)),
            SpawnOptions::default(),
        )
        .await
        .expect("open a shell");
    job.run_command(PAYLOAD_CMD, CommandOptions::default())
        .await
        .expect("the line publishes");

    let mut output = drain_output(&fixture, &job, "payload", |bytes| {
        bytes.len() >= PAYLOAD.len()
    })
    .await;

    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(completion.exit_code, Some(0));

    job.stop(false).await.expect("stop the shell");
    wait_for_close(&fixture, &job.sandbox().uid).await;
    complete_tails(&fixture, &job.sandbox().uid, &mut output);

    assert_eq!(
        output, PAYLOAD,
        "the terminal carries the payload byte for byte"
    );

    fixture.finish_mux().await;
}

/// Two jobs truncating one file: the loser is refused a capability, not told to rerun.
///
/// A truncating write reads nothing, so there is no dependency to resynchronize and a second
/// evaluation would be refused for exactly the same reason. The mux reports the shell's verdict
/// unchanged — what it adds is the receipt, not the decision.
#[tokio::test]
#[serial]
async fn concurrent_writers_report_capability_denial() {
    let mut fixture = Fixture::new();
    let slow = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("slow")),
            SpawnOptions::default(),
        )
        .await
        .expect("open slow");
    let quick = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("quick")),
            SpawnOptions::default(),
        )
        .await
        .expect("open quick");

    start_marked(
        &fixture,
        &slow,
        &format!("printf 'slow\\n' > src/file1.txt; {RACE_HOLD}"),
        CommandOptions::default(),
    )
    .await;

    quick
        .run_command(
            "printf 'quick\\n' > src/file1.txt",
            CommandOptions::default(),
        )
        .await
        .expect("quick's line publishes");
    let quick_completion = concluded(&fixture, &quick.sandbox().uid).await;
    assert_eq!(quick_completion.exit_code, Some(0));
    assert!(matches!(
        verdict(&quick_completion.outcome),
        Outcome::Published { .. }
    ));

    let slow_completion = concluded(&fixture, &slow.sandbox().uid).await;
    match verdict(&slow_completion.outcome) {
        Outcome::Denied { denials, .. } => assert!(
            denials.iter().any(|denial| {
                denial.event.action == Action::Edit
                    && denial.event.resource.segments() == ["src", "file1.txt"]
            }),
            "the loser is refused its edit of the path the winner owns: {denials:?}"
        ),
        other => panic!("expected the slow line to be denied, got {other:?}"),
    }

    assert_eq!(
        std::fs::read(fixture.seed("src/file1.txt")).expect("read the seed file"),
        b"quick\n"
    );

    slow.stop(true).await.expect("stop slow");
    fixture.finish_mux().await;
}

/// Two jobs writing *different* files both publish, and each sees the other's result afterwards.
///
/// The held command must run exactly once: a publication elsewhere in the seed is not a
/// dependency, and a design that reran every older snapshot's line would serialize the whole host.
#[tokio::test]
#[serial]
async fn disjoint_concurrent_writes_both_publish() {
    let mut fixture = Fixture::new();
    let slow = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("slow")),
            SpawnOptions::default(),
        )
        .await
        .expect("open slow");
    let quick = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("quick")),
            SpawnOptions::default(),
        )
        .await
        .expect("open quick");

    // Outside every seed: a counter inside the snapshot would be part of the footprint under test,
    // and a replay would throw it away along with everything else.
    let attempts = fixture.seed.parent().expect("a scratch root").join("slow-attempts");
    start_marked(
        &fixture,
        &slow,
        &format!(
            "printf 'x\\n' >> {}; printf 'b\\n' > src/b.txt; {RACE_HOLD}",
            attempts.display()
        ),
        CommandOptions::default(),
    )
    .await;

    quick
        .run_command("printf 'a\\n' > src/a.txt", CommandOptions::default())
        .await
        .expect("quick's line publishes");
    let quick_completion = concluded(&fixture, &quick.sandbox().uid).await;
    assert_eq!(quick_completion.exit_code, Some(0));
    assert!(matches!(
        verdict(&quick_completion.outcome),
        Outcome::Published { .. }
    ));

    let slow_completion = concluded(&fixture, &slow.sandbox().uid).await;
    match verdict(&slow_completion.outcome) {
        Outcome::Published { granted, .. } => assert!(
            granted.iter().all(|event| {
                event.action == Action::Edit && event.resource.segments() == ["src", "b.txt"]
            }),
            "the held line asked only for what it wrote: {granted:?}"
        ),
        other => panic!("expected the held line to publish, got {other:?}"),
    }

    assert_eq!(
        std::fs::read(fixture.seed("src/a.txt")).expect("read a"),
        b"a\n"
    );
    assert_eq!(
        std::fs::read(fixture.seed("src/b.txt")).expect("read b"),
        b"b\n"
    );
    assert_eq!(
        std::fs::read_to_string(&attempts)
            .expect("the attempt counter")
            .lines()
            .count(),
        1,
        "a disjoint publication is not a dependency, so nothing was run twice"
    );

    // The held shell's own snapshot was retaken after it published, so it can now see the file
    // the other job wrote while it was running.
    assert_eq!(
        run_line(&fixture, &slow, "cat src/a.txt").await,
        "a",
        "the publisher's tree is the current seed afterwards"
    );

    slow.stop(true).await.expect("stop slow");
    fixture.finish_mux().await;
}

/// A forced stop while a line is being evaluated again ends it once, unchecked.
///
/// The replay is the shell's own business, so the mux must still see exactly one command: one
/// receipt, one completion, and a discard that publishes nothing. A caller that asked for the
/// command to end did not ask for it to be run a third time.
#[tokio::test]
#[serial]
async fn a_forced_stop_during_a_replay_discards_the_command_once() {
    let mut fixture = Fixture::new();
    std::fs::write(fixture.seed("src/foo.txt"), b"old\n").expect("seed foo.txt");
    let victim = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("victim")),
            SpawnOptions::default(),
        )
        .await
        .expect("open victim");
    let writer = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("writer")),
            SpawnOptions::default(),
        )
        .await
        .expect("open writer");

    // Outside every seed: a replay throws the snapshot away, so nothing inside one can count how
    // many times the line ran.
    let scratch = fixture.seed.parent().expect("a scratch root");
    let attempts = scratch.join("victim-attempts");
    let gate = scratch.join("victim-gate");
    start_marked(
        &fixture,
        &victim,
        &format!(
            "printf 'x\\n' >> {}; echo started; while [ ! -e {} ]; do sleep 0.02; done; \
             /bin/cat src/foo.txt > src/observed.txt; sh -c 'echo held; sleep 30'",
            attempts.display(),
            gate.display()
        ),
        CommandOptions::default(),
    )
    .await;

    writer
        .run_command("printf 'new\\n' > src/foo.txt", CommandOptions::default())
        .await
        .expect("the writer publishes");
    concluded(&fixture, &writer.sandbox().uid).await;

    // Releasing the barrier is what makes the victim read the file the writer has republished.
    std::fs::write(&gate, b"").expect("open the gate");
    let attempted = |count: usize| {
        std::fs::read_to_string(&attempts).map_or(0, |text| text.lines().count()) >= count
    };
    let deadline = Instant::now() + TIMEOUT;
    while !attempted(2) {
        assert!(Instant::now() < deadline, "the victim never ran a second time");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The replay's own child announcing itself, so the stop below has something live to reach:
    // waiting a duration instead would be measuring the machine rather than the order.
    drain_output(&fixture, &victim, "the replay's hold", |bytes| {
        contains_subslice(bytes, b"held")
    })
    .await;

    victim.stop(true).await.expect("force-stop the victim");
    let completion = concluded(&fixture, &victim.sandbox().uid).await;
    assert!(
        matches!(verdict(&completion.outcome), Outcome::Discarded),
        "a forced stop outranks the replay: {:?}",
        completion.outcome
    );
    assert!(
        !fixture.seed("src/observed.txt").exists(),
        "a discarded command publishes nothing"
    );
    assert_eq!(
        std::fs::read_to_string(&attempts)
            .expect("the attempt counter")
            .lines()
            .count(),
        2,
        "one invalidation, one replay, then the stop — never a third evaluation"
    );
    assert!(
        fixture
            .recorder()
            .take_result(&victim.sandbox().uid)
            .is_none(),
        "one logical command, one completion"
    );

    writer.stop(true).await.expect("stop the writer");
    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn one_terminal_size_governs_every_job() {
    let mut fixture = Fixture::new();
    let nested_job = fixture
        .mux()
        .open_shell(
            &fixture.seed("src"),
            Some(Principal::from("nested")),
            SpawnOptions::default(),
        )
        .await
        .expect("open a nested shell");
    observed_ready(&fixture, &nested_job);
    let root_job = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("root")),
            SpawnOptions::default(),
        )
        .await
        .expect("open a root shell");
    observed_ready(&fixture, &root_job);

    assert_eq!(size_of_job(&fixture, &nested_job).await, "24 80");
    assert_eq!(size_of_job(&fixture, &root_job).await, "24 80");

    let nested_dir = run_line(&fixture, &nested_job, "pwd").await;
    let root_dir = run_line(&fixture, &root_job, "pwd").await;
    assert!(
        nested_dir.ends_with("/src"),
        "nested job works in src: {nested_dir}"
    );
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
        let seed = fixture.seed.clone();
        async move {
            mux.open_shell(
                &seed,
                Some(Principal::from("late")),
                SpawnOptions::default(),
            )
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
    let late_job = spawned
        .expect("join the spawn task")
        .expect("spawn a late job");
    resized
        .expect("join the resize task")
        .expect("resize the mux");
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
    // A shell first, so the mux actually holds the seed's lease when it is torn down.
    let job = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("1")),
            SpawnOptions::default(),
        )
        .await
        .expect("open a shell");
    observed_ready(&fixture, &job);
    fixture.finish_mux().await;

    let result = ShellMux::new_with(
        MuxProfile {
            environment: ShellEnvironment::new(),
            ..MuxProfile::default()
        },
        Arc::new(Mutex::new(Recorder::new(0, COLS))),
        fixture.fs.clone(),
    );
    assert!(matches!(
        result,
        Err(MuxError::InvalidTerminalSize {
            rows: 0,
            cols: COLS
        })
    ));

    assert!(
        fixture.open_executor().is_ok(),
        "the shut-down mux released the seed's lease"
    );
}

#[tokio::test]
#[serial]
async fn a_shell_is_in_the_table_before_its_command_is_admitted() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("bg")),
            SpawnOptions::default(),
        )
        .await
        .expect("open a shell");

    // Opening runs nothing at all: the row is there, idle, before any command has been submitted
    // into it — which is what makes a shell a sandbox rather than a command.
    let view = fixture
        .mux()
        .job(job.id())
        .expect("the row exists before any command does");
    assert!(!view.starting, "nothing is being launched into it");
    assert!(view.running.is_none(), "and nothing is running in it");

    assert!(matches!(
        fixture
            .mux()
            .open_shell(
                &fixture.seed,
                Some(job.principal().clone()),
                SpawnOptions::default()
            )
            .await,
        Err(MuxError::JobExists(_))
    ));

    // The receipt is reserved as the line is admitted, before it can emit a byte or finish, so a
    // caller correlating a shell's output with the command that caused it has it first.
    let reserved = schedule(
        &job,
        "printf 'done\\n' > src/file0.txt",
        CommandOptions::default(),
    )
    .await;
    assert_eq!(reserved.text(), "printf 'done\\n' > src/file0.txt");

    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(
        completion.id,
        reserved.id(),
        "the reserved receipt is the one that resolved"
    );
    assert_eq!(completion.exit_code, Some(0));
    assert!(matches!(
        verdict(&completion.outcome),
        Outcome::Published { .. }
    ));
    assert_eq!(
        std::fs::read(fixture.seed("src/file0.txt")).expect("read the seed file"),
        b"done\n"
    );

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn a_graceful_stop_lets_the_running_line_finish_and_then_closes() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("1")),
            SpawnOptions::default(),
        )
        .await
        .expect("open a shell");
    start_marked(
        &fixture,
        &job,
        &format!("printf 'done\\n' > src/file0.txt; {RACE_HOLD}"),
        CommandOptions::default(),
    )
    .await;

    job.stop(false).await.expect("graceful stop while running");
    assert!(
        !job.keep().expect("the shell is still live"),
        "an explicit stop is not a closure keep may quietly revoke"
    );

    assert!(
        fixture.mux().job(job.id()).is_some(),
        "a graceful stop does not retire the row"
    );
    assert!(matches!(
        job.run_command("echo too-late", CommandOptions::default())
            .await,
        Err(RunError::Admission(MuxError::JobClosing(_)))
    ));

    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(completion.exit_code, Some(0));
    assert!(matches!(
        verdict(&completion.outcome),
        Outcome::Published { .. }
    ));
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

    // The handle outlived its shell, so it names an instance that is gone rather than a name that
    // is merely free: it can never reach whatever takes the name next.
    assert!(matches!(job.stop(false).await, Err(MuxError::StaleJob(_))));

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn a_forced_stop_retires_the_shell_and_frees_its_name() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("forced")),
            SpawnOptions::default(),
        )
        .await
        .expect("open a shell");
    start_held(
        &fixture,
        &job,
        "printf 'partial\\n' > src/file0.txt; ",
        CommandOptions::default(),
    )
    .await;

    job.stop(true).await.expect("force stop the running line");

    // Retired the moment force was accepted, while the line it discarded, that line's verdict and
    // its name all remain its own until the reclamation `wait_for_close` below waits out.
    assert!(
        fixture.mux().job(job.id()).is_none(),
        "the shell left public view at once"
    );
    assert!(!fixture.mux().jobs().iter().any(|view| &view.id == job.id()));
    assert!(
        fixture.mux().get_shell(job.principal()).is_none(),
        "a retired principal answers to nothing"
    );

    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(
        completion.exit_code,
        Some(137),
        "the external was killed by SIGKILL"
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
            .open_shell(
                &fixture.seed,
                Some(job.principal().clone()),
                SpawnOptions::default()
            )
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
        .open_shell(
            &fixture.seed,
            Some(Principal::from(MAIN)),
            SpawnOptions::default(),
        )
        .await
        .expect("open a shell");

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

    job.stop(true).await.expect("force stop the running line");

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
        std::fs::read_dir(fixture.snap()).map_or(true, |mut entries| entries.next().is_none()),
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
        .open_shell(
            &fixture.seed,
            Some(Principal::from(MAIN)),
            SpawnOptions::default(),
        )
        .await
        .expect("open a shell");

    let (tx1, rx1) = tokio::sync::oneshot::channel();
    let done1: OnFinish = Box::new(move |code| {
        let _ = tx1.send(code);
    });
    job.run_command(
        "sh -c 'exit 3'",
        CommandOptions {
            on_finish: Some(done1),
            ..CommandOptions::default()
        },
    )
    .await
    .expect("a nonzero exit under an approved publication is a successful run");
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
        !job.on_finish(extra).expect("the shell is still live"),
        "a callback is already registered for the running command"
    );

    job.stop(true).await.expect("force stop the held line");
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
    assert_eq!(
        fixture.recorder().exit_codes(&job.sandbox().uid),
        vec![3, 137]
    );

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn a_callback_nobody_can_register_is_dropped_at_once() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from(MAIN)),
            SpawnOptions::default(),
        )
        .await
        .expect("open a shell");

    let (tx_idle, rx_idle) = tokio::sync::oneshot::channel();
    let idle: OnFinish = Box::new(move |code| {
        let _ = tx_idle.send(code);
    });
    assert!(
        !job.on_finish(idle).expect("the shell is live"),
        "the shell is idle: nothing to report"
    );
    assert!(
        rx_idle.await.is_err(),
        "the callback was dropped, not called"
    );

    // A handle whose job has closed names an instance that is gone. Registering through it is
    // refused outright rather than silently attached to whatever holds the name now.
    let ghost = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("ghost")),
            SpawnOptions::default(),
        )
        .await
        .expect("open ghost");
    let ghost_uid = ghost.sandbox().uid.clone();
    ghost.stop(true).await.expect("stop ghost");
    wait_for_close(&fixture, &ghost_uid).await;

    let (tx_unknown, rx_unknown) = tokio::sync::oneshot::channel();
    let unknown: OnFinish = Box::new(move |code| {
        let _ = tx_unknown.send(code);
    });
    assert!(matches!(
        ghost.on_finish(unknown),
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
        job.run_command(
            "echo too-late",
            CommandOptions {
                on_finish: Some(refused),
                ..CommandOptions::default()
            },
        )
        .await,
        Err(RunError::Admission(MuxError::JobBusy(_)))
    ));
    assert!(
        rx_refused.await.is_err(),
        "the refused callback was never registered"
    );

    job.stop(true).await.expect("force stop the held line");
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
        .open_shell(
            &fixture.seed,
            Some(Principal::from("idle")),
            SpawnOptions::default(),
        )
        .await
        .expect("open idle");
    let busy = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("busy")),
            SpawnOptions::default(),
        )
        .await
        .expect("open busy");

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
    assert!(std::fs::read_dir(fixture.snap()).map_or(true, |mut entries| entries.next().is_none()));
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
        .open_shell(
            &fixture.seed,
            Some(Principal::from("blocked")),
            SpawnOptions::default(),
        )
        .await
        .expect("open the blocked shell");
    // Scheduled, not awaited: `cat` only ends at the end of file this test sends it much later.
    schedule(&blocked, "cat", CommandOptions::default()).await;

    let worker = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("worker")),
            SpawnOptions::default(),
        )
        .await
        .expect("open the worker shell");

    assert_eq!(fixture.mux().jobs().len(), 2);
    assert_eq!(
        fixture.mux().history(&fixture.seed),
        Some(Vec::new()),
        "an opened seed with no grants answers an empty history, not `None`"
    );

    fixture
        .mux()
        .resize_all(TerminalGeometry { rows: 28, cols: 96 })
        .await
        .expect("resize while a shell is blocked");

    worker
        .run_command("printf 'x\\n' > src/file0.txt", CommandOptions::default())
        .await
        .expect("the worker's line publishes");
    let worker_completion = concluded(&fixture, &worker.sandbox().uid).await;
    assert_eq!(worker_completion.exit_code, Some(0));
    assert!(matches!(
        verdict(&worker_completion.outcome),
        Outcome::Published { .. }
    ));
    assert_eq!(
        fixture
            .mux()
            .history(&fixture.seed)
            .expect("the seed is open")
            .len(),
        1
    );

    blocked.write_input(b"\x04").await.expect("send eof to cat");
    let blocked_completion = concluded(&fixture, &blocked.sandbox().uid).await;
    assert_eq!(blocked_completion.exit_code, Some(0));

    blocked.stop(false).await.expect("stop the blocked shell");
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
        .open_shell(
            &fixture.seed,
            Some(Principal::from(MAIN)),
            SpawnOptions::default(),
        )
        .await
        .expect("open a shell");
    job.run_command(
        "dd if=/dev/zero bs=65536 count=16 2>/dev/null",
        CommandOptions::default(),
    )
    .await
    .expect("the line publishes");

    let completion = concluded(&fixture, &job.sandbox().uid).await;
    assert_eq!(completion.exit_code, Some(0));

    job.stop(false).await.expect("stop the shell");
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
            fixture
                .recorder()
                .mux()
                .expect("the frontend is bound")
                .as_ref()
        ),
        Arc::as_ptr(fixture.mux()),
        "the frontend is bound to this fixture's own mux"
    );

    let alpha = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("alpha")),
            SpawnOptions::default(),
        )
        .await
        .expect("open alpha");
    observed_ready(&fixture, &alpha);
    let beta = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("beta")),
            SpawnOptions::default(),
        )
        .await
        .expect("open beta");
    observed_ready(&fixture, &beta);

    let ids: Vec<_> = fixture
        .recorder()
        .observed_jobs()
        .iter()
        .map(|job| job.id.clone())
        .collect();
    assert_eq!(ids, vec![alpha.id().clone(), beta.id().clone()]);

    assert!(matches!(
        fixture
            .mux()
            .open_shell(
                &fixture.seed,
                Some(alpha.principal().clone()),
                SpawnOptions::default()
            )
            .await,
        Err(MuxError::JobExists(_))
    ));
    // An existing directory that no subvolume contains: the seed walk finds nothing, so the
    // admission fails before a name or a descriptor is reserved.
    let unregistered = fixture.scratch_dir("unregistered");
    let escape = ShellId::from("escape");
    assert!(matches!(
        fixture
            .mux()
            .open_shell(
                &unregistered,
                Some(escape.principal().clone()),
                SpawnOptions::default()
            )
            .await,
        Err(MuxError::Marsh(MarshError::Btrfs(
            marsh_btrfs::Error::NoSubvolume(_)
        )))
    ));

    assert!(fixture.recorder().handle(&escape).is_none());
    assert!(
        fixture
            .recorder()
            .observed_jobs()
            .iter()
            .all(|job| job.id != escape),
        "a refused discovery admits no shell"
    );
    // The name was never reserved, so a valid creation may still take it — free *and* idle: the
    // retaken name accepts a command of its own, and that command is the only one it concludes.
    let reused = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(escape.principal().clone()),
            SpawnOptions::default(),
        )
        .await
        .expect("the refused name is free");
    observed_ready(&fixture, &reused);
    reused
        .run_command(
            "printf 'taken\\n' > src/taken.txt",
            CommandOptions::default(),
        )
        .await
        .expect("the retaken name publishes its own line");
    let taken = concluded(&fixture, &reused.sandbox().uid).await;
    assert_eq!(taken.exit_code, Some(0));
    assert_eq!(
        std::fs::read(fixture.seed("src/taken.txt")).expect("the retaken shell's file"),
        b"taken\n"
    );
    reused.stop(true).await.expect("stop the retaken shell");
    wait_for_close(&fixture, &reused.sandbox().uid).await;
    assert_eq!(
        fixture.recorder().exit_codes(&reused.sandbox().uid),
        vec![0],
        "no stranded reservation ran a second line"
    );
    assert_eq!(
        fixture
            .recorder()
            .handle(alpha.id())
            .map(|shell| shell.sandbox().uid.clone()),
        Some(alpha.sandbox().uid.clone())
    );

    beta.stop(true).await.expect("force stop beta");
    let ids: Vec<_> = fixture
        .recorder()
        .observed_jobs()
        .iter()
        .map(|job| job.id.clone())
        .collect();
    assert_eq!(ids, vec![alpha.id().clone()]);

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn input_reaches_a_job_and_every_result_is_delivered_once() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from(MAIN)),
            SpawnOptions::default(),
        )
        .await
        .expect("open a shell");
    // Scheduled rather than awaited: this line ends only at the end of file the test itself sends,
    // so its receipt — not its verdict — is what says it is running.
    schedule(
        &job,
        "stty -echo; printf READY; cat",
        CommandOptions::default(),
    )
    .await;

    drain_output(&fixture, &job, "ready marker", |bytes| {
        contains_subslice(bytes, b"READY")
    })
    .await;

    job.write_input(b"roundtrip\n").await.expect("write to cat");
    drain_output(&fixture, &job, "roundtrip echo", |bytes| {
        contains_subslice(bytes, b"roundtrip\r\n")
    })
    .await;

    job.write_input(b"\x04").await.expect("send eof");
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
    assert_eq!(
        fixture.recorder().exit_codes(&job.sandbox().uid),
        vec![0, 7]
    );

    job.stop(false).await.expect("stop the shell");
    wait_for_close(&fixture, &job.sandbox().uid).await;
    history_intact(&fixture, &job.sandbox().uid, &[0, 7]);
    assert!(fixture.recorder().handle(job.id()).is_none());

    let reopened = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from(MAIN)),
            SpawnOptions::default(),
        )
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
            .map(|shell| shell.sandbox().uid.clone()),
        Some(reopened.sandbox().uid.clone())
    );

    fixture.finish_mux().await;
}

/// A principal names a shell, and the shell it names is the one already open.
///
/// The collection is an index rather than a factory: [`ShellMux::get_shell`] answers with the live
/// generation — the same interpreter, carrying the same variables — and never opens a second one.
/// A principal nothing answers to is `None` and stays that way; a principal a live shell holds is
/// refused to a second creation; and an object whose generation was stopped can never reach the
/// replacement that took its name.
#[tokio::test]
#[serial]
async fn principals_index_independent_shells() {
    let mut fixture = Fixture::new();
    let alpha_name = Principal::from("alpha");
    let beta_name = Principal::from("beta");
    let alpha = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(alpha_name.clone()),
            SpawnOptions::default(),
        )
        .await
        .expect("open alpha");
    observed_ready(&fixture, &alpha);
    let beta = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(beta_name.clone()),
            SpawnOptions::default(),
        )
        .await
        .expect("open beta");
    observed_ready(&fixture, &beta);

    assert_eq!(
        run_line(&fixture, &alpha, "VALUE=alpha; printf 'set\\n'").await,
        "set"
    );
    // Fetched rather than reopened: only the very interpreter the first line ran in still holds
    // what that line set, so reading it back is what proves the lookup is an index.
    let fetched = fixture
        .mux()
        .get_shell(&alpha_name)
        .expect("alpha answers to its principal");
    assert_eq!(
        fetched.sandbox().uid,
        alpha.sandbox().uid,
        "the fetched shell is alpha's own generation"
    );
    assert_eq!(
        run_line(&fixture, &fetched, "printf '%s\\n' \"$VALUE\"").await,
        "alpha"
    );

    assert_eq!(
        run_line(&fixture, &beta, "VALUE=beta; printf 'set\\n'").await,
        "set"
    );
    let beta_again = fixture
        .mux()
        .get_shell(&beta_name)
        .expect("beta answers to its principal");
    assert_eq!(
        run_line(&fixture, &beta_again, "printf '%s\\n' \"$VALUE\"").await,
        "beta"
    );
    assert_eq!(
        run_line(&fixture, &fetched, "printf '%s\\n' \"$VALUE\"").await,
        "alpha",
        "two principals are two independent shells"
    );

    let open = fixture.mux().jobs().len();
    assert!(
        fixture
            .mux()
            .get_shell(&Principal::from("missing"))
            .is_none(),
        "a principal nothing answers to resolves to nothing"
    );
    assert_eq!(
        fixture.mux().jobs().len(),
        open,
        "and a lookup that found nothing opened nothing"
    );

    assert!(
        matches!(
            fixture
                .mux()
                .open_shell(
                    &fixture.seed,
                    Some(alpha_name.clone()),
                    SpawnOptions::default()
                )
                .await,
            Err(MuxError::JobExists(_))
        ),
        "a live principal is not creatable twice"
    );

    // The name comes back; the object that held it does not come with it.
    let first_uid = alpha.sandbox().uid.clone();
    alpha.stop(true).await.expect("stop alpha");
    wait_for_close(&fixture, &first_uid).await;
    assert!(
        fixture.mux().get_shell(&alpha_name).is_none(),
        "a stopped principal answers to nothing"
    );

    let reopened = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(alpha_name.clone()),
            SpawnOptions::default(),
        )
        .await
        .expect("reopen alpha");
    observed_ready(&fixture, &reopened);
    assert_ne!(
        reopened.sandbox().uid,
        first_uid,
        "a reused name is a new generation"
    );

    let refused = alpha
        .run_command(
            "printf 'stale\\n' > src/stale-handle.txt",
            CommandOptions::default(),
        )
        .await;
    assert!(
        matches!(refused, Err(RunError::Admission(MuxError::StaleJob(_)))),
        "the retained handle names an instance that is gone: {refused:?}"
    );
    assert!(
        !fixture.seed("src/stale-handle.txt").exists(),
        "the stale handle's line reached no seed"
    );
    assert!(
        !fixture
            .snap()
            .join(reopened.sandbox().uid.as_str())
            .join("src/stale-handle.txt")
            .exists(),
        "nor the replacement's own snapshot"
    );
    assert_eq!(
        run_line(
            &fixture,
            &reopened,
            "if [ -z \"$VALUE\" ]; then printf 'unset\\n'; else printf '%s\\n' \"$VALUE\"; fi",
        )
        .await,
        "unset",
        "the replacement starts with none of the old shell's state"
    );

    fixture.finish_mux().await;
}

#[tokio::test]
#[serial]
async fn shutdown_detaches_frontend_without_retaining_session() {
    let mut fixture = Fixture::new();
    let idle = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("idle")),
            SpawnOptions::default(),
        )
        .await
        .expect("open idle");
    let worked = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("worked")),
            SpawnOptions::default(),
        )
        .await
        .expect("open worked");

    worked
        .run_command("printf 'done\\n'", CommandOptions::default())
        .await
        .expect("the line publishes");
    let completion = concluded(&fixture, &worked.sandbox().uid).await;
    assert_eq!(completion.exit_code, Some(0));

    tokio::time::timeout(TIMEOUT, fixture.finish_mux())
        .await
        .expect("shutdown timed out");

    assert!(fixture.recorder().mux().is_none());
    assert!(fixture.recorder().handle(idle.id()).is_none());
    assert!(fixture.recorder().handle(worked.id()).is_none());
    assert_eq!(
        fixture.recorder().exit_codes(&worked.sandbox().uid),
        vec![0]
    );

    assert!(
        fixture.open_executor().is_ok(),
        "the session lease was released with the mux"
    );
}

/// A collection dropped without a shutdown still gives its seed back.
///
/// The other half of the teardown contract above, and a different path through it: nothing here
/// calls [`ShellMux::shutdown`], and the shell object is deliberately still held when the last
/// reference to the collection goes. A retained shell whose collection is gone is a dead object —
/// it must keep neither the seed's lease nor a snapshot alive, and it must run nothing.
#[tokio::test]
#[serial]
async fn dropping_collection_releases_seed_with_retained_shell() {
    let mut fixture = Fixture::new();
    let idle = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("idle")),
            SpawnOptions::default(),
        )
        .await
        .expect("open an idle shell");
    observed_ready(&fixture, &idle);
    assert!(
        fixture.snap().join(idle.sandbox().uid.as_str()).is_dir(),
        "the shell holds a snapshot to release"
    );

    // Dropped, never shut down, while `idle` is still held.
    let mux = fixture.mux.take().expect("the mux is still present");
    drop(mux);

    assert!(
        fixture.open_executor().is_ok(),
        "the dropped collection released the seed's lease"
    );
    let refused = idle
        .run_command("printf 'x\\n' > src/file0.txt", CommandOptions::default())
        .await;
    assert!(
        matches!(refused, Err(RunError::Admission(MuxError::ShuttingDown))),
        "a shell whose collection is gone runs nothing: {refused:?}"
    );
    assert_eq!(
        std::fs::read(fixture.seed("src/file0.txt")).expect("read the seed file"),
        b"zero\n",
        "and reaches no seed"
    );
}

#[tokio::test]
#[serial]
async fn a_denial_is_atomic_and_a_claim_outlives_its_job() {
    let mut fixture = Fixture::new();
    let owner_name = Principal::from("owner");
    let other_name = Principal::from("other");
    fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(owner_name.clone()),
            SpawnOptions::default(),
        )
        .await
        .expect("open owner");
    fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(other_name.clone()),
            SpawnOptions::default(),
        )
        .await
        .expect("open other");

    // Both are reached by principal rather than through the object creation handed back: the
    // collection is an index, and a line runs through whatever that index answers with.
    let owner = fixture
        .mux()
        .get_shell(&owner_name)
        .expect("owner answers to its principal");
    let other = fixture
        .mux()
        .get_shell(&other_name)
        .expect("other answers to its principal");

    let owner_completion = owner
        .run_command(
            "printf 'owned\\n' > src/file1.txt",
            CommandOptions::default(),
        )
        .await
        .expect("owner's line publishes");
    assert_eq!(owner_completion.exit_code, Some(0));
    assert!(matches!(
        verdict(&owner_completion.outcome),
        Outcome::Published { .. }
    ));

    // The refusal is the *return value*, not something a test has to go looking for in an event
    // stream: a line the gate denied never answers `Ok`, whatever its process did.
    let refusal = other
        .run_command(
            "printf 'new\\n' > src/new.txt; printf 'bad\\n' > src/file1.txt",
            CommandOptions::default(),
        )
        .await
        .expect_err("the gate refuses other's line");
    let denied = match refusal {
        RunError::Policy(denied) => denied,
        unexpected => panic!("expected a policy refusal, got {unexpected:?}"),
    };
    let other_completion = denied.completion();
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
        fixture
            .mux()
            .history(&fixture.seed)
            .expect("the seed is open"),
        vec![Event::new(
            "owner",
            Action::Edit,
            Resource::from(vec!["src", "file1.txt"])
        )],
        "the granted first edit of the denied line is not recorded"
    );

    owner.stop(true).await.expect("force stop owner");
    assert!(fixture.mux().job(owner.id()).is_none());
    assert_eq!(
        fixture
            .mux()
            .history(&fixture.seed)
            .expect("the seed is open")
            .len(),
        1,
        "the claim outlives the shell that earned it"
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
        .open_shell(
            &fixture.seed,
            Some(Principal::from("1")),
            SpawnOptions::default(),
        )
        .await
        .expect("open a shell");

    // A line the front-end answers itself: `jobs`, `fg`, an invalid line. Nothing is admitted, so
    // nothing reaps the reservation on an admission's behalf.
    let lease = job.idle_terminal().expect("lease the idle terminal");
    assert!(!lease.is_revoked(), "a fresh lease starts live");
    drop(lease);

    // The prompt asks for its own terminal back. This is the iteration that used to be refused
    // with `TerminalBusy` forever.
    let lease = job
        .idle_terminal()
        .expect("lease again after releasing without admitting a command");
    assert!(read_through(&job, &lease, b"typed\n").await);
    drop(lease);

    assert_eq!(
        run_line(
            &fixture,
            &job,
            "printf 'one\\n' > src/file0.txt; printf 'ok1\\n'"
        )
        .await,
        "ok1"
    );

    // And again after a command, which is the iteration the un-lowered run-revocation used to
    // leave granted-but-deaf: the lease was handed over, and every read answered `None`.
    let lease = job.idle_terminal().expect("lease again after a command");
    assert!(
        !lease.is_revoked(),
        "the finished command's revocation was lowered"
    );
    assert!(read_through(&job, &lease, b"typed again\n").await);
    drop(lease);

    assert_eq!(
        run_line(
            &fixture,
            &job,
            "printf 'two\\n' > src/file1.txt; printf 'ok2\\n'"
        )
        .await,
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
async fn read_through(job: &Shell, lease: &marsh::shellmux::IdleTerminal, bytes: &[u8]) -> bool {
    job.write_input(bytes)
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

/// A repository with a root commit already in it, so `git add` has an index to stage into.
fn init_repository(work: &Path) {
    let repository = git2::Repository::init(work).expect("init a repository");
    let who = git2::Signature::new(
        "Test",
        "test@example.com",
        &git2::Time::new(1_112_911_993, 0),
    )
    .expect("signature");
    let empty = repository
        .index()
        .expect("index")
        .write_tree()
        .expect("write the empty tree");
    let tree = repository.find_tree(empty).expect("find the empty tree");
    repository
        .commit(Some("HEAD"), &who, &who, "root\n", &tree, &[])
        .expect("root commit");
}

/// Two shells started in two different seeds are two sessions, and stay apart.
///
/// The three things one mux over several seeds can silently break, all in one run:
///
/// * **Policy.** A resource carries no seed identity, so a shared validator would let A's claim on
///   `src/shared.txt` deny B's identical path. Both writes must publish.
/// * **Instrumentation.** The builtin hook is installed process-wide and keeps only the newest
///   one, so per-session recorders would leave the first seed's `git add` unobserved and its
///   capability unrequested. Both stages must reach both histories.
/// * **Storage.** Each seed keeps its own state beside itself, so each job's snapshot must lie
///   under its *own* seed's state parent.
///
/// Before either shell exists the mux holds nothing at all, which is the other half of the claim:
/// a host opens no seed until a shell names one.
#[tokio::test]
#[serial]
async fn initial_directories_select_independent_seeds() {
    let mut fixture = Fixture::new();
    let other = fixture.sibling_seed("other");
    init_repository(&fixture.seed);
    init_repository(&other);

    assert!(fixture.mux().seeds().is_empty(), "no shell, no seed");
    assert!(
        !fixture.state.exists() && !fixture.root.join(".marsh/other").exists(),
        "neither state directory exists before a shell asks for one"
    );

    let a = fixture
        .mux()
        .open_shell(
            &fixture.seed("src"),
            Some(Principal::from("a")),
            SpawnOptions::default(),
        )
        .await
        .expect("open a in the first seed");
    observed_ready(&fixture, &a);
    let b = fixture
        .mux()
        .open_shell(
            &other.join("src"),
            Some(Principal::from("b")),
            SpawnOptions::default(),
        )
        .await
        .expect("open b in the second seed");
    observed_ready(&fixture, &b);

    assert_eq!(a.sandbox().seed, fixture.seed);
    assert_eq!(b.sandbox().seed, other);
    assert_eq!(a.sandbox().dir.as_str(), "src");
    assert_eq!(b.sandbox().dir.as_str(), "src");

    // The same relative path in both, written by both: a shared validator denies the second.
    //
    // Each line ends in a quoted `printf` so the escape reaches `printf` itself: unquoted, the
    // shell would strip the backslash and the terminal would never see the newline the line
    // reader waits for.
    assert_eq!(
        run_line(&fixture, &a, "printf A > shared.txt; printf 'done\\n'").await,
        "done"
    );
    assert_eq!(
        run_line(&fixture, &b, "printf B > shared.txt; printf 'done\\n'").await,
        "done"
    );
    // A git capability comes only from the instrumented builtin, so a silenced recorder makes
    // this line request nothing and publish nothing.
    assert_eq!(
        run_line(&fixture, &a, "git add -- shared.txt; printf 'done\\n'").await,
        "done"
    );
    assert_eq!(
        run_line(&fixture, &b, "git add -- shared.txt; printf 'done\\n'").await,
        "done"
    );

    assert_eq!(
        std::fs::read(fixture.seed("src/shared.txt")).expect("the first seed's file"),
        b"A"
    );
    assert_eq!(
        std::fs::read(other.join("src/shared.txt")).expect("the second seed's file"),
        b"B"
    );

    let a_history = fixture
        .mux()
        .history(&fixture.seed)
        .expect("the first seed is open");
    let b_history = fixture
        .mux()
        .history(&other)
        .expect("the second seed is open");
    let stage_of = |history: &[Event], principal: &str| {
        history.iter().any(|event| {
            event.action == Action::Stage
                && event.principal.as_str() == principal
                && event.resource == Resource::from(vec!["src", "shared.txt"])
        })
    };
    assert!(
        stage_of(&a_history, "a"),
        "the first seed records its own stage: {a_history:?}"
    );
    assert!(
        stage_of(&b_history, "b"),
        "the second seed records its own stage: {b_history:?}"
    );
    assert!(
        !a_history
            .iter()
            .any(|event| event.principal.as_str() == "b"),
        "the second seed's principal is absent from the first seed's history: {a_history:?}"
    );
    assert!(
        !b_history
            .iter()
            .any(|event| event.principal.as_str() == "a"),
        "the first seed's principal is absent from the second seed's history: {b_history:?}"
    );

    // Each job stages into its own seed's state tree, never the other's.
    let a_cwd = job_cwd(&fixture, &a);
    let b_cwd = job_cwd(&fixture, &b);
    assert!(
        a_cwd.starts_with(fixture.snap()) && a_cwd.ends_with("src"),
        "{} is not under the first seed's snapshots",
        a_cwd.display()
    );
    assert!(
        b_cwd.starts_with(fixture.sibling_snap("other")) && b_cwd.ends_with("src"),
        "{} is not under the second seed's snapshots",
        b_cwd.display()
    );

    let opened: Vec<PathBuf> = fixture
        .mux()
        .seeds()
        .into_iter()
        .map(|info| info.seed)
        .collect();
    let mut expected = vec![fixture.seed.clone(), other.clone()];
    expected.sort();
    assert_eq!(
        opened, expected,
        "both seeds are reported, in canonical order"
    );

    fixture.finish_mux().await;
}

/// Two shells started at two spellings of one tree share that tree's single session.
///
/// A symlinked alias and a different subdirectory are two ways of naming the same seed. Both must
/// resolve to one canonical key, because a second key would mean a second lease — and because
/// policy only works if both shells are judged against the same history. The claim outliving the
/// jobs is the other half: a later job on that seed must still find the first one's.
#[tokio::test]
#[serial]
async fn different_directories_share_one_seed_session() {
    let mut fixture = Fixture::new();
    std::fs::create_dir_all(fixture.seed("other")).expect("a second directory in the seed");
    let alias = fixture.root.join("alias");
    std::os::unix::fs::symlink(&fixture.seed, &alias).expect("an alias to the seed");

    // Both request paths are bound before the join: a temporary built inside the macro's argument
    // list is dropped before the futures it lends to are polled.
    let a_dir = fixture.seed("src");
    let b_dir = alias.join("other");
    let (a, b) = tokio::join!(
        fixture
            .mux()
            .open_shell(&a_dir, Some(Principal::from("a")), SpawnOptions::default()),
        fixture
            .mux()
            .open_shell(&b_dir, Some(Principal::from("b")), SpawnOptions::default())
    );
    let a = a.expect("open a");
    let b = b.expect("open b");
    observed_ready(&fixture, &a);
    observed_ready(&fixture, &b);

    assert_eq!(a.sandbox().seed, fixture.seed);
    assert_eq!(
        b.sandbox().seed,
        fixture.seed,
        "an alias resolves to the same canonical seed"
    );
    assert_ne!(
        a.sandbox().uid,
        b.sandbox().uid,
        "one seed, two jobs, two snapshots"
    );
    assert_eq!(
        fixture.mux().seeds().len(),
        1,
        "two spellings of one tree opened one session"
    );

    // `visible` is published *with* its newline: `cat` is what b reads it back through, and a
    // file with no terminator would leave b's line reader waiting for one forever.
    assert_eq!(
        run_line(
            &fixture,
            &a,
            "printf 'shared\\n' > visible; printf 'done\\n'"
        )
        .await,
        "done"
    );
    assert_eq!(
        run_line(&fixture, &b, "cat ../src/visible").await,
        "shared",
        "b's refreshed snapshot carries a's publication"
    );

    let refused = b
        .run_command("printf stolen > ../src/visible", CommandOptions::default())
        .await;
    assert!(
        matches!(refused, Err(RunError::Policy(_))),
        "a's claim is in force for b: {refused:?}"
    );
    let denied = concluded(&fixture, &b.sandbox().uid).await;
    let visible = Resource::from(vec!["src", "visible"]);
    let Outcome::Denied { denials, .. } = verdict(&denied.outcome) else {
        panic!(
            "a's claim is in force for b: {:?}",
            verdict(&denied.outcome)
        )
    };
    assert!(
        denials
            .iter()
            .any(|denial| denial.event.resource == visible),
        "b was refused a's exact path, judged against a's history: {denials:?}"
    );

    a.stop(true).await.expect("stop a");
    b.stop(true).await.expect("stop b");
    wait_for_close(&fixture, &a.sandbox().uid).await;
    wait_for_close(&fixture, &b.sandbox().uid).await;

    // The session outlives both jobs, and so does what they earned in it.
    let history = fixture
        .mux()
        .history(&fixture.seed)
        .expect("the seed stays open after its last job closes");
    assert!(
        history.contains(&Event::new("a", Action::Edit, visible.clone())),
        "a's grant is still the seed's live history: {history:?}"
    );

    let later = fixture
        .mux()
        .open_shell(
            &fixture.seed("src"),
            Some(Principal::from("later")),
            SpawnOptions::default(),
        )
        .await
        .expect("open a later shell on the same seed");
    observed_ready(&fixture, &later);
    assert_eq!(
        later.sandbox().seed,
        fixture.seed,
        "the later shell joined the one existing session"
    );
    assert_eq!(fixture.mux().seeds().len(), 1, "still one session");
    let still_refused = later
        .run_command("printf later > visible", CommandOptions::default())
        .await;
    assert!(
        matches!(still_refused, Err(RunError::Policy(_))),
        "the live claim outlives the shell that earned it: {still_refused:?}"
    );
    let still_denied = concluded(&fixture, &later.sandbox().uid).await;
    let Outcome::Denied { denials, .. } = verdict(&still_denied.outcome) else {
        panic!(
            "the live claim outlives the job that earned it: {:?}",
            verdict(&still_denied.outcome)
        )
    };
    assert!(
        denials
            .iter()
            .any(|denial| denial.event.resource == visible),
        "the later job was refused the same path: {denials:?}"
    );
    assert_eq!(
        std::fs::read(fixture.seed("src/visible")).expect("the published file"),
        b"shared\n"
    );

    fixture.finish_mux().await;
}

/// A failed publication blocks the seed it happened on, and nothing else.
///
/// Recovery is armed before the log is opened, so a directory where `meta/wal.jsonl` belongs makes
/// an approved publication fail after the point of no return. Every job over *that* seed then
/// refuses — including a brand-new one — while the other seed of the same mux keeps working.
#[tokio::test]
#[serial]
async fn a_failed_publication_blocks_only_its_seed() {
    let mut fixture = Fixture::new();
    let other = fixture.sibling_seed("healthy");

    let a = fixture
        .mux()
        .open_shell(
            &fixture.seed("src"),
            Some(Principal::from("a")),
            SpawnOptions::default(),
        )
        .await
        .expect("open a");
    observed_ready(&fixture, &a);
    let a2 = fixture
        .mux()
        .open_shell(
            &fixture.seed("src"),
            Some(Principal::from("a2")),
            SpawnOptions::default(),
        )
        .await
        .expect("open a second shell on the same seed");
    observed_ready(&fixture, &a2);
    let b = fixture
        .mux()
        .open_shell(
            &other.join("src"),
            Some(Principal::from("b")),
            SpawnOptions::default(),
        )
        .await
        .expect("open b");
    observed_ready(&fixture, &b);

    // A directory where the log file belongs: `JsonLog::open` fails, and the boundary has already
    // armed recovery by then.
    let log = fixture.state.join("meta/wal.jsonl");
    std::fs::create_dir_all(&log).expect("obstruct the first seed's log");

    // `exit 7` on the end: the publication breaks *after* the line asked the shell to close, so
    // this also pins that a boundary failure cannot swallow the request.
    let broke = a
        .run_command(
            "printf failed > needs-recovery; exit 7",
            CommandOptions::default(),
        )
        .await;
    assert!(
        matches!(broke, Err(RunError::Unpublished { .. })),
        "a publication that broke is not an approved one: {broke:?}"
    );
    let failed = concluded(&fixture, &a.sandbox().uid).await;
    assert_eq!(
        failed.exit_code, None,
        "a publication that broke produced no verdict at all"
    );
    assert!(
        matches!(
            failed.outcome.as_ref(),
            Err(MuxError::Marsh(MarshError::Wal(marsh_wal::Error::Io(_))))
        ),
        "the unwritable log surfaces as the write-ahead layer's own I/O failure: {:?}",
        failed.outcome
    );

    // Both ways into the poisoned seed refuse: an existing job's next command, and a brand-new
    // job's admission.
    assert!(matches!(
        a2.run_command("printf blocked > x", CommandOptions::default())
            .await,
        Err(RunError::Admission(MuxError::RecoveryRequired))
    ));
    assert!(matches!(
        fixture
            .mux()
            .open_shell(
                &fixture.seed("src"),
                Some(Principal::from("a3")),
                SpawnOptions::default()
            )
            .await,
        Err(MuxError::RecoveryRequired)
    ));
    assert!(
        !fixture.seed("src/x").exists(),
        "the refused command never reached the seed"
    );

    // Neither way into the healthy seed is affected.
    assert_eq!(
        run_line(&fixture, &b, "printf healthy > healthy; printf 'done\\n'").await,
        "done",
        "the other seed of the same mux is untouched"
    );
    assert_eq!(
        std::fs::read(other.join("src/healthy")).expect("the healthy seed's file"),
        b"healthy"
    );
    let b2 = fixture
        .mux()
        .open_shell(
            &other.join("src"),
            Some(Principal::from("b2")),
            SpawnOptions::default(),
        )
        .await
        .expect("the healthy seed still admits new shells");
    observed_ready(&fixture, &b2);
    assert_eq!(b2.sandbox().seed, other);
    b2.stop(true).await.expect("stop b2");
    wait_for_close(&fixture, &b2.sandbox().uid).await;

    let poisoned = fixture
        .mux()
        .seeds()
        .into_iter()
        .find(|info| info.seed == fixture.seed)
        .expect("the poisoned seed is open");
    let healthy = fixture
        .mux()
        .seeds()
        .into_iter()
        .find(|info| info.seed == other)
        .expect("the healthy seed is open");
    assert!(poisoned.recovery_required);
    assert!(!healthy.recovery_required);

    let a_uid = a.sandbox().uid.clone();
    let b_uid = b.sandbox().uid.clone();
    // `a` is never stopped: its own `exit` closes it, and an explicit stop here would be a stale
    // handle. The request was recorded before the boundary ran, so the failure at that boundary
    // did not lose it.
    b.stop(true).await.expect("stop b");
    let a_end = tokio::time::timeout(TIMEOUT, a.wait_closed())
        .await
        .expect("a closes on its own `exit`, with nothing stopping it")
        .expect("a's end");
    assert_eq!(
        a_end.close_mode,
        Some(JobCloseMode::Graceful),
        "a closed because its line exited, not because anything retired it"
    );
    let b_end = tokio::time::timeout(TIMEOUT, b.wait_closed())
        .await
        .expect("b closes")
        .expect("b's end");
    assert!(a_end.recovery_required, "a's seed still owes a replay");
    assert!(!b_end.recovery_required, "b's seed owes nothing");
    // `wait_closed` resolves from the job's own end-of-life, which reclamation settles *before*
    // the frontend is told; the recorder's flags are only final once its `Closed` callback has
    // actually run for that uid.
    wait_for_close(&fixture, &a_uid).await;
    wait_for_close(&fixture, &b_uid).await;
    assert!(
        fixture.recorder().closed_with_storage(&a_uid),
        "the failed snapshot is retained as the recovery source"
    );
    assert!(
        !fixture.recorder().closed_with_storage(&b_uid),
        "a healthy job's snapshot is reclaimed"
    );
    assert!(
        fixture.snap().join(a_uid.as_str()).exists(),
        "a's tree is still on disk for a replay to read"
    );
    assert!(
        !fixture
            .sibling_snap("healthy")
            .join(b_uid.as_str())
            .exists(),
        "b's tree is gone"
    );

    a2.stop(true).await.expect("stop a2");
    fixture.finish_mux().await;
}

/// A line that exits the shell closes its own job, after its publication, and nothing else's.
///
/// `exit` is brush's own builtin, run inside the job's interpreter: the status it is handed is the
/// line's status, and everything written before it is published exactly as any other line's writes
/// are. The request is recorded on the running command, so the closure lands at the same boundary
/// a one-shot command's does — after the verdict, never instead of it.
///
/// The job here is a persistent terminal nobody asked to close ([`SpawnOptions::default`],
/// [`CommandOptions::default`]): the one-shot path already closed correctly, so it would prove
/// nothing.
#[tokio::test]
#[serial]
async fn exit_shell_closes_persistent_job_after_publication() {
    let mut fixture = Fixture::new();
    let exiting = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("exiting")),
            SpawnOptions::default(),
        )
        .await
        .expect("open the exiting shell");
    observed_ready(&fixture, &exiting);
    let sibling = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("sibling")),
            SpawnOptions::default(),
        )
        .await
        .expect("open a sibling shell");
    observed_ready(&fixture, &sibling);
    let uid = exiting.sandbox().uid.clone();

    let ran = exiting
        .run_command(
            "printf EXIT_BEFORE; printf final > src/file0.txt; exit 7; printf EXIT_AFTER",
            CommandOptions::default(),
        )
        .await
        .expect("the line publishes, whatever its process exited with");
    assert_eq!(
        ran.exit_code,
        Some(7),
        "the builtin's argument is the line's status, not a discarded one"
    );
    let completion = concluded(&fixture, &uid).await;
    assert_eq!(
        completion.id, ran.id,
        "the verdict the frontend was given is this line's own"
    );
    match verdict(&completion.outcome) {
        Outcome::Published { publication, .. } => assert_eq!(publication.ops, 1),
        other => panic!("the writes before `exit` are published: {other:?}"),
    }

    let end = tokio::time::timeout(TIMEOUT, exiting.wait_closed())
        .await
        .expect("the job closes on its own `exit`")
        .expect("its end");
    assert_eq!(
        end.close_mode,
        Some(JobCloseMode::Graceful),
        "a shell that exited closed gracefully, not by being retired"
    );
    let final_verdict = end
        .completion
        .as_ref()
        .expect("the end carries the line that closed it");
    assert_eq!(final_verdict.id, ran.id);
    assert_eq!(
        final_verdict.exit_code,
        Some(7),
        "the closure carries the status the shell exited with"
    );

    wait_for_close(&fixture, &uid).await;
    assert_eq!(
        fixture.recorder().take_terminal(&uid),
        b"EXIT_BEFORE",
        "the line stopped at `exit`: nothing after it ran"
    );
    assert_eq!(
        std::fs::read(fixture.seed("src/file0.txt")).expect("read the seed file"),
        b"final"
    );
    assert!(
        fixture.mux().job(exiting.id()).is_none(),
        "the exited job left the table"
    );
    assert!(
        !fixture.recorder().closed_with_storage(&uid),
        "a healthy job's snapshot is reclaimed rather than kept for a replay"
    );
    assert!(
        !fixture.snap().join(uid.as_str()).exists(),
        "the exited job's snapshot is gone"
    );
    let refused = exiting
        .run_command("printf 'again\\n'", CommandOptions::default())
        .await;
    assert!(
        matches!(refused, Err(RunError::Admission(MuxError::StaleJob(_)))),
        "the retained handle names a generation that exited: {refused:?}"
    );

    assert_eq!(
        run_line(&fixture, &sibling, "printf 'ALIVE\\n'").await,
        "ALIVE",
        "one shell's exit is not another's"
    );

    fixture.finish_mux().await;
}

/// A subshell's exit is the subshell's, and a bare `exit` leaves with the status the shell last
/// saw.
///
/// Both are the builtin's own semantics rather than this collection's — brush strips a subshell's
/// control flow at its boundary, and `exit` with no argument reuses the last status. What is under
/// test is that the job boundary honours exactly the request that survives: `(exit 23)` leaves the
/// pane open holding 23, and the bare `exit` submitted next closes it with that same 23.
#[tokio::test]
#[serial]
async fn subshell_exit_keeps_parent_open_and_bare_exit_reuses_status() {
    let mut fixture = Fixture::new();
    let job = fixture
        .mux()
        .open_shell(
            &fixture.seed,
            Some(Principal::from("sub")),
            SpawnOptions::default(),
        )
        .await
        .expect("open a shell");
    observed_ready(&fixture, &job);
    let uid = job.sandbox().uid.clone();

    let inner = job
        .run_command("(exit 23)", CommandOptions::default())
        .await
        .expect("the line publishes, whatever its process exited with");
    assert_eq!(
        inner.exit_code,
        Some(23),
        "the subshell's status is still the line's"
    );
    assert_eq!(concluded(&fixture, &uid).await.id, inner.id);
    assert!(
        !job.is_closed(),
        "a subshell's exit is the subshell's, not the shell's"
    );
    assert!(
        fixture.mux().job(job.id()).is_some(),
        "the pane is still open after a subshell exited"
    );

    let bare = job
        .run_command("exit", CommandOptions::default())
        .await
        .expect("the line publishes, whatever its process exited with");
    assert_eq!(
        bare.exit_code,
        Some(23),
        "bare `exit` leaves with the status the shell last saw"
    );
    let end = tokio::time::timeout(TIMEOUT, job.wait_closed())
        .await
        .expect("the job closes on its bare `exit`")
        .expect("its end");
    assert_eq!(end.close_mode, Some(JobCloseMode::Graceful));
    assert_eq!(
        end.completion.as_ref().and_then(|last| last.exit_code),
        Some(23),
        "the closure carries the reused status"
    );

    wait_for_close(&fixture, &uid).await;
    assert!(
        fixture.mux().job(job.id()).is_none(),
        "the exited job left the table"
    );

    fixture.finish_mux().await;
}
