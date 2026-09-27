//! The git command grammar: what a command line asks git to do to a resource.
//!
//! The `git` builtin forwards every command line to the system git unchanged, so this grammar
//! decides nothing about *how* a command runs. It classifies one: [`parse`] names the operation a
//! command line requests as a [`GitAction`], and the builtin maps that onto the policy vocabulary
//! to decide how closely the run has to be observed. What the run actually did to each path is
//! observed afterwards and recorded in the same vocabulary; the classification never becomes a
//! request by itself.
//!
//! The grammar is git's own where it matters: global options before the subcommand, option
//! operands, clustered short options, unique-prefix abbreviations of long options, `--no-`
//! negations, nested subcommands (`stash`, `bisect`, `history`) and the `--` boundary. A form it
//! cannot decide is classified as [`GitAction::Edit`] — never as something harmless — and git
//! itself reports whatever is wrong with the line.

use std::path::{Component, Path, PathBuf};

/// A git operation, named by what it does to a resource.
///
/// Execution-local by design: the grammar reports what a command asked git to do, and a caller
/// maps that onto whatever authorization vocabulary it uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitAction {
    /// `git add`/`git stage`: worktree state into the index.
    Stage,
    /// `git rm`: the path leaves the index, and the worktree unless `--cached`.
    Delete,
    /// `git commit`: staged state becomes a commit.
    Commit {
        /// Commit message. `None` is distinct from an empty message.
        message: Option<String>,
    },
    /// `git restore --staged`, `git reset`: `HEAD` state back into the index.
    Unstage,
    /// `git checkout`, `git switch`, `git reset --hard`: repository state into the worktree.
    Checkout,
    /// `git stash push`: worktree and index state moves to `refs/stash`.
    Stash,
    /// `git clean`: untracked content is deleted.
    Clean,
    /// `git diff`, `git status`: differences between HEAD, index and worktree.
    Diff,
    /// `git log`, `git show`: committed history.
    History,
    /// `git grep`: resource contents.
    Read,
    /// A resource's contents change without being settled: a conflict, a moved path's new name,
    /// a popped stash — or a command line whose effect this grammar cannot decide.
    Edit,
}

/// A classified git command line, borrowing every word from the vector it was parsed from.
#[derive(Debug)]
pub struct GitInvocation<'a, S: AsRef<str>> {
    /// The operation the command line requests, or `None` for one that names no file resource:
    /// repository and ref metadata, object transfer, or git's own help and version output.
    pub action: Option<GitAction>,
    /// The subcommand word, or the informational option (`--version`, `--exec-path`) that took
    /// its place; `None` for a bare `git` and for a global option this grammar does not know.
    pub subcommand: Option<&'a str>,
    /// The options between `git` and the subcommand.
    pub global_args: &'a [S],
    /// Everything after the subcommand.
    pub command_args: &'a [S],
}

