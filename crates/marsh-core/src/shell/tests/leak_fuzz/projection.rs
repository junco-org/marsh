//! What the untraced world can observe of one run, for the differential leak oracle.
//!
//! An attack runs twice with independent canaries. Everything an untraced observer could read
//! afterwards is captured as normalised facts; any fact present in one run and not the other
//! depends on the secret, so it is a leak whatever its encoding or channel. Run-specific noise
//! (the fixture's path, the sinks' ports, pids, inode numbers, times and client ports) is
//! normalised or never recorded, so two runs of a blocked attack project equally.
//!
//! The projection covers the sinks' bytes, files in the directories every user shares, listening
//! and bound sockets, bound UNIX socket names, process names and arguments, visible keys and
//! System V IPC objects. Shared state is recorded as the change between a capture before the
//! steps and one after them, so what earlier cases left behind is not attributed to this one, and
//! what a run creates in the shared directories is removed afterwards, so its twin starts alike.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use super::sink::Hit;

/// Directories any local user can read and governed commands might write: their files are
/// visible to the untraced world.
pub(super) const SHARED_DIRS: &[&str] = &["/tmp", "/var/tmp", "/dev/shm", "/dev/mqueue"];

/// Bytes of a file read for its fact; longer files are recorded by length and this prefix's hash.
const FILE_LIMIT: usize = 1 << 20;
/// Bytes shown verbatim in a fact, enough to recognise a payload in a report.
const HEAD: usize = 48;

/// What one run may observe, and how its run-specific names are made comparable.
pub(super) struct Scope {
    /// Directories walked for files.
    pub(super) dirs: Vec<PathBuf>,
    /// Governed state beneath them, never walked: its commands may hold the secret there.
    pub(super) governed: Vec<PathBuf>,
    /// Run-specific text and the stable name it is recorded as.
    pub(super) names: Vec<(String, &'static str)>,
}

impl Scope {
    fn normalise(&self, text: &str) -> String {
        self.names
            .iter()
            .fold(text.to_owned(), |text, (from, to)| text.replace(from.as_str(), to))
    }

    fn normalise_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        self.names.iter().fold(bytes.to_vec(), |bytes, (from, to)| {
            replace(&bytes, from.as_bytes(), to.as_bytes())
        })
    }

    /// Length, hash and head of `bytes`, after normalisation.
    fn describe(&self, bytes: &[u8]) -> String {
        let bytes = self.normalise_bytes(bytes);
        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        let head = bytes[..bytes.len().min(HEAD)].escape_ascii();
        format!("len={} hash={:016x} head={head}", bytes.len(), hasher.finish())
    }
}

fn replace(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some(at) = rest.windows(from.len()).position(|window| window == from) {
        out.extend_from_slice(&rest[..at]);
        out.extend_from_slice(to);
        rest = &rest[at + from.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// Shared state as the untraced world sees it at one moment: each object's normalised key and
/// what is observable of it.
#[derive(Debug, Default)]
pub(super) struct State {
    facts: BTreeMap<String, String>,
    /// Every path found in the shared directories, as it is on disk.
    files: BTreeSet<PathBuf>,
}

impl State {
    /// Records `key`, counting repeats: several identical objects are each observable.
    fn count(&mut self, key: String) {
        let count = self.facts.entry(key).or_insert_with(|| "0".to_owned());
        *count = (count.parse::<usize>().unwrap_or(0) + 1).to_string();
    }
}

/// Removes what a run created in the shared directories, so neither its twin nor a later case
/// starts from what it left behind: a file its twin would create again shows as no change.
pub(super) fn remove_created(before: &State, after: &State) {
    // Reversed, so a directory's entries go before the directory.
    for path in after.files.iter().rev().filter(|path| !before.files.contains(*path)) {
        let removed = match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
            Ok(_) => std::fs::remove_file(path),
            Err(error) => Err(error),
        };
        if let Err(error) = removed
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("leak_corpus: cannot remove {}: {error}", path.display());
        }
    }
}

/// Captures every source of shared state in `scope`.
pub(super) fn capture(scope: &Scope) -> State {
    let mut state = State::default();
    for dir in &scope.dirs {
        walk(scope, dir, &mut state);
    }
    sockets(scope, &mut state);
    processes(scope, &mut state);
    keys(scope, &mut state);
    sysvipc(scope, &mut state);
    state
}

fn walk(scope: &Scope, dir: &Path, state: &mut State) {
    if scope.governed.iter().any(|governed| dir.starts_with(governed)) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if scope.governed.iter().any(|governed| path.starts_with(governed)) {
            continue;
        }
        let key = format!("file {}", scope.normalise(&path.to_string_lossy()));
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let owner = format!(
            "mode={:o} uid={} gid={}",
            metadata.mode() & 0o7777,
            metadata.uid(),
            metadata.gid()
        );
        let kind = metadata.file_type();
        let value = if kind.is_dir() {
            walk(scope, &path, state);
            format!("dir {owner}")
        } else if kind.is_symlink() {
            let target = std::fs::read_link(&path).unwrap_or_default();
            format!("symlink {owner} -> {}", scope.normalise(&target.to_string_lossy()))
        } else if kind.is_file() {
            match read_prefix(&path) {
                Ok(bytes) => format!(
                    "file {owner} size={} {}",
                    metadata.len(),
                    scope.describe(&bytes)
                ),
                Err(_) => format!("file {owner} size={} unreadable", metadata.len()),
            }
        } else {
            format!("special {owner}")
        };
        state.facts.insert(key, value);
        state.files.insert(path);
    }
}

