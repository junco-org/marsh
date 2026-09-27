//! A frontend's line grammar and its report text.
//!
//! Everything about a submitted line that is a pure function of the line, and everything about a
//! concluded line that is a pure function of its outcome.
//!
//! Both halves live here because both are the parts of a front-end that can be *proved*. Job
//! control, terminal handoff and mux calls are all effects; parsing `sd foo ./api` and
//! rendering `%foo denied 1 of 2:` are not, so they are separated out and unit-tested directly.
//!
//! The grammar is deliberately tiny and resolved *before* any brush parsing: the frontend builtins
//! (`jobs`, `fg`, `bg`, `stop`, `kill`, `sd`, and the trailing `&`) never reach the text,
//! because the shell that composes the prompt is not the shell that runs commands — every real
//! command line is handed to a job instead. `exit` is deliberately not among them: it is the
//! shell's own builtin, so its status argument reaches the job that runs it.

use crate::shellmux::ShellId;
use crate::{ExecutionResult, ShellError};

/// The foreground principal: every line submitted without `&` or `spawn` runs as this one.
///
/// It is also the reserved job name, so `%main` can never mean two different things.
pub const FOREGROUND: &str = "main";

/// What a submitted line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    /// Nothing was typed.
    Empty,
    /// List the job table.
    Jobs,
    /// Attach a job to the terminal; `None` means the most recent one.
    Fg(Option<String>),
    /// Signal process ids; the tokens are passed through verbatim, signal flag included.
    Kill(Vec<String>),
    /// Close a job; the tokens are the optional `-f` flag, `--`, and the job's name.
    Stop(Vec<String>),
    /// Create a named sandbox rooted at a seed directory.
    SpawnDir {
        /// The job's name, which is also its principal; `None` takes the next number.
        name: Option<String>,
        /// The directory as typed, resolved against the current job by [`job_dir`].
        dir: String,
    },
    /// Start the line in a job of its own, without waiting for it (a trailing `&`), `&` stripped.
    Background {
        /// The command line, with the `&` form removed.
        cmd: String,
        /// The job's name; `None` takes the next number, as `bg` does.
        name: Option<String>,
    },
    /// Run the line as the foreground job.
    Foreground(String),
    /// The line named a console builtin but got its arguments wrong; the message is for stderr.
    Invalid(String),
}

/// Parses one submitted line.
///
/// Dispatch is on the first token of the trimmed line, so a console builtin is recognized before
/// anything else can interpret it. Everything else is a command line, taken as the verbatim
/// remainder of the line: a builtin would receive word-split, expansion-processed argv, and
/// rebuilding a command *string* from it would need lossy re-quoting, so
/// `sh -c 'sleep 1; echo hi'` would not survive the round trip.
#[allow(
    clippy::string_slice,
    reason = "`first` is a prefix of the trimmed line, so its length is a char boundary in it"
)]
pub fn parse(line: &str) -> Input {
    let line = line.trim();
    if line.is_empty() {
        return Input::Empty;
    }
    let mut tokens = line.split_whitespace();
    let first = tokens.next().unwrap_or_default();
    let rest: Vec<&str> = tokens.collect();

    match first {
        "jobs" if rest.is_empty() => Input::Jobs,
        "fg" => Input::Fg(job_reference(&line[first.len()..])),
        // `kill` keeps its argv verbatim — the signal flag and every target are the builtin's to
        // interpret, exactly as in bash, where `kill -9 1234 5678` is one invocation. Its tokens
        // stay whitespace-split because a process id never has spaces in it.
        "kill" => Input::Kill(rest.iter().map(|token| (*token).to_string()).collect()),
        "stop" => stop(line[first.len()..].trim()),
        "sd" => match rest.as_slice() {
            [name, dir] => sd(name, dir),
            _ => Input::Invalid("sd: usage: sd NAME DIR".to_string()),
        },
        // `bg DIR` is `sd` with the naming left to the series: it takes a directory rather than a
        // job reference, because nothing marsh runs can be suspended.
        "bg" => match rest.as_slice() {
            [dir] => Input::SpawnDir {
                name: None,
                dir: (*dir).to_string(),
            },
            _ => Input::Invalid("bg: usage: bg DIR".to_string()),
        },
        // A trailing `&`, `&NAME` or `&"NAME"` opens a job; `&&` is an operator and belongs to the
        // command line.
        _ => background(line).unwrap_or_else(|| Input::Foreground(line.to_string())),
    }
}