/// Resolves `path` against `base` and normalizes it lexically.
///
/// No symlink resolution: a pathspec is resolved the way git resolves it (textually, against the
/// caller's working directory), and syscall paths arrive from the trace already kernel-resolved
/// wherever it matters.
pub fn resolve(base: &Path, path: &str) -> PathBuf {
    let joined = if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// How one global option before the subcommand is spelled.
enum Global {
    /// A switch that is the whole word.
    Flag,
    /// An option whose value is the next word.
    Value,
    /// An option that makes git print something and exit instead of running a subcommand.
    Informational,
    /// Not an option git knows, which git refuses.
    Unknown,
}

/// Git's global options, as `git.c`'s `handle_options` reads them.
///
/// `-C` and `-c` take the next word and never an attached value; the long options that take one
/// accept it either attached with `=` or as the next word, except `--shallow-file`, which only
/// takes the next word. `--exec-path` alone prints the exec path; only `--exec-path=<path>` sets
/// it. `--help`, `-h`, `--version` and `-v` end option handling and run as the `help` and
/// `version` commands.
fn global_option(arg: &str) -> Global {
    const ATTACHED: [&str; 6] = [
        "--git-dir=",
        "--work-tree=",
        "--namespace=",
        "--config-env=",
        "--attr-source=",
        "--exec-path=",
    ];
    match arg {
        "-h" | "--help" | "-v" | "--version" | "--exec-path" | "--html-path" | "--man-path"
        | "--info-path" => Global::Informational,
        "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env"
        | "--attr-source" | "--shallow-file" => Global::Value,
        "-p"
        | "--paginate"
        | "-P"
        | "--no-pager"
        | "--bare"
        | "--no-replace-objects"
        | "--literal-pathspecs"
        | "--glob-pathspecs"
        | "--noglob-pathspecs"
        | "--icase-pathspecs"
        | "--no-optional-locks"
        | "--no-lazy-fetch"
        | "--no-advice" => Global::Flag,
        _ if arg.starts_with("--list-cmds=") => Global::Informational,
        _ if ATTACHED.iter().any(|prefix| arg.starts_with(prefix)) => Global::Flag,
        _ => Global::Unknown,
    }
}

/// Each global option in `globals`, with the next word when that is the value it takes.
pub(crate) fn global_options<S: AsRef<str>>(
    globals: &[S],
) -> impl Iterator<Item = (&str, Option<&str>)> {
    let mut words = globals.iter().map(AsRef::as_ref);
    std::iter::from_fn(move || {
        let word = words.next()?;
        Some((
            word,
            if matches!(global_option(word), Global::Value) {
                words.next()
            } else {
                None
            },
        ))
    })
}

/// Classifies a git command line, `argv[0]` included.
///
/// Never fails: a line git would refuse is still a line git will run, and what it would do is the
/// question. An unknown global option or a value option missing its value is classified as
/// [`GitAction::Edit`]; git reports the real error when it runs.
///
/// The argument vector may be borrowed or owned — `&[String]` from a builtin's own argv, `&[&str]`
/// from a recorded line — because nothing about the grammar needs to own its input.
pub fn parse<S: AsRef<str>>(argv: &[S]) -> GitInvocation<'_, S> {
    let args = argv.get(1..).unwrap_or(&[]);
    let mut index = 0;
    // The requested action, the word that names it, and where the words after that word begin.
    let (action, subcommand, rest) = loop {
        // A bare `git` prints its help.
        let Some(arg) = args.get(index).map(AsRef::as_ref) else {
            break (None, None, index);
        };
        if !arg.starts_with('-') {
            break (classify(arg, &args[index + 1..]), Some(arg), index + 1);
        }
        match global_option(arg) {
            Global::Flag => index += 1,
            Global::Value if index + 1 < args.len() => index += 2,
            Global::Informational => break (None, Some(arg), index + 1),
            Global::Value | Global::Unknown => break (Some(GitAction::Edit), None, index),
        }
    };
    GitInvocation {
        action,
        subcommand,
        global_args: &args[..index],
        command_args: &args[rest..],
    }
}

/// The primary action of `subcommand` run with `args`.
fn classify<S: AsRef<str>>(subcommand: &str, args: &[S]) -> Option<GitAction> {
    match subcommand {
        "add" | "stage" => Some(GitAction::Stage),
        "diff" | "status" => Some(GitAction::Diff),
        "grep" => Some(GitAction::Read),
        "log" | "show" => Some(GitAction::History),
        "switch" | "checkout" | "pull" => Some(GitAction::Checkout),
        "init" | "branch" | "tag" | "fetch" | "push" | "backfill" | "help" | "version" => None,
        "clone" => {
            let scan = scan(args, &CLONE);
            let no_checkout = scan.has(Some('n'), "no-checkout") && !scan.has(None, "checkout");
            if scan.has(None, "bare") || scan.has(None, "mirror") || no_checkout {
                None
            } else {
                Some(GitAction::Checkout)
            }
        }
        "mv" => Some(unless_dry_run(args, &MV, GitAction::Delete)),
        "rm" => Some(unless_dry_run(args, &RM, GitAction::Delete)),
        "clean" => Some(unless_dry_run(args, &CLEAN, GitAction::Clean)),
        "restore" => {
            let scan = scan(args, &RESTORE);
            let (staged, worktree) = (
                scan.has(Some('S'), "staged"),
                scan.has(Some('W'), "worktree"),
            );
            Some(if staged && !worktree {
                GitAction::Unstage
            } else {
                GitAction::Checkout
            })
        }
        "reset" => reset_mode(&scan(args, &RESET)),
        "commit" => Some(GitAction::Commit {
            message: commit_message(&scan(args, &COMMIT)),
        }),
        "merge" => ends_without_checkout(&scan(args, &MERGE), &["quit"]),
        "rebase" => ends_without_checkout(&scan(args, &REBASE), &["quit", "edit-todo"]),
        "stash" => match args.first().map(AsRef::as_ref) {
            None | Some("push" | "save") => Some(GitAction::Stash),
            Some(option) if option.starts_with('-') => Some(GitAction::Stash),
            Some("apply" | "pop") => Some(GitAction::Edit),
            Some("branch") => Some(GitAction::Checkout),
            Some("show") => Some(GitAction::Diff),
            Some("list") => Some(GitAction::History),
            Some("create" | "drop" | "clear" | "store") => None,
            Some(_) => Some(GitAction::Edit),
        },
        "bisect" => match args.first().map(AsRef::as_ref) {
            None | Some("help") => None,
            Some("log" | "terms" | "visualize" | "view") => Some(GitAction::History),
            Some("start") if scan(&args[1..], &BISECT_START).has(None, "no-checkout") => None,
            // Every other word — the built-in terms, custom ones, `run`, `replay`, `reset` —
            // can move the worktree, and a mode persisted by an earlier `start` is not visible
            // on this line.
            Some(_) => Some(GitAction::Checkout),
        },
        "history" => match args.first().map(AsRef::as_ref) {
            None | Some("reword" | "split") => None,
            Some("fixup") => Some(GitAction::Commit { message: None }),
            Some(_) => Some(GitAction::Edit),
        },
        _ => Some(GitAction::Edit),
    }
}