fn read_prefix(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(FILE_LIMIT as u64)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Listening TCP and bound UDP sockets by local address, and bound UNIX socket names. Client
/// ports and inode numbers differ between runs and carry no chosen value.
fn sockets(scope: &Scope, state: &mut State) {
    for (protocol, wanted) in [("tcp", "0A"), ("tcp6", "0A"), ("udp", "07"), ("udp6", "07")] {
        let Ok(table) = std::fs::read_to_string(format!("/proc/net/{protocol}")) else {
            continue;
        };
        for line in table.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (Some(local), Some(st)) = (fields.get(1), fields.get(3)) else {
                continue;
            };
            if *st != wanted {
                continue;
            }
            let Some((address, port)) = local.split_once(':') else {
                continue;
            };
            let port = u16::from_str_radix(port, 16).map_or_else(|_| port.to_owned(), |port| port.to_string());
            state.count(scope.normalise(&format!("socket {protocol} {address}:{port}")));
        }
    }
    if let Ok(table) = std::fs::read_to_string("/proc/net/unix") {
        for line in table.lines().skip(1) {
            if let Some(name) = line.split_whitespace().nth(7) {
                state.count(scope.normalise(&format!("socket unix {name}")));
            }
        }
    }
}

/// Every other process's name and arguments, which any user can read. A child not yet past
/// `exec` still shows this binary's arguments, which are no fact about the case.
fn processes(scope: &Scope, state: &mut State) {
    let own = std::process::id().to_string();
    let own_cmdline = std::fs::read("/proc/self/cmdline").unwrap_or_default();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().filter(|pid| pid.bytes().all(|byte| byte.is_ascii_digit())) else {
            continue;
        };
        if pid == own {
            continue;
        }
        let path = entry.path();
        let Ok(cmdline) = std::fs::read(path.join("cmdline")) else {
            continue;
        };
        // Kernel threads have no arguments; their names are the kernel's.
        if cmdline.is_empty() || cmdline == own_cmdline {
            continue;
        }
        let comm = std::fs::read_to_string(path.join("comm")).unwrap_or_default();
        let arguments = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        state.count(scope.normalise(&format!(
            "process {} {}",
            comm.trim_end(),
            arguments.trim_end()
        )));
    }
}

/// Keys the observer may view, without their serials, usage counts and expiry.
fn keys(scope: &Scope, state: &mut State) {
    let Ok(table) = std::fs::read_to_string("/proc/keys") else {
        return;
    };
    for line in table.lines() {
        // serial flags usage timeout perm uid gid type description...
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 {
            continue;
        }
        state.count(scope.normalise(&format!("key {}", fields[4..].join(" "))));
    }
}

/// System V message queues, semaphore sets and shared memory segments: their keys, owners and
/// sizes, without identifiers, pids and times.
fn sysvipc(scope: &Scope, state: &mut State) {
    const KEPT: &[&str] = &["key", "perms", "cbytes", "qnum", "size", "nsems", "uid", "gid"];
    for kind in ["msg", "sem", "shm"] {
        let Ok(table) = std::fs::read_to_string(format!("/proc/sysvipc/{kind}")) else {
            continue;
        };
        let mut lines = table.lines();
        let header: Vec<&str> = lines.next().unwrap_or_default().split_whitespace().collect();
        for line in lines {
            let kept: Vec<String> = header
                .iter()
                .zip(line.split_whitespace())
                .filter(|(name, _)| KEPT.contains(name))
                .map(|(name, value)| format!("{name}={value}"))
                .collect();
            state.count(scope.normalise(&format!("sysvipc {kind} {}", kept.join(" "))));
        }
    }
}

