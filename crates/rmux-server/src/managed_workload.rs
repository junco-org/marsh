//! Where this daemon's non-pane work becomes a managed command.
//!
//! A pane is a terminal a user is looking at. Everything else this daemon runs on a user's behalf
//! — `run-shell`, an `if-shell` predicate, a status-line `#()` producer, a `pipe-pane` logger, a
//! `copy-pipe` filter, a hook's shell fallback, a tunnel provider, and every file `load-buffer`,
//! `save-buffer` and `source-file` touch — is *requested work* with no screen attached to it.
//!
//! Upstream ran all of it with [`std::process::Command`] and plain [`std::fs`]. That made each one
//! invisible to the instrumentation, unattributable to a principal, and unstaged: a `run-shell`
//! redirection wrote straight into the seed, while the identical text typed into a pane had to
//! pass the gate first. This module closes that gap. Every workload here is a managed pipe job,
//! and every file operation is the `__rmux_io` builtin inside one.
//!
//! # The callsite ledger for this trust boundary
//!
//! Exhaustive on purpose. "Every syscall is confined" would be both a blanket statement and a lie;
//! what is true is that these specific requested workloads go through here, and the list is short
//! enough to check.
//!
//! | Requested workload | Entry point used |
//! |---|---|
//! | `handler_scripting/runtime.rs` — `run-shell`, `if-shell` | [`spec`] + [`start`], [`truncating`]/[`DISCARD`] |
//! | `handler_scripting/hook_commands.rs` — the unknown-command shell fallback | [`spec`] + [`start`], [`DISCARD`] |
//! | `handler_scripting/source_runtime.rs` — `source-file` | [`source_files`] |
//! | `handler_shell_processes.rs` — the helper ledger | the handles the above admit |
//! | `status_jobs.rs` — status-line `#()` producers | [`spec`] + [`collect`], [`truncating`] |
//! | `pane_pipe.rs`, `pane_terminals/pipes.rs`, `handler_pane/process.rs` — `pipe-pane` | [`spec`] + [`start`] |
//! | `handler_copy_mode/pipe_command.rs` — `copy-pipe` | [`spec`] + [`start`], [`DISCARD`] |
//! | `handler_buffer.rs` — `load-buffer`, `save-buffer` | [`read_file`], [`write_file`] |
//! | `web/tunnel/{preset,runner,output}.rs` — presets and providers | [`preset_names`], [`read_file`], [`spec`] + [`start`] |
//! | `handler_overlay/popup_job.rs` — `display-popup` | its own terminal job, not this module: a popup has a screen |
//!
//! # What is not routed through here, and deliberately
//!
//! The daemon's own socket, its pseudoterminal allocation, its IPC framing, its startup
//! configuration bootstrap read, its write-ahead log and its fixed operator diagnostics. Those are
//! trusted runtime mechanics. Opening a shell per syscall would be recursive, and calling it
//! confinement would be a lie — the gate observes requested work, it does not sandbox the process.
//!
//! # Exit status is not approval
//!
//! Every collection here returns the whole [`CapturedOutput`], because a workload has two
//! independent results and collapsing them loses the interesting one:
//!
//! * `completion.exit_code` is what the program did;
//! * `completion.outcome` is what the gate did with the filesystem changes behind it.
//!
//! A command that prints, exits zero and had its writes refused is a *failure* for any caller that
//! was going to trust the result — a cached status value, a loaded buffer, a sourced
//! configuration, an `if-shell` predicate. [`require_published`] is how such a caller says so, and
//! it renders the refusal with the console's own `repl::report_lines` text rather than inventing a
//! second vocabulary for denials.

use std::ffi::OsStr;
use std::path::Path;

use marsh_core::shellmux::ShellId;
use rmux_proto::{ProcessCommand, RmuxError};

use crate::io::{
    CapturedOutput, CollectOptions, Execution, ExecutionSpec, IoError, OutputLimit, OverflowPolicy,
    ShellIo,
};