/// `git reset`'s mode: the last mode option wins, as git's own option table has it.
fn reset_mode(scan: &Scan<'_>) -> Option<GitAction> {
    let mut mode = Some(GitAction::Unstage);
    for option in &scan.options {
        match option {
            Opt::Short('p', _)
            | Opt::Long {
                name: "mixed" | "patch",
                negated: false,
                ..
            } => mode = Some(GitAction::Unstage),
            Opt::Long {
                name: "soft",
                negated: false,
                ..
            } => mode = None,
            Opt::Long {
                name: "hard" | "merge" | "keep",
                negated: false,
                ..
            } => mode = Some(GitAction::Checkout),
            _ => {}
        }
    }
    mode
}

/// `action`, unless the line is a dry run (`-n`/`--dry-run`), which only reports what it would do.
fn unless_dry_run<S: AsRef<str>>(args: &[S], grammar: &Grammar, action: GitAction) -> GitAction {
    if scan(args, grammar).has(Some('n'), "dry-run") {
        GitAction::Diff
    } else {
        action
    }
}

/// `Checkout`, unless one of `quits` — an ending that leaves the worktree alone — was given.
fn ends_without_checkout(scan: &Scan<'_>, quits: &[&str]) -> Option<GitAction> {
    if quits.iter().any(|quit| scan.has(None, quit)) {
        None
    } else {
        Some(GitAction::Checkout)
    }
}

/// The message `git commit` was given on its command line: `-m`/`--message` values joined by a
/// blank line, exactly as git composes multiple `-m` paragraphs.
///
/// `None` when the message comes from anywhere else — a file, a reused commit, an editor — or an
/// `-m` has no operand. Nothing is read and no editor is started to find out: the recorded
/// commit's own message is what a request carries.
fn commit_message(scan: &Scan<'_>) -> Option<String> {
    let mut messages: Vec<&str> = Vec::new();
    for option in &scan.options {
        match option {
            Opt::Short('m', value)
            | Opt::Long {
                name: "message",
                value,
                ..
            } => messages.push((*value)?),
            Opt::Short('F' | 'C' | 'c', _)
            | Opt::Long {
                name: "file" | "reuse-message" | "reedit-message",
                ..
            } => return None,
            _ => {}
        }
    }
    (!messages.is_empty()).then(|| messages.join("\n\n"))
}

/// How a subcommand's options take their operands, as far as classifying it needs to know.
pub(crate) struct Grammar {
    /// Short options that take a value, attached (`-bmain`) or as the next word.
    short_values: &'static str,
    /// Long options that take a value, attached (`--branch=main`) or as the next word.
    long_values: &'static [&'static str],
    /// Long options that take none — or only an attached one — listed so that a unique prefix
    /// of one is read as the option it abbreviates.
    long_flags: &'static [&'static str],
}

/// `git clone`, as `builtin/clone.c` declares its options.
pub(crate) const CLONE: Grammar = Grammar {
    short_values: "jobuc",
    long_values: &[
        "jobs",
        "template",
        "reference",
        "reference-if-able",
        "origin",
        "branch",
        "revision",
        "upload-pack",
        "depth",
        "shallow-since",
        "shallow-exclude",
        "separate-git-dir",
        "ref-format",
        "config",
        "server-option",
        "filter",
        "bundle-uri",
    ],
    long_flags: &[
        "verbose",
        "quiet",
        "progress",
        "reject-shallow",
        "no-checkout",
        "checkout",
        "bare",
        "mirror",
        "local",
        "no-hardlinks",
        "hardlinks",
        "shared",
        "recurse-submodules",
        "recursive",
        "dissociate",
        "single-branch",
        "tags",
        "shallow-submodules",
        "ipv4",
        "ipv6",
        "also-filter-submodules",
        "remote-submodules",
        "sparse",
    ],
};

