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

    /// Finds the seed containing `initial_dir` and derives the state directory beside it.
    ///
    /// Returns the layer together with the canonicalized starting directory. The canonical form is
    /// what the seed walk was run against, so a caller computing a seed-relative working directory
    /// gets a path that actually strips against [`Self::seed`] — a symlinked spelling would not.
    ///
    /// Touches the filesystem only to canonicalize `initial_dir` and to ask `fs` whether each
    /// ancestor is a subvolume; nothing is created, leased or recovered here.
    ///
    /// # Errors
    ///
    /// Fails when `initial_dir` cannot be canonicalized or is not a directory, when no ancestor of
    /// it is a btrfs subvolume, or when the seed is its mount's root — there would be nowhere
    /// beside it to keep state.
    pub fn discover(initial_dir: &Path, fs: &dyn Subvolumes) -> Result<(Self, PathBuf), Error> {
        let start = initial_dir.canonicalize().map_err(|error| Error::SeedDir {
            path: initial_dir.to_path_buf(),
            reason: error.to_string(),
        })?;
        // A regular file canonicalizes fine and would otherwise seed off its parent, silently
        // starting a shell somewhere the caller never named.
        if !start.is_dir() {
            return Err(Error::SeedDir {
                path: initial_dir.to_path_buf(),
                reason: "not a directory".to_owned(),
            });
        }
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
        Ok((Self::new(seed, root), start))
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

    /// A canonicalized scratch directory: `discover` canonicalizes `start`, so expectations keyed
    /// on a path must be keyed on the resolved one.
    fn scratch() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("scratch directory");
        let root = dir.path().canonicalize().expect("canonical scratch");
        (dir, root)
    }

    /// A mock answering `is_subvolume` true for exactly `seed` and reporting no mount roots.
    fn subvolume_at(seed: &Path) -> crate::snapshot::MockSubvolumes {
        let seed = seed.to_path_buf();
        let mut fs = crate::snapshot::MockSubvolumes::new();
        fs.expect_is_subvolume()
            .returning(move |candidate| candidate == seed);
        fs.expect_is_mount_root().returning(|_| Ok(false));
        fs
    }

    /// Discovery is the seed walk plus the state-directory derivation: the state of a seed goes
    /// *beside* it, named after it, so two sibling seeds keep separate histories. The canonical
    /// starting directory comes back with it, because that is what a caller has to strip against
    /// the seed to know where inside it a shell begins.
    #[test]
    fn discovery_puts_state_beside_the_nearest_enclosing_subvolume() {
        let (_dir, base) = scratch();
        let seed = base.join("seed");
        let start = seed.join("src/deep");
        std::fs::create_dir_all(&start).expect("start directory");
        let alias = base.join("alias");
        std::os::unix::fs::symlink(&seed, &alias).expect("alias to the seed");

        let (layer, canonical) =
            PersistenceLayer::discover(&start, &subvolume_at(&seed)).expect("discovery");

        assert_eq!(layer.seed, seed);
        assert_eq!(layer.root, base.join(STATE_DIR).join("seed"));
        assert_eq!(layer.snap(), layer.root.join("snap"));
        assert_eq!(layer.meta(), layer.root.join("meta"));
        assert_eq!(canonical, start);

        let (layer, canonical) =
            PersistenceLayer::discover(&alias.join("src/deep"), &subvolume_at(&seed))
                .expect("discovery through a symlink");
        assert_eq!(layer.seed, seed);
        assert_eq!(
            canonical, start,
            "the resolved directory strips against the seed; the alias spelling would not"
        );
    }

    /// A seed that is its own mount root has no usable parent directory, so discovery refuses it
    /// rather than writing state onto whatever filesystem holds the mount point.
    #[test]
    fn a_seed_that_is_its_mount_root_is_refused() {
        let (_dir, base) = scratch();
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let mut fs = crate::snapshot::MockSubvolumes::new();
        let expected = seed.clone();
        fs.expect_is_subvolume()
            .returning(move |candidate| candidate == expected);
        fs.expect_is_mount_root().returning(|_| Ok(true));

        let error = PersistenceLayer::discover(&seed, &fs).expect_err("refused");
        assert!(
            matches!(&error, Error::SeedIsMountRoot(path) if *path == seed),
            "got {error:?}"
        );
    }

    /// Nothing on the way up is a subvolume: there is no seed to commit into.
    #[test]
    fn discovery_without_any_enclosing_subvolume_fails() {
        let (_dir, base) = scratch();
        let start = base.join("plain");
        std::fs::create_dir_all(&start).expect("start directory");
        let mut fs = crate::snapshot::MockSubvolumes::new();
        fs.expect_is_subvolume().returning(|_| false);

        let error = PersistenceLayer::discover(&start, &fs).expect_err("refused");
        assert!(
            matches!(&error, Error::NoSubvolume(path) if *path == start),
            "got {error:?}"
        );
    }

    /// An unreadable mount table is not a "not a mount root" answer: it surfaces as the failure it
    /// is, so a session never starts on a guess.
    #[test]
    fn a_mount_table_failure_stops_discovery() {
        let (_dir, base) = scratch();
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let mut fs = crate::snapshot::MockSubvolumes::new();
        fs.expect_is_subvolume().returning(|_| true);
        fs.expect_is_mount_root()
            .returning(|_| Err(Error::Io(std::io::Error::other("no mount table"))));

        let error = PersistenceLayer::discover(&seed, &fs).expect_err("refused");
        assert!(matches!(&error, Error::Io(_)), "got {error:?}");
    }

    /// A starting directory that does not exist is reported with its path and the reason, rather
    /// than as a missing seed.
    #[test]
    fn a_starting_directory_that_cannot_be_resolved_is_reported_as_such() {
        let (_dir, base) = scratch();
        let start = base.join("absent");
        let fs = crate::snapshot::MockSubvolumes::new();

        let error = PersistenceLayer::discover(&start, &fs).expect_err("refused");
        assert!(
            matches!(&error, Error::SeedDir { path, reason }
                if *path == start && !reason.is_empty()),
            "got {error:?}"
        );
    }

    /// A file is not a place a shell can start: it resolves, so without the check it would seed
    /// off its parent directory and run somewhere nobody asked for.
    #[test]
    fn a_starting_path_that_is_not_a_directory_is_refused() {
        let (_dir, base) = scratch();
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let file = seed.join("file");
        std::fs::write(&file, b"not a directory").expect("a regular file");

        let error = PersistenceLayer::discover(&file, &subvolume_at(&seed)).expect_err("refused");
        assert!(
            matches!(&error, Error::SeedDir { path, reason }
                if *path == file && reason == "not a directory"),
            "got {error:?}"
        );
    }

    /// Materialization is what makes the layout usable: both state subtrees exist afterwards, and
    /// running it again on a live layout changes nothing.
    #[test]
    fn materializing_creates_the_state_layout_and_is_repeatable() {
        let (_dir, base) = scratch();
        let layer = PersistenceLayer::new(base.join("seed"), base.join("state"));
        let mut fs = crate::snapshot::MockSubvolumes::new();
        fs.expect_assert_btrfs().returning(|_| Ok(()));
        fs.expect_assert_user_subvol_rm_allowed()
            .returning(|_| Ok(()));

        layer.materialize(&fs).expect("first materialization");
        std::fs::write(layer.snap().join("keep"), b"x").expect("a file in snap");
        layer.materialize(&fs).expect("second materialization");

        assert!(layer.snap().is_dir());
        assert!(layer.meta().join("runs").is_dir());
        assert!(
            layer.snap().join("keep").exists(),
            "repeating materialization does not wipe existing state"
        );
    }

    /// The btrfs assertions are the reason materialization exists; neither is swallowed.
    #[test]
    fn materializing_propagates_the_btrfs_assertions() {
        let (_dir, base) = scratch();
        let layer = PersistenceLayer::new(base.join("seed"), base.join("state"));

        let mut not_btrfs = crate::snapshot::MockSubvolumes::new();
        not_btrfs
            .expect_assert_btrfs()
            .returning(|path| Err(Error::NotBtrfs(path.to_path_buf())));
        let error = layer.materialize(&not_btrfs).expect_err("refused");
        assert!(
            matches!(&error, Error::NotBtrfs(path) if *path == layer.root),
            "got {error:?}"
        );

        let mut not_allowed = crate::snapshot::MockSubvolumes::new();
        not_allowed.expect_assert_btrfs().returning(|_| Ok(()));
        not_allowed
            .expect_assert_user_subvol_rm_allowed()
            .returning(|path| Err(Error::NotUserSubvolRmAllowed(path.to_path_buf())));
        let error = layer.materialize(&not_allowed).expect_err("refused");
        assert!(
            matches!(&error, Error::NotUserSubvolRmAllowed(path) if *path == layer.root),
            "got {error:?}"
        );
    }

    /// A file where the state root belongs would otherwise surface as a bare `EEXIST` from
    /// `create_dir_all`, which names neither the path nor the problem.
    #[test]
    fn a_file_occupying_the_state_root_is_named() {
        let (_dir, base) = scratch();
        let root = base.join("state");
        std::fs::write(&root, b"not a directory").expect("occupying file");
        let layer = PersistenceLayer::new(base.join("seed"), root.clone());
        let fs = crate::snapshot::MockSubvolumes::new();

        let error = layer.materialize(&fs).expect_err("refused");
        assert!(
            matches!(&error, Error::StateNotDirectory(path) if *path == root),
            "got {error:?}"
        );

        let mut layer = layer;
        let error = layer.acquire().expect_err("refused");
        assert!(
            matches!(&error, Error::StateNotDirectory(path) if *path == root),
            "acquiring is refused for the same reason: got {error:?}"
        );
    }

    /// A job's work tree and a version's reader tree are distinct names under one `snap/`.
    #[test]
    fn work_and_reader_trees_are_named_under_snap() {
        let layer = PersistenceLayer::new(PathBuf::from("/seed"), PathBuf::from("/state"));
        assert_eq!(
            layer.work("a9993e36"),
            PathBuf::from("/state/snap/a9993e36")
        );
        assert_eq!(layer.reader(7), PathBuf::from("/state/snap/read-7"));
    }
}
