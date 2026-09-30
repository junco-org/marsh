//! Path searching utilities.
//!
//! The public [`search_for_executable`] probes the filesystem without reporting anything. The
//! shell's own searches use the crate-internal observed variants, which resolve relative
//! search directories against the shell's working directory and report every probe to the
//! execution observer as a host filesystem access.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

use crate::sys;
use crate::{extensions::ExecutionObserver, hostfs};

/// Encapsulates the result of a path search.
pub struct ExecutablePathSearch<PI, N> {
    paths: VecDeque<PI>,
    filename: N,
}

impl<PI, N> Iterator for ExecutablePathSearch<PI, N>
where
    PI: AsRef<Path>,
    N: AsRef<Path>,
{
    type Item = PathBuf;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(path) = self.paths.pop_front() {
            let path = PathBuf::from(path.as_ref()).join(self.filename.as_ref());
            // Skip directories outright, then ask the platform to resolve
            // the path to an actual executable file (which, on Windows, may
            // involve appending a PATHEXT extension). The helper takes
            // ownership so Unix — where no resolution is needed — can return
            // the path unchanged without allocating.
            if path.is_dir() {
                continue;
            }
            if let Some(resolved) = sys::fs::resolve_executable(path) {
                return Some(resolved);
            }
        }
        None
    }
}

/// An executable search whose probes are reported to an execution observer.
pub(crate) struct ObservedExecutablePathSearch<'o, O, PI, N> {
    observer: &'o O,
    working_dir: &'o Path,
    paths: VecDeque<PI>,
    filename: N,
}

impl<O, PI, N> Iterator for ObservedExecutablePathSearch<'_, O, PI, N>
where
    O: ExecutionObserver,
    PI: AsRef<Path>,
    N: AsRef<Path>,
{
    type Item = PathBuf;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(path) = self.paths.pop_front() {
            let path = PathBuf::from(path.as_ref()).join(self.filename.as_ref());
            // Probe the absolute form (a relative search directory is relative to the shell's
            // working directory), reporting each probe; yield the candidate as found. Skip
            // directories outright, then check that the path is executable.
            let probed = self.working_dir.join(&path);
            if hostfs::is_dir(self.observer, &probed) {
                continue;
            }
            if hostfs::executable(self.observer, &probed) {
                return Some(path);
            }
        }
        None
    }
}

pub(crate) struct ExecutablePathPrefixSearch<'o, O, PI> {
    observer: &'o O,
    working_dir: &'o Path,
    paths: VecDeque<PI>,
    queued_items: VecDeque<PathBuf>,
    filename_prefix: String,
    case_insensitive: bool,
}

impl<O, PI> Iterator for ExecutablePathPrefixSearch<'_, O, PI>
where
    O: ExecutionObserver,
    PI: AsRef<Path>,
{
    type Item = PathBuf;

    fn next(&mut self) -> Option<Self::Item> {
        // If we already found some items and queued them, then yield one now.
        if let Some(item) = self.queued_items.pop_front() {
            return Some(item);
        }

        while let Some(path) = self.paths.pop_front() {
            let path = PathBuf::from(path.as_ref());
            // As above: enumerate and probe the absolute form, yield entries as found.
            let probed_dir = self.working_dir.join(&path);

            if let Ok(readdir) = hostfs::read_dir(self.observer, &probed_dir) {
                for entry in readdir.flatten() {
                    if let Ok(mut filename) = entry.file_name().into_string() {
                        if self.case_insensitive {
                            filename = filename.to_ascii_lowercase();
                        }

                        if !filename.starts_with(&self.filename_prefix) {
                            continue;
                        }
                    }

                    if let Ok(file_type) = entry.file_type()
                        && (file_type.is_file() || file_type.is_symlink())
                        && hostfs::executable(self.observer, &entry.path())
                    {
                        self.queued_items.push_back(path.join(entry.file_name()));
                    }
                }
            }
            if let Some(item) = self.queued_items.pop_front() {
                return Some(item);
            }
        }

        None
    }
}

/// Search for the given executable name in the provided paths.
///
/// Probes are made as given (relative paths against the process's working directory) and are
/// not reported to any execution observer.
///
/// # Arguments
///
/// * `paths` - An iterator over the paths to search.
/// * `filename` - The name of the executable file to search for.
pub fn search_for_executable<P, PI, N>(paths: P, filename: N) -> ExecutablePathSearch<PI, N>
where
    P: Iterator<Item = PI>,
    PI: AsRef<Path>,
    N: AsRef<Path>,
{
    ExecutablePathSearch {
        paths: paths.collect(),
        filename,
    }
}

/// Search for the given executable name in the provided paths, reporting each probe to
/// `observer` as a host filesystem access.
///
/// # Arguments
///
/// * `observer` - The execution observer to report probes to.
/// * `working_dir` - The directory relative search paths are resolved against.
/// * `paths` - An iterator over the paths to search.
/// * `filename` - The name of the executable file to search for.
pub(crate) fn search_for_executable_observed<'o, O, P, PI, N>(
    observer: &'o O,
    working_dir: &'o Path,
    paths: P,
    filename: N,
) -> ObservedExecutablePathSearch<'o, O, PI, N>
where
    O: ExecutionObserver,
    P: Iterator<Item = PI>,
    PI: AsRef<Path>,
    N: AsRef<Path>,
{
    ObservedExecutablePathSearch {
        observer,
        working_dir,
        paths: paths.collect(),
        filename,
    }
}

pub(crate) fn search_for_executable_with_prefix<'o, O, P, PI>(
    observer: &'o O,
    working_dir: &'o Path,
    paths: P,
    filename_prefix: &str,
    case_insensitive: bool,
) -> ExecutablePathPrefixSearch<'o, O, PI>
where
    O: ExecutionObserver,
    P: Iterator<Item = PI>,
    PI: AsRef<Path>,
{
    let stored_prefix = if case_insensitive {
        filename_prefix.to_ascii_lowercase()
    } else {
        filename_prefix.into()
    };

    ExecutablePathPrefixSearch {
        observer,
        working_dir,
        paths: paths.collect(),
        queued_items: VecDeque::new(),
        filename_prefix: stored_prefix,
        case_insensitive,
    }
}
