//! Recording builtin invocations.
//!
//! Builtins run inside the shell process, so `strace` sees their syscalls but never the invocation
//! itself: `git add foo` executed as a builtin looks like a few reads and writes under `.git/`. This
//! module supplies the other half of the instrumentation — a [`crate::BuiltinHook`] that
//! records every builtin lifecycle in memory, plus the record vocabulary a consumer merges with
//! a syscall trace.
//!
//! [`CommandRecord`] is the executor-level counterpart: what a `brush_core::CommandExecutor` sees,
//! which is every simple command the shell dispatches — builtin, function or external — before its
//! name is resolved. It is stamped from the same clock and the same tid namespace as
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
/// [`CommandRecord`] — into two files of the same format.
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

/// What resolved the command name, mirroring `SimpleCommand::execute`'s order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandKind {
    /// A registered, enabled builtin.
    Builtin,
    /// A shell function.
    Function,
    /// Anything else: an external program, or a name that resolves to none.
    External,
}

/// One simple-command lifecycle record, as seen by a `brush_core::CommandExecutor`.
///
/// An invocation is either a [`Self::Begin`] followed by an [`Self::End`] — the executor observed
/// the result — or a [`Self::Begin`] followed by a [`Self::Spawned`], which says the command was
/// handed back to the interpreter still running and its exit code belongs to nobody here.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "k")]
pub enum CommandRecord {
    /// The executor received the command.
    #[serde(rename = "cb")]
    Begin {
        /// Invocation id, unique within one recorder; echoed by the matching terminator.
        id: u64,
        /// `CLOCK_REALTIME` microseconds at the dispatch.
        ts: u64,
        /// Thread that dispatched the command.
        tid: u32,
        /// Full argument vector, including `argv[0]`.
        argv: Vec<String>,
        /// The shell's logical working directory at the dispatch.
        cwd: PathBuf,
        /// What the name resolves to.
        kind: CommandKind,
    },
    /// The executor observed the command's result.
    #[serde(rename = "ce")]
    End {
        /// Invocation id from the matching [`Self::Begin`].
        id: u64,
        /// `CLOCK_REALTIME` microseconds at the result.
        ts: u64,
        /// Thread that observed the result.
        tid: u32,
        /// Exit code the command produced.
        exit: u8,
    },
    /// The command started a process or task whose completion the executor does not observe.
    #[serde(rename = "cs")]
    Spawned {
        /// Invocation id from the matching [`Self::Begin`].
        id: u64,
        /// `CLOCK_REALTIME` microseconds at the spawn.
        ts: u64,
        /// Thread that spawned it.
        tid: u32,
        /// The child's process id, when there is one.
        pid: Option<i32>,
    },
}

impl CommandRecord {
    /// The invocation the record belongs to.
    pub const fn id(&self) -> u64 {
        match self {
            Self::Begin { id, .. } | Self::End { id, .. } | Self::Spawned { id, .. } => *id,
        }
    }

    /// The record's timestamp, whichever variant it is.
    pub const fn ts(&self) -> u64 {
        match self {
            Self::Begin { ts, .. } | Self::End { ts, .. } | Self::Spawned { ts, .. } => *ts,
        }
    }
}

/// Records every simple command an executor dispatches, in memory.
///
/// The counterpart of [`RecordingHook`] one level up, and deliberately not a trait: an executor
/// owns its recorder, where a builtin hook has to be reachable from a process-global installation.
#[derive(Default)]
pub struct CommandRecorder {
    /// The log, in dispatch order.
    records: Mutex<Vec<CommandRecord>>,
    /// The next invocation id.
    next: AtomicU64,
}