/// The `Input` a trailing `&`, `&NAME` or `&"NAME"` asks for, or `None` when the line has neither.
///
/// The `&` that opens a job may not be preceded by another one, so `a && b` and `a &&b` stay whole
/// command lines. A bare `&NAME` is recognized only when `NAME` is a single word of name
/// characters, which is what keeps `echo a & b` — a `&` with a space after it — a command line as
/// well. The quoted form is looked for only when the line *ends* with `"`, takes the last `&"`
/// before it, and is abandoned when what that yields contains a `"` of its own — which is what
/// keeps a command line like `grep -e "&" -f "x"` whole rather than reading `-f "x` as a job name.
#[allow(
    clippy::string_slice,
    reason = "every index here is a byte offset of an ASCII `&` or `\"` this scanner matched, so it is always a char boundary"
)]
fn background(line: &str) -> Option<Input> {
    let (start, name) = if let Some(head) = line.strip_suffix('"') {
        let start = head.rfind("&\"")?;
        let name = &head[start + 2..];
        if name.contains('"') {
            return None;
        }
        (start, Some(name))
    } else if let Some(head) = line.strip_suffix('&') {
        (head.len(), None)
    } else {
        let start = line.rfind('&')?;
        let name = &line[start + 1..];
        if !ShellId::from(name).is_bare() {
            return None;
        }
        (start, Some(name))
    };
    if start == 0 || line.as_bytes()[start - 1] == b'&' {
        return None;
    }
    let cmd = line[..start].trim_end();
    if cmd.is_empty() {
        return None;
    }
    Some(match name {
        None => Input::Background {
            cmd: cmd.to_string(),
            name: None,
        },
        Some(name) => named_background(cmd, name),
    })
}

/// A named background line, or the diagnostic for a name a job may not answer to.
///
/// `%` is how a job is marked inside a line of prose and comes off the front of a typed reference,
/// and `"` is the quote that wraps one, so neither may appear in a name; a control character would
/// corrupt the table the name is printed in; and [`FOREGROUND`] is the one name already taken.
fn named_background(cmd: &str, name: &str) -> Input {
    if name == FOREGROUND {
        return Input::Invalid(format!(
            "&: invalid job name {name:?} ({FOREGROUND:?} is the foreground job)"
        ));
    }
    if name.is_empty() || name.chars().any(|c| c == '%' || c == '"' || c.is_control()) {
        return Input::Invalid(format!(
            "&: invalid job name {name:?} (not empty, and no % or \")"
        ));
    }
    Input::Background {
        cmd: cmd.to_string(),
        name: Some(name.to_string()),
    }
}

/// The job a `fg` or `stop` argument names, or `None` when there is no argument.
///
/// The whole rest of the line is the name, because each of those builtins takes exactly one job and
/// a job name may hold spaces: `fg a long name` needs no quoting. `"a long name"` is the one
/// accepted wrapping — what `&"a long name"` opened the job with, and what `jobs` prints it as —
/// and it is the way to reach a name the bare form cannot, one that begins with `-` or ends in a
/// space. One leading `%` comes off as well, so a row copied out of `jobs`, which writes
/// `%"a long name"`, pastes back unchanged.
fn job_reference(text: &str) -> Option<String> {
    let text = text.trim();
    let text = text.strip_prefix('%').unwrap_or(text);
    let name = text
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(text);
    (!name.is_empty()).then(|| name.to_string())
}

/// Parses `stop [-f] JOB`.
///
/// Leading option tokens come off the front verbatim, so the builtin's own parser decides what they
/// mean and reports what it does not accept; the scan ends at `--`, which is consumed, or at the
/// first token that is not an option. Everything left is the job — one name, however it was
/// wrapped — and a `--` is inserted before it, so a job whose name looks like a flag is still read
/// as a name. A quoted or `%`-prefixed token is a name, not an option, so `stop "-f"` closes the
/// job named `-f` while `stop -f build` forces `build`.
fn stop(text: &str) -> Input {
    let mut args = Vec::new();
    let mut rest = text.trim();
    loop {
        let (token, tail) = rest
            .split_once(char::is_whitespace)
            .map_or((rest, ""), |(token, tail)| (token, tail.trim_start()));
        if token == "--" {
            rest = tail;
            break;
        }
        if !token.starts_with('-') {
            break;
        }
        args.push(token.to_string());
        rest = tail;
    }
    if let Some(name) = job_reference(rest) {
        args.push("--".to_string());
        args.push(name);
    }
    Input::Stop(args)
}

