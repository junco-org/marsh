//! The shell engine a unit-test handler's panes run on.
//!
//! Production binds one facade per daemon in `listener::serve`, before startup configuration can
//! create a pane. Unit tests build a [`RequestHandler`](crate::handler::RequestHandler) on its own
//! — there is no socket, no [`RmuxFrontend`](crate::RmuxFrontend) and no `serve` — so nothing
//! would ever bind one, and
//! every pane creation would fail for want of an engine.
//!
//! This builds one per handler, lazily, the first time a pane needs it:
//!
//! * a private seed in a scratch tree behind [`marsh_btrfs::fake::CopyTree`], so these tests keep
//!   running on hosts with no btrfs and never touch a real subvolume;
//! * its own [`PolicyValidator`](marsh_core::PolicyValidator), so one test's grants are invisible
//!   to every other test running beside it;
//! * the same profile production uses, so the support builtins a pane's commands
//!   rely on are registered before `Shell::attach` here exactly as they are there;
//! * the same observation consumer, so a job's terminal bytes reach the pane's transcript through
//!   the production path rather than a test-only shortcut.
//!
//! Lazily rather than in the constructor because a mux owns a task per job pump: it needs a Tokio
//! runtime, and `RequestHandler::new` is synchronous and is called from synchronous tests too. A
//! handler used entirely outside a runtime never builds one and never needs one.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use marsh_core::shellmux::TerminalGeometry;
use tempfile::TempDir;

use crate::handler::RequestHandler;
use crate::io::ShellIo;

/// A directory inside one handler's own default tree, for a test that *names* a start directory.
///
/// # Why a test cannot just use `std::env::temp_dir()`
///
/// A pane, a popup and every workload helper runs in a snapshot of the seed its own starting
/// directory lies in. A directory the caller names that is under no subvolume at all is a request
/// the daemon genuinely cannot honour, so it is refused loudly. A test that allocates under the
/// process temp directory and passes it as `start_directory` is pinning the old unconfined
/// semantics, where a pane could start anywhere on the host.
///
/// # Why this is built per handler and never cached
///
/// [`open`] leases a fresh scratch tree for *every* handler, so there is no such thing as "the"
/// test seed. A process-global scratch root would hand one handler a directory that belongs to
/// another handler's seed, which is the same reused-name defect this whole class of failure is
/// made of, only harder to see.
///
/// # Both spellings, because both are load-bearing
///
/// * [`path`](Self::path) is a real host directory — the seed is an ordinary tree behind
///   [`marsh_btrfs::fake::CopyTree`] — so it is what the request carries and what a probe file the
///   job published is read back through with plain [`std::fs`].
/// * [`relative`](Self::relative) is the seed-relative label. A job's own view of its cwd is the
///   *snapshot*, not the host seed, so a test asserting on `pwd` compares against this suffix
///   rather than against the absolute host path.
#[derive(Debug, Clone)]
pub(crate) struct SeedScratch {
    /// The host path, `<seed>/<relative>`.
    host: PathBuf,
    /// The same place spelled the way a job directory is spelled: `/`-joined, seed-relative.
    relative: String,
}

impl SeedScratch {
    /// The host path, for the request to carry and for `std::fs` to read back through.
    pub(crate) fn path(&self) -> &Path {
        &self.host
    }

    /// The seed-relative label, for an assertion against a job's own view of its directory.
    pub(crate) fn relative(&self) -> &str {
        &self.relative
    }

    /// Creates and returns a directory beneath this one, in the same seed.
    ///
    /// For the tests that need several named directories at once — a session's, a caller's and an
    /// explicitly requested one — without each of them re-deriving the seed.
    ///
    /// # Panics
    ///
    /// Panics when the directory cannot be created, which is a fixture failure: the test was
    /// about to name a directory and now has none.
    pub(crate) fn child(&self, name: &str) -> Self {
        let host = self.host.join(name);
        std::fs::create_dir_all(&host).expect("create a scratch directory in the test seed");
        Self {
            host,
            relative: format!("{}/{name}", self.relative),
        }
    }
}

