//! One managed command with real pipes: its input, its two output streams, its verdict.
//!
//! This is the shape every non-pane workload takes. A helper that runs `git status`, a status-line
//! producer, a `pipe-pane` logger, a tunnel provider — none of them want a pseudoterminal, and all
//! of them want the publication gate. [`Execution`] is both.
//!
//! # Why the job is created idle
//!
//! A fast process can exit before anything has subscribed to its output. So an execution is built
//! in three steps, in this order and no other:
//!
//! 1. create an **idle** pipe job — no command, nothing can write yet;
//! 2. arm both lossless owner receivers on its two streams;
//! 3. admit exactly one command.
//!
//! Step 2 before step 3 is the whole reason this is not a one-call spawn.
//!
//! # What drop does, and does not do
//!
//! Dropping an [`Execution`], a handle or a stream detaches an *observation*. It does not cancel
//! admitted work: the command was accepted, it may have spawned processes, and silently killing it
//! because a caller stopped watching would make cancellation depend on garbage collection.
//! [`Execution::cancel`] is the explicit way, and it forces and discards.

use std::sync::Arc;

use marsh_core::shellmux::{
    CommandCompletion, CommandHandle, CommandOptions, JobIo, OutputChannel, ShellId, SpawnOptions,
};
use rmux_core::events::OutputCursorItem;

use crate::io::streams::{Poll, StreamKey};
use crate::io::{IoError, IoResult, ShellHandle, ShellIo};

/// What to run, where, and as whom.
#[derive(Clone, Debug)]
pub struct ExecutionSpec {
    /// Host filesystem directory the command starts in, which also selects the seed it publishes
    /// into.
    ///
    /// Empty selects the host's [`default_dir`](ShellIo::default_dir); a relative path is joined
    /// onto that default; an absolute one keeps its host meaning.
    pub initial_dir: std::path::PathBuf,
    /// The job name, which is also the capability principal. `None` draws the next automatic one.
    ///
    /// A recurring slot — a status-line producer, a cache entry — should keep its own name so its
    /// approved state can be updated without inventing a new principal every time. A one-off
    /// helper should take an automatic one.
    pub id: Option<ShellId>,
    /// The workload.
    ///
    /// [`ProcessCommand::Shell`](rmux_proto::ProcessCommand) text is interpreted by the embedded
    /// brush interpreter, so marsh's own builtins and instrumentation apply to it. An `sh -c`
    /// wrapper would skip both.
    /// [`ProcessCommand::Argv`](rmux_proto::ProcessCommand) becomes an explicit `exec --` plan with
    /// every argument individually quoted, so an empty or special argument survives.
    pub process: rmux_proto::ProcessCommand,
    /// Variables replacing the host profile's for this execution, or `None` to inherit them.
    pub environment: Option<brush_core::env::ShellEnvironment>,
}

/// How much collected output to retain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputLimit {
    /// At most this many bytes across stdout and stderr *combined*.
    ///
    /// Zero is a legitimate limit and permits no output at all.
    Bytes(usize),
    /// No limit. Explicit, because an accidental unbounded collection of a program's output is a
    /// memory bug waiting for the wrong program.
    Unlimited,
}

/// What to do when a bounded collection overflows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverflowPolicy {
    /// Cancel and discard the execution, and report [`IoError::OutputLimit`].
    ///
    /// A caller asking for complete output does not want a silent prefix.
    Error,
    /// Keep draining to end of file, retain the prefix, and mark the result truncated.
    ///
    /// For the internal collectors whose upstream behaviour already truncates. The verdict is
    /// still awaited and still real.
    Truncate,
}

