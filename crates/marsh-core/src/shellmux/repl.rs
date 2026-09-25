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

use crate::policy::Action;

use crate::shellmux::{MuxError, ShellId};
use crate::{Outcome, policy::Event};

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

/// The console label for an action: the capability's own name.
///
/// Never a reconstructed command line. One capability is requested by many git commands — a
/// `checkout` comes from `git switch`, `git reset --hard` and `git restore` alike — so naming a
/// command would claim the user ran something they did not.
fn action_label(action: &Action) -> String {
    match action {
        Action::Read => "read".to_string(),
        Action::Edit => "edit".to_string(),
        Action::Stage => "stage".to_string(),
        Action::Delete => "delete".to_string(),
        Action::Unstage => "unstage".to_string(),
        Action::Commit {
            message: Some(message),
        } => format!("commit {message:?}"),
        Action::Commit { message: None } => "commit".to_string(),
        Action::Checkout => "checkout".to_string(),
        Action::Stash => "stash".to_string(),
        Action::Clean => "clean".to_string(),
        Action::Diff => "diff".to_string(),
        Action::History => "history".to_string(),
    }
}

/// Renders a concluded line as the lines the console prints in gray.
///
/// The verdict is the point of the console, so every outcome renders the same three things where
/// it has them: the capabilities the line requested, what the policy did with them, and what the
/// user's next move is. A publication names the sequence number it occupies; a denial names every
/// refused capability, the precondition it failed and the fixes that would unblock it; a lost race
/// says plainly that the line must be rerun. Command output is not here: a job writes it straight
/// to the terminal as it runs.
pub fn report_lines(id: &ShellId, outcome: &Result<Outcome, MuxError>) -> Vec<String> {
    let mut lines = Vec::new();
    let job = id.reference();
    let push_events = |lines: &mut Vec<String>, events: &[Event]| {
        for event in events {
            let label = action_label(&event.action);
            let resource = event.resource.to_string();
            lines.push(format!("{job}: {label} {resource:?}"));
        }
    };

    match outcome {
        Ok(Outcome::Published {
            publication,
            granted,
        }) => {
            push_events(&mut lines, granted);
            lines.push(format!(
                "{job} committed seq={} ops={}",
                publication.seq, publication.ops
            ));
        }
        Ok(Outcome::Denied { requested, denials }) => {
            push_events(&mut lines, requested);
            lines.push(format!(
                "{job} denied {} of {}:",
                denials.len(),
                requested.len()
            ));
            for denial in denials {
                lines.push(format!(
                    "  - {} {} {}: {}",
                    denial.event.principal,
                    denial.event.action,
                    denial.event.resource,
                    denial.failed_precondition
                ));
                if !denial.allowed_fixes.is_empty() {
                    lines.push(format!("    fix: {}", denial.allowed_fixes.join("; ")));
                }
            }
        }
        Ok(Outcome::Discarded) => {
            lines.push(format!(
                "{job} stopped — the line was discarded, nothing published"
            ));
        }
        Ok(Outcome::Detached) => {
            lines.push(format!("{job} ran outside a session — nothing to publish"));
        }
        Err(error) => {
            lines.push(format!("{job} error: {error}"));
        }
    }
    lines
}

#[allow(
    clippy::panic,
    reason = "a grammar test that parsed the wrong form has nothing to assert"
)]
#[cfg(test)]
mod tests {
    use super::*;

    use crate::policy::Resource;
    use crate::{Denial, Publication};

    /// A typed directory becomes a path under the originating shell's seed, so the one thing it
    /// must never do is name something outside it.
    #[test]
    fn a_typed_directory_cannot_climb_out_of_the_seed() {
        assert_eq!(seed_relative(""), Some(String::new()));
        assert_eq!(seed_relative("."), Some(String::new()));
        assert_eq!(seed_relative("./src"), Some("src".to_string()));
        assert_eq!(seed_relative("/src/"), Some("src".to_string()));
        assert_eq!(seed_relative("deep/../src"), Some("src".to_string()));
        assert_eq!(seed_relative(".."), None);
        assert_eq!(seed_relative("src/../.."), None);
    }

    #[test]
    fn an_empty_line_asks_for_nothing() {
        assert_eq!(parse(""), Input::Empty);
        assert_eq!(parse("   \t "), Input::Empty);
    }

    #[test]
    fn console_builtins_are_recognized_before_the_shell_sees_them() {
        assert_eq!(parse("jobs"), Input::Jobs);
        assert_eq!(parse("  jobs  "), Input::Jobs);
    }

