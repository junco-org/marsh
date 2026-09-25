//! Observing what a command actually touched, through the system `strace`.
//!
//! Builtins run inside the shell process and external commands run beside it, so neither the
//! builtin records nor the spawn records say which *files* a line read or wrote. This module is
//! the other half: one tracer attached to the host process, following every thread and every
//! descendant, whose decoded lines are handed to whichever snapshot they belong to.
//!
//! The system `strace` binary is used deliberately. `-y` descriptor decoration is what makes a
//! relative path resolvable without reimplementing the kernel's path walk, and `-ttt` stamps the
//! same `CLOCK_REALTIME` microseconds [`crate::now_micros`] does, so a trace line and a builtin
//! record sort together.
//!
//! # Attribution
//!
//! One host process runs every shell, so a syscall is not attributable by pid alone. The shell's
//! own code brackets the work it is doing with a *scope*: a traced `readlink` of
//! `/proc/self/marsh-trace/<pid>-<id>/enter` before it and `…/leave` after it. The decoder keeps
//! a per-thread stack of those scopes, and a `clone`/`fork` that is not a thread creation hands
//! the child the parent's current attribution — which is how an external command's own syscalls
//! reach the shell that spawned it.
//!
//! `…/barrier` is the same mechanism used as a drain marker: a caller that needs every syscall
//! issued so far to have been decoded emits one and waits for the decoder to report it. That is a
//! proof, not a quiet period.
//!
//! # Failure
//!
//! Tracing is never optional. A missing `strace`, a refused attachment, a lost reader or
//! undecodable evidence all fail the caller through [`std::io::Error`]; there is no untraced
//! fallback, because a command whose file accesses were not observed cannot be told apart from one
//! that touched nothing.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::Waker;
use std::time::{Duration, Instant};

use marsh_lib::Recorder;

/// How long a tracer has to attach, and a drain has to complete.
///
/// Both are proofs that the decoder is keeping up with the kernel, so the budget is generous and
/// fixed: exceeding it means the evidence stream is broken, not that the machine is busy.
const DEADLINE: Duration = Duration::from_secs(10);

/// The syscalls the tracer is asked for.
///
/// `%file` is every call that takes a path and `%process` every fork/exec/exit; the rest is the
/// descriptor and mapping bookkeeping a path resolver needs — `-y` decorates a descriptor with the
/// path it points at, but only a call that *has* a descriptor argument.
///
/// `read` and `write` are deliberately absent. The decoder reads the tracer's own output from this
/// process — with `pread64`, `poll`, an inotify `read` and `fallocate`, none of which is in this
/// set either — and a traced read of that output would describe itself.
const TRACE_SET: &str = "%file,%process,fchdir,dup,dup2,dup3,fcntl,close,mmap,mprotect,munmap,\
                         ftruncate,fchmod";

/// What a registered root's classifier is: called with every line attributed to it.
///
/// Shared and `Send + Sync` because the classifying thread is not the caller's: a root is
/// registered once and observed for as long as it lives.
pub type TraceObserver = Arc<dyn Fn(&TraceLine) -> std::io::Result<()> + Send + Sync>;

/// One decoded line of the trace.
#[derive(Clone, Debug)]
pub struct TraceLine {
    /// Thread id that issued the call.
    pub tid: u32,
    /// `CLOCK_REALTIME` microseconds `-ttt` stamped the line with.
    ///
    /// For a call `strace` split across a context switch this is the *entry* stamp: per-thread
    /// entry order is program order, which is the ordering every consumer relies on.
    pub ts_us: u64,
    /// What the line records.
    pub call: Call,
}

impl std::fmt::Display for TraceLine {
    /// The line as the tracer printed it, which is what a run's `trace.log` carries.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} {}.{:06} ",
            self.tid,
            self.ts_us / 1_000_000,
            self.ts_us % 1_000_000
        )?;
        match &self.call {
            Call::Syscall {
                name,
                args,
                ret,
                ret_path,
            } => {
                write!(formatter, "{name}({args}) = {ret}")?;
                if let Some(path) = ret_path {
                    write!(formatter, "<{path}>")?;
                }
                Ok(())
            }
            Call::Exited { status } => write!(formatter, "+++ exited with {status} +++"),
        }
    }
}

/// A trace line's payload.
#[derive(Clone, Debug)]
pub enum Call {
    /// A completed syscall.
    Syscall {
        /// Syscall name.
        name: String,
        /// Raw argument text between the outermost parentheses.
        args: String,
        /// Return value. `?` (as printed for `exit_group`) and unparsable returns become `-1`,
        /// which reads as "not a success" everywhere a consumer looks at it.
        ret: i64,
        /// Path `-y` printed for a returned descriptor, e.g. `= 3</abs/path>`.
        ret_path: Option<String>,
    },
    /// Process exit record.
    Exited {
        /// Exit status the process reported.
        status: i32,
    },
}

/// Splits raw argument text into top-level arguments.
#[allow(
    clippy::string_slice,
    reason = "`start` and `index` are byte offsets of the ASCII delimiters this scanner matched, \
              so both are char boundaries"
)]
#[must_use]
pub fn split_args(args: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut start = 0usize;
    for (index, byte) in args.bytes().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(args[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    let last = args[start..].trim();
    if !last.is_empty() || !parts.is_empty() {
        parts.push(last);
    }
    parts
}

/// Decodes one quoted, C-escaped `strace` string argument.
///
/// Returns `None` for anything that is not a quoted string (flag names, numbers, structs), and for
/// a string the tracer truncated: `-s 4096` is longer than any path the kernel accepts, so a
/// truncated string is an unresolvable path rather than a prefix worth guessing from.
#[must_use]
pub fn parse_quoted(arg: &str) -> Option<String> {
    let arg = arg.trim();
    let inner = arg.strip_prefix('"')?;
    let close = inner.rfind('"')?;
    if inner.get(close + 1..).is_some_and(|tail| tail.contains("...")) {
        return None;
    }
    Some(unescape(inner.get(..close)?))
}

/// Reverses `strace`'s C escaping of string arguments.
///
/// Byte-oriented, because a path is bytes: the tracer prints a non-ASCII name as a run of octal
/// escapes, one per *byte* of its encoding. Decoding each escape to a character instead would turn
/// `\303\251` into two Latin-1 characters rather than back into `é`, and name a file that does not
/// exist. A name that is not valid UTF-8 is lossy-converted, which is also how the rest of this
/// crate spells a path it has to compare.
fn unescape(text: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(text.len());
    let mut bytes = text.bytes().peekable();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        match bytes.next() {
            Some(b'n') => out.push(b'\n'),
            Some(b't') => out.push(b'\t'),
            Some(b'r') => out.push(b'\r'),
            Some(b'"') => out.push(b'"'),
            Some(b'\\') => out.push(b'\\'),
            Some(digit @ b'0'..=b'7') => {
                // Octal escape: up to three digits, the first already consumed.
                let mut value = u32::from(digit - b'0');
                let mut taken = 1;
                while taken < 3 {
                    match bytes.peek() {
                        Some(next @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(*next - b'0');
                            bytes.next();
                            taken += 1;
                        }
                        _ => break,
                    }
                }
                out.push(u8::try_from(value).unwrap_or(b'?'));
            }
            Some(other) => out.push(other),
            None => out.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A syscall interrupted by a context switch: `(tid, name)` and the entry timestamp with the
/// argument text printed before the interruption.
type PendingCall = ((u32, String), (u64, String));

/// The incremental half of the lexer: whatever the last chunk left unterminated, and the calls the
/// tracer has printed an entry for but not yet a return.
#[derive(Default)]
struct Decoder {
    /// Bytes after the last newline of the previous chunk. A split line is not an end of input.
    partial: String,
    /// Entry timestamp and buffered argument text of every `<unfinished ...>` call.
    pending: Vec<PendingCall>,
}

impl Decoder {
    /// Decodes whatever complete lines `chunk` finished, handing each to `sink`.
    ///
    /// Bytes are parsed exactly once: the remainder is kept for the next chunk rather than
    /// re-scanned.
    fn feed(&mut self, chunk: &str, mut sink: impl FnMut(TraceLine)) {
        self.partial.push_str(chunk);
        loop {
            let Some(end) = self.partial.find('\n') else {
                return;
            };
            let line: String = self.partial.drain(..=end).collect();
            if let Some(decoded) = self.line(line.trim_end()) {
                sink(decoded);
            }
        }
    }

    /// Decodes one whole line, rejoining an interrupted call with its resumption.
    fn line(&mut self, raw: &str) -> Option<TraceLine> {
        if raw.is_empty() {
            return None;
        }
        let (tid, ts_us, rest) = split_prefix(raw)?;

        if let Some(status) = rest.strip_prefix("+++ exited with ") {
            let status = status.trim_end_matches(" +++").trim().parse::<i32>().ok()?;
            return Some(TraceLine {
                tid,
                ts_us,
                call: Call::Exited { status },
            });
        }
        if rest.starts_with("+++") || rest.starts_with("---") {
            return None;
        }
        if rest.starts_with("strace:") {
            return None;
        }

        // Resumption of an interrupted call: `<... openat resumed>) = 3</abs/path>`.
        if let Some(after) = rest.strip_prefix("<... ") {
            let (name, tail) = after.split_once(" resumed>")?;
            let (entry_ts, prefix) = take_pending(&mut self.pending, tid, name)
                .unwrap_or_else(|| (ts_us, String::new()));
            let (args_tail, ret_text) = split_close(tail)?;
            let mut args = prefix;
            args.push_str(args_tail);
            let (ret, ret_path) = parse_return(ret_text);
            return Some(TraceLine {
                tid,
                ts_us: entry_ts,
                call: Call::Syscall {
                    name: name.to_string(),
                    args,
                    ret,
                    ret_path,
                },
            });
        }

        let open = rest.find('(')?;
        let name = rest.get(..open)?;
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return None;
        }
        let body = rest.get(open + 1..)?;

        // A call its process never returned from — killed inside it — is printed whole and still
        // unfinished: `openat(…, "p" <unfinished ...>) = ?`. What it did is as unknown as for a
        // call still in flight, so it is buffered the same way, and nothing ever resumes it.
        let partial = body.strip_suffix("<unfinished ...>").or_else(|| {
            body.strip_suffix(") = ?")
                .map(str::trim_end)
                .and_then(|call| call.strip_suffix("<unfinished ...>"))
        });
        if let Some(partial) = partial {
            self.pending.push((
                (tid, name.to_string()),
                (ts_us, partial.trim_end().to_string()),
            ));
            return None;
        }

        let (args, ret_text) = split_close(body)?;
        let (ret, ret_path) = parse_return(ret_text);
        Some(TraceLine {
            tid,
            ts_us,
            call: Call::Syscall {
                name: name.to_string(),
                args: args.to_string(),
                ret,
                ret_path,
            },
        })
    }
}

/// Splits the leading thread id and `-ttt` timestamp from a trace line.
///
/// The prefix `strace` prints is `TID <ws> SECS.MICROS <ws> rest`; an unstamped line (there is
/// none while `-ttt` is passed) yields no line at all rather than one that would merge at time
/// zero.
fn split_prefix(line: &str) -> Option<(u32, u64, &str)> {
    let end = line.find(|c: char| !c.is_ascii_digit())?;
    if end == 0 {
        return None;
    }
    let tid = line.get(..end)?.parse::<u32>().ok()?;
    let rest = line.get(end..)?.trim_start();
    let (stamp, rest) = rest.split_once(' ')?;
    let (seconds, micros) = stamp.split_once('.')?;
    if micros.len() != 6 {
        return None;
    }
    let seconds = seconds.parse::<u64>().ok()?;
    let micros = micros.parse::<u64>().ok()?;
    Some((tid, seconds * 1_000_000 + micros, rest.trim_start()))
}

/// Removes and returns the entry timestamp and buffered argument prefix of an interrupted call.
fn take_pending(pending: &mut Vec<PendingCall>, tid: u32, name: &str) -> Option<(u64, String)> {
    let index = pending
        .iter()
        .rposition(|((ptid, pname), _)| *ptid == tid && pname == name)?;
    Some(pending.remove(index).1)
}

/// Splits argument text from the return text at the call's closing parenthesis.
///
/// `body` starts just after the opening parenthesis. Nesting (`[`, `{`, `(`) and quoted strings
/// are tracked so that a `)` inside a struct or a filename does not end the argument list.
fn split_close(body: &str) -> Option<(&str, &str)> {
    let bytes = body.as_bytes();
    let mut depth = 1usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, &byte) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    let tail = body.get(index + 1..)?.trim_start();
                    let ret = tail.strip_prefix('=').map_or(tail, str::trim_start);
                    return Some((body.get(..index)?, ret));
                }
            }
            _ => {}
        }
    }
    None
}