/// `git init`, as `builtin/init-db.c` declares its options.
pub(crate) const INIT: Grammar = Grammar {
    short_values: "b",
    long_values: &[
        "template",
        "separate-git-dir",
        "initial-branch",
        "object-format",
        "ref-format",
    ],
    long_flags: &["bare", "shared", "quiet"],
};

/// `git mv`.
const MV: Grammar = Grammar {
    short_values: "",
    long_values: &[],
    long_flags: &["dry-run", "force", "verbose", "sparse"],
};

/// `git rm`.
const RM: Grammar = Grammar {
    short_values: "",
    long_values: &["pathspec-from-file"],
    long_flags: &[
        "dry-run",
        "quiet",
        "cached",
        "force",
        "ignore-unmatch",
        "sparse",
        "pathspec-file-nul",
    ],
};

/// `git clean`.
const CLEAN: Grammar = Grammar {
    short_values: "e",
    long_values: &["exclude"],
    long_flags: &["dry-run", "quiet", "force", "interactive"],
};

/// `git restore`.
const RESTORE: Grammar = Grammar {
    short_values: "s",
    long_values: &["source", "pathspec-from-file", "conflict"],
    long_flags: &[
        "staged",
        "worktree",
        "patch",
        "ours",
        "theirs",
        "merge",
        "overlay",
        "ignore-unmerged",
        "ignore-skip-worktree-bits",
        "recurse-submodules",
        "progress",
        "quiet",
        "pathspec-file-nul",
    ],
};

/// `git reset`.
const RESET: Grammar = Grammar {
    short_values: "",
    long_values: &["pathspec-from-file"],
    long_flags: &[
        "soft",
        "mixed",
        "hard",
        "merge",
        "keep",
        "patch",
        "quiet",
        "intent-to-add",
        "recurse-submodules",
        "refresh",
        "pathspec-file-nul",
    ],
};

/// `git commit`.
const COMMIT: Grammar = Grammar {
    short_values: "mFCct",
    long_values: &[
        "message",
        "file",
        "reuse-message",
        "reedit-message",
        "fixup",
        "squash",
        "author",
        "date",
        "template",
        "cleanup",
        "pathspec-from-file",
        "trailer",
    ],
    long_flags: &[
        "all",
        "patch",
        "amend",
        "allow-empty",
        "allow-empty-message",
        "no-verify",
        "verify",
        "dry-run",
        "include",
        "only",
        "signoff",
        "edit",
        "quiet",
        "verbose",
        "short",
        "porcelain",
        "long",
        "status",
        "null",
        "reset-author",
        "no-post-rewrite",
    ],
};

/// `git merge`.
const MERGE: Grammar = Grammar {
    short_values: "mFsX",
    long_values: &[
        "message",
        "file",
        "strategy",
        "strategy-option",
        "into-name",
        "cleanup",
    ],
    long_flags: &[
        "quit",
        "abort",
        "continue",
        "commit",
        "edit",
        "ff",
        "ff-only",
        "squash",
        "stat",
        "summary",
        "log",
        "signoff",
        "verify",
        "verbose",
        "quiet",
        "progress",
        "autostash",
        "allow-unrelated-histories",
        "rerere-autoupdate",
        "overwrite-ignore",
    ],
};

/// `git rebase`.
const REBASE: Grammar = Grammar {
    short_values: "sXx",
    long_values: &["onto", "strategy", "strategy-option", "exec", "whitespace"],
    long_flags: &[
        "quit",
        "edit-todo",
        "abort",
        "continue",
        "skip",
        "show-current-patch",
        "interactive",
        "merge",
        "apply",
        "root",
        "autosquash",
        "autostash",
        "update-refs",
        "keep-base",
        "fork-point",
        "quiet",
        "verbose",
        "stat",
        "verify",
        "signoff",
        "reapply-cherry-picks",
        "rebase-merges",
        "empty",
        "committer-date-is-author-date",
        "ignore-date",
        "reschedule-failed-exec",
    ],
};