/// How much output a collection that is only draining may retain.
///
/// Zero, and [`OverflowPolicy::Truncate`]: keep reading to end of file so the program is never
/// blocked writing into a full pipe, and keep none of it. This is the honest spelling of
/// upstream's `Stdio::null()` — the bytes still have to be consumed, they just have nowhere to go.
pub(crate) const DISCARD: CollectOptions = CollectOptions {
    limit: OutputLimit::Bytes(0),
    overflow: OverflowPolicy::Truncate,
};

/// Collection for a caller that needs every byte or none.
///
/// [`OverflowPolicy::Error`] over an unlimited budget never actually triggers; it is stated rather
/// than defaulted so that narrowing the limit later cannot silently start returning prefixes to a
/// caller that parses what it gets.
pub(crate) const COMPLETE: CollectOptions = CollectOptions {
    limit: OutputLimit::Unlimited,
    overflow: OverflowPolicy::Error,
};

/// Collection that retains at most `limit` bytes and reports having done so.
///
/// For the internal collectors whose upstream behaviour already truncated at a cap. The verdict is
/// still awaited and still real; only the retained bytes are a prefix.
pub(crate) const fn truncating(limit: usize) -> CollectOptions {
    CollectOptions {
        limit: OutputLimit::Bytes(limit),
        overflow: OverflowPolicy::Truncate,
    }
}

/// The shell engine this handler serves over.
///
/// The one accessor the handlers use. In a test build it also *builds* the engine on first use,
/// because a `RequestHandler` constructed directly by a unit test never went through the listener
/// that would have installed one. That fallback is test scaffolding and is compiled out of every
/// shipped build: production has exactly one construction path, in
/// [`crate::listener::serve`].
///
/// # Errors
///
/// Fails with [`RmuxError::Server`] when no facade is installed. That is not a state a served
/// daemon reaches — [`crate::listener::serve`] installs one before it accepts its first
/// connection — so it is reachable only from a handler constructed but never served, which is a
/// programming error rather than a user's. The message therefore says what is missing instead of
/// pretending the request was malformed.
pub(crate) fn handler_facade(
    handler: &crate::handler::RequestHandler,
) -> Result<ShellIo, RmuxError> {
    if let Some(io) = handler.shell_io() {
        return Ok(io);
    }
    #[cfg(test)]
    if let Some(io) = test_engine::install(handler) {
        return Ok(io);
    }
    Err(RmuxError::Server(
        "the rmux shell engine is not attached to this server".to_owned(),
    ))
}

/// Builds the specification for one workload.
///
/// `cwd` is a host path — a pane's current directory, a caller's working directory — and is
/// forwarded as the directory the job's shell starts in, which is also what selects the seed it
/// publishes into. An empty one takes this host's default directory, because a workload helper's
/// directory is *inherited*, never named: `run-shell`, `if-shell`, `pipe-pane` and the status
/// `#()` commands have no `-c` in their grammar, so there is no request to honour when there is
/// no path. A directory that was supplied is used as it stands, even on another seed.
///
/// `id` names a *recurring slot*. A status-line producer or a cache entry keeps its own name so
/// its approved state is updated under one principal across generations; a genuine one-off passes
/// `None` and draws an automatic name, because minting a fresh principal for every invocation
/// would churn the policy history with names nothing will ever refer to again.
///
/// # Errors
///
/// Fails when the environment holds non-UTF-8 data.
pub(crate) fn spec<'a, I>(
    io: &ShellIo,
    cwd: &Path,
    environment: I,
    id: Option<ShellId>,
    process: ProcessCommand,
) -> Result<ExecutionSpec, RmuxError>
where
    I: IntoIterator<Item = (&'a OsStr, &'a OsStr)>,
{
    let initial_dir = if cwd.as_os_str().is_empty() {
        io.default_dir().to_path_buf()
    } else {
        cwd.to_path_buf()
    };
    let environment = crate::terminal::shell_environment_from_pairs(environment)?;
    Ok(ExecutionSpec {
        initial_dir,
        id,
        process,
        environment: Some(environment),
    })
}

/// Admits one workload and collects it.
///
/// # Errors
///
/// Fails when the engine refused the job, when the collection exceeded an
/// [`OverflowPolicy::Error`] budget — having cancelled the job — and when the wait ended without a
/// verdict.
pub(crate) async fn collect(
    io: &ShellIo,
    spec: ExecutionSpec,
    options: CollectOptions,
) -> Result<CapturedOutput, RmuxError> {
    let execution = io.execute(spec).await.map_err(io_error)?;
    execution.collect(options).await.map_err(io_error)
}