/// Parses the text after `= ` into a return value and its optional `-y` path decoration.
#[allow(
    clippy::string_slice,
    reason = "every index here is a byte offset into ASCII syntax — a leading `-`, an ASCII \
              digit/hexdigit run, or a `<`/`>` found by `find`/`rfind` — so it is always a char \
              boundary"
)]
fn parse_return(text: &str) -> (i64, Option<String>) {
    let text = text.trim();
    if text.is_empty() || text.starts_with('?') {
        return (-1, None);
    }
    let mut end = 0;
    let bytes = text.as_bytes();
    if bytes[0] == b'-' {
        end = 1;
    }
    if text[end..].starts_with("0x") {
        let hex_end = end
            + 2
            + text[end + 2..]
                .find(|c: char| !c.is_ascii_hexdigit())
                .unwrap_or(text.len() - end - 2);
        let value = i64::from_str_radix(&text[end + 2..hex_end], 16).unwrap_or(-1);
        return (value, None);
    }
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    let Ok(value) = text[..end].parse::<i64>() else {
        return (-1, None);
    };
    let decoration = text[end..].strip_prefix('<').and_then(|rest| {
        rest.rfind('>')
            .map(|close| rest[..close].to_string())
            .filter(|path| path.starts_with('/'))
    });
    (value, decoration)
}

/// What a scope marker says about the work bracketed by it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    /// The work belongs to the snapshot registered at this index.
    ///
    /// An index rather than a path: a scope is allocated once and entered many times, and the
    /// decoder compares it on every line.
    Root(usize),
    /// The work is the implementation's own — a tree diff, a log write, a subvolume copy, a record
    /// dump — and belongs to no command's footprint.
    ///
    /// It has to be said explicitly, because such work reads and writes the snapshot tree by name:
    /// a boundary that measured itself would report every file in the tree as read and every file
    /// it copied back as written.
    Internal,
}

/// What a marker line asks the decoder to do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Marker {
    /// Push the scope onto the issuing thread.
    Enter,
    /// Pop it.
    Leave,
    /// Report that everything issued before it has been decoded.
    Barrier,
}

/// How one thread's syscalls are attributed.
#[derive(Default)]
struct ThreadState {
    /// Scopes the thread is currently inside, innermost last.
    stack: Vec<Target>,
    /// Attribution inherited from the process that forked this one, used outside any scope.
    inherited: Option<Target>,
}

impl ThreadState {
    /// The scope a syscall on this thread belongs to.
    fn target(&self) -> Option<Target> {
        self.stack.last().copied().or(self.inherited)
    }
}

/// One registered snapshot: where it is, how it is told it must stop, and who sees its syscalls.
struct Root {
    /// Canonical snapshot root. Registration is keyed on it.
    path: PathBuf,
    /// The same path as text, for the substring test every decoded line is routed by.
    path_text: String,
    /// Set when a read dependency on another principal's publication was observed.
    interrupted: Arc<AtomicBool>,
    /// The snapshot's own classifier, called outside every lock this module holds.
    observe: TraceObserver,
    /// Drivers waiting to learn that this root was interrupted.
    waiters: Vec<Waker>,
    /// Cleared on unregistration so a late line finds no root rather than a replaced one.
    live: bool,
}

/// One decoded item on its way from the decoder to the classifier.
enum Work {
    /// A line belonging to the root at this index.
    Line(usize, TraceLine),
    /// A drain marker: everything before it has been classified once this is reached.
    Barrier(u64),
}

/// The hand-off between the two tracing threads, and the reason there are two.
///
/// The decoder must never wait on anything a *traced* thread can hold for long. A boundary holds
/// the seed's authority across a tree diff and a log write, and every syscall of those is traced —
/// so a decoder that classified inline would stop decoding for as long as that boundary lasts, and
/// every drain behind it would wait with it.
///
/// So the decoder only decodes, and a second thread does the classifying and may block for as long
/// as a boundary takes. Its own lock is held for a push and a drain and never across a syscall.
///
/// Held behind its own [`Arc`] rather than inside [`Tracing`]: the classifier waits here, and a
/// waiter holding the tracing state alive would keep the very destructor that stops it from ever
/// running.
#[derive(Default)]
struct Pipeline {
    /// The queue and the stop flag.
    items: Mutex<Queued>,
    /// Signalled when either changes.
    ready: Condvar,
}

/// The queued half of [`Pipeline`].
#[derive(Default)]
struct Queued {
    /// Decoded work in decode order, which is the order it must be classified in.
    work: std::collections::VecDeque<Work>,
    /// Set when the tracer is going away, so the classifier stops waiting for more.
    stopping: bool,
}

/// The tracer process and the two threads behind it.
struct Attached {
    /// The `sh` that `exec`ed `strace`; its pid is the one `PR_SET_PTRACER` permits.
    child: std::process::Child,
    /// The decoder thread, joined when the last root goes.
    reader: Option<std::thread::JoinHandle<()>>,
    /// The decoder's own thread id, whose lines are never evidence.
    reader_tid: u32,
    /// The classifier thread, joined with it.
    classifier: Option<std::thread::JoinHandle<()>>,
}

/// How much consumed trace output is released back to the filesystem at a time.
///
/// The tracer only appends, so everything before the reader's position is dead weight: a daemon
/// traced for weeks keeps its unread backlog on disk, not its lifetime trace.
const TRACE_OUTPUT_RECLAIM_BYTES: u64 = 1024 * 1024;

/// Space kept allocated for the tracer's first complaint about itself.
///
/// A failed write of the trace is reported on the diagnostic stream, and a full filesystem is one
/// reason for it; the reservation is what lets that report still be stored.
const TRACE_DIAGNOSTIC_RESERVE_BYTES: usize = 4096;

/// The tracer's output, as the decoder thread reads it.
///
/// A pipe makes the tracer wait for its reader, and the reader is a traced thread of this host
/// that takes a lock other traced threads hold: a full pipe whose reader waits on such a lock stops
/// the tracer, and with it every tracee, for good. A regular file never makes its writer wait for
/// a reader, so the tracer's progress depends on nothing this process does.
///
/// Reads are positional, so neither file's offset — shared with the tracer's own descriptors — is
/// ever moved. Wakeups come from inotify, and the end of output from a pidfd for the tracer rather
/// than from the last writer's close: descriptors are duplicated freely, a process exits once.
struct TraceOutput {
    /// The trace itself, written by the tracer through `-o`.
    file: std::fs::File,
    /// The tracer's stderr: with `-q` and a named trace stream, only complaints reach it.
    diagnostics: std::fs::File,
    /// One inotify instance watching both files for writes.
    notifications: std::fs::File,
    /// The tracer process.
    process: OwnedFd,
    /// How much of `file` has been handed to the decoder.
    position: u64,
    /// How much of `file` has been released back to the filesystem.
    reclaimed: u64,
    /// Set once the tracer exited: whatever `file` holds then is all it will ever hold.
    exited: bool,
    /// Set once the tracer was told to stop, so it is told once.
    stop_requested: bool,
}

impl TraceOutput {
    /// Fails when the tracer said anything about itself.
    ///
    /// Read from the start every time and never cleared: a complaint says the trace is incomplete,
    /// and no later output makes it whole again.
    fn check_diagnostics(&self) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;

        let mut buffer = [0u8; TRACE_DIAGNOSTIC_RESERVE_BYTES];
        let read = loop {
            match self.diagnostics.read_at(&mut buffer, 0) {
                Ok(read) => break read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        };
        match buffer.get(..read) {
            None | Some([]) => Ok(()),
            Some(said) => Err(std::io::Error::other(format!(
                "strace reported: {}",
                String::from_utf8_lossy(said).trim_end()
            ))),
        }
    }

    /// Releases every whole quantum of output the decoder has been handed.
    fn reclaim(&mut self) -> std::io::Result<()> {
        let consumed = self.position - self.position % TRACE_OUTPUT_RECLAIM_BYTES;
        if consumed <= self.reclaimed {
            return Ok(());
        }
        allocate(
            &self.file,
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            self.reclaimed,
            consumed - self.reclaimed,
        )?;
        self.reclaimed = consumed;
        Ok(())
    }

