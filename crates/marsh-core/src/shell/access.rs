//! What a traced syscall says a command did to a file.
//!
//! The tracer reports calls; this turns them into the two sets a boundary is decided from — the
//! seed-relative paths a command *read*, which is what another principal's publication can
//! invalidate, and the ones it *wrote*, which is what its own publication is allowed to carry.
//!
//! The rule is the syscall, never the command line. `> p` is an `openat` with `O_TRUNC`, so it is a
//! write and not a read; `>> p` keeps what was there, so it is both. A command that opened a file
//! and never read a byte still declares the dependency, because the open is where the kernel
//! resolved the name and the read is not traced at all. That costs an occasional extra evaluation
//! and never misses a real dependency, which is the direction this has to err in.
//!
//! # What the descriptor decoration does
//!
//! `strace -y` prints the path behind every descriptor argument — `3</work/a.txt>`,
//! `AT_FDCWD</work/src>` — resolved by the kernel at the moment of the call. That is the whole
//! reason the system tracer is used, and it is why there is no descriptor table here: a duplicated,
//! inherited or long-since-opened descriptor arrives already resolved, and a second table
//! reconstructing the same answer from `dup`/`close` traffic could only ever disagree with it.
//!
//! What decoration cannot supply is an *address*: `mprotect` and `munmap` name a mapping, not a
//! file. Those are what [`Access::mappings`] exists for.
//!
//! # What is not observed
//!
//! Directory *listings*. `getdents64` takes no path and is not traced, so a command whose answer
//! depends on which names a directory contains — a glob — declares a dependency on the directory
//! it opened but not on its contents list. Writes outside the snapshot are real and are ignored:
//! the gate decides what enters a seed, it does not sandbox a command.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use marsh_instrument::{Call, TraceLine, parse_quoted, split_args};

use super::builtins::gitcmd;

/// What one syscall did, in seed-relative paths.
///
/// Empty for the overwhelming majority of calls: a command's own library loading, its pipes and
/// its terminal all lie outside the snapshot and name no resource.
#[derive(Debug, Default)]
pub(crate) struct Effects {
    /// Paths whose content or existence the call observed.
    pub(crate) reads: Vec<PathBuf>,
    /// Paths the call changed.
    pub(crate) writes: Vec<PathBuf>,
    /// Subtrees whose *contents* the call depended on.
    pub(crate) recursive_reads: Vec<PathBuf>,
    /// Subtrees the call restructured, so anything under them may have moved.
    pub(crate) recursive_writes: Vec<PathBuf>,
}

impl Effects {
    /// Whether the call touched nothing inside the snapshot.
    pub(crate) const fn is_empty(&self) -> bool {
        self.reads.is_empty()
            && self.writes.is_empty()
            && self.recursive_reads.is_empty()
            && self.recursive_writes.is_empty()
    }
}

/// One file-backed mapping, so a later `mprotect` or `munmap` can name what it covers.
struct Mapping {
    /// First address of the mapping.
    start: u64,
    /// One past its last address.
    end: u64,
    /// The file behind it, absolute.
    path: PathBuf,
    /// Whether writes through it reach that file.
    shared: bool,
}

/// The classifier's memory: where each thread resolves relative paths from, and what is mapped.
#[derive(Default)]
pub(crate) struct Access {
    /// Thread id → logical working directory.
    ///
    /// Only a fallback. Every `*at` syscall carries `AT_FDCWD</abs/cwd>` under `-y`, so this is
    /// consulted for the handful of calls that take a bare path — `truncate` is the common one —
    /// and for a thread whose `chdir` the tracer saw.
    cwds: HashMap<u32, PathBuf>,
    /// File-backed mappings, in creation order.
    mappings: Vec<Mapping>,
}

/// A path argument, as far as the tracer's printing lets it be read.
enum Decoded {
    /// A usable path.
    Path(PathBuf),
    /// The argument is not a string at all: a flag, a number, a struct.
    NotAPath,
    /// A string the tracer truncated. The command touched *something* and the evidence does not
    /// say what, which is an incomplete record rather than an absent access.
    Unreadable,
}

