//! `__rmux_io`: the instrumented builtin every native file helper in this daemon goes through.
//!
//! # Why a builtin and not a function call
//!
//! The daemon does real filesystem work on a user's behalf: `load-buffer` reads a file,
//! `save-buffer` writes one, `source-file` reads configuration, the tunnel runner enumerates
//! presets. Every one of those is *requested work*. Doing it with a plain `std::fs` call inside a
//! handler would make it invisible to the instrumentation, unattributable to a principal, and
//! — worst — unstaged: it would write straight into the seed, bypassing the gate that every shell
//! command has to pass.
//!
//! So it runs as a builtin, inside a managed command, in a job with its own snapshot and its own
//! principal. Its writes are staged. Its attempt is recorded. Its result becomes visible only when
//! the gate approves the boundary.
//!
//! # Registered uniformly, before attachment
//!
//! This builtin is in the one profile `ShellIo::new` freezes, which every shell of this daemon's
//! mux is built from — panes, popups and helpers alike — and it is registered
//! *before* `Shell::attach`. Attaching installs one process-wide instrumented builtin table, so a
//! late registration would run uninstrumented and a partial one would break the table for every
//! other shell.
//!
//! # What it is not
//!
//! Not a wire protocol. Its subcommands are a closed set, its output is private command output
//! inside a managed job, and nothing outside this crate speaks it.
//!
//! Not a confinement boundary either. A FIFO endpoint or a `/dev` node reached through it is still
//! an ordinary OS effect: it is *recorded*, and it is not claimed to be snapshotted, confidential
//! or reversible. Only staged regular-file changes get the publication guarantee.

use std::path::{Path, PathBuf};

use brush_core::builtins::Registration;
use brush_core::commands::{CommandArg, ExecutionContext};
use brush_core::results::ExecutionResult;
use marsh_core::shellmux::{current_command_context, CommandContext};
use marsh_core::MarshShellExtensions;

/// The name this builtin is registered under.
///
/// Double-underscored because it is private daemon machinery, not a command a user should discover
/// or a script should call.
pub(crate) const RMUX_IO_BUILTIN: &str = "__rmux_io";

/// The registration installed into every shell of this daemon's mux.
pub(crate) fn registration() -> Registration<MarshShellExtensions> {
    Registration {
        execute_func: execute,
        content_func: content,
        disabled: false,
        special_builtin: false,
        declaration_builtin: false,
    }
}

/// Help content. Deliberately terse: this is not a user-facing command.
fn content(
    _name: &str,
    _content_type: brush_core::builtins::ContentType,
    _options: &brush_core::builtins::ContentOptions,
) -> Result<String, brush_core::Error> {
    Ok(String::from(
        "__rmux_io: private rmux daemon file helper; not for interactive use\n",
    ))
}

/// Runs one subcommand.
fn execute(
    context: ExecutionContext<'_, MarshShellExtensions>,
    args: Vec<CommandArg>,
) -> brush_core::builtins::BoxFuture<'_, Result<ExecutionResult, brush_core::Error>> {
    Box::pin(async move {
        let words: Vec<String> = args
            .into_iter()
            .map(|argument| match argument {
                CommandArg::String(value) => value,
                CommandArg::Assignment(assignment) => assignment.to_string(),
            })
            .collect();
        Ok(dispatch(&context, &words).await)
    })
}

/// Dispatches to a subcommand, refusing outright without a managed command context.
///
/// The refusal is the point. A context is how this builtin learns which snapshot to stage into and
/// which cancellation to honour; without one it would be doing unmanaged filesystem work under a
/// name that promises the opposite.
///
/// The shell hands a builtin its own command name as argument zero, the way a program's `argv`
/// carries it, so that leading word is dropped before the subcommand is read. It is matched
/// rather than counted: an invocation that already arrives without it — anything that strips
/// argument zero the way `exec` does for an external program — still finds its subcommand.
async fn dispatch(
    context: &ExecutionContext<'_, MarshShellExtensions>,
    words: &[String],
) -> ExecutionResult {
    let Some(managed) = current_command_context() else {
        let _ = writeln!(
            context.stderr(),
            "{RMUX_IO_BUILTIN}: refusing to run outside a managed command"
        );
        return ExecutionResult::new(2);
    };

    let words = match words.split_first() {
        Some((first, rest)) if first == RMUX_IO_BUILTIN => rest,
        _ => words,
    };
    let Some((subcommand, rest)) = words.split_first() else {
        let _ = writeln!(context.stderr(), "{RMUX_IO_BUILTIN}: missing subcommand");
        return ExecutionResult::new(2);
    };

    match subcommand.as_str() {
        "read" => read(context, &managed, rest).await,
        "write" => write(context, &managed, rest).await,
        "source" => source(context, rest).await,
        "presets" => presets(context, &managed, rest).await,
        other => {
            let _ = writeln!(
                context.stderr(),
                "{RMUX_IO_BUILTIN}: unknown subcommand {other}"
            );
            ExecutionResult::new(2)
        }
    }
}