    #[test]
    fn fg_takes_one_optional_job_name() {
        assert_eq!(parse("fg"), Input::Fg(None));
        assert_eq!(parse("fg foo"), Input::Fg(Some("foo".to_string())));
        assert_eq!(
            parse("fg a b"),
            Input::Fg(Some("a b".to_string())),
            "the rest of the line is the name: fg takes one job, so nothing else could be meant"
        );
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
        let stop = |line: &str| match parse(line) {
            Input::Stop(args) => args,
            other => panic!("{line:?} parsed as {other:?}"),
        };
        assert_eq!(
            stop("stop -f a name"),
            vec!["-f".to_string(), "--".to_string(), "a name".to_string()],
            "the option comes off the front; everything after it is the job"
        );
        assert_eq!(
            stop("stop \"-f\""),
            vec!["--".to_string(), "-f".to_string()],
            "a quoted token is a name, so the option scan never sees it"
        );
        assert_eq!(
            stop("stop -f -- \"-f\""),
            vec!["-f".to_string(), "--".to_string(), "-f".to_string()],
            "an explicit -- ends the option scan and is not passed on twice"
        );
        assert_eq!(
            stop("stop %\"a name\""),
            vec!["--".to_string(), "a name".to_string()],
            "a row copied out of the job table pastes back"
        );
        assert_eq!(
            stop("stop build"),
            vec!["--".to_string(), "build".to_string()]
        );
        assert_eq!(
            stop("stop -f"),
            vec!["-f".to_string()],
            "an operand-less stop is the builtin's usage error to report, not the parser's"
        );
        assert_eq!(stop("stop"), Vec::<String>::new());
        assert_eq!(
            parse("close build"),
            Input::Foreground("close build".to_string()),
            "close is gone, so the word is an ordinary command line"
        );
        assert_eq!(
            parse("kill -9 1234"),
            Input::Kill(vec!["-9".to_string(), "1234".to_string()]),
            "kill keeps its verbatim tokens, and jobs are stop's now"
        );
    }

    /// `kill` is the one console builtin with a real argument grammar, so the grammar stays in the
    /// builtin: the parser only has to keep the tokens — signal flag included — intact and ordered.
    #[test]
    fn kill_passes_its_arguments_through_verbatim() {
        assert_eq!(parse("kill 1234"), Input::Kill(vec!["1234".to_string()]));
        assert_eq!(
            parse("kill -9 1234 5678"),
            Input::Kill(vec![
                "-9".to_string(),
                "1234".to_string(),
                "5678".to_string()
            ])
        );
        assert_eq!(
            parse("kill"),
            Input::Kill(Vec::new()),
            "an argument-less kill is the builtin's usage error to report, not the parser's"
        );
    }

    /// A job name becomes a principal, so the grammar has to refuse the ones that would collide
    /// with the foreground principal or survive a round trip through `%name` badly.
    #[test]
    fn sd_names_a_sandbox_and_bg_numbers_it() {
        assert_eq!(
            parse("sd api ./foo1"),
            Input::SpawnDir {
                name: Some("api".to_string()),
                dir: "./foo1".to_string(),
            }
        );
        assert_eq!(
            parse("bg ./foo1"),
            Input::SpawnDir {
                name: None,
                dir: "./foo1".to_string(),
            }
        );
        for wrong in ["sd", "sd api", "sd api dir extra"] {
            assert_eq!(
                parse(wrong),
                Input::Invalid("sd: usage: sd NAME DIR".to_string()),
                "{wrong}"
            );
        }
        for wrong in ["bg", "bg a b"] {
            assert_eq!(
                parse(wrong),
                Input::Invalid("bg: usage: bg DIR".to_string()),
                "{wrong}"
            );
        }
        assert_eq!(
            parse("sd main ."),
            Input::Invalid(
                "sd: invalid name \"main\" (use letters, digits, _ or -; not \"main\")".to_string()
            ),
            "the foreground principal is reserved"
        );
        assert_eq!(
            parse("sd a/b ."),
            Input::Invalid(
                "sd: invalid name \"a/b\" (use letters, digits, _ or -; not \"main\")".to_string()
            )
        );
    }

    /// The directory typed at `sd` is a path in the job it was typed in. Reading it from the seed
    /// root made `sd api docs` unusable in any session started below the seed root, which is every
    /// session started anywhere but the top of a subvolume.
    #[test]
    fn a_job_directory_hangs_below_the_current_job() {
        assert_eq!(job_dir("marsh", "docs"), "marsh/docs");
        assert_eq!(job_dir("marsh", "docs/how-to"), "marsh/docs/how-to");
        assert_eq!(
            job_dir("marsh", ".."),
            "marsh/..",
            "the mux normalizes; `..` from a job one level down is the seed root"
        );
        assert_eq!(
            job_dir("marsh", "/other"),
            "/other",
            "a leading slash names the seed root, not the current job"
        );
        assert_eq!(
            job_dir("", "docs"),
            "docs",
            "a job at the seed root joins nothing"
        );
    }