impl Access {
    /// Classifies one line, in `root`'s terms.
    ///
    /// # Errors
    ///
    /// Fails when a call that names a path printed one this decoder cannot read: the command
    /// touched something and the evidence does not say what, and a boundary built on that would
    /// silently omit a footprint.
    pub(crate) fn observe(&mut self, line: &TraceLine, root: &Path) -> Result<Effects, String> {
        let Call::Syscall {
            name,
            args,
            ret,
            ret_path,
        } = &line.call
        else {
            self.cwds.remove(&line.tid);
            return Ok(Effects::default());
        };
        let args = split_args(args);
        let succeeded = *ret >= 0;
        if self.bookkeep(line.tid, name, &args, *ret, root) {
            return Ok(Effects::default());
        }
        let mut effects = Effects::default();
        self.names(&mut effects, line, name, &args, succeeded, ret_path.as_deref(), root)?;
        self.memory(&mut effects, name, &args, *ret, succeeded, root);
        Ok(effects)
    }

    /// The effects of every call that names a path or a descriptor.
    ///
    /// # Errors
    ///
    /// Fails when such a call printed a path this decoder cannot read.
    #[allow(
        clippy::too_many_arguments,
        reason = "every one of these is a separate, already-decoded field of the trace line; \
                  bundling them into a struct would name the argument list and nothing else"
    )]
    fn names(
        &self,
        effects: &mut Effects,
        line: &TraceLine,
        name: &str,
        args: &[&str],
        succeeded: bool,
        ret_path: Option<&str>,
        root: &Path,
    ) -> Result<(), String> {
        match name {
            // Opens: the flags are the classification, and nothing else is.
            "open" => {
                let path = self.at(None, args.first().copied(), line.tid, root, ret_path)?;
                open_effects(effects, path, args.get(1).copied(), succeeded);
            }
            "openat" => {
                let path =
                    self.at(args.first().copied(), args.get(1).copied(), line.tid, root, ret_path)?;
                open_effects(effects, path, args.get(2).copied(), succeeded);
            }
            "openat2" => {
                let path =
                    self.at(args.first().copied(), args.get(1).copied(), line.tid, root, ret_path)?;
                open_effects(effects, path, args.get(2).copied(), succeeded);
            }
            "creat" => {
                let path = self.at(None, args.first().copied(), line.tid, root, ret_path)?;
                push(&mut effects.writes, path.filter(|_| succeeded));
            }

            // Metadata and existence probes. A failure is evidence too: a command that learned a
            // path does not exist depends on it not existing.
            "stat" | "lstat" | "statx" | "access" | "readlink" | "truncate" | "chmod"
            | "chown" | "lchown" | "utimes" | "statfs" | "getxattr" | "lgetxattr"
            | "listxattr" | "llistxattr" | "setxattr" | "lsetxattr" | "removexattr"
            | "lremovexattr" => {
                let path = self.at(None, args.first().copied(), line.tid, root, ret_path)?;
                probe_effects(effects, name, args, path, succeeded);
            }
            "newfstatat" | "fstatat64" | "faccessat" | "faccessat2" | "readlinkat"
            | "fchmodat" | "fchmodat2" | "fchownat" | "utimensat" | "futimesat" | "statx_at" => {
                let path =
                    self.at(args.first().copied(), args.get(1).copied(), line.tid, root, ret_path)?;
                probe_effects(effects, name, args, path, succeeded);
            }
            // Descriptor-based metadata changes: `-y` already resolved which file they name.
            "ftruncate" | "fchmod" | "fchown" | "fsetxattr" | "fremovexattr" if succeeded => {
                let path = args
                    .first()
                    .copied()
                    .and_then(decorated)
                    .and_then(|path| relative(root, &path));
                // Truncating to nothing keeps nothing of what was there; every other change to a
                // file the caller already holds open leaves the rest of it in place.
                if !(name == "ftruncate" && truncates_to_empty(args.get(1).copied())) {
                    push(&mut effects.reads, path.clone());
                }
                push(&mut effects.writes, path);
            }

            _ => return self.structure(effects, line, name, args, succeeded, root),
        }
        Ok(())
    }

    /// The effects of every call that changes the shape of the tree rather than a file's content.
    ///
    /// Split from [`Self::names`] only for length: the two are one table, read top to bottom, and
    /// a call reaches this one exactly when the first did not claim it.
    ///
    /// # Errors
    ///
    /// Fails when such a call printed a path this decoder cannot read.
    fn structure(
        &self,
        effects: &mut Effects,
        line: &TraceLine,
        name: &str,
        args: &[&str],
        succeeded: bool,
        root: &Path,
    ) -> Result<(), String> {
        match name {
            // Removals and creations restructure the tree they are in.
            "unlink" | "rmdir" => {
                let path = self.at(None, args.first().copied(), line.tid, root, None)?;
                if succeeded {
                    push(&mut effects.writes, path.clone());
                    if name == "rmdir" {
                        push(&mut effects.recursive_writes, path);
                    }
                } else {
                    push(&mut effects.reads, path);
                }
            }
            "unlinkat" => {
                let path =
                    self.at(args.first().copied(), args.get(1).copied(), line.tid, root, None)?;
                if !succeeded {
                    push(&mut effects.reads, path);
                } else {
                    if args.get(2).is_some_and(|flags| flags.contains("AT_REMOVEDIR")) {
                        push(&mut effects.recursive_writes, path.clone());
                    }
                    push(&mut effects.writes, path);
                }
            }
            "mkdir" => {
                let path = self.at(None, args.first().copied(), line.tid, root, None)?;
                structural(effects, path, succeeded);
            }
            "mkdirat" => {
                let path =
                    self.at(args.first().copied(), args.get(1).copied(), line.tid, root, None)?;
                structural(effects, path, succeeded);
            }

            // A rename reads what it moves and writes both ends; either may be a whole subtree.
            "rename" => {
                let from = self.at(None, args.first().copied(), line.tid, root, None)?;
                let to = self.at(None, args.get(1).copied(), line.tid, root, None)?;
                rename_effects(effects, from, to, succeeded);
            }
            "renameat" | "renameat2" => {
                let from =
                    self.at(args.first().copied(), args.get(1).copied(), line.tid, root, None)?;
                let to =
                    self.at(args.get(2).copied(), args.get(3).copied(), line.tid, root, None)?;
                rename_effects(effects, from, to, succeeded);
            }
            // A hard link reads its source and writes only the new name.
            "link" => {
                let from = self.at(None, args.first().copied(), line.tid, root, None)?;
                let to = self.at(None, args.get(1).copied(), line.tid, root, None)?;
                push(&mut effects.reads, from);
                push(&mut effects.writes, to.filter(|_| succeeded));
            }
            "linkat" => {
                let from =
                    self.at(args.first().copied(), args.get(1).copied(), line.tid, root, None)?;
                let to =
                    self.at(args.get(2).copied(), args.get(3).copied(), line.tid, root, None)?;
                push(&mut effects.reads, from);
                push(&mut effects.writes, to.filter(|_| succeeded));
            }
            // A symlink's target is text, not an access: only the link itself is written.
            "symlink" if succeeded => {
                let to = self.at(None, args.get(1).copied(), line.tid, root, None)?;
                push(&mut effects.writes, to);
            }
            "symlinkat" if succeeded => {
                let to =
                    self.at(args.get(1).copied(), args.get(2).copied(), line.tid, root, None)?;
                push(&mut effects.writes, to);
            }

            "execve" => {
                let path = self.at(None, args.first().copied(), line.tid, root, None)?;
                push(&mut effects.reads, path);
            }
            "execveat" => {
                let path =
                    self.at(args.first().copied(), args.get(1).copied(), line.tid, root, None)?;
                push(&mut effects.reads, path);
            }

            _ => {}
        }
        Ok(())
    }

    /// The effects of the mapping calls, which name an address rather than a path.
    fn memory(
        &mut self,
        effects: &mut Effects,
        name: &str,
        args: &[&str],
        ret: i64,
        succeeded: bool,
        root: &Path,
    ) {
        match name {
            "mmap" | "mmap2" => self.map(effects, args, ret, root),
            "mprotect" | "pkey_mprotect" if succeeded => self.protect(effects, args, root),
            "munmap" if succeeded => {
                if let Some(start) = address(args.first().copied()) {
                    self.mappings.retain(|mapping| mapping.start != start);
                }
            }
            _ => {}
        }
    }

    /// Tracks where a thread stands, and whether that is all this call was.
    ///
    /// Applies to every thread whatever else it is doing, and is deliberately separate from the
    /// classification below: a `chdir` names a path and touches no file, and a `clone` names none
    /// at all.
    fn bookkeep(&mut self, tid: u32, name: &str, args: &[&str], ret: i64, root: &Path) -> bool {
        let succeeded = ret >= 0;
        match name {
            "chdir" if succeeded => {
                if let Decoded::Path(path) = Self::decode(args.first().copied()) {
                    self.cwds.insert(tid, path);
                }
                true
            }
            "fchdir" if succeeded => {
                if let Some(path) = args.first().copied().and_then(decorated) {
                    self.cwds.insert(tid, path);
                }
                true
            }
            "clone" | "clone3" | "fork" | "vfork" if ret > 0 => {
                let inherited = self.cwd(tid, root);
                if let Ok(child) = u32::try_from(ret) {
                    self.cwds.insert(child, inherited);
                }
                true
            }
            "exit_group" | "exit" => {
                self.cwds.remove(&tid);
                true
            }
            _ => false,
        }
    }

    /// The directory `tid` resolves relative paths from.
    fn cwd(&self, tid: u32, root: &Path) -> PathBuf {
        self.cwds
            .get(&tid)
            .cloned()
            .unwrap_or_else(|| root.to_path_buf())
    }

    /// Resolves a `(dirfd, path)` argument pair to a seed-relative path inside `root`.
    ///
    /// The kernel's own answer — the `-y` decoration of the returned descriptor — wins when there
    /// is one, because it is the path the kernel actually walked. Otherwise the directory
    /// descriptor's decoration supplies the base, and the thread's tracked directory is the last
    /// resort.
    fn at(
        &self,
        dirfd: Option<&str>,
        path: Option<&str>,
        tid: u32,
        root: &Path,
        ret_path: Option<&str>,
    ) -> Result<Option<PathBuf>, String> {
        if let Some(resolved) = ret_path {
            return Ok(relative(root, Path::new(resolved)));
        }
        match Self::decode(path) {
            Decoded::NotAPath => Ok(None),
            Decoded::Unreadable => Err(format!(
                "a traced call printed a path this decoder cannot read: {}",
                path.unwrap_or_default()
            )),
            Decoded::Path(decoded) => {
                let base = dirfd
                    .and_then(decorated)
                    .unwrap_or_else(|| self.cwd(tid, root));
                Ok(relative(
                    root,
                    &gitcmd::resolve(&base, &decoded.to_string_lossy()),
                ))
            }
        }
    }

    /// Reads one path argument, without resolving it.
    fn decode(arg: Option<&str>) -> Decoded {
        let Some(arg) = arg.map(str::trim) else {
            return Decoded::NotAPath;
        };
        if !arg.starts_with('"') {
            return Decoded::NotAPath;
        }
        match parse_quoted(arg) {
            None => Decoded::Unreadable,
            Some(text) => Decoded::Path(PathBuf::from(text)),
        }
    }

    /// Records a file-backed mapping and the access establishing it is.
    fn map(&mut self, effects: &mut Effects, args: &[&str], ret: i64, root: &Path) {
        let Some(path) = args.get(4).copied().and_then(decorated) else {
            return;
        };
        let Ok(start) = u64::try_from(ret) else {
            return;
        };
        let prot = args.get(2).copied().unwrap_or("");
        let flags = args.get(3).copied().unwrap_or("");
        let shared = flags.contains("MAP_SHARED");
        let length = args
            .get(1)
            .and_then(|arg| arg.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if let Some(relative) = relative(root, &path) {
            if prot.contains("PROT_READ") || prot.contains("PROT_EXEC") {
                effects.reads.push(relative.clone());
            }
            if shared && prot.contains("PROT_WRITE") {
                effects.writes.push(relative);
            }
        }
        self.mappings.push(Mapping {
            start,
            end: start.saturating_add(length),
            path,
            shared,
        });
    }

    /// Reclassifies the mapping a protection change covers.
    fn protect(&self, effects: &mut Effects, args: &[&str], root: &Path) {
        let Some(start) = address(args.first().copied()) else {
            return;
        };
        let prot = args.get(2).copied().unwrap_or("");
        for mapping in &self.mappings {
            if start < mapping.start || start >= mapping.end {
                continue;
            }
            let Some(relative) = relative(root, &mapping.path) else {
                continue;
            };
            if prot.contains("PROT_READ") || prot.contains("PROT_EXEC") {
                effects.reads.push(relative.clone());
            }
            if mapping.shared && prot.contains("PROT_WRITE") {
                effects.writes.push(relative);
            }
        }
    }
}