/// Admits one workload without collecting it, for a caller that streams.
///
/// # Errors
///
/// Fails for the reasons [`collect`] does, minus the collection.
pub(crate) async fn start(io: &ShellIo, spec: ExecutionSpec) -> Result<Execution, RmuxError> {
    io.execute(spec).await.map_err(io_error)
}

/// Rejects a result the gate did not approve.
///
/// For every caller whose next step trusts the workload's effects: applying a loaded buffer,
/// dispatching sourced commands, caching a status value, believing a predicate. A zero exit code
/// is not enough for any of them, because the thing they are about to trust is precisely what the
/// gate refused.
///
/// The diagnostic is the console's own verdict text, so the refusal a user reads from `run-shell`
/// says the same thing as the refusal they would have read from the pane prompt.
///
/// # Errors
///
/// Fails with [`RmuxError::Server`] for a denial, a lost race, a discard, a detached result and an
/// infrastructure failure alike. Only [`marsh_core::Outcome::Published`] succeeds.
pub(crate) fn require_published(captured: &CapturedOutput) -> Result<(), RmuxError> {
    if captured.completion.is_published() {
        return Ok(());
    }
    Err(unapproved_error(captured))
}

/// The refusal diagnostic for a completion that was not published.
pub(crate) fn unapproved_error(captured: &CapturedOutput) -> RmuxError {
    RmuxError::Server(
        completion_report(&captured.completion)
            .trim_end()
            .to_owned(),
    )
}

