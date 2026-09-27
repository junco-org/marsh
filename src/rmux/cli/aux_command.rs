//! The skeleton shared by rmux's auxiliary top-level commands.
//!
//! `capabilities`, `diagnose`, the tmux drop-in `doctor`/`setup` pair and `claude` are served
//! from raw argv before the typed command queue is parsed, because their arguments are not tmux
//! commands. Each one implements [`AuxCommand`]; what they have in common — finding the command
//! word behind tmux's top-level flags, choosing an output format, wording failures and writing
//! stdout — lives here once.

use std::ffi::{OsStr, OsString};
use std::fmt::Display;
use std::io::{self, ErrorKind, Write};
use std::path::PathBuf;

use super::ExitFailure;

/// An rmux extension command recognized in raw argv ahead of the typed parser.
pub(super) trait AuxCommand: Sized {
    /// Recognizes this command in `arguments`, returning `None` when they name another command.
    fn parse(arguments: &[OsString]) -> Result<Option<Self>, ExitFailure>;

    /// Runs the command for the invocation whose complete argv is `argv`, yielding its exit code.
    fn run(self, argv: &[OsString]) -> Result<i32, ExitFailure>;

    /// Parses the arguments after argv0 and runs the command, or `None` to leave argv to others.
    fn dispatch(argv: &[OsString]) -> Option<Result<i32, ExitFailure>> {
        Self::parse(argv.get(1..).unwrap_or(&[]))
            .transpose()
            .map(|parsed| parsed.and_then(|command| command.run(argv)))
    }
}

/// A top-level flag [`command_word`] passed on its way to the command word.
#[derive(Clone, Copy)]
pub(super) enum TopLevelFlag<'a> {
    /// A run of value-less switches such as `2` or `2u`, without its leading `-`.
    Switches(&'a str),
    /// A value-taking flag letter with its value, given separately (`-L name`) or glued (`-Lname`).
    Value(char, &'a OsStr),
}

/// tmux's value-less top-level switches, accepted alone or clustered.
const SWITCHES: &str = "2CDNluv";

/// Finds the command word behind tmux's top-level flags, together with the arguments after it.
///
/// `separate` and `glued` name the value-taking flags accepted as `-L name` and as `-Lname`, and
/// `visit` sees every flag passed. Scanning stops at `--`, at the first word that is not a flag,
/// and at a flag outside those sets, which is then taken as the command word. `None` means argv
/// ran out, or held non-UTF-8 text, before a command word.
pub(super) fn command_word<'a>(
    arguments: &'a [OsString],
    separate: &str,
    glued: &str,
    mut visit: impl FnMut(TopLevelFlag<'a>),
) -> Option<(&'a str, &'a [OsString])> {
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        let argument = argument.to_str()?;
        if argument == "--" {
            index += 1;
            break;
        }
        let Some(flags) = argument.strip_prefix('-') else {
            break;
        };
        let mut letters = flags.chars();
        let Some(letter) = letters.next() else {
            break;
        };
        let value = letters.as_str();
        if value.is_empty() && separate.contains(letter) {
            index += 1;
            visit(TopLevelFlag::Value(letter, arguments.get(index)?));
        } else if !value.is_empty() && glued.contains(letter) {
            visit(TopLevelFlag::Value(letter, OsStr::new(value)));
        } else if flags.chars().all(|flag| SWITCHES.contains(flag)) {
            visit(TopLevelFlag::Switches(flags));
        } else {
            break;
        }
        index += 1;
    }
    let (command, rest) = arguments.get(index..)?.split_first()?;
    Some((command.to_str()?, rest))
}

/// Whether a report command prints its human summary or JSON.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum OutputFormat {
    #[default]
    Human,
    Json,
}

impl OutputFormat {
    /// Parses the `--human`, `--json` and `--help` arguments after `command`, defaulting to human.
    pub(super) fn parse(command: &str, arguments: &[OsString]) -> Result<Self, ExitFailure> {
        let mut chosen = None;
        for argument in arguments {
            let format = match argument.to_str() {
                Some("--human") => Self::Human,
                Some("--json") => Self::Json,
                Some("--help") => {
                    return Err(ExitFailure::new_stdout(
                        0,
                        format!("usage: rmux {command} [--human|--json]"),
                    ));
                }
                Some(other) => {
                    return Err(failure(command, format_args!("unknown argument '{other}'")));
                }
                None => return Err(failure(command, "arguments must be valid UTF-8")),
            };
            if chosen.is_some_and(|chosen| chosen != format) {
                return Err(failure(command, "choose only one of --human or --json"));
            }
            chosen = Some(format);
        }
        Ok(chosen.unwrap_or_default())
    }
}

/// An exit-status-1 failure worded `rmux {command}: {message}`.
pub(super) fn failure(command: &str, message: impl Display) -> ExitFailure {
    ExitFailure::new(1, format!("rmux {command}: {message}"))
}

/// The user's home directory from a nonempty `HOME`, failing as `rmux {command}: HOME is not set`.
pub(super) fn user_home(command: &str) -> Result<PathBuf, ExitFailure> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| failure(command, "HOME is not set"))
}

/// Settles a stdout write as exit status 0, treating a reader that hung up as a finished write.
///
/// Any error other than a broken pipe fails with exit status 1 and the message `describe` gives.
pub(super) fn stdout_written(
    written: io::Result<()>,
    describe: impl FnOnce(io::Error) -> String,
) -> Result<i32, ExitFailure> {
    match written {
        Err(error) if error.kind() != ErrorKind::BrokenPipe => {
            Err(ExitFailure::new(1, describe(error)))
        }
        _ => Ok(0),
    }
}

/// Writes `bytes` to stdout, failing as `failed to write {subject} output: {error}`.
pub(super) fn write_stdout(bytes: &[u8], subject: &str) -> Result<i32, ExitFailure> {
    stdout_written(io::stdout().lock().write_all(bytes), |error| {
        format!("failed to write {subject} output: {error}")
    })
}

/// Builds an argument vector from string literals.
#[cfg(test)]
pub(super) fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}