/// How to collect an execution's output.
///
/// The two knobs answer different questions. `limit` is how much is *retained* — it never stops
/// the program writing, and both streams draw on it together. `overflow` is what exceeding it
/// means. Neither is about the command: a truncated collection still awaits the command's real
/// verdict and still returns it.
///
/// # Examples
///
/// Pure option construction, so this runs as an ordinary doctest.
///
/// ```
/// use rmux_server::io::{CollectOptions, OutputLimit, OverflowPolicy};
///
/// // Output that is data: bound it, and fail loudly rather than hand back a silent prefix.
/// // Overflow under this policy cancels and discards the execution.
/// let strict = CollectOptions {
///     limit: OutputLimit::Bytes(64 * 1024),
///     overflow: OverflowPolicy::Error,
/// };
/// // Output that is a display: keep the prefix, keep draining to end of file so the program is
/// // never left blocked writing into a full pipe, and report it with `CapturedOutput::truncated`.
/// let lossy = CollectOptions {
///     limit: OutputLimit::Bytes(4 * 1024),
///     overflow: OverflowPolicy::Truncate,
/// };
/// // Zero is a legitimate bound rather than an accident: a command run purely for its effects.
/// let silent = CollectOptions {
///     limit: OutputLimit::Bytes(0),
///     overflow: OverflowPolicy::Truncate,
/// };
///
/// assert_eq!(strict.overflow, OverflowPolicy::Error);
/// assert_eq!(lossy.limit, OutputLimit::Bytes(4096));
/// assert_eq!(silent.limit, OutputLimit::Bytes(0));
///
/// // The default is explicit about being unbounded, and refuses to truncate should that ever be
/// // narrowed: an accidental unbounded collection is a memory bug waiting for the wrong program.
/// assert_eq!(CollectOptions::default().limit, OutputLimit::Unlimited);
/// assert_eq!(CollectOptions::default().overflow, OverflowPolicy::Error);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct CollectOptions {
    /// How much to retain.
    pub limit: OutputLimit,
    /// What to do when that is exceeded.
    pub overflow: OverflowPolicy,
}

impl Default for CollectOptions {
    /// Unlimited, and an explicit error if that is ever narrowed.
    fn default() -> Self {
        Self {
            limit: OutputLimit::Unlimited,
            overflow: OverflowPolicy::Error,
        }
    }
}

/// Everything one collected execution produced.
///
/// Four independent facts, deliberately not collapsed into one: the two byte streams, whether
/// they are complete, and the verdict. A caller that read only one of them would be reporting
/// something it never checked.
#[derive(Debug)]
pub struct CapturedOutput {
    /// Standard output, byte for byte.
    pub stdout: Vec<u8>,
    /// Standard error, byte for byte and independent of stdout.
    ///
    /// There is no relative order between the two, and none is invented.
    pub stderr: Vec<u8>,
    /// Whether the retained bytes are a prefix under [`OverflowPolicy::Truncate`].
    pub truncated: bool,
    /// The verdict. A zero exit code here still does not mean anything was published.
    pub completion: Arc<CommandCompletion>,
}

/// The write end of an execution's standard input.
///
/// Bound to one generation of one job: a handle that outlived its execution cannot write into
/// whatever took its place.
#[derive(Clone, Debug)]
pub struct InputWriter {
    /// The host.
    io: ShellIo,
    /// The generation-bound job.
    job: ShellHandle,
}

impl InputWriter {
    /// Writes every byte, or reports why it could not.
    ///
    /// A failure partway through has already delivered a prefix — this is a pipe, and bytes
    /// cannot be taken back. Nor does a successful write say the program consumed anything.
    ///
    /// # Errors
    ///
    /// Fails once the input has been closed, once the job is closing, and after host teardown.
    pub async fn write_all(&self, bytes: &[u8]) -> IoResult<()> {
        self.io.write_input(&self.job, bytes).await
    }

    /// Closes the input: a real end-of-file, after every write already accepted.
    ///
    /// Idempotent. Not a Ctrl-D: this is a pipe's write end going away, which is what a program
    /// blocked on `read` actually observes.
    ///
    /// # Errors
    ///
    /// Fails after host teardown and for a stale job.
    pub async fn close(&self) -> IoResult<()> {
        self.io.close_input(&self.job).await
    }
}

/// One reader of one job stream.
///
/// Two flavours, deliberately not distinguished in the type: the **owner** of a managed execution
/// gets every byte with bounded backpressure, and an **observer** gets an independent cursor over
/// a bounded ring with explicit gaps. A caller holding one never has to know which it has, because
/// both answer the same way and both report a gap the same way.
#[derive(Debug)]
pub struct OutputStream {
    /// The underlying reader.
    source: Source,
}