    /// Blocks until either file was written to or the tracer exited.
    fn wait(&mut self) -> std::io::Result<()> {
        let mut ready = [
            libc::pollfd {
                fd: self.notifications.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.process.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: `ready` is an initialised array of this frame and the count is its length;
            // `poll` writes only the `revents` fields.
            let count = unsafe { libc::poll(ready.as_mut_ptr(), 2, -1) };
            if count >= 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        let [notified, process] = ready;
        if (notified.revents | process.revents) & (libc::POLLERR | libc::POLLNVAL) != 0
            || notified.revents & libc::POLLHUP != 0
        {
            return Err(std::io::Error::other("trace output readiness failed"));
        }
        if process.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            self.exited = true;
        }
        if notified.revents & libc::POLLIN != 0 {
            self.drain_notifications()?;
        }
        Ok(())
    }

    /// Consumes every queued inotify event.
    ///
    /// Events are wakeups, not byte counts: the kernel coalesces them, and the files themselves say
    /// how much there is. A lost watch or an overflowed queue would mean a later write could go
    /// unnoticed, so both fail the stream rather than risk waiting forever.
    fn drain_notifications(&self) -> std::io::Result<()> {
        const HEADER: usize = std::mem::size_of::<libc::inotify_event>();
        let malformed = || {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "malformed trace output notification",
            )
        };
        let field = |header: &[u8], offset: usize| {
            header
                .get(offset..offset + 4)
                .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
                .map(u32::from_ne_bytes)
        };

        // Long enough for an event carrying the longest name, though watches on files carry none.
        let mut buffer = [0u8; 4096];
        loop {
            let read = match (&self.notifications).read(&mut buffer) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "trace output notification stream closed",
                    ));
                }
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            };
            let mut events = buffer.get(..read).ok_or_else(malformed)?;
            while !events.is_empty() {
                let header = events.get(..HEADER).ok_or_else(malformed)?;
                let mask = field(header, std::mem::offset_of!(libc::inotify_event, mask))
                    .ok_or_else(malformed)?;
                let length = field(header, std::mem::offset_of!(libc::inotify_event, len))
                    .and_then(|length| usize::try_from(length).ok())
                    .and_then(|length| HEADER.checked_add(length))
                    .ok_or_else(malformed)?;
                events = events.get(length..).ok_or_else(malformed)?;
                if mask & libc::IN_Q_OVERFLOW != 0 {
                    return Err(std::io::Error::other(
                        "trace output notification queue overflowed",
                    ));
                }
                if mask & (libc::IN_IGNORED | libc::IN_UNMOUNT) != 0 {
                    return Err(std::io::Error::other("trace output watch was lost"));
                }
            }
        }
    }

    /// Tells the tracer to stop, once. A tracer that already exited needs no telling.
    ///
    /// This is what ends a tracer whose output can no longer be used: nothing would ever read what
    /// it kept appending.
    fn request_stop(&mut self) -> std::io::Result<()> {
        if self.exited || self.stop_requested {
            return Ok(());
        }
        kill_process(self.process.as_fd())?;
        self.stop_requested = true;
        Ok(())
    }
}

impl Read for TraceOutput {
    /// Reads what the tracer has written, waiting until it writes more or exits.
    ///
    /// Diagnostics are checked after every read of the trace, before its bytes — or its end — are
    /// handed over: a complaint the tracer wrote before a later line is never accepted after that
    /// line, and a tracer that died before writing anything is reported by its cause.
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        use std::os::unix::fs::FileExt;

        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let read = match self.file.read_at(buffer, self.position) {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            self.check_diagnostics()?;
            if read > 0 {
                self.position += read as u64;
                self.reclaim()?;
                return Ok(read);
            }
            if self.exited {
                return Ok(0);
            }
            self.wait()?;
        }
    }
}

impl Drop for TraceOutput {
    /// A reader that went away early stops the tracer rather than leave it appending unread.
    fn drop(&mut self) {
        let _ = self.request_stop();
    }
}

/// Creates the trace and diagnostic files, and the inotify instance watching both.
///
/// Both are created by name in `/tmp`, private to this user, watched and then unlinked before
/// anything is launched: the watches follow the inodes, and a host that crashes leaves no named
/// trace behind.
///
/// # Errors
///
/// Fails when either file or its watch cannot be set up, and when the storage cannot release
/// consumed output or reserve the diagnostics' space.
fn trace_output_files() -> std::io::Result<(std::fs::File, std::fs::File, std::fs::File)> {
    let data = tempfile::NamedTempFile::new_in("/tmp")?;
    let diagnostics = tempfile::NamedTempFile::new_in("/tmp")?;
    // SAFETY: `inotify_init1` takes flags and touches no memory.
    let notifications = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    if notifications < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the kernel just returned `notifications` as a new descriptor nothing else owns.
    let notifications = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(notifications) });

    // Probed on the empty file: storage that cannot release consumed output fails here, not once
    // it has filled up.
    allocate(
        data.as_file(),
        libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
        0,
        TRACE_OUTPUT_RECLAIM_BYTES,
    )?;
    watch(&notifications, data.path())?;
    allocate(
        diagnostics.as_file(),
        libc::FALLOC_FL_KEEP_SIZE,
        0,
        TRACE_DIAGNOSTIC_RESERVE_BYTES as u64,
    )?;
    watch(&notifications, diagnostics.path())?;

    let (data, name) = data.into_parts();
    name.close()?;
    let (diagnostics, name) = diagnostics.into_parts();
    name.close()?;
    Ok((data, diagnostics, notifications))
}

