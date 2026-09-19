//! Recording builtin invocations.
//!
//! Builtins run inside the shell process, so `strace` sees their syscalls but never the invocation
//! itself: `git add foo` executed as a builtin looks like a few reads and writes under `.git/`. This
//! module supplies the other half of the instrumentation — a [`crate::BuiltinHook`] that
//! records every builtin lifecycle in memory, plus the record vocabulary a consumer merges with
//! a syscall trace.
//!
//! [`SpawnRecord`] is the spawner-level counterpart: what a
//! `brush_core::extensions::ExternalCommandSpawner` sees, which is every *external* command the
//! shell hands it — never a builtin or a function — and with no exit code, because the shell, not
//! the spawner, awaits the child. It is stamped from the same clock and the same tid namespace as
//! [`BuiltinRecord`], so the two streams sort together.
//!
//! There is exactly one hook implementation, and it writes nothing: tests install it and assert on
//! [`RecordingHook::records`], and the worker dumps the same records once, at exit, to the path
//! its caller named on its command line. A hook that wrote to a file would make those two consumers
//! different code paths.
//!
//! Records carry `CLOCK_REALTIME` microseconds and the emitting thread id — the same clock domain
//! and tid namespace `strace -ttt -f` stamps its lines with, which is what makes merging the two
//! streams into one ordered sequence well defined.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// One builtin lifecycle record.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "k")]
pub enum BuiltinRecord {
    /// A builtin began executing.
    #[serde(rename = "b")]
    Begin {
        /// Invocation id, unique within one recorder; echoed by the matching [`Self::End`].
        id: u64,
        /// `CLOCK_REALTIME` microseconds at the call.
        ts: u64,
        /// Thread that executed the builtin.
        tid: u32,
        /// Registered builtin name. Every git subcommand is recorded as `git`; `argv[1]`
        /// carries the subcommand.
        builtin: String,
        /// Full argument vector, including `argv[0]`.
        argv: Vec<String>,
        /// The shell's logical working directory at the call.
        cwd: PathBuf,
    },
    /// The builtin identified by `id` finished.
    #[serde(rename = "e")]
    End {
        /// Invocation id from the matching [`Self::Begin`].
        id: u64,
        /// `CLOCK_REALTIME` microseconds at the return.
        ts: u64,
        /// Thread that executed the builtin.
        tid: u32,
        /// Exit code the builtin produced.
        exit: u8,
    },
}

impl BuiltinRecord {
    /// The record's timestamp, whichever variant it is.
    pub const fn ts(&self) -> u64 {
        match self {
            Self::Begin { ts, .. } | Self::End { ts, .. } => *ts,
        }
    }
}

/// The canonical [`crate::BuiltinHook`]: records every builtin lifecycle in memory.
#[derive(Default)]
pub struct RecordingHook {
    records: Mutex<Vec<BuiltinRecord>>,
    next: AtomicU64,
}

impl RecordingHook {
    /// Every record collected so far, in the order the shell produced them.
    ///
    /// Poisoning is recovered rather than propagated: the guarded code is a `clone` and a `push`,
    /// neither of which can panic, so a poisoned lock could only come from an unrelated thread
    /// dying — and losing the whole record log to that would defeat the instrumentation.
    pub fn records(&self) -> Vec<BuiltinRecord> {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Appends one record, with the same poisoning recovery as [`Self::records`].
    fn push(&self, record: BuiltinRecord) {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(record);
    }
}

impl crate::BuiltinHook for RecordingHook {
    fn begin(&self, name: &str, argv: &[String], cwd: &Path) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.push(BuiltinRecord::Begin {
            id,
            ts: now_micros(),
            tid: current_tid(),
            builtin: name.to_string(),
            argv: argv.to_vec(),
            cwd: cwd.to_path_buf(),
        });
        id
    }

    fn end(&self, id: u64, exit: u8) {
        self.push(BuiltinRecord::End {
            id,
            ts: now_micros(),
            tid: current_tid(),
            exit,
        });
    }
}

/// `CLOCK_REALTIME` microseconds — the clock `strace -ttt` stamps its lines with.
///
/// A pre-epoch clock is impossible on a running system; were it to happen, 0 orders the record
/// before every syscall, which refuses to merge rather than mis-attributing one.
pub fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
        })
}