/// The console's verdict text for one completion, as trailing lines.
///
/// Rendered with `repl::report_lines` so a refusal reaching a user through `run-shell` output
/// reads exactly like the one they would have seen at a pane prompt: the capabilities the line
/// asked for, which were refused, and what would unblock them. A second vocabulary for the same
/// decision would be one more thing to keep in step.
pub(crate) fn completion_report(completion: &marsh_core::shellmux::CommandCompletion) -> String {
    let mut lines =
        marsh_core::shellmux::repl::report_lines(&completion.shell.id, completion.result.as_ref());
    if lines.is_empty() {
        lines.push(format!(
            "{}: the shell engine refused to publish this command",
            completion.shell.id.reference()
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}

/// Maps a facade failure onto the wire error vocabulary the handlers already speak.
///
/// The source is preserved in the message rather than discarded: a caller reading a server error
/// needs to be able to tell "the engine is closed" from "the directory does not exist".
pub(crate) fn io_error(error: IoError) -> RmuxError {
    match error {
        IoError::Protocol(error) => (*error).clone(),
        other => RmuxError::Server(other.to_string()),
    }
}

/// Reads a file as recorded, instrumented work.
///
/// Goes through `__rmux_io read`, not [`std::fs`]: the read is a request, and a request that is
/// invisible to the instrumentation cannot be attributed, cancelled with its command, or refused.
/// The result is required to be published — a read whose job was discarded produced bytes nobody
/// checked, and handing those to `load-buffer` would apply unverified content.
///
/// # Errors
///
/// Fails when the engine refused the job, when the file could not be read, and when the gate did
/// not approve the work.
pub(crate) async fn read_file(io: &ShellIo, cwd: &Path, path: &Path) -> Result<Vec<u8>, RmuxError> {
    let line = crate::io::protocol::builtin_plan(&[
        crate::io::builtins::RMUX_IO_BUILTIN.to_owned(),
        "read".to_owned(),
        "--".to_owned(),
        path.to_string_lossy().into_owned(),
    ]);
    let captured = collect(io, builtin_spec(io, cwd, line)?, COMPLETE).await?;
    finish_builtin(&captured, "read")?;
    // A zero exit is not approval. A read whose job was discarded or denied produced bytes no
    // gate ever checked, and every caller of this function is about to *trust* them — apply them
    // to a buffer, parse them as configuration. Returning them would be the one failure mode this
    // whole path exists to prevent.
    require_published(&captured)?;
    Ok(captured.stdout)
}

/// Writes a file as staged, instrumented work.
///
/// The bytes go in through the job's real standard input rather than an argument, so a buffer
/// holding NUL bytes, invalid UTF-8 or a gigabyte of content is written byte for byte. The write
/// lands in the helper's own snapshot; it reaches the seed only when the gate approves this
/// command's boundary, which is what makes a refused `save-buffer` leave the original file alone.
///
/// # Errors
///
/// Fails when the engine refused the job, when the write failed, and when the gate refused to
/// publish it — which is a user-visible failure, because the file the user asked for does not
/// exist.
pub(crate) async fn write_file(
    io: &ShellIo,
    cwd: &Path,
    path: &Path,
    content: Vec<u8>,
    append: bool,
) -> Result<(), RmuxError> {
    let mut argv = vec![
        crate::io::builtins::RMUX_IO_BUILTIN.to_owned(),
        "write".to_owned(),
    ];
    if append {
        argv.push("--append".to_owned());
    }
    // No `--mkdirs`. The path is one the caller NAMED — `save-buffer -f DIR/out.txt` — and a
    // named destination that cannot be honoured fails loudly rather than being made to work:
    // creating an arbitrary directory tree on the user's disk is a larger effect than the save
    // they asked for, and upstream reports the missing directory instead.
    // `save_buffer_failure_does_not_mutate_existing_buffer` is that contract.
    argv.push("--".to_owned());
    argv.push(path.to_string_lossy().into_owned());

    let spec = builtin_spec(io, cwd, crate::io::protocol::builtin_plan(&argv))?;
    // Admitted and fed on the host's runtime, not in the caller's future: the builtin reads its
    // standard input to end of file on a blocking thread, and a caller dropped between admission
    // and `close` — a client that disconnected — would strand it on a pipe that never ends, where
    // not even shutdown can reach it.
    let feeder = io.clone();
    let execution = io
        .runtime()
        .spawn(async move {
            let execution = start(&feeder, spec).await?;
            let input = execution.input();
            let written = input.write_all(&content).await;
            // Closed even after a failed write, and before collecting: a collection that waited
            // for the command without closing standard input first would wait forever.
            let closed = input.close().await;
            written.and(closed).map_err(io_error)?;
            Ok::<_, RmuxError>(execution)
        })
        .await
        .map_err(|error| RmuxError::Server(format!("save feeder failed: {error}")))??;
    let captured = execution.collect(COMPLETE).await.map_err(io_error)?;
    finish_builtin(&captured, "write")?;
    require_published(&captured)
}

/// Reads one or more configuration files as recorded work.
///
/// Returns what the existing source reader produced, decoded from the builtin's standard output.
/// The reader itself is not reimplemented on this side: its glob expansion, byte-order-mark
/// handling, identity checks, size limits and nesting rules are one implementation, called from
/// inside the managed job.
///
/// `cwd` resolves the *operand* patterns and is encoded as `--cwd`; `initial_dir` is where the
/// helper job's own shell starts, and so which seed it opens on. They are separate because a
/// relative source pattern must keep meaning what the caller meant by it even when the helper
/// itself has to start somewhere else.
///
/// # Errors
///
/// Fails when the engine refused the job, when the read failed, when the gate did not approve it,
/// and when the encoded result could not be decoded.
pub(crate) async fn source_files(
    io: &ShellIo,
    initial_dir: &Path,
    cwd: &Path,
    patterns: &[String],
    quiet: bool,
    strict: bool,
) -> Result<Vec<crate::handler::scripting_support::ManagedSourceFile>, RmuxError> {
    let mut argv = vec![
        crate::io::builtins::RMUX_IO_BUILTIN.to_owned(),
        "source".to_owned(),
        "--policy".to_owned(),
        if strict { "strict" } else { "compat" }.to_owned(),
    ];
    if quiet {
        argv.push("--quiet".to_owned());
    }
    argv.push("--cwd".to_owned());
    argv.push(cwd.to_string_lossy().into_owned());
    argv.push("--".to_owned());
    argv.extend(patterns.iter().cloned());

    let captured = collect(
        io,
        builtin_spec(io, initial_dir, crate::io::protocol::builtin_plan(&argv))?,
        COMPLETE,
    )
    .await?;
    finish_builtin(&captured, "source")?;
    // Before decoding, not after: these files become dispatched configuration commands, and
    // dispatching a single one of them from a read the gate refused is exactly the trust the
    // instrumentation exists to withhold.
    require_published(&captured)?;
    bincode::deserialize(&captured.stdout).map_err(|error| {
        RmuxError::Server(format!("failed to decode managed source result: {error}"))
    })
}

/// Enumerates the tunnel preset names configured in `directories`.
///
/// Behind `web` because tunnel presets are: `web/tunnel/preset.rs` is this function's only caller,
/// and the whole tunnel provider is compiled out without the feature. The `__rmux_io presets`
/// subcommand it invokes is unconditional, because the builtin's subcommand set is frozen for
/// every shell of this mux and must not vary with a cargo feature.
///
/// # Errors
///
/// Fails when the engine refused the job, when the enumeration failed, and when the gate did not
/// approve it.
#[cfg(feature = "web")]
pub(crate) async fn preset_names(
    io: &ShellIo,
    cwd: &Path,
    directories: &[std::path::PathBuf],
) -> Result<Vec<String>, RmuxError> {
    let mut argv = vec![
        crate::io::builtins::RMUX_IO_BUILTIN.to_owned(),
        "presets".to_owned(),
        "--".to_owned(),
    ];
    argv.extend(
        directories
            .iter()
            .map(|directory| directory.to_string_lossy().into_owned()),
    );

    let captured = collect(
        io,
        builtin_spec(io, cwd, crate::io::protocol::builtin_plan(&argv))?,
        COMPLETE,
    )
    .await?;
    finish_builtin(&captured, "presets")?;
    // Same rule as every other read here: the names become a user-visible menu of things the
    // daemon will launch, so an unchecked enumeration is not an answer.
    require_published(&captured)?;
    Ok(String::from_utf8_lossy(&captured.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// The specification for one `__rmux_io` invocation.
///
/// [`ProcessCommand::Shell`] carrying a plain quoted command line from
/// [`crate::io::protocol::builtin_plan`], and deliberately *not*
/// [`crate::io::protocol::exec_plan`]. `exec` is marsh's external-argv path: it resolves its
/// operand with `find_first_executable_in_path` and runs with functions disabled, exactly as bash
/// does, so `exec -- __rmux_io read …` cannot reach a registered builtin and exits 127. The
/// builtin has to be the command word of an ordinary top-level line — which is also what plan
/// line 386 requires, since a builtin invoked inside a subshell or a pipeline would lose the
/// scoped managed context it refuses to run without.
///
/// The environment is inherited rather than replaced: the builtin reads no variables of its own,
/// and replacing the profile's would strip the snapshot root the managed context resolves paths
/// against. The name is automatic, because a file helper is a one-off; a recurring slot that wants
/// a stable principal builds its own specification.
fn builtin_spec(io: &ShellIo, cwd: &Path, line: String) -> Result<ExecutionSpec, RmuxError> {
    // The helper's own directory only has to exist: its operands are absolute or resolved against
    // an explicit `--cwd`, so an omitted one falls back to this host's default rather than
    // stopping a save that would have reached a path that still exists.
    let initial_dir = if cwd.as_os_str().is_empty() {
        io.default_dir().to_path_buf()
    } else {
        cwd.to_path_buf()
    };
    Ok(ExecutionSpec {
        initial_dir,
        id: None,
        process: ProcessCommand::Shell(line),
        environment: None,
    })
}

/// Turns a nonzero builtin exit into the error its standard error describes.
///
/// The builtin writes one diagnostic line and exits nonzero; reporting "exit status 1" instead of
/// that line would throw away the only part a user can act on.
fn finish_builtin(captured: &CapturedOutput, what: &str) -> Result<(), RmuxError> {
    if captured.completion.exit_code() == Some(0) {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&captured.stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        return Err(RmuxError::Server(format!(
            "managed file {what} failed with status {}",
            captured
                .completion
                .exit_code()
                .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
        )));
    }
    Err(RmuxError::Server(detail.to_owned()))
}

#[cfg(test)]
#[path = "managed_workload/test_engine.rs"]
pub(crate) mod test_engine;