/// `fallocate(file, mode, offset, length)`, retried when interrupted.
fn allocate(
    file: &std::fs::File,
    mode: libc::c_int,
    offset: u64,
    length: u64,
) -> std::io::Result<()> {
    let range = |value: u64| {
        libc::off_t::try_from(value)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
    };
    let (offset, length) = (range(offset)?, range(length)?);
    loop {
        // SAFETY: `fallocate` takes a descriptor the borrow keeps open and three integers.
        if unsafe { libc::fallocate(file.as_raw_fd(), mode, offset, length) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Watches the file at `path` for writes through `notifications`.
fn watch(notifications: &std::fs::File, path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: `path` is NUL-terminated and outlives the call, and the borrow keeps the descriptor
    // open; the kernel only reads the path.
    let added = unsafe {
        libc::inotify_add_watch(notifications.as_raw_fd(), path.as_ptr(), libc::IN_MODIFY)
    };
    if added < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Everything one host's tracing owns, behind one lock.
#[derive(Default)]
struct State {
    /// Registered snapshots. Entries are retired rather than removed, so a scope index stays
    /// valid for lines the decoder has not caught up with yet.
    roots: Vec<Root>,
    /// Scope id → what it attributes to.
    scopes: HashMap<u64, Target>,
    /// Scope ids whose owner is gone, removed once a drain proves their markers were decoded.
    retiring: Vec<u64>,
    /// Per-thread attribution.
    threads: HashMap<u32, ThreadState>,
    /// The incremental lexer.
    decoder: Decoder,
    /// Barrier ids the decoder has reported.
    barriers: Vec<u64>,
    /// The tracer, once the first root started it.
    attached: Option<Attached>,
    /// The first thing that made the evidence stream unusable.
    failure: Option<String>,
}

/// The tracing half of [`crate::RecordingHook`].
pub(crate) struct Tracing {
    /// Scope and barrier ids, from the same series the builtin records use.
    ids: Recorder<()>,
    /// The `/proc/self/marsh-trace/<pid>-` every marker of this host starts with.
    prefix: String,
    /// Everything the decoder and the emitters share.
    state: Mutex<State>,
    /// Signalled whenever the classifier made progress or the stream failed.
    progress: Condvar,
    /// The decoder-to-classifier hand-off.
    pipeline: Arc<Pipeline>,
}

impl Tracing {
    /// An idle tracing half: no tracer runs until the first root is registered.
    pub(crate) fn new() -> Self {
        Self {
            ids: Recorder::default(),
            prefix: format!("/proc/self/marsh-trace/{}-", std::process::id()),
            state: Mutex::new(State::default()),
            progress: Condvar::new(),
            pipeline: Arc::new(Pipeline::default()),
        }
    }

    /// Takes the shared state, recovering a poisoned lock.
    ///
    /// The guarded code decodes text and moves handles; a poisoned lock therefore means an
    /// unrelated thread died while observing, and refusing to trace because of that would lose
    /// every shell's evidence rather than one observer's.
    fn locked(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers `root`, starting the tracer when it is the first.
    ///
    /// # Errors
    ///
    /// Fails when `root` is already registered, and for the reasons [`Self::start`] does — in
    /// which case the registration is rolled back, so a failed start leaves no half-registered
    /// root behind.
    pub(crate) fn register_root(
        self: &Arc<Self>,
        root: &Path,
        interrupted: Arc<AtomicBool>,
        observe: TraceObserver,
    ) -> std::io::Result<()> {
        let mut state = self.locked();
        if state
            .roots
            .iter()
            .any(|entry| entry.live && entry.path == root)
        {
            return Err(failure(format!(
                "{} is already registered for tracing",
                root.display()
            )));
        }
        let first = !state.roots.iter().any(|entry| entry.live);
        state.roots.push(Root {
            path: root.to_path_buf(),
            path_text: root.to_string_lossy().into_owned(),
            interrupted,
            observe,
            waiters: Vec::new(),
            live: true,
        });
        if !first {
            drop(state);
            return Ok(());
        }
        match self.start(state) {
            Ok(()) => Ok(()),
            Err(error) => {
                let mut state = self.locked();
                if let Some(entry) = state.roots.last_mut() {
                    entry.live = false;
                }
                drop(state);
                Err(error)
            }
        }
    }

    /// Retires `root`, stopping the tracer when it was the last.
    ///
    /// Unregistering something already gone is a teardown no-op: a snapshot's drop runs after a
    /// failed start rolled its registration back.
    ///
    /// # Errors
    ///
    /// Fails when the tracer could not be stopped or its decoder could not be joined.
    pub(crate) fn unregister_root(&self, root: &Path) -> std::io::Result<()> {
        let mut state = self.locked();
        for entry in &mut state.roots {
            if entry.live && entry.path == root {
                entry.live = false;
            }
        }
        if state.roots.iter().any(|entry| entry.live) {
            drop(state);
            return Ok(());
        }
        self.stop(state)
    }

    /// Whether `root`'s evaluation has been asked to stop, registering `waker` to be told.
    ///
    /// A waker is registered whatever the answer, so a driver that observed `false` and then
    /// suspended is still woken by a dependency that lands immediately afterwards.
    pub(crate) fn interrupted(&self, root: &Path, waker: &Waker) -> bool {
        let mut state = self.locked();
        let Some(entry) = state
            .roots
            .iter_mut()
            .find(|entry| entry.live && root.starts_with(&entry.path))
        else {
            return false;
        };
        let interrupted = entry.interrupted.load(Ordering::Acquire);
        if !entry.waiters.iter().any(|known| known.will_wake(waker)) {
            entry.waiters.push(waker.clone());
        }
        drop(state);
        interrupted
    }

    /// Allocates a scope attributing the work bracketed by it to `root`, or marking it as the
    /// implementation's own when `root` names none.
    ///
    /// `None` when nothing is being traced, or when `root` is not registered: an untraced shell
    /// emits no markers at all rather than markers nobody can attribute.
    pub(crate) fn scope(self: &Arc<Self>, root: Option<&Path>) -> Option<TraceScope> {
        let mut state = self.locked();
        state.attached.as_ref()?;
        let target = match root {
            None => Target::Internal,
            Some(root) => Target::Root(
                state
                    .roots
                    .iter()
                    .position(|entry| entry.live && root.starts_with(&entry.path))?,
            ),
        };
        let id = self.ids.next_id();
        state.scopes.insert(id, target);
        drop(state);
        Some(TraceScope {
            tracing: Arc::clone(self),
            id,
        })
    }

    /// Emits one marker and returns once the kernel has issued it.
    ///
    /// A `readlink` of a path that cannot exist: the call is what the tracer prints, and its
    /// failure is the point — nothing is created, read or written.
    fn mark(&self, id: u64, kind: Marker) {
        let mut path = [0u8; 128];
        let mut end = 0;
        let kind = match kind {
            Marker::Enter => "/enter\0",
            Marker::Leave => "/leave\0",
            Marker::Barrier => "/barrier\0",
        };
        let mut digits = [0u8; 20];
        let mut count = 0;
        let mut value = id;
        loop {
            digits[count] = b'0' + u8::try_from(value % 10).unwrap_or(0);
            count += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        let prefix = self.prefix.as_bytes();
        let Some(slot) = path.get_mut(end..end + prefix.len()) else {
            return;
        };
        slot.copy_from_slice(prefix);
        end += prefix.len();
        for index in (0..count).rev() {
            let Some(slot) = path.get_mut(end) else {
                return;
            };
            *slot = digits[index];
            end += 1;
        }
        let Some(slot) = path.get_mut(end..end + kind.len()) else {
            return;
        };
        slot.copy_from_slice(kind.as_bytes());

        let mut sink = [0u8; 1];
        // SAFETY: `path` is a NUL-terminated byte array this function just filled, and `sink` is a
        // one-byte buffer of this stack frame; `readlink` writes at most that one byte and has no
        // other memory requirement.
        unsafe {
            libc::readlink(path.as_ptr().cast(), sink.as_mut_ptr().cast(), sink.len());
        }
    }

    /// Emits a drain marker and waits until the decoder reports it.
    ///
    /// Everything the kernel issued before the marker is decoded when it returns, which is what
    /// makes a boundary's evidence complete rather than merely recent. Untraced hosts drain
    /// immediately.
    ///
    /// # Errors
    ///
    /// Fails when the tracer is already broken, when the decoder does not reach the marker within
    /// the shared deadline, or when the marker's own emission was lost.
    pub(crate) fn drain(&self) -> std::io::Result<()> {
        let state = self.locked();
        if state.attached.is_none() {
            let broken = state.failure.clone();
            drop(state);
            return match broken {
                None => Ok(()),
                Some(cause) => Err(failure(cause)),
            };
        }
        drop(state);
        if self.barrier(DEADLINE)? {
            return Ok(());
        }
        Err(self.stalled())
    }

    /// Emits one drain marker and waits up to `patience` for the decoder to report it.
    ///
    /// `false` is the marker not having come back yet, which is a different thing from the stream
    /// being broken: an attachment still being set up has simply not started printing this
    /// process's syscalls.
    ///
    /// # Errors
    ///
    /// Fails when the evidence stream is already broken.
    fn barrier(&self, patience: Duration) -> std::io::Result<bool> {
        let id = self.ids.next_id();
        self.mark(id, Marker::Barrier);

        let mut state = self.locked();
        let deadline = Instant::now() + patience;
        loop {
            if let Some(cause) = &state.failure {
                let cause = cause.clone();
                drop(state);
                return Err(failure(cause));
            }
            if let Some(index) = state.barriers.iter().position(|seen| *seen == id) {
                state.barriers.remove(index);
                let retiring = std::mem::take(&mut state.retiring);
                for retired in retiring {
                    state.scopes.remove(&retired);
                }
                drop(state);
                return Ok(true);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                drop(state);
                return Ok(false);
            };
            let (guard, _) = self
                .progress
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            state = guard;
        }
    }

    /// The failure a decoder that never reported a marker is described by: whatever already broke
    /// the stream, when something did.
    fn stalled(&self) -> std::io::Error {
        let state = self.locked();
        let recorded = state.failure.clone();
        drop(state);
        failure(recorded.unwrap_or_else(|| {
            format!(
                "the tracer did not report a drain marker within {}s",
                DEADLINE.as_secs()
            )
        }))
    }

    /// Whether any relevant call of `root`'s threads is still unresolved.
    ///
    /// A publication may not be built on a call the tracer printed an entry for and no return: the
    /// path is known and the effect is not. A discard may, which is why this is asked rather than
    /// enforced here.
    pub(crate) fn unresolved(&self, root: &Path) -> bool {
        let state = self.locked();
        let Some(index) = state
            .roots
            .iter()
            .position(|entry| entry.live && entry.path == root)
        else {
            return false;
        };
        let unresolved = state
            .pending_targets()
            .any(|pending| pending == Target::Root(index));
        drop(state);
        unresolved
    }

    /// Forgets `root`'s unfinished calls, for an evaluation that was cut short.
    pub(crate) fn retire_unresolved(&self, root: &Path) {
        let mut state = self.locked();
        let Some(index) = state
            .roots
            .iter()
            .position(|entry| entry.live && entry.path == root)
        else {
            return;
        };
        let targets: Vec<Option<Target>> = state
            .decoder
            .pending
            .iter()
            .map(|((tid, _), _)| state.threads.get(tid).and_then(ThreadState::target))
            .collect();
        let mut keep = targets.iter();
        state
            .decoder
            .pending
            .retain(|_| keep.next().copied().flatten() != Some(Target::Root(index)));
        drop(state);
    }

    /// Starts the tracer, taking the state guard it must fill in.
    ///
    /// The launcher is `/bin/sh` holding at a one-line read: the tracer's pid exists, and is
    /// therefore nameable to `PR_SET_PTRACER`, before it can possibly have tried to attach. That
    /// permission is granted to exactly that child — never to every process on the machine — and
    /// nothing about the host's own ptrace configuration is changed.
    fn start(self: &Arc<Self>, mut state: MutexGuard<'_, State>) -> std::io::Result<()> {
        state.failure = None;
        // A host that released its last root and then opened another seed reuses this state, and a
        // stop flag left raised would make the classifier below exit before it read anything.
        let mut items = self
            .pipeline
            .items
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        items.stopping = false;
        items.work.clear();
        drop(items);

        let (mut child, mut output) = Self::launch()?;
        let tid = Arc::new(Mutex::new(None::<u32>));
        let reported = Arc::clone(&tid);
        let started = Arc::new((Mutex::new(false), Condvar::new()));
        let signal = Arc::clone(&started);
        let tracing = Arc::downgrade(self);
        // A weak handle: the decoder must not keep the tracing state alive, because the state's
        // own drop is what stops the tracer this thread is reading from.
        let reader = std::thread::Builder::new()
            .name("marsh-trace".to_string())
            .spawn(move || {
                *reported.lock().unwrap_or_else(PoisonError::into_inner) =
                    Some(crate::current_tid());
                let (lock, condition) = &*signal;
                *lock.lock().unwrap_or_else(PoisonError::into_inner) = true;
                condition.notify_all();
                drop(signal);
                pump(&tracing, &mut output);
            });
        // The state guard goes before the tracer does: a reader that already started takes it to
        // record the end of its output.
        let reader = match reader {
            Ok(reader) => reader,
            Err(error) => {
                drop(state);
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };

        let (lock, condition) = &*started;
        let mut ready = lock.lock().unwrap_or_else(PoisonError::into_inner);
        while !*ready {
            ready = condition
                .wait(ready)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(ready);
        let reader_tid = tid
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .unwrap_or(0);

        let classifier = {
            let tracing = Arc::downgrade(self);
            let pipeline = Arc::clone(&self.pipeline);
            std::thread::Builder::new()
                .name("marsh-classify".to_string())
                .spawn(move || sift(&tracing, &pipeline))
        };
        let classifier = match classifier {
            Ok(classifier) => classifier,
            Err(error) => {
                drop(state);
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(error);
            }
        };

        state.attached = Some(Attached {
            child,
            reader: Some(reader),
            reader_tid,
            classifier: Some(classifier),
        });
        drop(state);

        match self.prove_attachment() {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = self.stop(self.locked());
                Err(error)
            }
        }
    }

    /// Spawns the tracer and hands back its process and its output.
    ///
    /// The launcher is `/bin/sh` holding at a one-line read: the tracer's pid exists, and is
    /// therefore nameable to `PR_SET_PTRACER`, before it can possibly have tried to attach. That
    /// permission is granted to exactly that child — never to every process on the machine — and
    /// nothing about the host's own ptrace configuration is changed. The same barrier is what lets
    /// the output hold a pidfd for the tracer before the tracer can have written or exited.
    ///
    /// The trace goes to the launcher's stdout through `-o /proc/self/fd/1`, a named stream apart
    /// from stderr: `strace` reports a failed write of a named stream on stderr, and of stderr
    /// itself on nothing at all.
    ///
    /// # Errors
    ///
    /// Fails when the output cannot be set up, when the launcher cannot be spawned or named by a
    /// pidfd, and when this process may not be traced at all.
    fn launch() -> std::io::Result<(std::process::Child, TraceOutput)> {
        use std::os::unix::process::CommandExt;

        let (file, diagnostics, notifications) = trace_output_files()?;
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("IFS= read -r _; exec \"$@\"")
            .arg("marsh-trace")
            .arg("strace")
            .arg("--always-show-pid")
            .arg("-f")
            .arg("-y")
            .arg("-ttt")
            .arg("-q")
            .arg("-o")
            .arg("/proc/self/fd/1")
            .arg("-s")
            .arg("4096")
            .arg("-e")
            .arg(format!("trace={TRACE_SET}"))
            .arg("-p")
            .arg(std::process::id().to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(file.try_clone()?)
            .stderr(diagnostics.try_clone()?)
            // Its own group, so a signal aimed at a command's processes never reaches the tracer.
            .process_group(0);
        let spawned = command.spawn();
        // The command holds its own duplicates of both files until it goes.
        drop(command);
        let mut child = spawned?;
        let process = match open_process(child.id()) {
            Ok(process) => process,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let output = TraceOutput {
            file,
            diagnostics,
            notifications,
            process,
            position: 0,
            reclaimed: 0,
            exited: false,
            stop_requested: false,
        };

        let tracer = libc::pid_t::try_from(child.id()).unwrap_or(0);
        // SAFETY: `prctl(PR_SET_PTRACER, pid)` takes an integer and touches no memory.
        let permitted = unsafe { libc::prctl(libc::PR_SET_PTRACER, tracer) } == 0;
        let started = permitted
            .then(|| child.stdin.take())
            .flatten()
            .map(|mut stdin| {
                // Releases the barrier: `sh` returns from its read and `exec`s the tracer.
                stdin.write_all(b"\n")
            });
        match started {
            Some(Ok(())) => Ok((child, output)),
            started => {
                let error = std::io::Error::last_os_error();
                let _ = child.kill();
                let _ = child.wait();
                Err(match started {
                    Some(Err(error)) => error,
                    _ => failure(format!("this process cannot be traced: {error}")),
                })
            }
        }
    }

    /// Waits until a marker of this host's has come back through the tracer.
    ///
    /// Attachment is asynchronous: `PTRACE_SEIZE` of every thread of a live process takes as long
    /// as it takes, and a marker emitted before it lands is simply never printed. So the proof is
    /// retried rather than waited for once — until one comes back, which is what says this process
    /// is actually being traced, or the shared deadline runs out.
    ///
    /// # Errors
    ///
    /// Fails when the stream broke or nothing came back in time.
    fn prove_attachment(&self) -> std::io::Result<()> {
        let deadline = Instant::now() + DEADLINE;
        loop {
            match self.barrier(Duration::from_millis(50)) {
                Err(error) => return Err(error),
                Ok(true) => return Ok(()),
                Ok(false) if Instant::now() >= deadline => return Err(self.stalled()),
                Ok(false) => {}
            }
        }
    }

    /// Stops the tracer and joins both of its threads, taking the state guard.
    fn stop(&self, mut state: MutexGuard<'_, State>) -> std::io::Result<()> {
        let Some(mut attached) = state.attached.take() else {
            return Ok(());
        };
        state.threads.clear();
        state.scopes.clear();
        state.retiring.clear();
        state.decoder = Decoder::default();
        state.barriers.clear();
        drop(state);

        // `strace` detaches every tracee on a terminating signal, so the shells it was following
        // keep running; the installed builtin map may still hold this hook.
        let _ = attached.child.kill();
        let _ = attached.child.wait();
        let mut items = self
            .pipeline
            .items
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        items.stopping = true;
        items.work.clear();
        drop(items);
        self.pipeline.ready.notify_all();

        // Both threads upgrade a weak handle to do their work, so the last strong reference can
        // fall on either of them — and a thread that joined itself would deadlock. Killing the
        // tracer and raising the stop flag above is what ends them either way.
        let here = std::thread::current().id();
        let mut failed = false;
        for thread in [attached.reader.take(), attached.classifier.take()]
            .into_iter()
            .flatten()
        {
            if here == thread.thread().id() {
                continue;
            }
            failed |= thread.join().is_err();
        }
        if failed {
            return Err(failure("a tracing thread failed"));
        }
        Ok(())
    }

    /// Whether this host is tracing at all.
    pub(crate) fn attached(&self) -> bool {
        let state = self.locked();
        let attached = state.attached.is_some();
        drop(state);
        attached
    }

    /// Decodes one chunk and queues what it found. Nothing here waits on a consumer.
    ///
    /// Every drain marker is behind whatever this thread has not decoded yet, so it must never wait
    /// for longer than the state lock is held.
    fn decode(&self, chunk: &str) {
        let mut state = self.locked();
        let reader_tid = state
            .attached
            .as_ref()
            .map_or(0, |attached| attached.reader_tid);
        let prefix = self.prefix.as_str();
        let mut decoded: Vec<TraceLine> = Vec::new();
        state.decoder.feed(chunk, |line| decoded.push(line));

        let mut queued: Vec<Work> = Vec::new();
        for line in decoded {
            if line.tid == reader_tid {
                continue;
            }
            if let Some((id, marker)) = Self::marker(&line, prefix) {
                match marker {
                    // Queued rather than recorded here, so reaching it proves every line before it
                    // was *classified* and not merely decoded.
                    Marker::Barrier => queued.push(Work::Barrier(id)),
                    Marker::Enter => {
                        if let Some(target) = state.scopes.get(&id).copied() {
                            state.threads.entry(line.tid).or_default().stack.push(target);
                        }
                    }
                    Marker::Leave => {
                        if let Some(thread) = state.threads.get_mut(&line.tid) {
                            thread.stack.pop();
                        }
                    }
                }
                continue;
            }
            state.inherit(&line);
            // Two independent routes to a root, because neither alone is enough. The thread's
            // scope covers a call whose path is *relative* and only resolvable from where that
            // thread stands. The root's own path appearing in the line covers everything a caller
            // ran without a scope at all — brush's interactive loop runs its own lines — and is
            // exact, because a snapshot root is a directory no other shell writes into.
            let scoped = state.threads.get(&line.tid).and_then(ThreadState::target);
            if scoped == Some(Target::Internal) {
                // A boundary's own work names the snapshot tree by path, so the route below would
                // claim every file it walked. Only an explicit bracket can say otherwise.
                continue;
            }
            let mut targets: Vec<usize> = Vec::new();
            if let Some(Target::Root(index)) = scoped {
                targets.push(index);
            }
            for (index, entry) in state.roots.iter().enumerate() {
                if entry.live && !targets.contains(&index) && mentions(&line, &entry.path_text) {
                    targets.push(index);
                }
            }
            for index in targets {
                queued.push(Work::Line(index, line.clone()));
            }
        }
        drop(state);

        if queued.is_empty() {
            return;
        }
        let mut items = self
            .pipeline
            .items
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        items.work.extend(queued);
        drop(items);
        self.pipeline.ready.notify_all();
    }

    /// Classifies one batch of queued work, in order.
    ///
    /// Observers are called with no lock of this module held, which is what lets one take the
    /// seed's authority and wait there for as long as another principal's boundary needs.
    fn classify(&self, batch: Vec<Work>) {
        let mut reached: Vec<u64> = Vec::new();
        let mut refused: Option<String> = None;
        let mut touched: Vec<usize> = Vec::new();
        for item in batch {
            let (index, line) = match item {
                Work::Barrier(id) => {
                    reached.push(id);
                    continue;
                }
                Work::Line(index, line) => (index, line),
            };
            let state = self.locked();
            let observe: Option<TraceObserver> = state
                .roots
                .get(index)
                .filter(|entry| entry.live)
                .map(|entry| Arc::clone(&entry.observe));
            drop(state);
            let Some(observe) = observe else {
                continue;
            };
            if let Err(error) = observe(&line) {
                refused.get_or_insert_with(|| error.to_string());
            }
            if !touched.contains(&index) {
                touched.push(index);
            }
        }

        // The observer may have concluded its shell must stop; waking is this caller's, after
        // every lock the module holds is released.
        let mut state = self.locked();
        if let Some(cause) = refused {
            state.failure.get_or_insert(cause);
        }
        let announce = !reached.is_empty() || state.failure.is_some();
        state.barriers.extend(reached);
        let mut wakers: Vec<Waker> = Vec::new();
        for index in touched {
            if let Some(entry) = state.roots.get_mut(index)
                && entry.interrupted.load(Ordering::Acquire)
            {
                wakers.append(&mut entry.waiters);
            }
        }
        drop(state);
        for waker in wakers {
            waker.wake();
        }
        if announce {
            self.progress.notify_all();
        }
    }

    /// The scope marker `line` is, when it is one of this host's.
    fn marker(line: &TraceLine, prefix: &str) -> Option<(u64, Marker)> {
        let Call::Syscall { name, args, .. } = &line.call else {
            return None;
        };
        if name != "readlink" && name != "readlinkat" {
            return None;
        }
        let args = split_args(args);
        let path = args
            .iter()
            .find_map(|arg| parse_quoted(arg).filter(|path| path.starts_with(prefix)))?;
        let rest = path.strip_prefix(prefix)?;
        let (id, kind) = rest.split_once('/')?;
        let marker = match kind {
            "enter" => Marker::Enter,
            "leave" => Marker::Leave,
            "barrier" => Marker::Barrier,
            _ => return None,
        };
        Some((id.parse().ok()?, marker))
    }

    /// Records `cause` as the first thing that broke the evidence stream.
    fn fail(&self, cause: String) {
        let mut state = self.locked();
        state.failure.get_or_insert(cause);
        drop(state);
        self.progress.notify_all();
    }
}

/// Drains the tracer's output until it ends, decoding as it goes.
///
/// A free function over a [`std::sync::Weak`] rather than a method: the decoder thread must not
/// keep the tracing state alive, because dropping that state is what stops the tracer this thread
/// reads from. The read buffer is reused and the decoder keeps the remainder of a split line, so a
/// chunk boundary in the middle of a syscall is not an end of input and nothing is scanned twice.
///
/// Output that became unreadable is a failure of every root at once, so the tracer is stopped
/// with it: nothing would read what it kept appending.
fn pump(tracing: &std::sync::Weak<Tracing>, output: &mut TraceOutput) {
    let mut buffer = vec![0u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = match output.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                if let Some(tracing) = tracing.upgrade() {
                    tracing.fail(format!("the tracer's output was lost: {error}"));
                }
                if let Err(error) = output.request_stop()
                    && let Some(tracing) = tracing.upgrade()
                {
                    let mut state = tracing.locked();
                    if let Some(cause) = state.failure.take() {
                        state.failure = Some(format!("{cause}; could not stop tracer: {error}"));
                    }
                    drop(state);
                }
                return;
            }
        };
        let chunk = String::from_utf8_lossy(buffer.get(..read).unwrap_or_default());
        let Some(tracing) = tracing.upgrade() else {
            return;
        };
        tracing.decode(&chunk);
        drop(tracing);
    }
    // End of output while a root is still registered is the tracer having died under us.
    let Some(tracing) = tracing.upgrade() else {
        return;
    };
    let mut state = tracing.locked();
    if state.attached.is_some() && state.failure.is_none() {
        state.failure = Some("the tracer exited before its last root was released".to_string());
    }
    drop(state);
    tracing.progress.notify_all();
}

/// Classifies queued work until the tracer is stopped and the queue is empty.
///
/// It holds only the pipeline while it waits, never the tracing state: a waiter that kept that
/// state alive would keep the destructor that stops it from ever running.
fn sift(tracing: &std::sync::Weak<Tracing>, pipeline: &Arc<Pipeline>) {
    loop {
        let batch = {
            let mut items = pipeline
                .items
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            while items.work.is_empty() {
                if items.stopping {
                    return;
                }
                items = pipeline
                    .ready
                    .wait(items)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            items.work.drain(..).collect::<Vec<_>>()
        };
        let Some(tracing) = tracing.upgrade() else {
            return;
        };
        tracing.classify(batch);
        drop(tracing);
    }
}

impl State {
    /// Propagates attribution across a fork, and forgets a thread that exited.
    ///
    /// A `clone` that creates a *thread* inherits nothing: the new thread will be handed its own
    /// scope markers by whatever work it is given, and copying the creator's current scope would
    /// attribute another shell's task to this one. A fork that creates a *process* inherits, which
    /// is how an external command's syscalls reach the shell that spawned it.
    fn inherit(&mut self, line: &TraceLine) {
        let Call::Syscall {
            name, args, ret, ..
        } = &line.call
        else {
            self.threads.remove(&line.tid);
            return;
        };
        if !matches!(name.as_str(), "clone" | "clone3" | "fork" | "vfork") || *ret <= 0 {
            return;
        }
        if args.contains("CLONE_THREAD") {
            return;
        }
        let inherited = self.threads.get(&line.tid).and_then(ThreadState::target);
        if let Ok(child) = u32::try_from(*ret) {
            self.threads.insert(
                child,
                ThreadState {
                    stack: Vec::new(),
                    inherited,
                },
            );
        }
    }

    /// The scope each still-unfinished call belongs to.
    fn pending_targets(&self) -> impl Iterator<Item = Target> + '_ {
        self.decoder
            .pending
            .iter()
            .filter_map(|((tid, _), _)| self.threads.get(tid).and_then(ThreadState::target))
    }
}

impl Drop for Tracing {
    /// Stops the tracer before the state the decoder borrows goes away.
    fn drop(&mut self) {
        let state = self.locked();
        let _ = self.stop(state);
    }
}

/// One bracket of work, attributed to the snapshot it was allocated for.
///
/// The id is reused by every enter/leave pair this scope emits: a future is polled many times and
/// each poll is a separate bracket, because between two polls the thread belongs to whatever else
/// the runtime put on it.
pub struct TraceScope {
    /// The host's tracing state.
    tracing: Arc<Tracing>,
    /// This scope's id in the shared series.
    id: u64,
}

impl TraceScope {
    /// Enters the scope, leaving it when the returned guard is dropped.
    #[must_use]
    pub fn enter(&self) -> TraceScopeGuard<'_> {
        self.tracing.mark(self.id, Marker::Enter);
        TraceScopeGuard { scope: self }
    }
}

impl Drop for TraceScope {
    /// Retires the id once a drain proves its markers were decoded.
    fn drop(&mut self) {
        let mut state = self.tracing.locked();
        state.retiring.push(self.id);
        drop(state);
    }
}

/// The open half of one bracket.
pub struct TraceScopeGuard<'scope> {
    /// The scope this guard closes.
    scope: &'scope TraceScope,
}

impl Drop for TraceScopeGuard<'_> {
    fn drop(&mut self) {
        self.scope.tracing.mark(self.scope.id, Marker::Leave);
    }
}

/// A future whose every poll is bracketed by its scope's markers.
///
/// Per poll, not per future: a runtime may poll two shells' futures on one thread, and a bracket
/// spanning the gap between two polls would attribute the other shell's syscalls to this one.
pub struct Scoped<F> {
    /// The future being observed.
    inner: F,
    /// The bracket, or `None` when nothing is being traced.
    scope: Option<TraceScope>,
}

impl<F: Future> Scoped<F> {
    /// Wraps `inner` in `scope`.
    pub const fn new(inner: F, scope: Option<TraceScope>) -> Self {
        Self { inner, scope }
    }
}

impl<F: Future> Future for Scoped<F> {
    type Output = F::Output;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // SAFETY: a structural projection. `inner` is never moved out of the pinned `Self`, and
        // `scope` is `Unpin` — it is only read to emit two syscalls.
        let this = unsafe { self.get_unchecked_mut() };
        let guard = this.scope.as_ref().map(TraceScope::enter);
        // SAFETY: `this` came from a `Pin<&mut Self>` that was never moved, so its `inner` field
        // is structurally pinned too.
        let polled = unsafe { std::pin::Pin::new_unchecked(&mut this.inner) }.poll(context);
        drop(guard);
        polled
    }
}

/// Whether `line` names something under `root`.
///
/// A substring test over the text the tracer printed, deliberately: it is an over-approximation
/// the classifier then resolves exactly, and it costs one scan of a line that has already been
/// decoded. Anything narrower would have to re-implement the path resolution that belongs to the
/// consumer.
fn mentions(line: &TraceLine, root: &str) -> bool {
    match &line.call {
        Call::Exited { .. } => false,
        Call::Syscall { args, ret_path, .. } => {
            args.contains(root) || ret_path.as_ref().is_some_and(|path| path.contains(root))
        }
    }
}

/// Opens a pidfd for the process `pid`.
///
/// A caller must own `pid` as an unreaped child, or verify what the descriptor names before
/// signalling through it: a pidfd is what keeps a later signal from reaching a reused pid.
fn open_process(pid: u32) -> std::io::Result<OwnedFd> {
    let pid = libc::pid_t::try_from(pid)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let flags: libc::c_uint = 0;
    // SAFETY: `pidfd_open(pid, flags)` takes two integers and touches no memory of this process.
    let opened = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, flags) };
    if opened < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let descriptor = std::os::fd::RawFd::try_from(opened)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
    // SAFETY: the kernel just returned `descriptor` as a new file descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

/// Sends `SIGKILL` to the process `process` names. A process already gone is already stopped.
fn kill_process(process: BorrowedFd<'_>) -> std::io::Result<()> {
    let flags: libc::c_uint = 0;
    loop {
        // SAFETY: `pidfd_send_signal(fd, SIGKILL, NULL, flags)` reads no memory: the only pointer
        // argument is null, which asks the kernel to synthesise the signal's information itself.
        let sent = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                process.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                flags,
            )
        };
        if sent == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::ESRCH) => return Ok(()),
            _ => return Err(error),
        }
    }
}

