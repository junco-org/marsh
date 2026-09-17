//! The persistence layer: the btrfs subvolume transactions commit into, and the state directory
//! beside it.
//!
//! Nothing is copied. A session starts at some directory, walks up to the first containing btrfs
//! subvolume — the *seed* — and commits straight into it. Its own state (job snapshots and logs)
//! lives beside the seed, in `<seed>/../.marsh/<seed name>`, so two sibling subvolumes under one
//! parent keep separate histories.
//!
//! A layer is owned by exactly one session, which takes its exclusive lease with [`
//! PersistenceLayer::acquire`]. That is why the layer is not `Clone`: the lease is a descriptor,
//! and two handles to one seed would be two claims on state only one process may own.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use sha1::{Digest, Sha1};

use crate::error::Error;
use crate::snapshot::Subvolumes;

/// Directory holding one seed's state, beside the seed itself.
pub const STATE_DIR: &str = ".marsh";

/// Number of hex characters kept from a digest.
const ID_LENGTH: usize = 8;

/// The first `ID_LENGTH` (8) hex characters of `sha1(input)`: how snapshot directories are named.
///
/// Short by design: these name directories a user types. Collisions are possible in principle and
/// harmless in practice — an id is redrawn from a monotonic counter, so an existing directory is
/// proof of a live id, never of a lost one.
#[must_use]
pub fn short_id(input: &str) -> String {
    let digest = Sha1::digest(input.as_bytes());
    // Two hex characters per byte, so encoding the first `ID_LENGTH / 2` bytes *is* the
    // `ID_LENGTH`-character prefix — without rendering the other 32 characters to throw away.
    hex::encode(&digest[..ID_LENGTH / 2])
}

/// One seed, the state directory that belongs to it, and the lease proving this process owns them.
#[derive(Debug)]
pub struct PersistenceLayer {
    /// The btrfs subvolume every transaction commits into: the first subvolume at or above the
    /// directory the session was started in.
    pub seed: PathBuf,
    /// `<seed>/../.marsh/<seed basename>`: holds `snap` and `meta`.
    pub root: PathBuf,
    /// Exclusive ownership of this session's persistent state, taken by [`Self::acquire`].
    /// Dropping the layer releases it.
    lock: Option<File>,
}

impl PersistenceLayer {
    /// A layer over explicit paths, touching nothing.
    ///
    /// The equivalent of naming `seed` and `root` directly: no btrfs operation happens here, so an
    /// ordinary directory pair is a usable layer for anything that does not snapshot.
    #[must_use]
    pub const fn new(seed: PathBuf, root: PathBuf) -> Self {
        Self {
            seed,
            root,
            lock: None,
        }
    }

    /// Finds the seed containing `start` and derives the state directory beside it.
    ///
    /// Touches the filesystem only to canonicalize `start` and to ask `fs` whether each ancestor
    /// is a subvolume; nothing is created here.
    ///
    /// # Errors
    ///
    /// Fails when `start` cannot be canonicalized, when no ancestor of it is a btrfs subvolume, or
    /// when the seed is its mount's root — there would be nowhere beside it to keep state.
    pub fn discover(start: &Path, fs: &dyn Subvolumes) -> Result<Self, Error> {
        let start = start.canonicalize().map_err(|error| Error::SeedDir {
            path: start.to_path_buf(),
            reason: error.to_string(),
        })?;
        let seed = find_seed(&start, &|candidate| fs.is_subvolume(candidate))
            .ok_or_else(|| Error::NoSubvolume(start.clone()))?;
        // Without this, a plain directory under a btrfs `/home` would resolve to `$SEED = /home`
        // and put the state at `/.marsh`.
        if fs.is_mount_root(&seed)? {
            return Err(Error::SeedIsMountRoot(seed));
        }
        let root = {
            let parent = seed
                .parent()
                .ok_or_else(|| Error::SeedIsMountRoot(seed.clone()))?;
            let name = seed
                .file_name()
                .ok_or_else(|| Error::SeedIsMountRoot(seed.clone()))?;
            parent.join(STATE_DIR).join(name)
        };
        Ok(Self::new(seed, root))
    }

    /// Creates the state directory if it is not there yet.
    ///
    /// No subvolume is ever created: the seed is the caller's own, and the state directory is a
    /// plain directory tree. What the btrfs assertions guarantee is that snapshots can be taken
    /// beside the seed and reclaimed unprivileged.
    ///
    /// # Errors
    ///
    /// Fails when something that is not a directory occupies the state path, when that path is not
    /// on a btrfs mount carrying `user_subvol_rm_allowed`, or when a directory cannot be created.
    pub fn materialize(&self, fs: &dyn Subvolumes) -> Result<(), Error> {
        // Before the create, which would otherwise fail with a bare `EEXIST` no user could act on.
        if self.root.exists() && !self.root.is_dir() {
            return Err(Error::StateNotDirectory(self.root.clone()));
        }
        std::fs::create_dir_all(&self.root)?;
        // Both checks need the path to exist, hence after the create.
        fs.assert_btrfs(&self.root)?;
        fs.assert_user_subvol_rm_allowed(&self.root)?;

        std::fs::create_dir_all(self.snap())?;
        std::fs::create_dir_all(self.meta().join("runs"))?;
        Ok(())
    }