/// Where an [`OutputStream`]'s items come from.
#[derive(Debug)]
enum Source {
    /// Lossless, armed before the command started.
    Owner {
        /// The bounded channel the frontend adapter pushes into.
        receiver: tokio::sync::mpsc::Receiver<OutputCursorItem>,
    },
    /// Bounded, independent, and allowed to miss events.
    Observer {
        /// The host.
        io: ShellIo,
        /// Which stream.
        key: StreamKey,
        /// This observer's position.
        cursor: rmux_core::events::OutputCursor,
        /// Wakes this observer when the stream advances.
        signal: Arc<tokio::sync::Notify>,
    },
}

impl OutputStream {
    /// An owner stream over an armed receiver.
    pub(crate) const fn owner(receiver: tokio::sync::mpsc::Receiver<OutputCursorItem>) -> Self {
        Self {
            source: Source::Owner { receiver },
        }
    }

    /// The next item, or `None` at end of stream.
    ///
    /// `None` means this stream is over: every write end is gone, and nothing further will arrive.
    /// It is **not** the command finishing — that is [`CommandHandle::wait`] — and it is not the
    /// job closing.
    ///
    /// An [`OutputCursorItem::Gap`] means this observer fell behind. Its bytes are gone; the
    /// stream continues.
    ///
    /// # Errors
    ///
    /// Fails after host teardown.
    pub async fn recv(&mut self) -> IoResult<Option<OutputCursorItem>> {
        match &mut self.source {
            Source::Owner { receiver } => Ok(receiver.recv().await),
            Source::Observer {
                io,
                key,
                cursor,
                signal,
            } => loop {
                // Registered before the poll, so an append landing between them still wakes this.
                let notified = signal.notified();
                match io.streams().poll(key, cursor) {
                    Poll::Item(item) => return Ok(Some(item)),
                    Poll::Ended => return Ok(None),
                    Poll::Pending => notified.await,
                }
            },
        }
    }
}

impl Drop for OutputStream {
    /// Releases an observer's claim on its stream's retained bytes.
    ///
    /// An ended stream is kept alive while any observer still holds a cursor into it, so this is
    /// what eventually reclaims one. An owner stream holds no claim and needs no release.
    fn drop(&mut self) {
        if let Source::Observer { io, key, .. } = &self.source {
            io.streams().release(key);
        }
    }
}

/// One admitted managed command, with everything it owns.
///
/// Created by [`ShellIo::execute`](crate::io::ShellIo::execute), which arms the output receivers
/// before admitting the command so no byte can be produced unobserved.
#[derive(Debug)]
pub struct Execution {
    /// The job the command runs in.
    shell: ShellHandle,
    /// The command's receipt.
    command: CommandHandle,
    /// Standard input.
    stdin: InputWriter,
    /// Standard output, lossless.
    stdout: OutputStream,
    /// Standard error, lossless and independent.
    stderr: OutputStream,
}

/// An [`Execution`] taken apart, for a caller that wants to own the pieces separately.
#[derive(Debug)]
pub struct ExecutionParts {
    /// The job.
    pub shell: ShellHandle,
    /// The command's receipt.
    pub command: CommandHandle,
    /// Standard input.
    pub stdin: InputWriter,
    /// Standard output.
    pub stdout: OutputStream,
    /// Standard error.
    pub stderr: OutputStream,
}

impl Execution {
    /// The job this command runs in.
    #[must_use]
    pub fn shell(&self) -> &ShellHandle {
        &self.shell
    }

    /// The command's receipt. Cloneable, and waitable from any number of holders.
    #[must_use]
    pub fn command(&self) -> &CommandHandle {
        &self.command
    }

    /// A writer for this command's standard input.
    #[must_use]
    pub fn input(&self) -> InputWriter {
        self.stdin.clone()
    }