/// One run's projection: what each sink received, and how shared state changed from `before`
/// to `after`. Sorted, so runs compare as multisets.
pub(super) fn project(scope: &Scope, hits: &[Hit], before: &State, after: &State) -> Vec<String> {
    let mut facts: Vec<String> = hits
        .iter()
        .map(|hit| {
            format!(
                "received {} from {}: {}",
                hit.listener,
                hit.from.ip(),
                scope.describe(&hit.bytes)
            )
        })
        .collect();
    for (key, value) in &after.facts {
        if before.facts.get(key) != Some(value) {
            facts.push(format!("{key}: {value}"));
        }
    }
    for key in before.facts.keys().filter(|key| !after.facts.contains_key(*key)) {
        facts.push(format!("{key}: removed"));
    }
    facts.sort();
    facts
}

/// Facts one projection has and the other lacks, counting repeats; empty when the untraced
/// world cannot tell the two runs apart.
pub(super) fn differences(first: &[String], second: &[String]) -> Vec<String> {
    let mut counts: BTreeMap<&str, isize> = BTreeMap::new();
    for fact in first {
        *counts.entry(fact).or_default() += 1;
    }
    for fact in second {
        *counts.entry(fact).or_default() -= 1;
    }
    counts
        .into_iter()
        .filter(|(_, count)| *count != 0)
        .map(|(fact, count)| {
            let run = if count > 0 { "first" } else { "second" };
            format!("only in the {run} run: {fact}")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One run writing `written` into a shared directory and `governed` into its own state.
    fn run(written: Option<&[u8]>, governed: &[u8], port: u16) -> Vec<String> {
        let shared = tempfile::tempdir().unwrap();
        // Governed state may sit beneath a shared directory, as fixtures do under /var/tmp.
        let fixture = shared.path().join("fixture");
        std::fs::create_dir(&fixture).unwrap();
        let scope = Scope {
            dirs: vec![shared.path().to_path_buf()],
            governed: vec![fixture.clone()],
            names: vec![
                (shared.path().to_string_lossy().into_owned(), "{SHARED}"),
                (port.to_string(), "{SINK_PORT}"),
            ],
        };
        let files = |scope: &Scope| {
            let mut state = State::default();
            walk(scope, &scope.dirs[0], &mut state);
            state
        };
        let before = files(&scope);
        std::fs::write(fixture.join("secret"), governed).unwrap();
        std::fs::write(shared.path().join("log"), format!("connected to {port}")).unwrap();
        if let Some(written) = written {
            std::fs::write(shared.path().join("stage"), written).unwrap();
        }
        let after = files(&scope);
        project(&scope, &[], &before, &after)
    }

    #[test]
    fn runs_differ_only_where_the_untraced_world_sees_the_secret() {
        // Governed state, run-specific paths and ports, and equal writes are no difference.
        let same = differences(
            &run(Some(b"same"), b"secret one", 40001),
            &run(Some(b"same"), b"secret two", 40002),
        );
        assert_eq!(same, Vec::<String>::new());
        // A secret-dependent file outside governed state is, whatever it encodes.
        let leaked = differences(
            &run(Some(b"x1"), b"one", 40001),
            &run(Some(b"x2"), b"two", 40002),
        );
        assert_eq!(leaked.len(), 2, "{leaked:?}");
        assert!(
            leaked.iter().all(|fact| fact.contains("file {SHARED}/stage")),
            "{leaked:?}"
        );
        // So is whether it exists at all.
        let existence = differences(&run(Some(b"x"), b"one", 40001), &run(None, b"two", 40002));
        assert_eq!(existence.len(), 1, "{existence:?}");
    }

    #[test]
    fn repeated_facts_are_observable() {
        let facts = |count: usize| vec!["received tcp".to_owned(); count];
        assert_eq!(differences(&facts(2), &facts(1)).len(), 1);
        assert!(differences(&facts(2), &facts(2)).is_empty());
    }

    #[test]
    fn a_run_leaves_shared_directories_as_it_found_them() {
        let shared = tempfile::tempdir().unwrap();
        let scope = Scope {
            dirs: vec![shared.path().to_path_buf()],
            governed: Vec::new(),
            names: Vec::new(),
        };
        let files = || {
            let mut state = State::default();
            walk(&scope, &scope.dirs[0], &mut state);
            state
        };
        std::fs::write(shared.path().join("kept"), b"x").unwrap();
        let before = files();
        std::fs::create_dir_all(shared.path().join("made/inner")).unwrap();
        std::fs::write(shared.path().join("made/inner/stage"), b"secret").unwrap();
        std::fs::write(shared.path().join("relay.sock"), b"").unwrap();
        remove_created(&before, &files());
        // A twin creating the same files again would otherwise see no change.
        assert_eq!(files().files, before.files);
        assert!(shared.path().join("kept").exists());
    }
}