use std::io::Write as _;

/// Splits `--`-terminated options from operands.
fn operands<'a>(words: &'a [String], flags: &mut Vec<&'a str>) -> &'a [String] {
    let mut index = 0;
    while index < words.len() {
        if words[index] == "--" {
            return &words[index + 1..];
        }
        flags.push(words[index].as_str());
        index += 1;
    }
    &[]
}

/// Rebases `path` so a write lands in the helper's own snapshot rather than the live seed.
///
/// Three cases, in this order:
///
/// * already inside this command's snapshot — used as is;
/// * inside the seed — the seed prefix is replaced by the snapshot root, so the write is staged;
/// * anywhere else — left alone. An absolute path outside the seed is an ordinary OS effect this
///   engine observes but does not confine, and pretending otherwise would be a false promise.
fn rebase(managed: &CommandContext, path: &Path) -> PathBuf {
    let Some(root) = managed.snapshot_root() else {
        return path.to_path_buf();
    };
    if path.starts_with(root) {
        return path.to_path_buf();
    }
    path.strip_prefix(managed.seed())
        .map_or_else(|_| path.to_path_buf(), |relative| root.join(relative))
}

/// `read -- PATH`: the file's raw bytes on standard output.
async fn read(
    context: &ExecutionContext<'_, MarshShellExtensions>,
    managed: &CommandContext,
    words: &[String],
) -> ExecutionResult {
    let mut flags = Vec::new();
    let operands = operands(words, &mut flags);
    let Some(path) = operands.first() else {
        let _ = writeln!(context.stderr(), "{RMUX_IO_BUILTIN} read: missing path");
        return ExecutionResult::new(2);
    };
    // A read is rebased too: a helper reading back what an earlier helper staged must see the
    // staged copy, not the seed's older one.
    let path = rebase(managed, Path::new(path));

    match crate::buffer_file_io::read(path).await {
        Ok(bytes) => {
            let mut out = context.stdout();
            if out.write_all(&bytes).is_err() {
                return ExecutionResult::new(1);
            }
            let _ = out.flush();
            ExecutionResult::success()
        }
        Err(error) => {
            let _ = writeln!(context.stderr(), "{RMUX_IO_BUILTIN} read: {error}");
            ExecutionResult::new(1)
        }
    }
}

/// `write [--append] [--mkdirs] -- PATH`: raw standard input into a staged file.
async fn write(
    context: &ExecutionContext<'_, MarshShellExtensions>,
    managed: &CommandContext,
    words: &[String],
) -> ExecutionResult {
    let mut flags = Vec::new();
    let operands = operands(words, &mut flags);
    let append = flags.contains(&"--append");
    let mkdirs = flags.contains(&"--mkdirs");
    let Some(path) = operands.first() else {
        let _ = writeln!(context.stderr(), "{RMUX_IO_BUILTIN} write: missing path");
        return ExecutionResult::new(2);
    };
    let path = rebase(managed, Path::new(path));

    // Standard input here is the job's real pipe, and the only writer is the caller that started
    // this helper. The read therefore blocks until *that caller* sends end of file — so doing it
    // on this task would hold an async worker for exactly as long as the caller needs one to
    // deliver it. On a single-worker runtime that is a deadlock, and on any runtime it is a
    // worker held hostage by a peer. It is registered blocking work, which is what
    // [`CommandContext::spawn_blocking`] is for: the core joins the worker before it decides any
    // boundary, so the bytes are still read before the gate sees them.
    let Some(mut input) = context.params.try_stdin(context.shell) else {
        let _ = writeln!(
            context.stderr(),
            "{RMUX_IO_BUILTIN} write: cannot read stdin"
        );
        return ExecutionResult::new(1);
    };
    let drained = managed.spawn_blocking(move || {
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut input, &mut content).map(|_| content)
    });
    let content = match drained {
        Ok(receiver) => match receiver.await {
            Ok(Ok(content)) => content,
            Ok(Err(error)) => {
                let _ = writeln!(context.stderr(), "{RMUX_IO_BUILTIN} write: {error}");
                return ExecutionResult::new(1);
            }
            Err(_) => {
                let _ = writeln!(
                    context.stderr(),
                    "{RMUX_IO_BUILTIN} write: cannot read stdin"
                );
                return ExecutionResult::new(1);
            }
        },
        Err(error) => {
            let _ = writeln!(context.stderr(), "{RMUX_IO_BUILTIN} write: {error}");
            return ExecutionResult::new(1);
        }
    };

    if mkdirs {
        if let Some(parent) = path.parent() {
            let parent = parent.to_path_buf();
            // Through the context's tracked worker, so the core joins it before any boundary: a
            // directory created after a discard would be a change nobody staged.
            match managed.spawn_blocking(move || std::fs::create_dir_all(parent)) {
                Ok(receiver) => {
                    if let Ok(Err(error)) = receiver.await {
                        let _ = writeln!(context.stderr(), "{RMUX_IO_BUILTIN} write: {error}");
                        return ExecutionResult::new(1);
                    }
                }
                Err(error) => {
                    let _ = writeln!(context.stderr(), "{RMUX_IO_BUILTIN} write: {error}");
                    return ExecutionResult::new(1);
                }
            }
        }
    }

    match crate::buffer_file_io::write(path, content, append).await {
        Ok(()) => ExecutionResult::success(),
        Err(error) => {
            let _ = writeln!(context.stderr(), "{RMUX_IO_BUILTIN} write: {error}");
            ExecutionResult::new(1)
        }
    }
}