    /// Takes the execution apart, for a caller that wants to own the pieces separately.
    ///
    /// Nothing is cancelled and nothing is detached from the command: every piece stays bound to
    /// the same generation of the same job, and dropping one of them ends an *observation* rather
    /// than the work. [`Execution::cancel`] is the explicit way to stop it.
    ///
    /// # Examples
    ///
    /// Full duplex over real pipes, with an explicit end-of-file. Needs a live host over a leased
    /// seed and a real `gzip` program, so it is compiled rather than executed.
    ///
    /// ```no_run
    /// use rmux_core::events::OutputCursorItem;
    /// use rmux_proto::ProcessCommand;
    /// use rmux_server::io::{ExecutionParts, ExecutionSpec, IoError, IoResult, ShellIo};
    ///
    /// # async fn compress(io: &ShellIo, input: Vec<u8>) -> IoResult<Vec<u8>> {
    /// let ExecutionParts { command, stdin, mut stdout, mut stderr, .. } = io
    ///     .execute(ExecutionSpec {
    ///         initial_dir: std::path::PathBuf::new(),
    ///         id: None,
    ///         process: ProcessCommand::Argv(vec!["gzip".to_owned(), "-c".to_owned()]),
    ///         environment: None,
    ///     })
    ///     .await?
    ///     .into_parts();
    ///
    /// // Writer and readers run concurrently. Writing everything first deadlocks the moment the
    /// // program's output fills its pipe; draining first deadlocks against a program that is
    /// // still blocked on `read`.
    /// let feed = async move {
    ///     stdin.write_all(&input).await?;
    ///     // A real end of file, ordered after every accepted write, and idempotent. Not a
    ///     // Ctrl-D: this is the pipe's write end going away, which is what `read` observes.
    ///     stdin.close().await
    /// };
    /// let compressed = async move {
    ///     let mut bytes = Vec::new();
    ///     // `None` is the end of *this stream* — every write end is gone — and not the command
    ///     // finishing. NUL bytes and bare carriage returns survive it, because a pipe is not a
    ///     // terminal and nothing here is translated.
    ///     while let Some(item) = stdout.recv().await? {
    ///         // An owner stream is lossless and never gaps; an observer's would say so here.
    ///         if let OutputCursorItem::Event(event) = item {
    ///             bytes.extend_from_slice(event.bytes());
    ///         }
    ///     }
    ///     Ok::<_, IoError>(bytes)
    /// };
    /// let diagnostics = async move {
    ///     let mut bytes = Vec::new();
    ///     // Byte-exact and wholly independent of stdout, with no promised order between them.
    ///     while let Some(item) = stderr.recv().await? {
    ///         if let OutputCursorItem::Event(event) = item {
    ///             bytes.extend_from_slice(event.bytes());
    ///         }
    ///     }
    ///     Ok::<_, IoError>(bytes)
    /// };
    ///
    /// let (fed, compressed, diagnostics) = tokio::join!(feed, compressed, diagnostics);
    /// fed?;
    /// let (compressed, _diagnostics) = (compressed?, diagnostics?);
    ///
    /// // A third boundary, later than either stream ending: the gate's answer for the line.
    /// // Exiting zero here still says nothing about whether anything was published.
    /// let completion = command.wait().await?;
    /// let _ = (completion.exit_code(), completion.is_published());
    /// Ok(compressed)
    /// # }
    /// ```
    #[must_use]
    pub fn into_parts(self) -> ExecutionParts {
        ExecutionParts {
            shell: self.shell,
            command: self.command,
            stdin: self.stdin,
            stdout: self.stdout,
            stderr: self.stderr,
        }
    }

