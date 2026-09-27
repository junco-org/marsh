//! The rmux host fixture, and the facade handles a managed line can run through.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use brush_core::env::ShellEnvironment;
use marsh::ShellError;
use marsh::rmux::{
    CapturedOutput, CollectOptions, Execution, ExecutionSpec, IoEnvelope, IoError, IoEventStream,
    IoResult, RmuxFrontend, ShellHandle, ShellIo,
};
use marsh::shellmux::{CommandCompletion, CommandOptions, JobIo, SpawnOptions, TerminalGeometry};
use marsh_btrfs::Subvolumes;
use marsh_btrfs::fake::CopyTree;
use rmux_proto::ProcessCommand;
use rmux_server::DaemonConfig;
use tempfile::TempDir;

use super::{Runner, TIMEOUT};

/// Two sibling seeds on one filesystem and a bound host whose default directory is the first.
///
/// The second seed is registered but never opened by the fixture itself: a host leases nothing
/// until a shell asks for a directory, so the tests that are about leases open their own shells
/// and can prove one host serves both trees.
pub struct Host {
    /// The running daemon.
    pub rmux: Option<RmuxFrontend>,
    /// The facade every test drives.
    pub io: ShellIo,
    /// The first seed's root, and the host's default directory.
    pub seed: PathBuf,
    /// A second registered seed beside the first.
    pub other: PathBuf,
    /// The daemon socket, outside both seeds so it is never part of what a workload publishes.
    pub socket: PathBuf,
    /// The filesystem both seeds live on, so a competing host sees the same trees.
    pub fs: Arc<dyn Subvolumes>,
    /// Kept alive: dropping it deletes the trees the host publishes into. Also the root a test
    /// joins into to reach paths outside the seeds.
    pub scratch: TempDir,
}

impl Host {
    /// Binds a host over fresh seeds on an ordinary fake btrfs.
    pub async fn new() -> Self {
        Self::with(Arc::new(CopyTree::new())).await
    }

    /// Binds a host over fresh seeds created through `fs`.
    ///
    /// The seam exists for the test that has to observe the daemon *mid*-teardown: the seeds are
    /// created through the same backend the host serves over, so a fixture can intercept a
    /// filesystem operation the engine's release performs.
    pub async fn with(fs: Arc<dyn Subvolumes>) -> Self {
        let scratch = tempfile::tempdir().expect("a scratch directory");
        // Siblings rather than parent and child: a nested tree would be reachable by walking up
        // out of the first seed, and the two would not be independent publication roots.
        let [seed, other, socket] =
            ["seed", "other", "rmux.sock"].map(|name| scratch.path().join(name));
        for root in [&seed, &other] {
            fs.create_subvolume(root).expect("create a seed root");
        }
        let rmux = frontend(&socket, &seed, Arc::clone(&fs), "open an rmux frontend").await;
        Self {
            io: rmux.io(),
            rmux: Some(rmux),
            seed,
            other,
            socket,
            fs,
            scratch,
        }
    }

    /// A path inside the first seed.
    pub fn seed(&self, path: &str) -> PathBuf {
        self.seed.join(path)
    }

    /// Stops the daemon and waits for every snapshot to be reclaimed.
    pub async fn shutdown(mut self) {
        if let Some(rmux) = self.rmux.take() {
            rmux.shutdown().await.expect("shut the host down");
        }
    }
}

/// Binds a daemon on `socket` whose default directory is `seed`, failing the test with `what`.
pub async fn frontend(
    socket: &Path,
    seed: &Path,
    fs: Arc<dyn Subvolumes>,
    what: &str,
) -> RmuxFrontend {
    rmux_server::test_support::open_frontend(
        DaemonConfig::new(socket.to_path_buf()),
        seed,
        ShellEnvironment::new(),
        TerminalGeometry { rows: 24, cols: 80 },
        fs,
    )
    .await
    .expect(what)
}

/// Spawn options for a job with independent standard input, output and error pipes.
pub fn pipes() -> SpawnOptions {
    SpawnOptions {
        io: JobIo::Pipes,
        ..SpawnOptions::default()
    }
}

/// Submits `process` as a nameless workload in the host's default directory.
pub async fn admit(io: &ShellIo, process: ProcessCommand) -> IoResult<Execution> {
    io.execute(ExecutionSpec {
        initial_dir: PathBuf::new(),
        id: None,
        process,
        environment: None,
    })
    .await
}

/// Whether `events` delivers an envelope `found` accepts before it ends, fails or [`TIMEOUT`]
/// passes.
pub async fn saw(events: &mut IoEventStream, mut found: impl FnMut(&IoEnvelope) -> bool) -> bool {
    tokio::time::timeout(TIMEOUT, async {
        while let Ok(Some(envelope)) = events.recv().await {
            if found(&envelope) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false)
}

/// A shell workload collected whole: a denial is part of the captured completion, not an error.
impl Runner for ShellIo {
    type Output = CapturedOutput;
    type Error = IoError;
    async fn attempt(&self, line: &str) -> IoResult<CapturedOutput> {
        let execution = admit(self, ProcessCommand::Shell(line.to_owned())).await?;
        execution.collect(CollectOptions::default()).await
    }
    fn verdict(error: &IoError) -> Option<&ShellError> {
        <ShellHandle as Runner>::verdict(error)
    }
}

impl Runner for ShellHandle {
    type Output = Arc<CommandCompletion>;
    type Error = IoError;
    async fn attempt(&self, line: &str) -> IoResult<Arc<CommandCompletion>> {
        self.run_command(line, CommandOptions::default()).await
    }
    fn verdict(error: &IoError) -> Option<&ShellError> {
        let IoError::Run(error) = error else {
            return None;
        };
        <marsh::shellmux::Shell as Runner>::verdict(error)
    }
}