/// `stop [-f] JOB`: the argument grammar the console builtin and every frontend parse identically.
///
/// [`Input::Stop`] carries the tokens; what they *mean* is this one declaration, so a frontend
/// without brush builtins cannot drift from the console's `stop -f`/`stop "-f"` behavior.
#[derive(Debug, PartialEq, Eq, clap::Parser)]
pub struct StopArgs {
    /// Kill the job's command now instead of letting it finish.
    #[arg(short = 'f')]
    pub force: bool,
    /// The job to close.
    pub job: String,
}

/// Parses the tokens [`Input::Stop`] produced.
///
/// # Errors
///
/// Returns clap's rendered diagnostic — `--help` output included — for anything the grammar does
/// not accept.
pub fn parse_stop(args: &[String]) -> Result<StopArgs, String> {
    use clap::Parser as _;

    StopArgs::try_parse_from(std::iter::once("stop").chain(args.iter().map(String::as_str)))
        .map_err(|error| error.to_string())
}

/// Parses `sd NAME DIR`, validating the name a job — hence a principal — will answer to.
fn sd(name: &str, dir: &str) -> Input {
    if !valid_name(name) {
        return Input::Invalid(format!(
            "sd: invalid name {name:?} (use letters, digits, _ or -; not \"{FOREGROUND}\")"
        ));
    }
    Input::SpawnDir {
        name: Some(name.to_string()),
        dir: dir.to_string(),
    }
}

/// Whether `name` can be a job name typed as a single word, hence a principal.
///
/// [`FOREGROUND`] is reserved so a job can never impersonate the foreground principal. A name with
/// spaces is printed `%"like this"` — see [`ShellId::reference`](crate::shellmux::ShellId) —
/// and is validated where it is created, by the trailing `&`.
pub fn valid_name(name: &str) -> bool {
    ShellId::from(name).is_bare() && name != FOREGROUND
}

/// Resolves the directory typed at `sd`/`bg` against the job it was typed in.
///
/// Read like a `cd` argument: a relative directory hangs below the current job's, which is what
/// makes `sd api docs` name the `docs` beside the files the prompt is showing rather than one at
/// the top of a seed the session may be deep inside. A leading `/` names the seed root — the only
/// way to reach a sibling of the current job's directory without counting `..`s, and never the
/// filesystem's root, since every path here is seed-relative.
///
/// Only the join is here. Normalizing `.` and `..`, and refusing a path that climbs out of the
/// seed, are [`seed_relative`]'s.
pub fn job_dir(base: &str, dir: &str) -> String {
    if base.is_empty() || dir.starts_with('/') {
        return dir.to_string();
    }
    format!("{base}/{dir}")
}

/// Normalizes a seed-relative directory, or `None` when it escapes the seed.
///
/// Purely lexical, and deliberately so: the seed's own layout decides what exists, and a `..` that
/// climbs past the root must be refused before any path is built from it. A leading `/` names the
/// seed root rather than the filesystem's, because everything in this grammar is seed-relative.
///
/// This is the `sd`/`bg` grammar's half of the job, kept apart from the host paths
/// [`ShellMux::open_shell`](crate::shellmux::ShellMux::open_shell) takes: a frontend joins the
/// result onto the seed of the shell the line was typed in, so `/` still means *that* shell's
/// root and never the daemon's default one.
#[must_use]
pub fn seed_relative(dir: &str) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    for component in dir.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    Some(segments.join("/"))
}