/// `git bisect start`.
const BISECT_START: Grammar = Grammar {
    short_values: "",
    long_values: &["term-new", "term-bad", "term-old", "term-good"],
    long_flags: &["no-checkout", "first-parent"],
};

/// One option word, or one option of a clustered short word.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Opt<'a> {
    /// `-x`, with the value it took when it takes one.
    Short(char, Option<&'a str>),
    /// `--name`, spelled out or abbreviated, with `negated` for its `--no-` form.
    Long {
        /// The option's full name as its grammar declares it, or the word itself when it is not
        /// one the grammar knows.
        name: &'a str,
        /// Whether it was given as `--no-<name>`.
        negated: bool,
        /// The value it took when it takes one; `None` also for a value option that ended the
        /// line without its value.
        value: Option<&'a str>,
    },
}

/// A subcommand's arguments, split into options and positional words.
pub(crate) struct Scan<'a> {
    /// Every option before `--`, in command-line order.
    pub(crate) options: Vec<Opt<'a>>,
    /// Every positional word, before and after `--`, in command-line order.
    pub(crate) positionals: Vec<&'a str>,
}

impl Scan<'_> {
    /// Whether the option was given, in either spelling, and not negated by a later `--no-` form.
    pub(crate) fn has(&self, short: Option<char>, long: &str) -> bool {
        self.options.iter().rev().find_map(|option| match option {
            Opt::Short(letter, _) if Some(*letter) == short => Some(true),
            Opt::Long { name, negated, .. } if *name == long => Some(!negated),
            _ => None,
        }) == Some(true)
    }

    /// The value of the last occurrence of a value option, in either spelling.
    pub(crate) fn value(&self, short: Option<char>, long: &str) -> Option<&str> {
        self.options.iter().rev().find_map(|option| match option {
            Opt::Short(letter, value) if Some(*letter) == short => *value,
            Opt::Long {
                name,
                negated: false,
                value,
            } if *name == long => *value,
            _ => None,
        })
    }
}

/// Splits `args` into options and positionals the way git's `parse-options` reads them.
///
/// A long option is matched exactly, then as a unique prefix of one the grammar declares, then
/// through its `--no-` form. A value option takes its operand attached or as the next word, so an
/// operand that looks like an option or like `--` is still that option's value. A single `-` is
/// a positional, and every word after the first `--` is one.
pub(crate) fn scan<'a, S: AsRef<str>>(args: &'a [S], grammar: &Grammar) -> Scan<'a> {
    let mut options = Vec::new();
    let mut positionals = Vec::new();
    let mut words = args.iter().map(AsRef::as_ref);
    while let Some(word) = words.next() {
        if word == "--" {
            positionals.extend(words.by_ref());
            break;
        }
        if let Some(long) = word.strip_prefix("--") {
            let (spelled, attached) = match long.split_once('=') {
                Some((spelled, value)) => (spelled, Some(value)),
                None => (long, None),
            };
            let (name, negated) = long_name(spelled, grammar);
            let value = if !negated && grammar.long_values.contains(&name) {
                attached.or_else(|| words.next())
            } else {
                attached
            };
            options.push(Opt::Long {
                name,
                negated,
                value,
            });
        } else if let Some(cluster) = word.strip_prefix('-').filter(|cluster| !cluster.is_empty()) {
            for (at, letter) in cluster.char_indices() {
                if grammar.short_values.contains(letter) {
                    let rest = cluster.get(at + letter.len_utf8()..).unwrap_or_default();
                    let value = if rest.is_empty() {
                        words.next()
                    } else {
                        Some(rest)
                    };
                    options.push(Opt::Short(letter, value));
                    break;
                }
                options.push(Opt::Short(letter, None));
            }
        } else {
            positionals.push(word);
        }
    }
    Scan {
        options,
        positionals,
    }
}