/// Creates `label` directly under `handler`'s own seed and returns both spellings of it.
///
/// Resolving the facade through [`crate::managed_workload::handler_facade`] rather than reading
/// the slot is what makes this usable *before* the test has created anything: a unit-test handler
/// builds its engine lazily, so a helper that only read the slot would answer `None` for exactly
/// the tests that need a directory up front. The engine it resolves is the one every later pane,
/// popup and workload in this test will use, because they all share the handler's one facade slot.
///
/// # Panics
///
/// Panics when the handler has no engine and when the directory cannot be created. Both are
/// fixture failures with nothing to fall back to.
pub(crate) fn seed_scratch_dir(handler: &RequestHandler, label: &str) -> SeedScratch {
    let io = crate::managed_workload::handler_facade(handler)
        .expect("a unit-test handler builds its own engine");
    // The engine's default directory *is* its registered seed: `open` below hands it exactly that
    // tree, so a path under it is inside the seed a job opened here will discover.
    let host = io.default_dir().join(label);
    std::fs::create_dir_all(&host).expect("create a scratch directory in the test seed");
    SeedScratch {
        host,
        relative: label.to_owned(),
    }
}

/// Default height for a test handler's terminals; any nonzero pair would do.
const ROWS: u16 = 24;
/// Default width, as above.
const COLS: u16 = 80;

/// One test handler's private engine.
///
/// The scratch tree is held for the engine's lifetime: dropping it deletes the seed the mux
/// publishes into, and a mux whose seed vanished underneath it is not what any test means to
/// exercise.
///
/// # The cycle this exists to break
///
/// The observation consumer owns a facade clone, that clone owns the multiplexer, and the
/// multiplexer owns the frontend's sender — which is the only thing that can ever end the
/// consumer's receive loop. Nothing in that ring is weak, so a consumer left to itself keeps its
/// own sender alive and runs forever, holding a mux, its pseudoterminals and its snapshots, while
/// the scratch tree is deleted underneath it. One test binary builds and drops many handlers, so
/// "forever" means the whole run.
///
/// Field order is the drop order and is load-bearing: the consumer is aborted and the core
/// released before the scratch tree the seed lives in goes away.
#[derive(Debug)]
pub(super) struct TestEngine {
    /// The observation consumer, aborted on drop.
    consumer: tokio::task::JoinHandle<()>,
    /// The engine's own facade handle, released on drop.
    io: ShellIo,
    /// The scratch tree the seed lives in. Dropped last.
    _scratch: TempDir,
}

impl Drop for TestEngine {
    /// Ends the consumer and releases the multiplexer, in that order.
    ///
    /// Abort first: the consumer is the strong reference that would otherwise keep the facade —
    /// and through it the multiplexer and the seed's lease — alive past this handler. Then
    /// `release_core`, which is synchronous precisely so a destructor can call it; the
    /// multiplexer's own drop reclaims the snapshots afterwards.
    fn drop(&mut self) {
        self.consumer.abort();
        self.io.release_core();
    }
}

/// Builds this state's private engine and installs it into `slot`.
///
/// Returns `None` when there is no Tokio runtime to own the mux's tasks, when the scratch tree
/// cannot be created, or when the handler this state belongs to is already gone. Each of those
/// leaves pane creation to fail with the ordinary "no shell facade" error, which is a far clearer
/// diagnostic than a panic inside a lazily built fixture.
pub(super) fn open(
    slot: &Arc<StdMutex<Option<ShellIo>>>,
    engine: &mut Option<TestEngine>,
    handler: Option<&crate::handler::WeakRequestHandler>,
) -> Option<ShellIo> {
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    let handler = handler?.clone();

    let scratch = tempfile::tempdir().ok()?;
    let root = scratch.path().canonicalize().ok()?;
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).ok()?;
    let filesystem = Arc::new(marsh_btrfs::fake::CopyTree::new());
    filesystem.register(&seed);

    // The socket never exists: these handlers serve no listener, and the facade only uses the
    // path to pin an SDK connection, which no unit test opens.
    let (io, events) = ShellIo::new(
        &seed,
        brush_core::env::ShellEnvironment::new(),
        TerminalGeometry {
            rows: ROWS,
            cols: COLS,
        },
        filesystem,
        runtime.clone(),
        root.join("rmux.sock"),
    )
    .ok()?;
    // Both directions, before the consumer starts. Selection is the facade's own state now, so a
    // `switch` publishes its event and then calls the handler back; a fixture that installed only
    // the forward consumer would see the event and never the pane selection it implies.
    io.install_handler(handler.clone());
    let consumer = runtime.spawn(crate::io::observation::consume(
        io.unleased(),
        handler,
        events,
    ));

    *engine = Some(TestEngine {
        consumer,
        io: io.unleased(),
        _scratch: scratch,
    });
    let installed = io.unleased();
    *slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(io);
    Some(installed)
}