    /// Closes stdin, drains both outputs, and waits for the verdict and the job's closure.
    ///
    /// The order matters and is not negotiable: a program reading stdin will not finish until it
    /// sees end of file, and draining only after it exits can deadlock on a full pipe. So stdin is
    /// closed first and both streams are drained *concurrently*.
    ///
    /// The result distinguishes every boundary it can: the exit code, the publication outcome,
    /// whether the retained bytes are complete. A zero exit code with a denied outcome is a
    /// perfectly ordinary result here, and callers that require approval must check for it.
    ///
    /// # Errors
    ///
    /// Fails with [`IoError::OutputLimit`] under [`OverflowPolicy::Error`] — having cancelled and
    /// discarded the execution — and with whatever the wait or the host reported.
    pub async fn collect(self, options: CollectOptions) -> IoResult<CapturedOutput> {
        let Self {
            shell,
            command,
            stdin,
            mut stdout,
            mut stderr,
        } = self;

        // Before draining: a program that reads its input never reaches end of file otherwise, and
        // a collection that waited for it would wait forever.
        let _ = stdin.close().await;

        let remaining = std::sync::Mutex::new(match options.limit {
            OutputLimit::Bytes(limit) => Some(limit),
            OutputLimit::Unlimited => None,
        });
        let out = drain(&mut stdout, &remaining);
        let err = drain(&mut stderr, &remaining);
        let (out, err) = tokio::join!(out, err);

        let overflowed = out.is_err() || err.is_err();
        if overflowed && options.overflow == OverflowPolicy::Error {
            // Cancel rather than return a silent prefix: the caller asked for complete output.
            let _ = stdin.io.stop(&shell, true).await;
            let limit = match options.limit {
                OutputLimit::Bytes(limit) => limit,
                OutputLimit::Unlimited => usize::MAX,
            };
            return Err(IoError::OutputLimit { limit });
        }

        let stdout = out.unwrap_or_else(|bytes| bytes);
        let stderr = err.unwrap_or_else(|bytes| bytes);
        let completion = command.wait().await?;
        // The job's own closure is a later boundary than the command's verdict: waiting for it is
        // what makes "the snapshot has been reclaimed" true when this returns.
        let _ = shell.wait_closed().await;

        Ok(CapturedOutput {
            stdout,
            stderr,
            truncated: overflowed,
            completion,
        })
    }

    /// Forces this execution to stop and discards whatever it staged.
    ///
    /// Explicit, because dropping does not cancel. A cancellation that lands after an approved
    /// publication has begun cannot undo it; the eventual completion is authoritative.
    ///
    /// # Errors
    ///
    /// Fails when the processes could not be signalled, and after host teardown.
    pub async fn cancel(&self) -> IoResult<()> {
        self.stdin.io.stop(&self.shell, true).await
    }
}

/// Drains one stream to end of file, retaining what the shared allowance permits.
///
/// `Err` carries the retained prefix and means the allowance was exceeded. Draining continues
/// either way: stopping early would leave the program blocked writing into a full pipe, and a
/// caller that wanted the process cancelled asks for that explicitly.
async fn drain(
    stream: &mut OutputStream,
    remaining: &std::sync::Mutex<Option<usize>>,
) -> Result<Vec<u8>, Vec<u8>> {
    let mut retained = Vec::new();
    let mut overflowed = false;
    while let Ok(Some(item)) = stream.recv().await {
        let OutputCursorItem::Event(event) = item else {
            // A gap on an owner stream cannot happen; on an observer stream it means bytes were
            // lost, which is itself a form of truncation.
            overflowed = true;
            continue;
        };
        let bytes = event.bytes();
        let allowed = {
            let mut allowance = remaining
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match allowance.as_mut() {
                None => bytes.len(),
                Some(left) => {
                    let allowed = bytes.len().min(*left);
                    *left -= allowed;
                    allowed
                }
            }
        };
        if allowed < bytes.len() {
            overflowed = true;
        }
        retained.extend_from_slice(&bytes[..allowed]);
    }
    if overflowed {
        Err(retained)
    } else {
        Ok(retained)
    }
}

