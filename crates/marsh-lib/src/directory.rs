//! Walking a directory tree, with the caller deciding what every entry means.
//!
//! Three owned callers walk a tree for three unrelated reasons — copying it, indexing it, deleting
//! things out of it — and all three had written the same stack, the same `read_dir` loop and the
//! same descent push. What differs between them is entirely inside the loop body: which metadata
//! they read, whether a symlink is followed, what they carry down to a child directory.
//!
//! So the walk is shared and the policy is not. [`walk_directory`] performs no `metadata` or
//! `file_type` call of its own, follows nothing, sorts nothing and knows no filesystem convention;
//! the visitor does all of that, and says whether to descend by handing back the state its child
//! directory should be visited with.

use std::fs::DirEntry;
use std::path::{Path, PathBuf};

/// Walks the tree rooted at `root`, calling `visit` once per entry.
///
/// The visitor receives an entry and the state its containing directory was reached with, and
/// answers with the state a child directory is to be walked with — `None` prunes, so an entry is
/// descended into exactly when the caller says it is, whatever the filesystem says it is.
///
/// Order is the order the three callers already had, and is load-bearing for none of them but
/// stable for all of them: one directory's entries are visited as a contiguous batch in `read_dir`
/// encounter order, and the directories descended from that batch are then walked last-first, each
/// subtree complete before the next one starts. `root` itself is never handed to the visitor.
///
/// # Errors
///
/// Fails with the caller's own error when a visitor rejects an entry, and with `E::from` an
/// [`std::io::Error`] when a directory cannot be read or an entry cannot be resolved. Both stop
/// the walk where it stands: whatever the visitor already did to earlier entries stays done.
pub fn walk_directory<S, E>(
    root: &Path,
    initial: S,
    mut visit: impl FnMut(&DirEntry, &S) -> Result<Option<S>, E>,
) -> Result<(), E>
where
    E: From<std::io::Error>,
{
    let mut pending: Vec<(PathBuf, S)> = vec![(root.to_path_buf(), initial)];
    while let Some((directory, state)) = pending.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            if let Some(descend) = visit(&entry, &state)? {
                pending.push((entry.path(), descend));
            }
        }
    }
    Ok(())
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    /// A caller error the walk's I/O failures convert into.
    #[derive(Debug)]
    enum TestError {
        /// A directory or entry could not be read.
        Io(std::io::Error),
        /// The visitor refused an entry by name.
        Refused(String),
    }

    impl From<std::io::Error> for TestError {
        fn from(error: std::io::Error) -> Self {
            Self::Io(error)
        }
    }

    /// The state carried down the tree: deliberately not `Clone`, so the walk cannot be tempted to
    /// duplicate it.
    struct Descent {
        /// The directory this state was reached with.
        directory: PathBuf,
        /// How many directories down from the root it is.
        depth: usize,
    }

    /// One visited entry.
    #[derive(Debug, PartialEq, Eq)]
    struct Visit {
        /// The directory being enumerated.
        directory: PathBuf,
        /// The entry's own name.
        name: String,
        /// The depth the enumerated directory was reached at.
        depth: usize,
    }

    fn file(path: &Path) {
        std::fs::write(path, b"x").expect("file");
    }

    fn directory(path: &Path) -> PathBuf {
        std::fs::create_dir_all(path).expect("directory");
        path.to_path_buf()
    }

    /// Walks `root`, descending into every directory and recording what was seen where.
    fn trace(root: &Path) -> Result<Vec<Visit>, TestError> {
        let mut seen = Vec::new();
        walk_directory::<Descent, TestError>(
            root,
            Descent {
                directory: root.to_path_buf(),
                depth: 0,
            },
            |entry, state| {
                seen.push(Visit {
                    directory: state.directory.clone(),
                    name: entry.file_name().to_string_lossy().into_owned(),
                    depth: state.depth,
                });
                if entry.file_type()?.is_dir() {
                    Ok(Some(Descent {
                        directory: entry.path(),
                        depth: state.depth + 1,
                    }))
                } else {
                    Ok(None)
                }
            },
        )?;
        Ok(seen)
    }

    /// The directories a trace enumerated, in the order it enumerated them, with no directory
    /// appearing twice in a row.
    fn batches(seen: &[Visit]) -> Vec<PathBuf> {
        let mut order: Vec<PathBuf> = Vec::new();
        for visit in seen {
            if order.last() != Some(&visit.directory) {
                order.push(visit.directory.clone());
            }
        }
        order
    }

    /// A directory's entries are one contiguous batch, and the directories it descends into are
    /// walked last-first with each subtree complete before the next one starts.
    ///
    /// The expectation is derived from the encounter order the trace itself reports: `read_dir`
    /// does not promise alphabetical names, so a test that assumed them would be testing the
    /// filesystem.
    #[test]
    fn a_parent_is_enumerated_before_its_children_are_walked_last_first() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = directory(&scratch.path().join("root"));
        let a = directory(&root.join("a"));
        let b = directory(&root.join("b"));
        let c = directory(&b.join("c"));
        file(&root.join("f.txt"));
        file(&a.join("a-leaf.txt"));
        file(&c.join("c-leaf.txt"));

        let seen = trace(&root).expect("walk");

        let root_batch: Vec<&Visit> = seen
            .iter()
            .filter(|visit| visit.directory == root)
            .collect();
        assert_eq!(root_batch.len(), 3, "every root entry is visited once");
        assert!(root_batch.iter().all(|visit| visit.depth == 0));

        let mut descended: Vec<PathBuf> = root_batch
            .iter()
            .filter(|visit| visit.name != "f.txt")
            .map(|visit| root.join(&visit.name))
            .collect();
        descended.reverse();
        let mut expected = vec![root.clone()];
        for child in descended {
            let subtree = if child == b {
                vec![b.clone(), c.clone()]
            } else {
                vec![a.clone()]
            };
            expected.extend(subtree);
        }

        assert_eq!(batches(&seen), expected);
        assert_eq!(
            seen.iter()
                .find(|visit| visit.name == "c-leaf.txt")
                .map(|visit| visit.depth),
            Some(2),
            "state descends with the walk"
        );
    }

    /// Answering `None` for a directory prunes it, however many entries it holds.
    #[test]
    fn a_pruned_directory_is_never_enumerated() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = directory(&scratch.path().join("root"));
        let skipped = directory(&root.join("skipped"));
        file(&skipped.join("hidden.txt"));
        directory(&skipped.join("deeper"));
        let kept = directory(&root.join("kept"));
        file(&kept.join("seen.txt"));

        let mut names = Vec::new();
        walk_directory::<(), TestError>(&root, (), |entry, ()| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let descend = entry.file_type()?.is_dir() && name != "skipped";
            names.push(name);
            Ok(descend.then_some(()))
        })
        .expect("walk");

        names.sort();
        assert_eq!(names, vec!["kept", "seen.txt", "skipped"]);
    }

    /// An empty directory is a successful walk that visits nothing.
    #[test]
    fn an_empty_tree_visits_nothing() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = directory(&scratch.path().join("root"));
        directory(&root.join("empty"));

        let seen = trace(&root).expect("walk");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].name, "empty");
    }

    /// A root that is not there is the caller's I/O error, not an empty walk.
    #[test]
    fn a_missing_root_is_an_io_error() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let error = trace(&scratch.path().join("absent")).expect_err("missing root");
        match error {
            TestError::Io(io) => assert_eq!(io.kind(), std::io::ErrorKind::NotFound),
            TestError::Refused(name) => panic!("unexpected refusal of {name}"),
        }
    }

    /// A visitor error ends the walk immediately: neither the rest of the current directory nor
    /// any directory still pending is enumerated.
    #[test]
    fn a_visitor_error_stops_the_walk() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let root = directory(&scratch.path().join("root"));
        let child = directory(&root.join("child"));
        file(&child.join("never.txt"));
        for index in 0..8 {
            file(&root.join(format!("entry-{index}.txt")));
        }

        let mut seen = Vec::new();
        let error = walk_directory::<(), TestError>(&root, (), |entry, ()| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if seen.len() == 2 {
                return Err(TestError::Refused(name));
            }
            seen.push(name);
            Ok(entry.file_type()?.is_dir().then_some(()))
        })
        .expect_err("refusal");

        assert_eq!(seen.len(), 2);
        assert!(
            !seen.iter().any(|name| name == "never.txt"),
            "a pending directory is not walked after a refusal"
        );
        match error {
            TestError::Refused(_) => {}
            TestError::Io(io) => panic!("unexpected io error: {io}"),
        }
    }
}