/// `source --policy strict|compat [--quiet] --cwd DIR -- PATTERN…`: the existing source reader.
///
/// The result is encoded with this crate's existing bincode support, because the caller is this
/// crate and the alternative — reimplementing the glob, BOM, identity and nesting rules on the
/// other side of a text format — is how two implementations of one thing diverge.
///
/// One encoded result per requested pattern, in order, and a read that failed is one of those
/// results rather than a nonzero exit: `source-file a b` reports `a`'s diagnostic and still loads
/// `b`, and the caller needs the reader's own error value, not a re-wrapped copy of its text. The
/// exit status is reserved for this helper failing to do its job at all.
///
/// # Paths are not rebased, and why the three subcommands differ
///
/// [`write`] rebases because it stages: the plan's rebase rule exists so a managed write never
/// touches the live seed and becomes visible only at an approved boundary. [`read`] rebases to
/// stay consistent with it — a helper reading back what an earlier helper in the same command
/// staged has to see the staged copy. Neither of them reports a path to anyone.
///
/// This one does, so its patterns and its `--cwd` arrive untouched. A source read's resolved
/// paths are *results*: they become `#{current_file}`, they are the location in every parse
/// diagnostic a user reads, and they are the directory a nested `source-file` resolves its own
/// relative paths against. A snapshot copy's name is none of those things — it is not a path the
/// user can act on, and it is not a path the caller can resolve against once the boundary has
/// closed. Rebasing `--cwd` would also silently move where a relative pattern resolves, which is
/// exactly the caller-cwd semantics this helper exists to preserve.
///
/// This is a deliberate narrowing of the plan's rebase sentence, which is about writes ("never
/// write the live seed directly"). Nothing is lost by it: a read cannot miss a staged
/// configuration file, because an earlier helper's write is visible here only once it has been
/// published into the seed, and a write that was not published must not be visible at all.
async fn source(
    context: &ExecutionContext<'_, MarshShellExtensions>,
    words: &[String],
) -> ExecutionResult {
    let mut flags = Vec::new();
    let patterns = operands(words, &mut flags);
    let quiet = flags.contains(&"--quiet");
    // Absent rather than defaulted: the in-process reader takes an optional directory, and `None`
    // is what leaves a relative pattern resolving the way `glob` would have resolved it.
    let cwd = flags
        .iter()
        .position(|flag| *flag == "--cwd")
        .and_then(|index| flags.get(index + 1))
        .map(|value| PathBuf::from(*value));

    let strict = flags.contains(&"--policy") && flags.contains(&"strict");

    let files: Vec<crate::handler::scripting_support::ManagedSourceFile> = patterns
        .iter()
        .map(|pattern| {
            crate::handler::scripting_support::managed_source_read(
                pattern,
                cwd.as_deref(),
                quiet,
                strict,
            )
        })
        .collect();

    let Ok(encoded) = bincode::serialize(&files) else {
        let _ = writeln!(
            context.stderr(),
            "{RMUX_IO_BUILTIN} source: cannot encode result"
        );
        return ExecutionResult::new(1);
    };

    let mut out = context.stdout();
    if out.write_all(&encoded).is_err() {
        return ExecutionResult::new(1);
    }
    let _ = out.flush();
    ExecutionResult::success()
}

/// `presets -- DIR…`: the configured tunnel preset names, one per line.
async fn presets(
    context: &ExecutionContext<'_, MarshShellExtensions>,
    managed: &CommandContext,
    words: &[String],
) -> ExecutionResult {
    let mut flags = Vec::new();
    let directories = operands(words, &mut flags);
    let mut names: Vec<String> = Vec::new();
    for directory in directories {
        let path = rebase(managed, Path::new(directory));
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let entry = entry.path();
            if entry
                .extension()
                .is_some_and(|extension| extension == "toml")
            {
                if let Some(stem) = entry.file_stem() {
                    names.push(stem.to_string_lossy().into_owned());
                }
            }
        }
    }
    names.sort_unstable();
    names.dedup();

    let mut out = context.stdout();
    for name in &names {
        if writeln!(out, "{name}").is_err() {
            return ExecutionResult::new(1);
        }
    }
    let _ = out.flush();
    ExecutionResult::success()
}