/// The calling thread's id, in the namespace `strace -f` reports.
pub fn current_tid() -> u32 {
    // SAFETY: `gettid(2)` reads the calling thread's own id; it takes no arguments, touches no
    // memory, and cannot fail.
    let tid = unsafe { libc::gettid() };
    u32::try_from(tid).unwrap_or(0)
}

/// Parses a dumped record array.
///
/// The dump is written once, whole, at executor exit, so there is no partial-line case to tolerate:
/// either the file parses as an array of records or the run's instrumentation is unusable.
///
/// Generic in the record type because a run dumps two streams — [`BuiltinRecord`] and
/// [`SpawnRecord`] — into two files of the same format.
///
/// # Errors
///
/// Fails when `text` is not a JSON array of `R`.
pub fn parse_records<R: serde::de::DeserializeOwned>(
    text: &str,
) -> Result<Vec<R>, serde_json::Error> {
    serde_json::from_str(text)
}

/// The dump [`parse_records`] reads: one JSON array, written whole.
///
/// # Errors
///
/// Fails when a record cannot be serialized.
pub fn dump_records<R: serde::Serialize>(records: &[R]) -> Result<String, serde_json::Error> {
    serde_json::to_string(records)
}

/// What the shell asked the spawner to run.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpawnRequest {
    /// The resolved executable, as the shell passed it to [`std::process::Command::new`].
    pub program: PathBuf,
    /// The arguments after `argv[0]`.
    ///
    /// [`std::process::Command`] exposes no getter for the `arg0` override, so the user-facing
    /// name the shell dispatched under is not recorded.
    pub args: Vec<String>,
    /// The directory the process starts in.
    pub cwd: PathBuf,
}

impl SpawnRequest {
    /// Describes `command` before it is consumed by the spawn.
    ///
    /// A command carrying no working directory of its own inherits the shell process's, so that
    /// is what [`Self::cwd`] records for it.
    #[must_use]
    pub fn of(command: &std::process::Command) -> Self {
        Self {
            program: PathBuf::from(command.get_program()),
            args: command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            cwd: command.get_current_dir().map_or_else(
                || std::env::current_dir().unwrap_or_default(),
                Path::to_path_buf,
            ),
        }
    }
}

/// One external-command spawn attempt, as a
/// `brush_core::extensions::ExternalCommandSpawner` sees it.
///
/// There is no exit code here: the spawner hands the child back and the *shell* awaits it, so the
/// only outcome observable at this seam is whether a process started at all.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "k")]
pub enum SpawnRecord {
    /// A process started.
    #[serde(rename = "s")]
    Spawned {
        /// Attempt id, unique within one recorder.
        id: u64,
        /// `CLOCK_REALTIME` microseconds at the spawn.
        ts: u64,
        /// Thread that asked for the spawn.
        tid: u32,
        /// What the shell asked to run.
        #[serde(flatten)]
        request: SpawnRequest,
        /// The child's process id, when there is one.
        pid: Option<u32>,
    },
    /// No process started; the shell reports 127 (`NotFound`) or 126 (anything else).
    #[serde(rename = "f")]
    Failed {
        /// Attempt id, unique within one recorder.
        id: u64,
        /// `CLOCK_REALTIME` microseconds at the failure.
        ts: u64,
        /// Thread that asked for the spawn.
        tid: u32,
        /// What the shell asked to run.
        #[serde(flatten)]
        request: SpawnRequest,
        /// The spawn failure, rendered.
        error: String,
    },
}

impl SpawnRecord {
    /// The attempt the record belongs to.
    pub const fn id(&self) -> u64 {
        match self {
            Self::Spawned { id, .. } | Self::Failed { id, .. } => *id,
        }
    }

    /// The record's timestamp, whichever variant it is.
    pub const fn ts(&self) -> u64 {
        match self {
            Self::Spawned { ts, .. } | Self::Failed { ts, .. } => *ts,
        }
    }

    /// What the shell asked to run, whichever variant it is.
    pub const fn request(&self) -> &SpawnRequest {
        match self {
            Self::Spawned { request, .. } | Self::Failed { request, .. } => request,
        }
    }
}