/// Appends `path` when there is one.
fn push(sink: &mut Vec<PathBuf>, path: Option<PathBuf>) {
    if let Some(path) = path {
        sink.push(path);
    }
}

/// The effects of an open, from its flags alone.
///
/// A failed open is still an observation of whether the path was there. A truncating open and a
/// successful exclusive creation keep nothing, so neither depends on what was there before;
/// everything else that writes — append, read/write, an ordinary create over an existing file —
/// does.
fn open_effects(
    effects: &mut Effects,
    path: Option<PathBuf>,
    flags: Option<&str>,
    succeeded: bool,
) {
    let Some(path) = path else {
        return;
    };
    let flags = flags.unwrap_or("");
    if !succeeded {
        effects.reads.push(path);
        return;
    }
    if flags.contains("O_DIRECTORY") {
        effects.reads.push(path);
        return;
    }
    let writes = ["O_WRONLY", "O_RDWR", "O_CREAT", "O_TRUNC", "O_APPEND"]
        .iter()
        .any(|flag| flags.contains(flag));
    if !writes {
        effects.reads.push(path);
        return;
    }
    if flags.contains("O_TRUNC") || (flags.contains("O_CREAT") && flags.contains("O_EXCL")) {
        effects.writes.push(path);
        return;
    }
    effects.reads.push(path.clone());
    effects.writes.push(path);
}