impl ShellIo {
    /// Runs one workload on real pipes.
    ///
    /// Creates an idle pipe job, arms both lossless output receivers, then admits exactly one
    /// command. That ordering is what makes a fast process's first bytes observable.
    ///
    /// The job is visible in [`jobs`](ShellIo::jobs) — it is real work with a real principal and a
    /// real snapshot — but it is never adopted as a window, never selected, and has no pane.
    ///
    /// The whole three-step transaction happens on this host's runtime, whichever runtime or
    /// plain thread asked for it: the job, its two byte pumps and the task its command runs on
    /// belong to this daemon, and an execution started from a status thread must not stop
    /// producing output when that thread's executor goes away.
    ///
    /// # Errors
    ///
    /// Fails after teardown, and with whatever the core reported about the name, the directory or
    /// the shell.
    pub async fn execute(&self, spec: ExecutionSpec) -> IoResult<Execution> {
        // Composed *before* anything is created. An unusable workload — an empty argv, a shape
        // this build cannot compose — must fail having allocated nothing; rejecting it after the
        // spawn would leave an idle job with a real snapshot and two armed receivers that nobody
        // will ever close.
        let line = crate::io::protocol::workload_line(&spec.process)?;

        // One hop for the whole transaction rather than one per step: the calls below then find
        // themselves already on this host's runtime and await inline, so the receivers are armed
        // between the spawn and the command with no runtime boundary in between — which is the
        // ordering that makes a fast process's first bytes observable.
        let io = self.unleased();
        self.dispatch(async move {
            let shell = io
                .open_shell(
                    &spec.initial_dir,
                    spec.id,
                    SpawnOptions {
                        io: JobIo::Pipes,
                        environment: spec.environment,
                        ..SpawnOptions::default()
                    },
                )
                .await?;

            let uid = shell.sandbox().uid.clone();
            let stdout =
                OutputStream::owner(io.streams().arm_owner((uid.clone(), OutputChannel::Stdout)));
            let stderr = OutputStream::owner(io.streams().arm_owner((uid, OutputChannel::Stderr)));

            // Scheduled rather than awaited to completion: the caller still has to feed this
            // execution's standard input and close it, and a program that reads to end of file
            // never finishes until it does. This returns at admission; the verdict arrives
            // through the receipt.
            let command = match io
                .start_command(
                    &shell,
                    &line,
                    CommandOptions {
                        // A pipe shell is one-shot by construction; the core closes it after this
                        // command whatever this says, and saying so here keeps the two agreeing.
                        close_on_finish: true,
                        on_accept: None,
                    },
                )
                .await
            {
                Ok(command) => command,
                Err(error) => {
                    // The shell was admitted and nothing will ever run in it. Without this it
                    // would sit open, holding a snapshot, until the host shut down — and a
                    // one-shot shell only closes when its command ends.
                    let _ = io.stop(&shell, true).await;
                    return Err(error);
                }
            };

            let stdin = InputWriter {
                io: io.unleased(),
                job: shell.clone(),
            };
            Ok(Execution {
                shell,
                command,
                stdin,
                stdout,
                stderr,
            })
        })
        .await
    }

    /// An independent bounded observer of one job stream.
    ///
    /// Cannot stall the job and cannot steal another reader's bytes. Falling behind produces an
    /// explicit [`OutputCursorItem::Gap`].
    ///
    /// # Errors
    ///
    /// Fails for a foreign handle, a channel the job does not produce, and after teardown.
    pub fn output(
        &self,
        job: &ShellHandle,
        channel: OutputChannel,
        start: rmux_sdk::PaneOutputStart,
    ) -> IoResult<OutputStream> {
        // Liveness and ownership first, both of which this method documents and neither of which
        // it used to check. Registering an observer after teardown would create a stream entry
        // nothing will ever write to or end, leaving the caller waiting forever; accepting a
        // foreign handle would hand it another host's bytes.
        self.ensure_open()?;
        let _ = self.owned(job)?;
        if !job.output_channels().contains(&channel) {
            return Err(IoError::NoPresentation);
        }
        let key = (job.sandbox().uid.clone(), channel);
        // The job's own closure is authoritative and cannot age out of the retention tombstones,
        // so a stream belonging to a job that closed long ago still reports end of file rather
        // than waiting for bytes that can never arrive.
        let (cursor, signal) = self
            .streams()
            .observe(key.clone(), start, job.shell().is_closed());
        Ok(OutputStream {
            source: Source::Observer {
                io: self.unleased(),
                key,
                cursor,
                signal,
            },
        })
    }
}
