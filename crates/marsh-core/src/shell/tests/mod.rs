//! White-box storage and semantic assertions stay below the ordinary Shell interface.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::session::Session;
use super::{Shell, ShellError, ShellErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod git_commands;
mod publication;
mod reconciliation;

/// A private seed holding `src/a.txt`, a subvolume of a fake filesystem backend.
struct Fixture {
    _scratch: tempfile::TempDir,
    seed: PathBuf,
    fs: Arc<marsh_btrfs::fake::CopyTree>,
}
impl Fixture {
    fn new() -> Self {
        let scratch = tempfile::tempdir().expect("scratch");
        let seed = scratch.path().canonicalize().unwrap().join("seed");
        std::fs::create_dir_all(seed.join("src")).unwrap();
        std::fs::write(seed.join("src/a.txt"), b"seed\n").unwrap();
        let fs = Arc::new(marsh_btrfs::fake::CopyTree::new());
        fs.register(&seed);
        Self {
            _scratch: scratch,
            seed,
            fs,
        }
    }
    /// A managed shell over the seed, which must start.
    async fn shell(&self) -> Shell {
        self.open().await.expect("build managed shell")
    }
    /// Startup's verdict on a managed shell over the seed.
    async fn open(&self) -> Result<Shell, ShellError> {
        self.open_on(self.fs.clone()).await
    }
    /// Startup's verdict on a managed shell over the seed, stored through `backend`.
    async fn open_on(
        &self,
        backend: Arc<dyn marsh_btrfs::Subvolumes>,
    ) -> Result<Shell, ShellError> {
        let mut builder = Shell::builder().working_dir(self.seed.clone());
        builder.backend = Some(backend);
        builder.build().await
    }
    fn outside(&self, name: &str) -> PathBuf {
        self.seed.parent().unwrap().join(name)
    }
    /// The seed's write-ahead log.
    fn log(&self) -> PathBuf {
        self.seed
            .parent()
            .unwrap()
            .join(marsh_btrfs::STATE_DIR)
            .join("seed/meta")
            .join(marsh_wal::LOG_FILE)
    }
    /// A seed file's contents.
    fn read(&self, path: &str) -> String {
        read(&self.seed.join(path))
    }
}

/// A file's contents.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}
/// The session a live `shell` publishes through.
async fn session(shell: &Shell) -> Arc<Session> {
    Arc::clone(
        &shell
            .shared
            .live
            .lock()
            .await
            .as_ref()
            .unwrap()
            .snapshot
            .session,
    )
}
/// Runs `line`, which must be accepted and exit 0.
async fn accepted(shell: &Shell, line: &str) {
    let result = shell
        .run(line)
        .await
        .unwrap_or_else(|error| panic!("{line}: {error}"));
    assert_eq!(u8::from(result.exit_code), 0, "{line}");
}
/// Runs `line`, whose publication the gate must refuse.
async fn refused(shell: &Shell, line: &str) {
    let Err(error) = shell.run(line).await else {
        panic!("{line}: accepted")
    };
    assert!(
        matches!(error.kind(), ShellErrorKind::Denied { .. }),
        "{line}: {error}"
    );
}
/// Closes `shell`, which must release everything it held.
async fn close(shell: Shell) {
    shell.close(false).await.expect("close shell");
}