/// The effects of a metadata call: always a read, and a write too when it changes something.
fn probe_effects(
    effects: &mut Effects,
    name: &str,
    args: &[&str],
    path: Option<PathBuf>,
    succeeded: bool,
) {
    let Some(path) = path else {
        return;
    };
    let mutates = matches!(
        name,
        "truncate"
            | "chmod"
            | "fchmodat"
            | "fchmodat2"
            | "chown"
            | "lchown"
            | "fchownat"
            | "utimes"
            | "utimensat"
            | "futimesat"
            | "setxattr"
            | "lsetxattr"
            | "removexattr"
            | "lremovexattr"
    );
    if !mutates || !succeeded {
        effects.reads.push(path);
        return;
    }
    if name == "truncate" && truncates_to_empty(args.get(1).copied()) {
        effects.writes.push(path);
        return;
    }
    effects.reads.push(path.clone());
    effects.writes.push(path);
}

/// The effects of creating a directory: a write, and a restructuring of what it now contains.
fn structural(effects: &mut Effects, path: Option<PathBuf>, succeeded: bool) {
    let Some(path) = path else {
        return;
    };
    if succeeded {
        effects.writes.push(path.clone());
        effects.recursive_writes.push(path);
    } else {
        effects.reads.push(path);
    }
}