/// The error every tracing failure reaches a caller as.
fn failure(cause: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(format!("file access tracing failed: {cause}"))
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;
    use std::sync::atomic::AtomicUsize;

    /// Decodes `text` whole, the way a single chunk from the tracer arrives.
    fn decode(text: &str) -> Vec<TraceLine> {
        let mut decoder = Decoder::default();
        let mut lines = Vec::new();
        decoder.feed(text, |line| lines.push(line));
        lines
    }

    fn syscall(line: &TraceLine) -> (&str, &str, i64, Option<&str>) {
        match &line.call {
            Call::Syscall {
                name,
                args,
                ret,
                ret_path,
            } => (name, args, *ret, ret_path.as_deref()),
            Call::Exited { .. } => panic!("expected a syscall"),
        }
    }

    #[test]
    fn parses_openat_with_return_decoration() {
        let lines = decode(
            "7844  1788295173.846003 openat(AT_FDCWD</work>, \"a.txt\", O_RDONLY) = 3</work/a.txt>\n",
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].tid, 7844);
        assert_eq!(lines[0].ts_us, 1_788_295_173_846_003);
        let (name, args, ret, ret_path) = syscall(&lines[0]);
        assert_eq!((name, ret, ret_path), ("openat", 3, Some("/work/a.txt")));
        assert_eq!(
            split_args(args),
            vec!["AT_FDCWD</work>", "\"a.txt\"", "O_RDONLY"]
        );
    }

    #[test]
    fn parses_failed_call_as_negative_return() {
        let lines = decode(
            "12  1788295173.851293 openat(AT_FDCWD</work>, \"missing\", O_RDONLY) = -1 ENOENT (No such file or directory)\n",
        );
        let (name, _, ret, ret_path) = syscall(&lines[0]);
        assert_eq!((name, ret, ret_path), ("openat", -1, None));
    }

    #[test]
    fn rejoins_unfinished_and_resumed_pair() {
        let lines = decode(concat!(
            "31  1788295173.886545 openat(AT_FDCWD</work>, \"slow.txt\" <unfinished ...>\n",
            "32  1788295173.899935 clone(child_stack=NULL, flags=SIGCHLD) = 33\n",
            "31  1788295173.900045 <... openat resumed>, O_WRONLY|O_CREAT, 0666) = 4</work/slow.txt>\n",
        ));
        assert_eq!(lines.len(), 2, "the unfinished line is not a separate call");
        let (name, args, ret, ret_path) = syscall(&lines[1]);
        assert_eq!((name, ret, ret_path), ("openat", 4, Some("/work/slow.txt")));
        assert_eq!(args, "AT_FDCWD</work>, \"slow.txt\", O_WRONLY|O_CREAT, 0666");
        assert_eq!(
            lines[1].ts_us, 1_788_295_173_886_545,
            "a rejoined call is stamped when it entered, not when it resumed"
        );
    }

    /// A process killed inside a call leaves the call unfinished for good: it is never a
    /// decoded line whose arguments carry the marker, and it stays pending, which is what makes
    /// its unknown effect visible to the boundary.
    #[test]
    fn a_call_killed_before_it_returned_stays_unfinished() {
        let mut decoder = Decoder::default();
        let mut lines = Vec::new();
        decoder.feed(
            "31  1788295173.886545 newfstatat(AT_FDCWD</work>, \"/work/dir/\" <unfinished ...>) = ?\n",
            |line| lines.push(line),
        );
        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!(decoder.pending.len(), 1);
    }

    /// The tracer's output arrives in whatever sizes a read hands over, and a chunk boundary
    /// falls wherever it falls. A split line is not an end of input, and the two halves must
    /// produce exactly one call rather than two broken ones.
    #[test]
    fn a_line_split_across_chunks_decodes_once() {
        let mut decoder = Decoder::default();
        let mut lines = Vec::new();
        let whole =
            "77  1788295173.846003 openat(AT_FDCWD</w>, \"a.txt\", O_RDWR) = 3</w/a.txt>\n";
        let (head, tail) = whole.split_at(40);
        decoder.feed(head, |line| lines.push(line));
        assert!(lines.is_empty(), "half a line is not a call: {lines:?}");
        decoder.feed(tail, |line| lines.push(line));
        assert_eq!(lines.len(), 1);
        assert_eq!(syscall(&lines[0]).3, Some("/w/a.txt"));
    }

    #[test]
    fn parses_exit_records_and_drops_signals() {
        let lines = decode(concat!(
            "50  1788295173.100000 clone(child_stack=NULL, flags=CLONE_CHILD_CLEARTID|SIGCHLD) = 51\n",
            "51  1788295173.200000 exit_group(0)                     = ?\n",
            "51  1788295173.300000 --- SIGCHLD {si_signo=SIGCHLD, si_code=CLD_EXITED} ---\n",
            "51  1788295173.400000 +++ exited with 0 +++\n",
            "50  1788295173.500000 +++ exited with 3 +++\n",
        ));
        assert_eq!(lines.len(), 4, "signal lines are dropped");
        assert_eq!(syscall(&lines[1]).2, -1, "`= ?` never reads as success");
        assert!(matches!(lines[2].call, Call::Exited { status: 0 }));
        assert!(matches!(lines[3].call, Call::Exited { status: 3 }));
    }

    #[test]
    fn keeps_parentheses_inside_quoted_arguments() {
        let lines = decode(
            "60  1788295173.846003 openat(AT_FDCWD</work>, \"weird)name.txt\", O_RDONLY) = 5</work/weird)name.txt>\n",
        );
        let (_, args, ret, ret_path) = syscall(&lines[0]);
        assert_eq!((ret, ret_path), (5, Some("/work/weird)name.txt")));
        assert_eq!(
            parse_quoted(split_args(args)[1]).as_deref(),
            Some("weird)name.txt")
        );
    }

    /// An escaped path has to come back as the bytes the kernel saw, or the resource it names is
    /// a different one.
    #[test]
    fn decodes_escaped_string_arguments() {
        assert_eq!(
            parse_quoted("\"a\\nb\\tc\\\\d\\\"e\"").as_deref(),
            Some("a\nb\tc\\d\"e")
        );
        assert_eq!(parse_quoted("\"caf\\303\\251\"").as_deref(), Some("café"));
        assert_eq!(parse_quoted("O_RDONLY"), None, "a flag is not a string");
    }

    /// `-s` bounds what the tracer prints. A truncated path is unresolvable, and guessing from its
    /// visible prefix would name a resource the command never touched.
    #[test]
    fn a_truncated_string_is_not_a_path() {
        assert_eq!(parse_quoted("\"/work/very-long-na\"..."), None);
        assert_eq!(
            parse_quoted("\"/work/short\"").as_deref(),
            Some("/work/short")
        );
    }

    /// Two threads' calls interleave freely, and each thread's own entry order is program order.
    #[test]
    fn interleaved_threads_keep_their_own_order() {
        let lines = decode(concat!(
            "10  1788295173.100000 openat(AT_FDCWD</w>, \"a\" <unfinished ...>\n",
            "11  1788295173.110000 openat(AT_FDCWD</w>, \"b\" <unfinished ...>\n",
            "11  1788295173.120000 <... openat resumed>, O_RDONLY) = 4</w/b>\n",
            "10  1788295173.130000 <... openat resumed>, O_RDONLY) = 3</w/a>\n",
        ));
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].tid, lines[0].ts_us), (11, 1_788_295_173_110_000));
        assert_eq!((lines[1].tid, lines[1].ts_us), (10, 1_788_295_173_100_000));
    }

    /// Attribution follows a forked process and stops at a thread: a runtime worker thread gets
    /// its own scope markers, and copying the creator's would credit another shell's task.
    #[test]
    fn a_fork_inherits_attribution_and_a_thread_does_not() {
        let mut state = State::default();
        state
            .threads
            .entry(10)
            .or_default()
            .stack
            .push(Target::Root(2));

        state.inherit(&decode("10 1788295173.100000 clone(child_stack=NULL, flags=SIGCHLD) = 20\n")[0]);
        state.inherit(
            &decode(
                "10 1788295173.200000 clone(child_stack=0x7f, flags=CLONE_VM|CLONE_THREAD|CLONE_SIGHAND) = 21\n",
            )[0],
        );

        assert_eq!(
            state.threads.get(&20).and_then(ThreadState::target),
            Some(Target::Root(2)),
            "an external command belongs to the shell that spawned it"
        );
        assert!(
            !state.threads.contains_key(&21),
            "a new runtime thread inherits nothing"
        );
    }

    /// A marker is recognised by this host's own prefix and nothing else: another marsh on the
    /// same machine emits the same shape of path under a different pid.
    #[test]
    fn markers_are_recognised_by_this_hosts_prefix() {
        let tracing = Tracing::new();
        let prefix = tracing.prefix.clone();
        let line = &decode(&format!(
            "9 1788295173.100000 readlink(\"{prefix}7/enter\", \"\", 1) = -1 ENOENT (No such file or directory)\n"
        ))[0];
        assert_eq!(
            Tracing::marker(line, &prefix).map(|(id, kind)| (id, kind == Marker::Enter)),
            Some((7, true))
        );

        let foreign = &decode(
            "9 1788295173.100000 readlink(\"/proc/self/marsh-trace/999999-7/enter\", \"\", 1) = -1 ENOENT (No such file or directory)\n",
        )[0];
        assert!(Tracing::marker(foreign, &prefix).is_none());
    }

    /// Names the test a re-executed child runs the scenario of; set only in that child.
    const TRACE_OUTPUT_CHILD: &str = "MARSH_TRACE_OUTPUT_CHILD";

    /// The supervising process, which the child requires to still be its parent.
    const TRACE_OUTPUT_PARENT: &str = "MARSH_TRACE_OUTPUT_PARENT";

    /// A supervised child's whole budget, well above every deadline inside its scenario.
    const SUPERVISED_DEADLINE: Duration = Duration::from_secs(30);

    /// How many real `readlink`s a pressure burst issues: megabytes of trace output, far more
    /// than any pipe buffers.
    const PRESSURE_CALLS: usize = 32_768;

    /// A registered snapshot root whose never-existing `pressure` path is counted as calls naming
    /// it are classified.
    struct PressureRoot {
        /// Removed with the root.
        _dir: tempfile::TempDir,
        /// The canonical registered root.
        root: PathBuf,
        /// `<root>/pressure`.
        pressure: std::ffi::CString,
        /// `readlink`/`readlinkat` calls naming `pressure` that reached the observer.
        calls: Arc<AtomicUsize>,
    }

    impl PressureRoot {
        /// Registers a fresh root, which waits for the tracer's attachment proof.
        fn register(tracing: &Arc<Tracing>) -> Self {
            let dir = tempfile::tempdir().expect("a temporary root");
            let root = dir.path().canonicalize().expect("a canonical root");
            let target = root.join("pressure").to_string_lossy().into_owned();
            let pressure = std::ffi::CString::new(target.clone()).expect("a path without NUL");
            let calls = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&calls);
            let observe: TraceObserver = Arc::new(move |line: &TraceLine| {
                if let Call::Syscall { name, args, .. } = &line.call
                    && matches!(name.as_str(), "readlink" | "readlinkat")
                    && split_args(args)
                        .iter()
                        .any(|arg| parse_quoted(arg).as_deref() == Some(target.as_str()))
                {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
                Ok(())
            });
            tracing
                .register_root(&root, Arc::new(AtomicBool::new(false)), observe)
                .expect("the tracer attaches");
            Self {
                _dir: dir,
                root,
                pressure,
                calls,
            }
        }

        /// How many calls naming `pressure` were classified so far.
        fn observed(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    /// Issues `calls` real `readlink`s of `path`, each of which must fail with `ENOENT`.
    fn pressure(path: &std::ffi::CStr, calls: usize) {
        let mut sink = [0u8; 1];
        for _ in 0..calls {
            // SAFETY: `path` is NUL-terminated and `sink` is a one-byte buffer of this frame;
            // `readlink` writes at most `sink.len()` bytes into it.
            let result =
                unsafe { libc::readlink(path.as_ptr(), sink.as_mut_ptr().cast(), sink.len()) };
            let error = std::io::Error::last_os_error();
            assert!(
                result == -1 && error.raw_os_error() == Some(libc::ENOENT),
                "readlink({path:?}) = {result}: {error}"
            );
        }
    }

    /// Tells the supervisor which tracer this generation runs, and waits until it holds a pidfd
    /// for it.
    fn announce_tracer(tracing: &Tracing) -> u32 {
        let state = tracing.locked();
        let pid = state.attached.as_ref().map(|attached| attached.child.id());
        drop(state);
        let pid = pid.expect("a started tracer");
        let mut stdout = std::io::stdout().lock();
        write!(stdout, "\nTRACE_PID={pid}\n").expect("the announcement");
        stdout.flush().expect("the flushed announcement");
        drop(stdout);
        let mut reply = String::new();
        std::io::stdin()
            .read_line(&mut reply)
            .expect("the supervisor's reply");
        assert_eq!(reply, "READY\n", "the supervisor did not adopt the tracer");
        pid
    }

    /// Arms the supervised child to die with its supervisor, before any tracer exists.
    fn enter_supervised_child() {
        let parent: libc::pid_t = std::env::var(TRACE_OUTPUT_PARENT)
            .expect("the supervisor's pid")
            .parse()
            .expect("a numeric pid");
        let signal = libc::c_ulong::try_from(libc::SIGKILL).expect("a signal number");
        // SAFETY: `prctl(PR_SET_PDEATHSIG, signal)` takes integers and touches no memory.
        let armed = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, signal) };
        assert_eq!(
            armed,
            0,
            "PR_SET_PDEATHSIG: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: `getppid` takes no arguments and cannot fail.
        if unsafe { libc::getppid() } != parent {
            std::process::exit(1);
        }
    }

    /// Waits until `process` is readable — for a pidfd, until the process exited — or `deadline`.
    fn wait_readable(process: BorrowedFd<'_>, deadline: Instant) -> bool {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout = libc::c_int::try_from(remaining.as_micros().div_ceil(1000))
                .unwrap_or(libc::c_int::MAX);
            let mut entry = libc::pollfd {
                fd: process.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `entry` is one initialised `pollfd` of this frame, and the count says one.
            let ready = unsafe { libc::poll(&raw mut entry, 1, timeout) };
            if ready > 0 {
                return true;
            }
            if ready == 0 && Instant::now() >= deadline {
                return false;
            }
            if ready < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return false;
            }
        }
    }

    /// A pidfd for `pid`, verified after opening it to name a child of `parent`.
    fn adopt_child(pid: u32, parent: u32) -> std::io::Result<OwnedFd> {
        let process = open_process(pid)?;
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
        let ppid = status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:"))
            .and_then(|value| value.trim().parse::<u32>().ok());
        if ppid == Some(parent) {
            Ok(process)
        } else {
            Err(std::io::Error::other(format!(
                "{pid} is not a child of {parent}"
            )))
        }
    }

    /// Every child of every task of the still-unreaped `pid`.
    fn children_of(pid: u32) -> Vec<u32> {
        let mut children = Vec::new();
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            return children;
        };
        for task in tasks.flatten() {
            let Ok(listed) = std::fs::read_to_string(task.path().join("children")) else {
                continue;
            };
            children.extend(
                listed
                    .split_whitespace()
                    .filter_map(|child| child.parse::<u32>().ok()),
            );
        }
        children.sort_unstable();
        children.dedup();
        children
    }

    /// Appends where each of `pid`'s tasks is: its name, scheduler state and kernel wait channel.
    fn describe_tasks(label: &str, pid: u32, out: &mut String) {
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            let _ = writeln!(out, "{label} {pid}: no tasks");
            return;
        };
        for task in tasks.flatten() {
            let path = task.path();
            let read = |name: &str| {
                std::fs::read_to_string(path.join(name))
                    .map_or_else(|error| format!("<{error}>"), |text| text.trim().to_string())
            };
            let stat = read("stat");
            let state = stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .unwrap_or("?");
            let _ = writeln!(
                out,
                "{label} {pid} task {} ({}): state {state}, wchan {}",
                task.file_name().to_string_lossy(),
                read("comm"),
                read("wchan"),
            );
        }
    }

    /// What a supervised child did.
    struct Supervised {
        /// How it ended.
        status: std::process::ExitStatus,
        /// Every line it printed to stdout.
        lines: Vec<String>,
        /// Everything it printed to stderr.
        stderr: String,
        /// Why supervision had to intervene, with where every task was blocked at the time.
        problem: Option<String>,
    }

    impl std::fmt::Display for Supervised {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            writeln!(f, "{}", self.status)?;
            if let Some(problem) = &self.problem {
                writeln!(f, "{problem}")?;
            }
            writeln!(f, "--- stdout\n{}", self.lines.join("\n"))?;
            write!(f, "--- stderr\n{}", self.stderr)
        }
    }

    /// Runs `scenario` in a re-executed copy of this test binary under a supervisor that is never
    /// traced itself.
    ///
    /// A tracer that stopped making progress leaves its tracee stopped with it, so the scenario
    /// cannot run in the test runner's own process. The supervisor owns the child, a pidfd for
    /// every tracer the child announced, and the deadline; on expiry it records where every task
    /// was blocked before it signals anything.
    fn run_isolated_trace_test(test_name: &str, expected_generations: usize, scenario: fn()) {
        if let Some(requested) = std::env::var_os(TRACE_OUTPUT_CHILD) {
            assert_eq!(requested, test_name, "a supervised child ran another test");
            enter_supervised_child();
            scenario();
            let mut stdout = std::io::stdout().lock();
            write!(stdout, "\nTRACE_SCENARIO_DONE\n").expect("the completion marker");
            stdout.flush().expect("the flushed completion marker");
            return;
        }
        let report = supervise(test_name);
        let announced = report
            .lines
            .iter()
            .filter(|line| line.starts_with("TRACE_PID="))
            .count();
        let done: Vec<usize> = report
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| *line == "TRACE_SCENARIO_DONE")
            .map(|(index, _)| index)
            .collect();
        let last_announcement = report
            .lines
            .iter()
            .rposition(|line| line.starts_with("TRACE_PID="));
        let completed =
            matches!(done.as_slice(), [done] if last_announcement.is_none_or(|last| last < *done));
        assert!(
            report.problem.is_none()
                && report.status.success()
                && announced == expected_generations
                && completed,
            "supervised {test_name}, expecting {expected_generations} tracer generations: {report}"
        );
    }

    /// Runs `test_name` alone in a child and supervises it to the end.
    fn supervise(test_name: &str) -> Supervised {
        use std::io::BufRead as _;

        let deadline = Instant::now() + SUPERVISED_DEADLINE;
        let mut child =
            std::process::Command::new(std::env::current_exe().expect("this test binary"))
                .args(["--exact", test_name, "--nocapture"])
                .env(TRACE_OUTPUT_CHILD, test_name)
                .env(TRACE_OUTPUT_PARENT, std::process::id().to_string())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("a supervised child");
        let child_pid = child.id();
        let process = open_process(child_pid).expect("a pidfd for the unreaped child");

        let (sender, received) = std::sync::mpsc::channel::<String>();
        let stdout = child.stdout.take().expect("the child's stdout");
        let stdout_reader = std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut stderr = child.stderr.take().expect("the child's stderr");
        let stderr_reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        });

        let mut stdin = child.stdin.take();
        let mut lines = Vec::new();
        let mut tracers: Vec<(u32, OwnedFd)> = Vec::new();
        let mut problem = None;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                problem = Some("the child missed its deadline".to_string());
                break;
            };
            let line = match received.recv_timeout(remaining) {
                Ok(line) => line,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    problem = Some("the child missed its deadline".to_string());
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if let Some(pid) = line.strip_prefix("TRACE_PID=") {
                let adopted = pid
                    .parse::<u32>()
                    .map_err(std::io::Error::other)
                    .and_then(|pid| adopt_child(pid, child_pid).map(|fd| (pid, fd)));
                match adopted {
                    Ok(tracer) => tracers.push(tracer),
                    Err(error) => {
                        problem = Some(format!("tracer {pid} could not be adopted: {error}"));
                    }
                }
                let replied = stdin
                    .as_mut()
                    .map(|stdin| stdin.write_all(b"READY\n").and_then(|()| stdin.flush()));
                if let Some(Err(error)) = replied {
                    problem.get_or_insert_with(|| format!("the child stopped listening: {error}"));
                }
            }
            lines.push(line);
            if problem.is_some() {
                break;
            }
        }
        if problem.is_none() && !wait_readable(process.as_fd(), deadline) {
            problem = Some("the child closed its output but missed its deadline".to_string());
        }

        if let Some(problem) = problem.as_mut() {
            intervene(child_pid, process.as_fd(), &tracers, problem);
        }
        drop(stdin);
        let status = child.wait().expect("the supervised child is reaped");
        let _ = stdout_reader.join();
        lines.extend(received.try_iter());
        let stderr = stderr_reader.join().unwrap_or_default();
        Supervised {
            status,
            lines,
            stderr,
            problem,
        }
    }

    /// Records where every task of the stalled child `child_pid` and its tracers is blocked, then
    /// kills the tracers and, failing a prompt exit, the child.
    ///
    /// Diagnosis comes before any signal: the state worth reporting is the stalled one. Only the
    /// announced tracers' pidfds and pidfds verified to name children of the still-unreaped child
    /// are signalled.
    fn intervene(
        child_pid: u32,
        process: BorrowedFd<'_>,
        tracers: &[(u32, OwnedFd)],
        problem: &mut String,
    ) {
        let mut diagnosis = String::new();
        if let Some((pid, _)) = tracers.last() {
            describe_tasks("tracer", *pid, &mut diagnosis);
        }
        describe_tasks("child", child_pid, &mut diagnosis);
        let candidates: Vec<(u32, OwnedFd)> = children_of(child_pid)
            .into_iter()
            .filter_map(|pid| adopt_child(pid, child_pid).ok().map(|fd| (pid, fd)))
            .collect();
        for (pid, _) in &candidates {
            if !tracers.iter().any(|(known, _)| known == pid) {
                describe_tasks("unannounced child", *pid, &mut diagnosis);
            }
        }
        let _ = write!(problem, "\n{diagnosis}");
        for (_, owned) in tracers.iter().chain(&candidates) {
            let _ = kill_process(owned.as_fd());
        }
        if !wait_readable(process, Instant::now() + Duration::from_secs(2)) {
            let _ = kill_process(process);
        }
    }

    /// The tracer writes into a stream the host drains, and one of the host's own threads holding
    /// the tracing state must not stop it: the classifier and the traced host take that lock too,
    /// and a tracer that waits for the reader waits for them. Every call of a state-held burst is
    /// still observed once the lock is released, and the next tracer generation works the same.
    #[test]
    fn trace_output_progresses_while_tracing_state_is_locked() {
        run_isolated_trace_test(
            "strace::tests::trace_output_progresses_while_tracing_state_is_locked",
            2,
            || {
                let tracing = Arc::new(Tracing::new());
                let first = PressureRoot::register(&tracing);
                announce_tracer(&tracing);
                let state = tracing.locked();
                pressure(&first.pressure, PRESSURE_CALLS);
                drop(state);
                tracing.drain().expect("a drain after a state-held burst");
                assert_eq!(first.observed(), PRESSURE_CALLS);
                tracing
                    .unregister_root(&first.root)
                    .expect("the first tracer stops");

                let second = PressureRoot::register(&tracing);
                announce_tracer(&tracing);
                pressure(&second.pressure, 1);
                tracing.drain().expect("a drain of the second generation");
                assert_eq!(second.observed(), 1);
                tracing
                    .unregister_root(&second.root)
                    .expect("the second tracer stops");
            },
        );
    }

    /// A tracer that can no longer write its trace complains on its diagnostic stream and keeps
    /// tracing; the host must take that as the end of the evidence. The tracer's own file size
    /// limit makes a real write of the trace fail, `SIGXFSZ` is ignored so the kernel does not end
    /// it first, and the host is what must kill it — while every drain afterwards keeps failing.
    #[test]
    fn trace_output_write_failure_stops_tracer_and_fails_drain() {
        run_isolated_trace_test(
            "strace::tests::trace_output_write_failure_stops_tracer_and_fails_drain",
            1,
            || {
                const LIMIT: libc::rlim_t = 1024 * 1024;
                // SAFETY: `signal(SIGXFSZ, SIG_IGN)` installs no handler and touches no memory.
                let previous = unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
                assert_ne!(
                    previous,
                    libc::SIG_ERR,
                    "{}",
                    std::io::Error::last_os_error()
                );

                let tracing = Arc::new(Tracing::new());
                let root = PressureRoot::register(&tracing);
                let pid = announce_tracer(&tracing);
                let tracer = open_process(pid).expect("a pidfd for the unreaped tracer");
                let target = libc::pid_t::try_from(pid).expect("a pid");
                let mut current = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                // SAFETY: `prlimit` only writes the tracer's current limit into `current`, a
                // local of this frame; the new-limit pointer is null.
                let read = unsafe {
                    libc::prlimit(
                        target,
                        libc::RLIMIT_FSIZE,
                        std::ptr::null(),
                        &raw mut current,
                    )
                };
                assert_eq!(read, 0, "{}", std::io::Error::last_os_error());
                assert!(
                    current.rlim_max >= LIMIT,
                    "the tracer's hard file size limit {} is below {LIMIT}",
                    current.rlim_max
                );
                let lowered = libc::rlimit {
                    rlim_cur: LIMIT,
                    rlim_max: current.rlim_max,
                };
                // SAFETY: `prlimit` only reads `lowered`, a local of this frame; the old-limit
                // pointer is null.
                let set = unsafe {
                    libc::prlimit(
                        target,
                        libc::RLIMIT_FSIZE,
                        &raw const lowered,
                        std::ptr::null_mut(),
                    )
                };
                assert_eq!(set, 0, "{}", std::io::Error::last_os_error());

                tracing.drain().expect("a drain under the lowered limit");
                pressure(&root.pressure, PRESSURE_CALLS);
                assert!(
                    wait_readable(tracer.as_fd(), Instant::now() + Duration::from_secs(10)),
                    "the host did not stop a tracer whose output failed"
                );

                let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
                // SAFETY: `waitid` writes one `siginfo_t` into `info`; `WNOWAIT` leaves the tracer
                // for its owner to reap.
                let waited = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        pid,
                        info.as_mut_ptr(),
                        libc::WEXITED | libc::WNOWAIT,
                    )
                };
                assert_eq!(waited, 0, "{}", std::io::Error::last_os_error());
                // SAFETY: `info` was zeroed, and a successful `waitid` filled it in.
                let info = unsafe { info.assume_init() };
                // SAFETY: a `waitid` report of a child's exit carries its status.
                let status = unsafe { info.si_status() };
                assert_eq!(
                    (info.si_code, status),
                    (libc::CLD_KILLED, libc::SIGKILL),
                    "the tracer did not end by the host's SIGKILL"
                );
                assert!(
                    tracing.drain().is_err(),
                    "a drain accepted incomplete evidence"
                );
                tracing
                    .unregister_root(&root.root)
                    .expect("teardown reaps the killed tracer");
                assert!(
                    tracing.drain().is_err(),
                    "the failure did not outlive teardown"
                );
            },
        );
    }
}