impl CommandRecorder {
    /// Records a dispatch and returns the id its terminator must echo.
    pub fn begin(&self, argv: &[String], cwd: &Path, kind: CommandKind) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.push(CommandRecord::Begin {
            id,
            ts: now_micros(),
            tid: current_tid(),
            argv: argv.to_vec(),
            cwd: cwd.to_path_buf(),
            kind,
        });
        id
    }

    /// Records the observed result of the invocation identified by `id`.
    pub fn end(&self, id: u64, exit: u8) {
        self.push(CommandRecord::End {
            id,
            ts: now_micros(),
            tid: current_tid(),
            exit,
        });
    }

    /// Records that the invocation identified by `id` was handed back still running.
    pub fn spawned(&self, id: u64, pid: Option<i32>) {
        self.push(CommandRecord::Spawned {
            id,
            ts: now_micros(),
            tid: current_tid(),
            pid,
        });
    }

    /// Every record collected so far, in the order the executor produced them.
    ///
    /// Poisoning is recovered rather than propagated, for the reason [`RecordingHook::records`]
    /// gives.
    pub fn records(&self) -> Vec<CommandRecord> {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Appends one record, with the same poisoning recovery as [`Self::records`].
    fn push(&self, record: CommandRecord) {
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

    #[test]
    fn the_command_recorder_numbers_invocations_and_echoes_their_ids() {
        let recorder = CommandRecorder::default();
        let cwd = PathBuf::from("/work");
        let builtin = recorder.begin(
            &["cd".to_string(), "src".to_string()],
            &cwd,
            CommandKind::Builtin,
        );
        let external = recorder.begin(&["cat".to_string()], &cwd, CommandKind::External);
        let function = recorder.begin(&["f".to_string()], &cwd, CommandKind::Function);
        recorder.end(builtin, 0);
        recorder.spawned(external, Some(4321));
        recorder.spawned(function, None);

        assert_eq!(
            [builtin, external, function],
            [0, 1, 2],
            "ids number the dispatches of one recorder"
        );

        let records = recorder.records();
        assert_eq!(records.len(), 6);
        assert_eq!(
            records.iter().map(CommandRecord::id).collect::<Vec<_>>(),
            vec![builtin, external, function, builtin, external, function],
            "every terminator echoes the id of its dispatch"
        );

        let stamps: Vec<u64> = records.iter().map(CommandRecord::ts).collect();
        assert!(
            stamps.windows(2).all(|pair| pair[0] <= pair[1]),
            "record order is time order: {stamps:?}"
        );
        assert!(stamps.iter().all(|stamp| *stamp > 0), "{stamps:?}");

        let tid = current_tid();
        assert!(
            records.iter().all(|record| match record {
                CommandRecord::Begin { tid: recorded, .. }
                | CommandRecord::End { tid: recorded, .. }
                | CommandRecord::Spawned { tid: recorded, .. } => *recorded == tid,
            }),
            "a command is recorded by the thread that dispatched it"
        );

        let CommandRecord::Begin { argv, cwd: recorded, kind, .. } = &records[0] else {
            panic!("expected a begin record, got {:?}", records[0]);
        };
        assert_eq!(argv, &["cd".to_string(), "src".to_string()]);
        assert_eq!(recorded, &cwd);
        assert_eq!(*kind, CommandKind::Builtin);

        assert_eq!(
            records[3],
            CommandRecord::End {
                id: builtin,
                ts: records[3].ts(),
                tid,
                exit: 0,
            }
        );
        assert_eq!(
            records[4],
            CommandRecord::Spawned {
                id: external,
                ts: records[4].ts(),
                tid,
                pid: Some(4321),
            },
            "a spawn carries the pid it was given"
        );
        assert_eq!(
            records[5],
            CommandRecord::Spawned {
                id: function,
                ts: records[5].ts(),
                tid,
                pid: None,
            },
            "a spawn with no process records none"
        );
    }

    #[test]
    fn recorded_commands_are_a_snapshot_not_a_live_view() {
        let recorder = CommandRecorder::default();
        let id = recorder.begin(&["true".to_string()], Path::new("/work"), CommandKind::Builtin);
        let taken = recorder.records();
        recorder.end(id, 0);

        assert_eq!(taken.len(), 1, "the snapshot does not grow with the log");
        assert_eq!(recorder.records().len(), 2);
    }

    #[test]
    fn command_records_round_trip_through_json() {
        let records = vec![
            CommandRecord::Begin {
                id: 3,
                ts: 1_700_000_000_000_001,
                tid: 11,
                argv: vec!["git".to_string(), "status".to_string()],
                cwd: PathBuf::from("/work/src"),
                kind: CommandKind::Builtin,
            },
            CommandRecord::End {
                id: 3,
                ts: 1_700_000_000_000_002,
                tid: 11,
                exit: 128,
            },
            CommandRecord::Spawned {
                id: 4,
                ts: 1_700_000_000_000_003,
                tid: 11,
                pid: Some(99),
            },
        ];
        let text = dump_records(&records).expect("serialize");
        assert_eq!(
            parse_records::<CommandRecord>(&text).expect("parse"),
            records
        );

        let json: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        let tags: Vec<&str> = json
            .as_array()
            .expect("an array")
            .iter()
            .map(|record| record["k"].as_str().expect("a tag"))
            .collect();
        assert_eq!(tags, ["cb", "ce", "cs"], "the wire tags are stable");
        assert_eq!(json[0]["kind"], "builtin");

        for (kind, expected) in [
            (CommandKind::Builtin, "builtin"),
            (CommandKind::Function, "function"),
            (CommandKind::External, "external"),
        ] {
            assert_eq!(
                serde_json::to_value(kind).expect("serialize the kind"),
                expected
            );
        }
    }

    #[test]
    fn a_malformed_command_dump_is_a_parse_error() {
        assert!(parse_records::<CommandRecord>("{\"k\":\"cb\"}").is_err());
        assert!(parse_records::<CommandRecord>("[{\"k\":\"cx\"}]").is_err());
    }
}
