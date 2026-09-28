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
//! So it runs as a builtin inside a shell command, under that shell's principal and the route
//! its sandbox policy selected. On the managed route its writes are staged, its attempt is
//! recorded, and its result becomes visible only when the gate approves the boundary; on the
//! direct route it acts on the source exactly as the rest of that command does.
//!
//! Registration is local to each shell. Brush's generic observer scopes the real builtin body
//! and every tracked worker, independently of other live muxes.
//!
//! # What it is not
//!
//! Not a wire protocol. Its subcommands are a closed set, its output is private command output
//! inside a shell command, and nothing outside this crate speaks it.
//!
//! Not a confinement boundary either. A FIFO endpoint or a `/dev` node reached through it is still
//! an ordinary OS effect: it is *recorded*, and it is not claimed to be snapshotted, confidential
//! or reversible. Only staged regular-file changes get the publication guarantee.

use std::path::{Path, PathBuf};

use brush_core::commands::ExecutionContext;
use brush_core::results::ExecutionResult;
use marsh_core::builtins::{current_context, BuiltinContext, Command, Registration};

/// The name this builtin is registered under.
///
/// Double-underscored because it is private daemon machinery, not a command a user should discover
/// or a script should call.
pub(crate) const RMUX_IO_BUILTIN: &str = "__rmux_io";

/// The opaque registration installed into every shell of this daemon's mux.
pub(crate) fn registration() -> Registration {
    marsh_core::builtins::builtin::<RmuxIoBuiltin>()
}

#[derive(clap::Parser)]
struct RmuxIoBuiltin {
    #[clap(allow_hyphen_values = true, num_args = 0..)]
    words: Vec<String>,
}
impl Command for RmuxIoBuiltin {
    type Error = brush_core::Error;
    fn new<I: IntoIterator<Item = String>>(args: I) -> Result<Self, clap::Error> {
        Ok(Self {
            words: args.into_iter().collect(),
        })
    }
    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        Ok(dispatch(&context, &self.words).await)
    }
}

/// Dispatches to a subcommand, refusing outright without an owning command context.
///
/// The refusal is the point. A context is how this builtin learns which filesystem view to act in
/// and which cancellation to honour; without one it would be doing work no command owns.
///
/// The shell hands a builtin its own command name as argument zero, the way a program's `argv`
/// carries it, so that leading word is dropped before the subcommand is read. It is matched
/// rather than counted: an invocation that already arrives without it — anything that strips
/// argument zero the way `exec` does for an external program — still finds its subcommand.
async fn dispatch(
    context: &ExecutionContext<'_, impl brush_core::ShellExtensions>,
    words: &[String],
) -> ExecutionResult {
    let Some(owner) = current_context() else {
        let _ = writeln!(
            context.stderr(),
            "{RMUX_IO_BUILTIN}: refusing to run outside a shell command"
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
        "read" => read(context, &owner, rest).await,
        "write" => write(context, &owner, rest).await,
        "source" => source(context, &owner, rest).await,
        "presets" => presets(context, &owner, rest).await,
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

/// `read -- PATH`: the file's raw bytes on standard output.
async fn read(
    context: &ExecutionContext<'_, impl brush_core::ShellExtensions>,
    owner: &BuiltinContext,
    words: &[String],
) -> ExecutionResult {
    let mut flags = Vec::new();
    let operands = operands(words, &mut flags);
    let Some(path) = operands.first() else {
        let _ = writeln!(context.stderr(), "{RMUX_IO_BUILTIN} read: missing path");
        return ExecutionResult::new(2);
    };
    let path = PathBuf::from(path);

    match crate::buffer_file_io::read(path, Some(owner)).await {
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
    context: &ExecutionContext<'_, impl brush_core::ShellExtensions>,
    owner: &BuiltinContext,
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
    let path = PathBuf::from(path);

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
    let drained = owner.spawn_blocking(move || {
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
            let worker_context = owner.clone();
            // Through the context's tracked worker, so the core joins it before any boundary: a
            // directory created after a discard would be a change nobody staged.
            match owner.spawn_blocking(move || worker_context.create_dir_all(&parent)) {
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

    match crate::buffer_file_io::write(path, content, append, Some(owner)).await {
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
/// Logical file paths and nested-source diagnostics are preserved while I/O targets the work view.
async fn source(
    context: &ExecutionContext<'_, impl brush_core::ShellExtensions>,
    owner: &BuiltinContext,
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
                Some(owner),
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
    context: &ExecutionContext<'_, impl brush_core::ShellExtensions>,
    owner: &BuiltinContext,
    words: &[String],
) -> ExecutionResult {
    let mut flags = Vec::new();
    let directories = operands(words, &mut flags);
    let mut names: Vec<String> = Vec::new();
    for directory in directories {
        let path = Path::new(directory);
        let Ok(entries) = owner.read_dir(path) else {
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