    /// Takes this session's exclusive lease, and keeps it for the rest of the layer's life.
    ///
    /// Called once, before anything reads, truncates or recovers a log: a competing owner must fail
    /// before it can touch another session's state. Only the metadata directory needed for the lock
    /// file is created, so an ordinary-directory layer needs no btrfs materialization to be owned.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::SessionBusy`] when another process owns the seed, with
    /// [`Error::StateNotDirectory`] when a non-directory occupies the state root, and with
    /// [`Error::Io`] for any other failure to create or lock the file.
    pub fn acquire(&mut self) -> Result<(), Error> {
        if self.root.exists() && !self.root.is_dir() {
            return Err(Error::StateNotDirectory(self.root.clone()));
        }
        let meta = self.meta();
        std::fs::create_dir_all(&meta)?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_CLOEXEC)
            .open(meta.join("session.lock"))?;
        // SAFETY: `lock` owns a valid descriptor, and `flock` does not retain the pointer because
        // it receives only that scalar descriptor.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err(Error::SessionBusy(self.seed.clone()));
            }
            return Err(error.into());
        }
        self.lock = Some(lock);
        Ok(())
    }

    /// Job snapshots: `<root>/snap`.
    #[must_use]
    pub fn snap(&self) -> PathBuf {
        self.root.join("snap")
    }

    /// Logs and retained instrumentation: `<root>/meta`.
    #[must_use]
    pub fn meta(&self) -> PathBuf {
        self.root.join("meta")
    }

    /// The directory one execution's retained instrumentation is written into:
    /// `<root>/meta/runs/<run_id>`.
    ///
    /// `run_id` is one path component and nothing else, so a caller's identifier can never place a
    /// log outside the session's own metadata.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::InvalidRunId`] when `run_id` is empty, absolute, `.`, `..`, or more than
    /// one component.
    pub fn run_dir(&self, run_id: &str) -> Result<PathBuf, Error> {
        let invalid = || Error::InvalidRunId(run_id.to_string());
        let mut components = Path::new(run_id).components();
        let Some(Component::Normal(single)) = components.next() else {
            return Err(invalid());
        };
        if components.next().is_some() || single != std::ffi::OsStr::new(run_id) {
            return Err(invalid());
        }
        Ok(self.meta().join("runs").join(single))
    }

    /// A job's work tree: `<root>/snap/<uid>`.
    #[must_use]
    pub fn work(&self, uid: &str) -> PathBuf {
        self.snap().join(uid)
    }

    /// The tree bypassed commands read at seed version `seq`: `<root>/snap/read-<seq>`.
    ///
    /// Named by the version rather than by the job, because it is shared: one snapshot per
    /// committed version serves every read-only command that starts while that version is current,
    /// and a version of the seed never changes once it is committed. A job uid is eight hex
    /// characters, so it can never collide with this name.
    #[must_use]
    pub fn reader(&self, seq: u64) -> PathBuf {
        self.snap().join(format!("read-{seq}"))
    }
}

/// The nearest of `start` and its ancestors that satisfies `is_subvolume`.
///
/// The predicate is a parameter so the walk is testable without btrfs; the one caller forwards it
/// to [`Subvolumes::is_subvolume`].
fn find_seed(start: &Path, is_subvolume: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|candidate| is_subvolume(candidate))
        .map(Path::to_path_buf)
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    /// The id names a directory the console shows in its prompt, so its derivation is pinned.
    #[test]
    fn a_short_id_is_the_first_eight_hex_characters_of_sha1() {
        assert_eq!(short_id("abc"), "a9993e36");
        assert_eq!(short_id("abc").len(), ID_LENGTH);
        assert_ne!(short_id("abc"), short_id("abd"));
    }

    /// The seed is the *nearest* enclosing subvolume, not the outermost one: a nested subvolume is
    /// its own seed, and the walk must not climb past it into its parent.
    #[test]
    fn the_seed_is_the_nearest_enclosing_subvolume() {
        let subvolumes = |path: &Path| path == Path::new("/home/u/work") || path == Path::new("/");
        assert_eq!(
            find_seed(Path::new("/home/u/work/src/deep"), &subvolumes),
            Some(PathBuf::from("/home/u/work"))
        );
        assert_eq!(
            find_seed(Path::new("/home/u/work"), &subvolumes),
            Some(PathBuf::from("/home/u/work")),
            "the starting directory is itself a candidate"
        );
        assert_eq!(find_seed(Path::new("/home/u/work/src"), &|_| false), None);
    }

    /// A run id names a directory inside this session's metadata, so anything that could climb out
    /// of it — or that is not one plain component — has to be refused before a log is written.
    #[test]
    fn a_run_id_is_exactly_one_plain_component() {
        let layer = PersistenceLayer::new(PathBuf::from("/seed"), PathBuf::from("/state"));
        assert_eq!(
            layer.run_dir("case").expect("a plain component"),
            PathBuf::from("/state/meta/runs/case")
        );
        for rejected in ["", ".", "..", "/", "/abs", "a/b", "../escape", "./here"] {
            let error = layer.run_dir(rejected).expect_err("a rejected run id");
            assert!(
                matches!(&error, Error::InvalidRunId(id) if id == rejected),
                "{rejected}: got {error:?}"
            );
        }
    }

    /// Two live layers over one seed are two claims on state only one process may own.
    #[test]
    fn a_second_acquire_reports_the_seed_as_busy() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let seed = scratch.path().join("seed");
        let root = scratch.path().join("state");
        std::fs::create_dir_all(&seed).expect("seed");

        let mut first = PersistenceLayer::new(seed.clone(), root.clone());
        first.acquire().expect("the first claim succeeds");

        let mut second = PersistenceLayer::new(seed.clone(), root);
        let error = second.acquire().expect_err("the second claim is refused");
        assert!(
            matches!(&error, Error::SessionBusy(path) if *path == seed),
            "got {error:?}"
        );

        drop(first);
        second
            .acquire()
            .expect("the lease is released with the layer");
    }
}