/// Renders typed Shell failures without exposing storage counters or capability history.
pub fn report_lines(id: &ShellId, result: &Result<ExecutionResult, ShellError>) -> Vec<String> {
    match result {
        Ok(_) => Vec::new(),
        Err(error) => vec![format!("{}: {error}", id.reference())],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Owned copies of `tokens`.
    fn words(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|token| (*token).to_string()).collect()
    }

    /// The background line `cmd`, opened under `name`.
    fn bg(cmd: &str, name: Option<&str>) -> Input {
        Input::Background {
            cmd: cmd.to_string(),
            name: name.map(str::to_string),
        }
    }

    /// The foreground command line `line`.
    fn foreground(line: &str) -> Input {
        Input::Foreground(line.to_string())
    }

    /// The diagnostic `message`.
    fn invalid(message: &str) -> Input {
        Input::Invalid(message.to_string())
    }

    /// Asserts that every line parses as expected; the third column says why, where that matters.
    fn parses<const N: usize>(cases: [(&str, Input, &str); N]) {
        for (line, expected, why) in cases {
            assert_eq!(parse(line), expected, "{line:?}: {why}");
        }
    }

    /// A typed directory becomes a path under the originating shell's seed, so the one thing it
    /// must never do is name something outside it.
    #[test]
    fn a_typed_directory_cannot_climb_out_of_the_seed() {
        for (dir, expected) in [
            ("", Some("")),
            (".", Some("")),
            ("./src", Some("src")),
            ("/src/", Some("src")),
            ("deep/../src", Some("src")),
            ("..", None),
            ("src/../..", None),
        ] {
            assert_eq!(seed_relative(dir).as_deref(), expected, "{dir:?}");
        }
    }

    #[test]
    fn an_empty_line_asks_for_nothing() {
        parses([("", Input::Empty, ""), ("   \t ", Input::Empty, "")]);
    }

    #[test]
    fn console_builtins_are_recognized_before_the_shell_sees_them() {
        parses([("jobs", Input::Jobs, ""), ("  jobs  ", Input::Jobs, "")]);
    }

    #[test]
    fn fg_takes_one_optional_job_name() {
        parses([
            ("fg", Input::Fg(None), ""),
            ("fg foo", Input::Fg(Some("foo".to_string())), ""),
            (
                "fg a b",
                Input::Fg(Some("a b".to_string())),
                "the rest of the line is the name: fg takes one job, so nothing else could be meant",
            ),
        ]);
    }

    /// A job name may hold spaces and a builtin that takes one job needs no quoting to find it,
    /// but the quoted form resolves too: it is what `&"…"` opened the job with and what `jobs`
    /// prints, so a row of the table pastes straight back.
    #[test]
    fn a_job_is_named_bare_or_quoted() {
        for text in ["a name", "\"a name\"", "%\"a name\"", "  a name  "] {
            assert_eq!(
                parse(&format!("fg {text}")),
                Input::Fg(Some("a name".to_string())),
                "{text}"
            );
        }
        parses([
            (
                "stop -f a name",
                Input::Stop(words(&["-f", "--", "a name"])),
                "the option comes off the front; everything after it is the job",
            ),
            (
                "stop \"-f\"",
                Input::Stop(words(&["--", "-f"])),
                "a quoted token is a name, so the option scan never sees it",
            ),
            (
                "stop -f -- \"-f\"",
                Input::Stop(words(&["-f", "--", "-f"])),
                "an explicit -- ends the option scan and is not passed on twice",
            ),
            (
                "stop %\"a name\"",
                Input::Stop(words(&["--", "a name"])),
                "a row copied out of the job table pastes back",
            ),
            ("stop build", Input::Stop(words(&["--", "build"])), ""),
            (
                "stop -f",
                Input::Stop(words(&["-f"])),
                "an operand-less stop is the builtin's usage error to report, not the parser's",
            ),
            ("stop", Input::Stop(Vec::new()), ""),
            (
                "close build",
                foreground("close build"),
                "close is gone, so the word is an ordinary command line",
            ),
            (
                "kill -9 1234",
                Input::Kill(words(&["-9", "1234"])),
                "kill keeps its verbatim tokens, and jobs are stop's now",
            ),
        ]);
    }

    /// `kill` is the one console builtin with a real argument grammar, so the grammar stays in the
    /// builtin: the parser only has to keep the tokens — signal flag included — intact and ordered.
    #[test]
    fn kill_passes_its_arguments_through_verbatim() {
        parses([
            ("kill 1234", Input::Kill(words(&["1234"])), ""),
            (
                "kill -9 1234 5678",
                Input::Kill(words(&["-9", "1234", "5678"])),
                "",
            ),
            (
                "kill",
                Input::Kill(Vec::new()),
                "an argument-less kill is the builtin's usage error to report, not the parser's",
            ),
        ]);
    }

    /// A job name becomes a principal, so the grammar has to refuse the ones that would collide
    /// with the foreground principal or survive a round trip through `%name` badly.
    #[test]
    fn sd_names_a_sandbox_and_bg_numbers_it() {
        parses([
            (
                "sd api ./foo1",
                Input::SpawnDir {
                    name: Some("api".to_string()),
                    dir: "./foo1".to_string(),
                },
                "",
            ),
            (
                "bg ./foo1",
                Input::SpawnDir {
                    name: None,
                    dir: "./foo1".to_string(),
                },
                "",
            ),
            ("sd", invalid("sd: usage: sd NAME DIR"), ""),
            ("sd api", invalid("sd: usage: sd NAME DIR"), ""),
            ("sd api dir extra", invalid("sd: usage: sd NAME DIR"), ""),
            ("bg", invalid("bg: usage: bg DIR"), ""),
            ("bg a b", invalid("bg: usage: bg DIR"), ""),
            (
                "sd main .",
                invalid("sd: invalid name \"main\" (use letters, digits, _ or -; not \"main\")"),
                "the foreground principal is reserved",
            ),
            (
                "sd a/b .",
                invalid("sd: invalid name \"a/b\" (use letters, digits, _ or -; not \"main\")"),
                "",
            ),
        ]);
    }

    /// The directory typed at `sd` is a path in the job it was typed in. Reading it from the seed
    /// root made `sd api docs` unusable in any session started below the seed root, which is every
    /// session started anywhere but the top of a subvolume.
    #[test]
    fn a_job_directory_hangs_below_the_current_job() {
        for (base, dir, expected, why) in [
            ("marsh", "docs", "marsh/docs", ""),
            ("marsh", "docs/how-to", "marsh/docs/how-to", ""),
            (
                "marsh",
                "..",
                "marsh/..",
                "the mux normalizes; `..` from a job one level down is the seed root",
            ),
            (
                "marsh",
                "/other",
                "/other",
                "a leading slash names the seed root, not the current job",
            ),
            ("", "docs", "docs", "a job at the seed root joins nothing"),
        ] {
            assert_eq!(job_dir(base, dir), expected, "{why}");
        }
    }

    #[test]
    fn a_trailing_ampersand_is_a_job_but_a_double_one_is_an_operator() {
        parses([
            ("sleep 5 &", bg("sleep 5", None), ""),
            ("sleep 5&", bg("sleep 5", None), ""),
            ("a && b", foreground("a && b"), ""),
            ("a &&", foreground("a &&"), ""),
            ("echo hi", foreground("echo hi"), ""),
            (
                "&",
                foreground("&"),
                "an empty command is left for the shell's parser to diagnose",
            ),
        ]);
    }

    #[test]
    fn an_ampersand_can_name_the_job_it_opens() {
        parses([
            ("echo foo &api", bg("echo foo", Some("api")), ""),
            (
                "echo foo &\"a long name\"",
                bg("echo foo", Some("a long name")),
                "quoting is the only way to name a job with spaces in it",
            ),
            (
                "echo \"x\" &\"parse\"",
                bg("echo \"x\"", Some("parse")),
                "the last `&\"` wins, so a command holding quotes of its own survives",
            ),
            (
                "echo \"a\" &",
                bg("echo \"a\"", None),
                "the quoted form is only looked for when the line ends with a quote",
            ),
            (
                "grep -e \"&\" -f \"x\"",
                foreground("grep -e \"&\" -f \"x\""),
                "a candidate name holding a quote of its own is no name, and the line stays a command",
            ),
            (
                "echo a & b",
                foreground("echo a & b"),
                "a space after the & is not a job name",
            ),
            (
                "a &&b",
                foreground("a &&b"),
                "the operator is still an operator with no space after it",
            ),
            (
                "echo x &\"main\"",
                invalid("&: invalid job name \"main\" (\"main\" is the foreground job)"),
                "",
            ),
            (
                "echo x &\"\"",
                invalid("&: invalid job name \"\" (not empty, and no % or \")"),
                "",
            ),
        ]);
    }
}