/// The effects of a rename: its source is read and both of its ends are written.
///
/// Recursive at both ends, because either may be a directory and the tracer does not say which:
/// every name underneath moved with it.
fn rename_effects(
    effects: &mut Effects,
    from: Option<PathBuf>,
    to: Option<PathBuf>,
    succeeded: bool,
) {
    if let Some(from) = from {
        effects.reads.push(from.clone());
        effects.recursive_reads.push(from.clone());
        if succeeded {
            effects.writes.push(from.clone());
            effects.recursive_writes.push(from);
        }
    }
    if succeeded && let Some(to) = to {
        effects.writes.push(to.clone());
        effects.recursive_writes.push(to);
    }
}

/// Whether a length argument asks for an empty file, which keeps none of the old content.
fn truncates_to_empty(length: Option<&str>) -> bool {
    length.is_some_and(|arg| arg.trim() == "0")
}

/// Extracts the `-y` path decoration from a descriptor argument such as `AT_FDCWD</work>` or
/// `5</work/src>`.
fn decorated(arg: &str) -> Option<PathBuf> {
    let start = arg.find('<')?;
    let end = arg.rfind('>')?;
    let path = arg.get(start + 1..end)?;
    path.starts_with('/').then(|| PathBuf::from(path))
}

/// Parses a hexadecimal address argument.
fn address(arg: Option<&str>) -> Option<u64> {
    let arg = arg?.trim();
    u64::from_str_radix(arg.strip_prefix("0x")?, 16).ok()
}