    #[test]
    fn a_trailing_ampersand_is_a_job_but_a_double_one_is_an_operator() {
        assert_eq!(
            parse("sleep 5 &"),
            Input::Background {
                cmd: "sleep 5".to_string(),
                name: None,
            }
        );
        assert_eq!(
            parse("sleep 5&"),
            Input::Background {
                cmd: "sleep 5".to_string(),
                name: None,
            }
        );
        assert_eq!(parse("a && b"), Input::Foreground("a && b".to_string()));
        assert_eq!(parse("a &&"), Input::Foreground("a &&".to_string()));
        assert_eq!(parse("echo hi"), Input::Foreground("echo hi".to_string()));
        assert_eq!(
            parse("&"),
            Input::Foreground("&".to_string()),
            "an empty command is left for the shell's parser to diagnose"
        );
    }

    #[test]
    fn an_ampersand_can_name_the_job_it_opens() {
        assert_eq!(
            parse("echo foo &api"),
            Input::Background {
                cmd: "echo foo".to_string(),
                name: Some("api".to_string()),
            }
        );
        assert_eq!(
            parse("echo foo &\"a long name\""),
            Input::Background {
                cmd: "echo foo".to_string(),
                name: Some("a long name".to_string()),
            },
            "quoting is the only way to name a job with spaces in it"
        );
        assert_eq!(
            parse("echo \"x\" &\"parse\""),
            Input::Background {
                cmd: "echo \"x\"".to_string(),
                name: Some("parse".to_string()),
            },
            "the last `&\"` wins, so a command holding quotes of its own survives"
        );
        assert_eq!(
            parse("echo \"a\" &"),
            Input::Background {
                cmd: "echo \"a\"".to_string(),
                name: None,
            },
            "the quoted form is only looked for when the line ends with a quote"
        );
        assert_eq!(
            parse("grep -e \"&\" -f \"x\""),
            Input::Foreground("grep -e \"&\" -f \"x\"".to_string()),
            "a candidate name holding a quote of its own is no name, and the line stays a command"
        );
        assert_eq!(
            parse("echo a & b"),
            Input::Foreground("echo a & b".to_string()),
            "a space after the & is not a job name"
        );
        assert_eq!(
            parse("a &&b"),
            Input::Foreground("a &&b".to_string()),
            "the operator is still an operator with no space after it"
        );
        assert_eq!(
            parse("echo x &\"main\""),
            Input::Invalid(
                "&: invalid job name \"main\" (\"main\" is the foreground job)".to_string()
            )
        );
        assert_eq!(
            parse("echo x &\"\""),
            Input::Invalid("&: invalid job name \"\" (not empty, and no % or \")".to_string())
        );
    }

    /// A publication names the sequence number it occupies and every capability it earned.
    #[test]
    fn a_publication_renders_its_granted_capabilities() {
        let outcome = Ok(Outcome::Published {
            publication: Publication { seq: 7, ops: 1 },
            granted: vec![Event::new(
                "main",
                Action::Edit,
                Resource::from(vec!["foo.txt"]),
            )],
        });

        assert_eq!(
            report_lines(&ShellId::from("main"), &outcome),
            vec![
                "%main: edit \"foo.txt\"".to_string(),
                "%main committed seq=7 ops=1".to_string(),
            ]
        );
    }

    /// A denial is only actionable if it names the precondition and the way out.
    #[test]
    fn a_denial_renders_its_precondition_and_fixes() {
        let event = Event::new("foo", Action::Stage, Resource::from(vec!["a.txt"]));
        let outcome = Ok(Outcome::Denied {
            requested: vec![
                Event::new("foo", Action::Edit, Resource::from(vec!["a.txt"])),
                event.clone(),
            ],
            denials: vec![Denial {
                event,
                failed_precondition: "no edit precedes the stage".to_string(),
                allowed_fixes: vec!["edit a.txt".to_string(), "git rm a.txt".to_string()],
            }],
        });

        assert_eq!(
            report_lines(&ShellId::from("foo"), &outcome),
            vec![
                "%foo: edit \"a.txt\"".to_string(),
                "%foo: stage \"a.txt\"".to_string(),
                "%foo denied 1 of 2:".to_string(),
                "  - foo stage a.txt: no edit precedes the stage".to_string(),
                "    fix: edit a.txt; git rm a.txt".to_string(),
            ]
        );
    }

    /// The three endings that are neither a publication nor a denial: a shell with no seed behind
    /// it, a forced stop that threw the line away, and the mux itself breaking.
    #[test]
    fn other_outcomes_render_distinctly() {
        assert_eq!(
            report_lines(&ShellId::from("main"), &Ok(Outcome::Detached)),
            vec!["%main ran outside a session — nothing to publish".to_string()]
        );

        assert_eq!(
            report_lines(&ShellId::from("main"), &Ok(Outcome::Discarded)),
            vec!["%main stopped — the line was discarded, nothing published".to_string()]
        );

        let error: Result<Outcome, MuxError> = Err(MuxError::JobBusy(ShellId::from("foo")));
        assert_eq!(
            report_lines(&ShellId::from("foo"), &error),
            vec!["%foo error: %foo is already running a command".to_string()]
        );
    }
}