/// The declared name `spelled` means, and whether it was the `--no-` form of it.
fn long_name<'a>(spelled: &'a str, grammar: &Grammar) -> (&'a str, bool) {
    let declared = grammar.long_values.iter().chain(grammar.long_flags);
    if let Some(exact) = declared.clone().find(|name| **name == spelled) {
        return (*exact, false);
    }
    let mut prefixed = declared.filter(|name| name.starts_with(spelled));
    if let (Some(only), None) = (prefixed.next(), prefixed.next()) {
        return (*only, false);
    }
    if let Some(positive) = spelled.strip_prefix("no-") {
        let (name, _) = long_name(positive, grammar);
        return (name, true);
    }
    (spelled, false)
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    /// A commit action with the message the command line carried.
    fn commit(message: Option<&str>) -> GitAction {
        GitAction::Commit {
            message: message.map(str::to_string),
        }
    }

    /// Every subcommand git 2.55 advertises, and the four other names the builtin has always
    /// answered to, classify by their mode — not by their name alone.
    #[allow(
        clippy::too_many_lines,
        reason = "one flat table over every advertised subcommand and its modes"
    )]
    #[test]
    fn subcommands_map_to_their_capabilities() {
        use GitAction::{
            Checkout, Clean, Delete, Diff, Edit, History, Read, Stage, Stash, Unstage,
        };
        let cases: &[(&[&str], Option<GitAction>)] = &[
            // Start a working area.
            (&["git", "clone", "origin", "dest"], Some(Checkout)),
            (&["git", "clone", "--bare", "origin"], None),
            (&["git", "clone", "--mirror", "origin"], None),
            (&["git", "clone", "-n", "origin"], None),
            (&["git", "clone", "--no-checkout", "origin"], None),
            (&["git", "clone", "--no-check", "origin"], None),
            (&["git", "clone", "-b", "--bare", "origin"], Some(Checkout)),
            (&["git", "init", "-b", "main"], None),
            // Work on the current change.
            (&["git", "add", "--", "p"], Some(Stage)),
            (&["git", "add", "-A"], Some(Stage)),
            (&["git", "add", "*.txt"], Some(Stage)),
            (&["git", "stage", "p"], Some(Stage)),
            (&["git", "mv", "--", "p", "q"], Some(Delete)),
            (&["git", "mv", "-n", "p", "q"], Some(Diff)),
            (&["git", "mv", "--", "-n", "q"], Some(Delete)),
            (&["git", "restore", "--staged", "--", "p"], Some(Unstage)),
            (&["git", "restore", "-S", "p"], Some(Unstage)),
            (&["git", "restore", "--", "p"], Some(Checkout)),
            (&["git", "restore", "-SW", "p"], Some(Checkout)),
            (
                &["git", "restore", "--staged", "--worktree", "p"],
                Some(Checkout),
            ),
            (
                &["git", "restore", "--staged", "--no-staged", "p"],
                Some(Checkout),
            ),
            (&["git", "restore", "-s", "--staged", "p"], Some(Checkout)),
            (&["git", "rm", "--", "p"], Some(Delete)),
            (&["git", "rm", "--cached", "p"], Some(Delete)),
            (&["git", "rm", "-rn", "p"], Some(Diff)),
            (&["git", "rm", "--dry-run", "p"], Some(Diff)),
            // Examine history and state.
            (&["git", "bisect", "start"], Some(Checkout)),
            (&["git", "bisect", "start", "--no-checkout"], None),
            (&["git", "bisect", "bad", "HEAD"], Some(Checkout)),
            (&["git", "bisect", "run", "true"], Some(Checkout)),
            (&["git", "bisect", "log"], Some(History)),
            (&["git", "bisect", "visualize"], Some(History)),
            (&["git", "bisect"], None),
            (&["git", "diff", "--", "p"], Some(Diff)),
            (&["git", "grep", "-n", "beta"], Some(Read)),
            (&["git", "grep", "--cached", "beta"], Some(Read)),
            (&["git", "log", "--oneline", "--", "p"], Some(History)),
            (&["git", "show", "HEAD:p"], Some(History)),
            (&["git", "status"], Some(Diff)),
            (&["git", "status", "--porcelain=v1"], Some(Diff)),
            // Grow, mark, and tweak history.
            (&["git", "backfill", "--min-batch-size=1"], None),
            (&["git", "branch", "feature"], None),
            (
                &["git", "commit", "-m", "saved"],
                Some(commit(Some("saved"))),
            ),
            (&["git", "history", "fixup", "HEAD~1"], Some(commit(None))),
            (&["git", "history", "reword", "HEAD"], None),
            (&["git", "history", "split", "HEAD"], None),
            (&["git", "merge", "--ff-only", "feature"], Some(Checkout)),
            (&["git", "merge", "--quit"], None),
            (&["git", "merge", "-m", "--quit", "feature"], Some(Checkout)),
            (&["git", "rebase", "main"], Some(Checkout)),
            (&["git", "rebase", "--quit"], None),
            (&["git", "rebase", "--edit-todo"], None),
            (&["git", "reset", "--hard", "HEAD~1"], Some(Checkout)),
            (&["git", "reset", "--merge"], Some(Checkout)),
            (&["git", "reset", "--keep", "HEAD"], Some(Checkout)),
            (&["git", "reset", "--soft", "HEAD~1"], None),
            (&["git", "reset", "--mixed", "HEAD~1"], Some(Unstage)),
            (&["git", "reset", "HEAD~1"], Some(Unstage)),
            (&["git", "reset", "--", "p"], Some(Unstage)),
            (&["git", "reset", "-p"], Some(Unstage)),
            (&["git", "reset", "--hard", "--soft"], None),
            (&["git", "reset", "--ha"], Some(Checkout)),
            (&["git", "reset", "--", "--hard"], Some(Unstage)),
            (&["git", "switch", "feature"], Some(Checkout)),
            (&["git", "tag", "v1"], None),
            // Collaborate.
            (&["git", "fetch", "origin"], None),
            (
                &["git", "pull", "--ff-only", "origin", "main"],
                Some(Checkout),
            ),
            (&["git", "push", "origin", "main"], None),
            // The builtin's other names.
            (&["git", "checkout", "HEAD", "--", "p"], Some(Checkout)),
            (&["git", "checkout", "feature"], Some(Checkout)),
            (&["git", "stash"], Some(Stash)),
            (&["git", "stash", "push", "--", "p"], Some(Stash)),
            (&["git", "stash", "-u"], Some(Stash)),
            (&["git", "stash", "apply"], Some(Edit)),
            (&["git", "stash", "pop"], Some(Edit)),
            (&["git", "stash", "branch", "b"], Some(Checkout)),
            (&["git", "stash", "show", "-p"], Some(Diff)),
            (&["git", "stash", "list"], Some(History)),
            (&["git", "stash", "drop"], None),
            (&["git", "stash", "store", "x"], None),
            (&["git", "clean", "-f", "--", "p"], Some(Clean)),
            (&["git", "clean", "-fn"], Some(Diff)),
            (&["git", "clean", "--dry-run"], Some(Diff)),
            (&["git", "clean", "-e", "-n", "-f"], Some(Clean)),
            // What the grammar cannot decide is never harmless.
            (&["git", "cherry-pick", "HEAD"], Some(Edit)),
            (&["git", "history", "rewrite"], Some(Edit)),
            (&["git", "stash", "unknown"], Some(Edit)),
        ];
        for (argv, action) in cases {
            let invocation = parse(argv);
            assert_eq!(&invocation.action, action, "{argv:?}");
            assert_eq!(invocation.subcommand, Some(argv[1]), "{argv:?}");
            assert_eq!(
                invocation.command_args,
                &argv[2..],
                "the raw arguments: {argv:?}"
            );
        }
    }

    /// Global options are consumed before the subcommand is found, with their operands; what
    /// makes git print and exit, or git refuses, never reaches a subcommand.
    #[test]
    fn global_options_precede_the_subcommand() {
        let invocation = parse(&[
            "git",
            "-C",
            "sub",
            "-C",
            "",
            "-c",
            "core.abbrev=12",
            "--no-pager",
            "--git-dir=.git",
            "--work-tree",
            ".",
            "--config-env",
            "a.b=ENV",
            "--shallow-file",
            "s",
            "--no-optional-locks",
            "--no-lazy-fetch",
            "--no-advice",
            "--exec-path=/x",
            "status",
            "-s",
        ]);
        assert_eq!(invocation.action, Some(GitAction::Diff));
        assert_eq!(invocation.subcommand, Some("status"));
        assert_eq!(invocation.global_args.len(), 18);
        assert_eq!(invocation.command_args, ["-s"]);

        for informational in [
            "--version",
            "-v",
            "--help",
            "-h",
            "--exec-path",
            "--html-path",
        ] {
            let argv = ["git", "-c", "a.b=c", informational, "status"];
            let invocation = parse(&argv);
            assert_eq!(invocation.action, None, "{informational}");
            assert_eq!(invocation.subcommand, Some(informational));
            assert_eq!(invocation.command_args, ["status"]);
        }
        let bare = parse(&["git", "--no-pager"]);
        assert_eq!((bare.action, bare.subcommand), (None, None));
        assert!(parse::<&str>(&[]).action.is_none());

        for undecidable in [
            &["git", "--frobnicate", "status"][..],
            &["git", "-C"][..],
            &["git", "-Csub", "status"][..],
        ] {
            let invocation = parse(undecidable);
            assert_eq!(invocation.action, Some(GitAction::Edit), "{undecidable:?}");
            assert_eq!(invocation.subcommand, None, "{undecidable:?}");
        }
    }

    #[test]
    fn multiple_messages_are_joined_as_paragraphs() {
        for (argv, message, why) in [
            (
                &["git", "commit", "-m", "one", "-m", "two", "--", "a.txt"][..],
                "one\n\ntwo",
                "paragraphs",
            ),
            (
                &["git", "commit", "-mshort"][..],
                "short",
                "the attached form is the same message",
            ),
            (
                &["git", "commit", "--message=long"][..],
                "long",
                "the long form",
            ),
            (
                &["git", "commit", "--mess", "abbreviated"][..],
                "abbreviated",
                "an abbreviation",
            ),
            (
                &["git", "commit", "-am", "all"][..],
                "all",
                "-m clustered after a switch",
            ),
            (
                &["git", "commit", "-m", "--", "p"][..],
                "--",
                "`--` after -m is that option's value",
            ),
            (
                &["git", "commit", "-m", ""][..],
                "",
                "an empty message is a message",
            ),
        ] {
            assert_eq!(
                parse(argv).action,
                Some(commit(Some(message))),
                "{why}: {argv:?}"
            );
        }
    }

    /// A message that is not on the command line is not a message the classification can know;
    /// the commit the run records carries the real one.
    #[test]
    fn a_message_from_elsewhere_is_unknown() {
        for argv in [
            &["git", "commit", "-F", "msg.txt"][..],
            &["git", "commit", "--file=msg.txt"][..],
            &["git", "commit", "-C", "HEAD"][..],
            &["git", "commit", "-m", "x", "-F", "msg.txt"][..],
            &["git", "commit"][..],
            &["git", "commit", "-m"][..],
        ] {
            assert_eq!(parse(argv).action, Some(commit(None)), "{argv:?}");
        }
    }

    /// The same grammar has to answer identically whether its caller owns its argument vector or
    /// borrows it: the builtin passes its own `Vec<String>`, and a recorded line arrives as `&str`.
    #[test]
    fn borrowed_and_owned_argument_vectors_parse_identically() {
        let borrowed = [
            "git", "-C", "sub", "commit", "-m", "one", "-m", "two", "--", "a.txt",
        ];
        let owned: Vec<String> = borrowed.iter().map(|arg| (*arg).to_string()).collect();

        let from_borrowed = parse(&borrowed);
        let from_owned = parse(&owned);
        assert_eq!(from_borrowed.action, from_owned.action);
        assert_eq!(from_borrowed.subcommand, from_owned.subcommand);
        assert_eq!(from_owned.global_args, ["-C", "sub"]);
        assert_eq!(from_owned.command_args, &owned[4..]);
        assert_eq!(from_owned.action, Some(commit(Some("one\n\ntwo"))));
    }

    /// The scanner is also what finds `git clone`'s and `git init`'s destinations, so an operand
    /// that looks like an option has to stay with the option that takes it.
    #[test]
    fn option_operands_and_positionals_are_told_apart() {
        let scan = scan(
            &[
                "-q",
                "--sep",
                "../meta",
                "-bmain",
                "--depth=1",
                "src",
                "--",
                "-dest",
            ],
            &CLONE,
        );
        assert_eq!(scan.value(None, "separate-git-dir"), Some("../meta"));
        assert_eq!(scan.value(Some('b'), "branch"), Some("main"));
        assert_eq!(scan.value(None, "depth"), Some("1"));
        assert_eq!(scan.positionals, ["src", "-dest"]);
        assert!(scan.has(Some('q'), "quiet"));
    }

    /// A pathspec is resolved textually against the caller's working directory, exactly as git
    /// resolves it: no symlink is followed and no filesystem is consulted.
    #[test]
    fn a_pathspec_is_resolved_lexically_against_the_working_directory() {
        let base = Path::new("/work/src");
        assert_eq!(resolve(base, "a.txt"), Path::new("/work/src/a.txt"));
        assert_eq!(resolve(base, "./a.txt"), Path::new("/work/src/a.txt"));
        assert_eq!(resolve(base, "../a.txt"), Path::new("/work/a.txt"));
        assert_eq!(resolve(base, "deep/../a.txt"), Path::new("/work/src/a.txt"));
        assert_eq!(
            resolve(base, "/etc/passwd"),
            Path::new("/etc/passwd"),
            "an absolute pathspec ignores the working directory"
        );
    }
}