/// Records every spawn attempt an `ExternalCommandSpawner` makes, in memory.
///
/// The counterpart of [`RecordingHook`] one level up, and deliberately not a trait: a spawner owns
/// its recorder, where a builtin hook has to be reachable from a process-global installation.
#[derive(Default)]
pub struct SpawnRecorder {
    /// The log, in attempt order.
    records: Mutex<Vec<SpawnRecord>>,
    /// The next attempt id.
    next: AtomicU64,
}

impl SpawnRecorder {
    /// Records a started process and returns the attempt's id.
    pub fn spawned(&self, request: SpawnRequest, pid: Option<u32>) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.push(SpawnRecord::Spawned {
            id,
            ts: now_micros(),
            tid: current_tid(),
            request,
            pid,
        });
        id
    }

    /// Records a spawn that started no process, and returns the attempt's id.
    pub fn failed(&self, request: SpawnRequest, error: &std::io::Error) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.push(SpawnRecord::Failed {
            id,
            ts: now_micros(),
            tid: current_tid(),
            request,
            error: error.to_string(),
        });
        id
    }

    /// Every record collected so far, in the order the spawner produced them.
    ///
    /// Poisoning is recovered rather than propagated, for the reason [`RecordingHook::records`]
    /// gives.
    pub fn records(&self) -> Vec<SpawnRecord> {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Appends one record, with the same poisoning recovery as [`Self::records`].
    fn push(&self, record: SpawnRecord) {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(record);
    }
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::BuiltinHook;

    #[test]
    fn records_round_trip_through_json() {
        let records = vec![
            BuiltinRecord::Begin {
                id: 7,
                ts: 1_700_000_000_000_001,
                tid: 42,
                builtin: "git".to_string(),
                argv: vec!["git".to_string(), "add".to_string(), "foo".to_string()],
                cwd: PathBuf::from("/work/src"),
            },
            BuiltinRecord::End {
                id: 7,
                ts: 1_700_000_000_000_009,
                tid: 42,
                exit: 0,
            },
        ];
        let text = dump_records(&records).expect("serialize");
        assert_eq!(
            parse_records::<BuiltinRecord>(&text).expect("parse"),
            records
        );
    }

    #[test]
    fn a_malformed_dump_is_a_parse_error() {
        assert!(parse_records::<BuiltinRecord>("{\"k\":\"b\"}").is_err());
    }

    #[test]
    fn the_recorder_pairs_begins_with_ends_in_order() {
        let hook = RecordingHook::default();
        let cwd = PathBuf::from("/work");
        let first = hook.begin("cd", &["cd".to_string(), "src".to_string()], &cwd);
        let second = hook.begin("git", &["git".to_string(), "add".to_string()], &cwd);
        hook.end(second, 0);
        hook.end(first, 1);

        assert_ne!(first, second, "ids identify invocations, not builtins");
        let records = hook.records();
        assert_eq!(records.len(), 4);
        let stamps: Vec<u64> = records.iter().map(BuiltinRecord::ts).collect();
        assert!(
            stamps.windows(2).all(|pair| pair[0] <= pair[1]),
            "record order is time order: {stamps:?}"
        );
        let tid = current_tid();
        assert!(
            records.iter().all(|record| match record {
                BuiltinRecord::Begin { tid: recorded, .. }
                | BuiltinRecord::End { tid: recorded, .. } => *recorded == tid,
            }),
            "a builtin is recorded by the thread that ran it"
        );
        let BuiltinRecord::Begin { builtin, argv, .. } = &records[1] else {
            panic!("expected a begin record, got {:?}", records[1]);
        };
        assert_eq!(builtin, "git");
        assert_eq!(argv, &["git".to_string(), "add".to_string()]);
        assert_eq!(
            records[3],
            BuiltinRecord::End {
                id: first,
                ts: records[3].ts(),
                tid,
                exit: 1,
            },
            "the exit code reaches the record verbatim"
        );
    }

    /// A request that names no real program: the recorder never runs one.
    fn request() -> SpawnRequest {
        SpawnRequest {
            program: PathBuf::from("/bin/echo"),
            args: vec!["hi".to_string()],
            cwd: PathBuf::from("/work"),
        }
    }

    #[test]
    fn a_spawn_request_describes_the_command() {
        let mut command = std::process::Command::new("/bin/echo");
        command.args(["a", "b"]).current_dir("/tmp");
        let described = SpawnRequest::of(&command);
        assert_eq!(described.program, PathBuf::from("/bin/echo"));
        assert_eq!(described.args, ["a".to_string(), "b".to_string()]);
        assert_eq!(described.cwd, PathBuf::from("/tmp"));

        let inherited = SpawnRequest::of(&std::process::Command::new("/bin/true"));
        assert_eq!(
            inherited.cwd,
            std::env::current_dir().expect("a current directory"),
            "a command with no working directory of its own inherits the process's"
        );
        assert!(inherited.args.is_empty());
    }

    #[test]
    fn the_spawn_recorder_numbers_attempts_in_order() {
        let recorder = SpawnRecorder::default();
        let started = recorder.spawned(request(), Some(4321));
        let refused = recorder.failed(
            request(),
            &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );

        assert_eq!(
            [started, refused],
            [0, 1],
            "ids number the attempts of one recorder"
        );

        let records = recorder.records();
        assert_eq!(
            records.iter().map(SpawnRecord::id).collect::<Vec<_>>(),
            vec![started, refused]
        );
        assert!(
            records.iter().all(|record| *record.request() == request()),
            "the request reaches the record verbatim: {records:?}"
        );

        let stamps: Vec<u64> = records.iter().map(SpawnRecord::ts).collect();
        assert!(
            stamps.windows(2).all(|pair| pair[0] <= pair[1]),
            "record order is time order: {stamps:?}"
        );
        assert!(stamps.iter().all(|stamp| *stamp > 0), "{stamps:?}");

        let tid = current_tid();
        assert!(
            records.iter().all(|record| match record {
                SpawnRecord::Spawned { tid: recorded, .. }
                | SpawnRecord::Failed { tid: recorded, .. } => *recorded == tid,
            }),
            "a spawn is recorded by the thread that asked for it"
        );

        assert!(
            matches!(
                records[0],
                SpawnRecord::Spawned {
                    pid: Some(4321),
                    ..
                }
            ),
            "a started process carries its pid: {:?}",
            records[0]
        );
        let SpawnRecord::Failed { error, .. } = &records[1] else {
            panic!("expected a failure record, got {:?}", records[1]);
        };
        assert!(
            error.to_lowercase().contains("permission denied"),
            "the failure is rendered as the shell would report it: {error}"
        );
    }

    #[test]
    fn recorded_spawns_are_a_snapshot_not_a_live_view() {
        let recorder = SpawnRecorder::default();
        recorder.spawned(request(), None);
        let taken = recorder.records();
        recorder.spawned(request(), None);

        assert_eq!(taken.len(), 1, "the snapshot does not grow with the log");
        assert_eq!(recorder.records().len(), 2);
    }

    #[test]
    fn spawn_records_round_trip_through_json() {
        let records = vec![
            SpawnRecord::Spawned {
                id: 3,
                ts: 1_700_000_000_000_001,
                tid: 11,
                request: request(),
                pid: Some(99),
            },
            SpawnRecord::Failed {
                id: 4,
                ts: 1_700_000_000_000_002,
                tid: 11,
                request: request(),
                error: "No such file or directory (os error 2)".to_string(),
            },
        ];
        let text = dump_records(&records).expect("serialize");
        assert_eq!(parse_records::<SpawnRecord>(&text).expect("parse"), records);

        let json: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        let array = json.as_array().expect("an array");
        assert_eq!(array[0]["k"], "s", "the wire tags are stable");
        assert_eq!(array[1]["k"], "f");
        for element in array {
            assert_eq!(element["program"], "/bin/echo");
            assert_eq!(element["args"], serde_json::json!(["hi"]));
            assert_eq!(element["cwd"], "/work");
            assert!(
                element.get("request").is_none(),
                "the request is flattened into the record: {element}"
            );
        }
    }

    #[test]
    fn a_malformed_spawn_dump_is_a_parse_error() {
        assert!(parse_records::<SpawnRecord>("{\"k\":\"s\"}").is_err());
        assert!(parse_records::<SpawnRecord>("[{\"k\":\"x\"}]").is_err());
    }
}