/// The `root`-relative form of a path inside the snapshot, `.git/` included.
///
/// `.git/` is deliberately *not* excluded. A `git add` decides from the index and the refs, so
/// those reads are exactly the dependency that has to be checked, and the bytes it writes there
/// are exactly what its publication carries. Excluding them would let two shells stage different
/// files and lose one of the index entries.
fn relative(root: &Path, path: &Path) -> Option<PathBuf> {
    let segments = gitcmd::relative_segments(root, path)?;
    if segments.is_empty() {
        return None;
    }
    Some(segments.iter().collect())
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    /// The snapshot every fixture line resolves against.
    const ROOT: &str = "/work";

    /// Classifies one raw trace line the way the decoder would hand it over.
    fn effects(access: &mut Access, name: &str, args: &str, ret: i64, ret_path: Option<&str>) -> Effects {
        access
            .observe(
                &TraceLine {
                    tid: 7,
                    ts_us: 1,
                    call: Call::Syscall {
                        name: name.to_string(),
                        args: args.to_string(),
                        ret,
                        ret_path: ret_path.map(ToString::to_string),
                    },
                },
                Path::new(ROOT),
            )
            .expect("a readable line")
    }

    fn paths(paths: &[PathBuf]) -> Vec<&str> {
        paths.iter().filter_map(|path| path.to_str()).collect()
    }

    /// `>` keeps nothing of what was there, so it depends on nothing; `>>` keeps all of it.
    #[test]
    fn truncation_is_a_write_and_appending_is_both() {
        let mut access = Access::default();
        let truncating = effects(
            &mut access,
            "openat",
            "AT_FDCWD</work>, \"a.txt\", O_WRONLY|O_CREAT|O_TRUNC, 0666",
            3,
            Some("/work/a.txt"),
        );
        assert_eq!(paths(&truncating.writes), ["a.txt"]);
        assert!(truncating.reads.is_empty(), "a truncating write reads nothing");

        let appending = effects(
            &mut access,
            "openat",
            "AT_FDCWD</work>, \"a.txt\", O_WRONLY|O_CREAT|O_APPEND, 0666",
            3,
            Some("/work/a.txt"),
        );
        assert_eq!(paths(&appending.reads), ["a.txt"]);
        assert_eq!(paths(&appending.writes), ["a.txt"]);

        let updating = effects(
            &mut access,
            "openat",
            "AT_FDCWD</work>, \"a.txt\", O_RDWR",
            3,
            Some("/work/a.txt"),
        );
        assert_eq!(paths(&updating.reads), ["a.txt"]);
        assert_eq!(paths(&updating.writes), ["a.txt"]);

        let exclusive = effects(
            &mut access,
            "openat",
            "AT_FDCWD</work>, \"new.txt\", O_WRONLY|O_CREAT|O_EXCL, 0666",
            3,
            Some("/work/new.txt"),
        );
        assert_eq!(paths(&exclusive.writes), ["new.txt"]);
        assert!(exclusive.reads.is_empty());
    }

    /// A read that failed still observed something: that the path was not there.
    #[test]
    fn a_failed_open_is_a_read_and_never_a_write() {
        let mut access = Access::default();
        let missing = effects(
            &mut access,
            "openat",
            "AT_FDCWD</work>, \"gone.txt\", O_WRONLY|O_CREAT|O_TRUNC, 0666",
            -1,
            None,
        );
        assert_eq!(paths(&missing.reads), ["gone.txt"]);
        assert!(
            missing.writes.is_empty(),
            "a failed call changed nothing: {:?}",
            missing.writes
        );
    }

    /// Existence is a dependency: a command that branched on a file not being there has to be
    /// evaluated again once someone creates it.
    #[test]
    fn metadata_probes_are_reads() {
        let mut access = Access::default();
        for (name, args) in [
            ("newfstatat", "AT_FDCWD</work>, \"a.txt\", 0x7ffd, 0"),
            ("faccessat2", "AT_FDCWD</work>, \"a.txt\", R_OK, 0"),
            ("readlinkat", "AT_FDCWD</work>, \"a.txt\", 0x7ffd, 4096"),
        ] {
            let observed = effects(&mut access, name, args, -1, None);
            assert_eq!(paths(&observed.reads), ["a.txt"], "{name}");
            assert!(observed.writes.is_empty(), "{name}");
        }
    }

    /// Both ends of a rename move, and either may be a whole subtree.
    #[test]
    fn a_rename_reads_its_source_and_writes_both_ends() {
        let mut access = Access::default();
        let observed = effects(
            &mut access,
            "renameat2",
            "AT_FDCWD</work>, \"src\", AT_FDCWD</work>, \"dst\", RENAME_NOREPLACE",
            0,
            None,
        );
        assert_eq!(paths(&observed.reads), ["src"]);
        assert_eq!(paths(&observed.writes), ["src", "dst"]);
        assert_eq!(paths(&observed.recursive_writes), ["src", "dst"]);
    }

    /// A shared writable mapping reaches the file; a private one never does.
    #[test]
    fn file_backed_mappings_are_accesses() {
        let mut access = Access::default();
        let shared = effects(
            &mut access,
            "mmap",
            "NULL, 4096, PROT_READ|PROT_WRITE, MAP_SHARED, 5</work/db>, 0",
            0x7f00_0000,
            None,
        );
        assert_eq!(paths(&shared.reads), ["db"]);
        assert_eq!(paths(&shared.writes), ["db"]);

        let private = effects(
            &mut access,
            "mmap",
            "NULL, 4096, PROT_READ|PROT_WRITE, MAP_PRIVATE, 6</work/copy>, 0",
            0x7f10_0000,
            None,
        );
        assert_eq!(paths(&private.reads), ["copy"]);
        assert!(
            private.writes.is_empty(),
            "a private mapping's writes never reach the file"
        );

        // The protection change names an address, which only the recorded mapping resolves.
        let promoted = effects(
            &mut access,
            "mprotect",
            "0x7f000000, 4096, PROT_READ|PROT_WRITE",
            0,
            None,
        );
        assert_eq!(paths(&promoted.writes), ["db"]);

        effects(&mut access, "munmap", "0x7f000000, 4096", 0, None);
        let forgotten = effects(
            &mut access,
            "mprotect",
            "0x7f000000, 4096, PROT_READ|PROT_WRITE",
            0,
            None,
        );
        assert!(forgotten.writes.is_empty(), "an unmapped range names nothing");
    }

    /// Git's own state is a resource like any other: its reads are what a staging decision
    /// depended on, and its writes are what a publication has to carry.
    #[test]
    fn git_internals_are_tracked_paths() {
        let mut access = Access::default();
        let observed = effects(
            &mut access,
            "openat",
            "AT_FDCWD</work>, \".git/index\", O_RDONLY",
            3,
            Some("/work/.git/index"),
        );
        assert_eq!(paths(&observed.reads), [".git/index"]);
    }

    /// Anything the command did outside its own snapshot is real and is not the seed's business.
    #[test]
    fn paths_outside_the_snapshot_are_ignored() {
        let mut access = Access::default();
        let observed = effects(
            &mut access,
            "openat",
            "AT_FDCWD</usr/lib>, \"libc.so.6\", O_RDONLY|O_CLOEXEC",
            3,
            Some("/usr/lib/libc.so.6"),
        );
        assert!(observed.is_empty(), "the loader's own reads name no resource");
    }

    /// A relative path with no descriptor decoration resolves against the thread's directory, and
    /// a `chdir` is what moves it.
    #[test]
    fn a_bare_path_follows_the_threads_directory() {
        let mut access = Access::default();
        effects(&mut access, "chdir", "\"/work/src\"", 0, None);
        let observed = effects(&mut access, "truncate", "\"a.txt\", 0", 0, None);
        assert_eq!(paths(&observed.writes), ["src/a.txt"]);
    }

    /// A descriptor this classifier never saw opened still names its file.
    ///
    /// The shell opens a redirection and forks; the child inherits the descriptor and the open
    /// itself was another process's. `-y` prints the path the kernel resolved at the moment of the
    /// call, which is why there is no descriptor table here to get out of step with the kernel.
    #[test]
    fn an_inherited_descriptor_still_names_its_file() {
        let mut access = Access::default();
        let truncated = effects(&mut access, "ftruncate", "1</work/out.txt>, 0", 0, None);
        assert_eq!(paths(&truncated.writes), ["out.txt"]);
        assert!(
            truncated.reads.is_empty(),
            "truncating to empty keeps nothing of what was there"
        );

        let shortened = effects(&mut access, "ftruncate", "1</work/out.txt>, 4", 0, None);
        assert_eq!(paths(&shortened.reads), ["out.txt"]);
        assert_eq!(paths(&shortened.writes), ["out.txt"]);

        let mode = effects(&mut access, "fchmod", "7</work/script.sh>, 0755", 0, None);
        assert_eq!(paths(&mode.reads), ["script.sh"]);
        assert_eq!(paths(&mode.writes), ["script.sh"]);
    }

    /// A path the tracer had to cut off means the command touched something the evidence cannot
    /// name. That is an incomplete record, not an absent access.
    #[test]
    fn an_unreadable_path_is_an_error() {
        let mut access = Access::default();
        let refused = access.observe(
            &TraceLine {
                tid: 7,
                ts_us: 1,
                call: Call::Syscall {
                    name: "unlinkat".to_string(),
                    args: "AT_FDCWD</work>, \"very-long-na\"..., 0".to_string(),
                    ret: 0,
                    ret_path: None,
                },
            },
            Path::new(ROOT),
        );
        assert!(refused.is_err(), "{refused:?}");
    }

    /// A child process resolves relative paths where its parent stood.
    #[test]
    fn a_fork_inherits_the_working_directory() {
        let mut access = Access::default();
        effects(&mut access, "chdir", "\"/work/src\"", 0, None);
        effects(&mut access, "clone", "child_stack=NULL, flags=SIGCHLD", 99, None);
        let observed = access
            .observe(
                &TraceLine {
                    tid: 99,
                    ts_us: 2,
                    call: Call::Syscall {
                        name: "truncate".to_string(),
                        args: "\"b.txt\", 0".to_string(),
                        ret: 0,
                        ret_path: None,
                    },
                },
                Path::new(ROOT),
            )
            .expect("a readable line");
        assert_eq!(paths(&observed.writes), ["src/b.txt"]);
    }
}
