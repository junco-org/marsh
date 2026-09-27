use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use super::ExitFailure;
use super::aux_command::{AuxCommand, command_word, failure, user_home, write_stdout};

/// Names a public `rmux` binary to link the tmux shims at instead of the running executable.
pub(super) const PUBLIC_BINARY_OVERRIDE_ENV: &str = "RMUX_INTERNAL_PUBLIC_BINARY_PATH";

/// The command every `setup tmux-shim` failure and output write is worded for.
const SETUP_COMMAND: &str = "setup tmux-shim";

/// Which tmux drop-in compatibility subcommand the argument vector selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DropinInvocation {
    DoctorTmuxDropin,
    SetupTmuxShim,
}

impl AuxCommand for DropinInvocation {
    /// Detects a `doctor` or `setup` drop-in subcommand after tmux's top-level flags.
    fn parse(arguments: &[OsString]) -> Result<Option<Self>, ExitFailure> {
        match command_word(arguments, "cfLST", "cfLST", |_| {}) {
            Some(("doctor", rest)) => expect_only(rest, "doctor", "tmux-dropin", "check")
                .map(|()| Some(Self::DoctorTmuxDropin)),
            Some(("setup", rest)) => expect_only(rest, "setup", "tmux-shim", "action")
                .map(|()| Some(Self::SetupTmuxShim)),
            _ => Ok(None),
        }
    }

    /// Runs the selected drop-in subcommand against the argv0 this binary was invoked as.
    fn run(self, argv: &[OsString]) -> Result<i32, ExitFailure> {
        match self {
            Self::DoctorTmuxDropin => run_doctor(argv.first()),
            Self::SetupTmuxShim => run_setup_tmux_shim(argv.first()),
        }
    }
}

/// Requires `expected` as the only argument after `command`, answering `--help` with its usage.
///
/// `noun` names what an unexpected argument was taken to be, such as a check or an action.
fn expect_only(
    arguments: &[OsString],
    command: &str,
    expected: &str,
    noun: &str,
) -> Result<(), ExitFailure> {
    let first = arguments.first().and_then(|argument| argument.to_str());
    if first == Some("--help") {
        return Err(ExitFailure::new_stdout(
            0,
            format!("usage: rmux {command} {expected}"),
        ));
    }
    let Some(subcommand) = first else {
        return Err(failure(command, format_args!("expected {expected}")));
    };
    if arguments.len() != 1 {
        return Err(failure(command, "expected exactly one argument"));
    }
    if subcommand != expected {
        return Err(failure(
            command,
            format_args!("unknown {noun} '{subcommand}'"),
        ));
    }
    Ok(())
}

/// Prints whether this binary was invoked through a `tmux` shim, plus setup hints.
fn run_doctor(argv0: Option<&OsString>) -> Result<i32, ExitFailure> {
    let argv0_name = argv0
        .and_then(|value| Path::new(value).file_name())
        .and_then(OsStr::to_str)
        .unwrap_or("rmux");
    let shim_detected = Path::new(argv0_name)
        .file_stem()
        .and_then(OsStr::to_str)
        .is_some_and(|stem| stem == "tmux");
    let shim = if shim_detected {
        "detected"
    } else {
        "not detected"
    };

    let mut output = String::new();
    output.push_str("rmux tmux-dropin doctor\n");
    let _ = writeln!(output, "shim:        {shim}   (argv[0]={argv0_name})");
    if !shim_detected {
        output.push_str("suggested:   ln -s $(command -v rmux) ~/.local/bin/tmux\n");
        output.push_str("setup:       rmux setup tmux-shim\n");
    }
    write_stdout(output.as_bytes(), "doctor")
}

/// Links `~/.local/bin/tmux` at the public `rmux` binary, refusing to clobber real files.
fn run_setup_tmux_shim(argv0: Option<&OsString>) -> Result<i32, ExitFailure> {
    let bin_dir = user_home(SETUP_COMMAND)?.join(".local").join("bin");
    fs::create_dir_all(&bin_dir).map_err(|error| {
        failure(
            SETUP_COMMAND,
            format_args!("failed to create '{}': {error}", bin_dir.display()),
        )
    })?;

    let target = setup_tmux_shim_target(argv0)?;
    let shim = bin_dir.join("tmux");
    let link = || format!("{} -> {}", shim.display(), target.link_target.display());
    match fs::symlink_metadata(&shim) {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                && symlink_points_to(&shim, &target.link_target) =>
        {
            write_stdout(
                format!("exists:      {}\n", link()).as_bytes(),
                SETUP_COMMAND,
            )
        }
        Ok(metadata)
            if metadata.file_type().is_symlink()
                && symlink_is_previous_packaged_rmux(&shim, &target.executable) =>
        {
            replace_symlink(&shim, &target.link_target).map_err(|error| {
                failure(
                    SETUP_COMMAND,
                    format_args!("failed to refresh '{}': {error}", shim.display()),
                )
            })?;
            write_stdout(
                format!("updated:     {}\n", link()).as_bytes(),
                SETUP_COMMAND,
            )
        }
        Ok(_) => Err(failure(
            SETUP_COMMAND,
            format_args!("'{}' already exists; refusing to overwrite", shim.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            symlink(&target.link_target, &shim).map_err(|error| {
                failure(
                    SETUP_COMMAND,
                    format_args!("failed to create '{}': {error}", shim.display()),
                )
            })?;
            write_stdout(
                format!(
                    "created:     {}\nnext:        ensure ~/.local/bin is before tmux in PATH\n",
                    link()
                )
                .as_bytes(),
                SETUP_COMMAND,
            )
        }
        Err(error) => Err(failure(
            SETUP_COMMAND,
            format_args!("failed to inspect '{}': {error}", shim.display()),
        )),
    }
}

/// The shim's symlink destination plus the canonical `rmux` executable it resolves to.
struct SetupTmuxShimTarget {
    link_target: PathBuf,
    executable: PathBuf,
}

/// Resolves the shim's link target, honouring the internal public-binary path override.
fn setup_tmux_shim_target(argv0: Option<&OsString>) -> Result<SetupTmuxShimTarget, ExitFailure> {
    let current = std::env::current_exe().map_err(|error| {
        failure(
            SETUP_COMMAND,
            format_args!("failed to resolve current rmux binary: {error}"),
        )
    })?;
    let public = std::env::var_os(PUBLIC_BINARY_OVERRIDE_ENV)
        .map(PathBuf::from)
        .and_then(|path| absolute_without_resolving_links(&path))
        .filter(|path| path.is_file())
        .unwrap_or(current);
    let executable = fs::canonicalize(&public).unwrap_or_else(|_| public.clone());
    let link_target = stable_rmux_invocation_path(argv0, &public).unwrap_or(public);
    Ok(SetupTmuxShimTarget {
        link_target,
        executable,
    })
}

/// Returns the stable `rmux` path that `argv0` names, when it is the same binary.
fn stable_rmux_invocation_path(argv0: Option<&OsString>, public_binary: &Path) -> Option<PathBuf> {
    let invoked = Path::new(argv0?);
    if invoked.file_stem().and_then(OsStr::to_str) != Some("rmux") {
        return None;
    }

    if invoked.components().count() > 1 {
        let candidate = absolute_without_resolving_links(invoked)?;
        return paths_resolve_to_same_file(&candidate, public_binary).then_some(candidate);
    }

    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(invoked))
        .filter_map(|candidate| absolute_without_resolving_links(&candidate))
        .find(|candidate| paths_resolve_to_same_file(candidate, public_binary))
}

/// Makes `path` absolute by joining the process cwd, without resolving symlinks.
fn absolute_without_resolving_links(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

/// Reports whether `shim` points at an older `rmux` from the same package root.
fn symlink_is_previous_packaged_rmux(shim: &Path, current_executable: &Path) -> bool {
    resolved_link_target(shim)
        .is_some_and(|target| same_packaged_rmux_lineage(&target, current_executable))
}

/// The package manager tree a packaged `rmux` binary lives under.
#[derive(Clone, Copy)]
enum PackagedRmuxRoot<'a> {
    Homebrew(&'a Path),
    Nix(&'a Path),
}

/// Reports whether both paths are packaged by the same manager under one root.
fn same_packaged_rmux_lineage(left: &Path, right: &Path) -> bool {
    let roots = match (packaged_rmux_root(left), packaged_rmux_root(right)) {
        (Some(PackagedRmuxRoot::Homebrew(left)), Some(PackagedRmuxRoot::Homebrew(right)))
        | (Some(PackagedRmuxRoot::Nix(left)), Some(PackagedRmuxRoot::Nix(right))) => {
            Some((left, right))
        }
        _ => None,
    };
    match roots {
        Some((left, right)) => left == right || paths_resolve_to_same_file(left, right),
        None => false,
    }
}

/// Classifies `binary` as a Homebrew Cellar or Nix store installation of `rmux`.
fn packaged_rmux_root(binary: &Path) -> Option<PackagedRmuxRoot<'_>> {
    if binary.file_name()? != OsStr::new("rmux") {
        return None;
    }
    let bin = binary.parent()?;
    if bin.file_name()? != OsStr::new("bin") {
        return None;
    }
    let package = bin.parent()?;

    if let Some(formula) = package.parent() {
        if formula.file_name() == Some(OsStr::new("rmux"))
            && formula.parent()?.file_name() == Some(OsStr::new("Cellar"))
        {
            return Some(PackagedRmuxRoot::Homebrew(formula));
        }
    }

    if !nix_derivation_is_rmux(package.file_name()?) {
        return None;
    }
    let store = package.parent()?;
    let nix = store.parent()?;
    (store.file_name()? == OsStr::new("store") && nix.file_name()? == OsStr::new("nix"))
        .then_some(PackagedRmuxRoot::Nix(store))
}

/// Reports whether a Nix store directory name is an `rmux` derivation.
fn nix_derivation_is_rmux(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some((hash, package)) = name.split_once('-') else {
        return false;
    };
    hash.len() == 32
        && hash.bytes().all(|byte| {
            matches!(
                byte,
                b'0'..=b'9'
                    | b'a'..=b'd'
                    | b'f'..=b'n'
                    | b'p'..=b's'
                    | b'v'..=b'z'
            )
        })
        && (package == "rmux" || package.starts_with("rmux-"))
}

/// Atomically repoints `shim` at `target`, failing if the link changed meanwhile.
fn replace_symlink(shim: &Path, target: &Path) -> io::Result<()> {
    let previous = fs::read_link(shim)?;
    let parent = shim.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = None;
    for attempt in 0..16 {
        let candidate = parent.join(format!(
            ".tmux.rmux-update-{}-{attempt}",
            std::process::id()
        ));
        match symlink(target, &candidate) {
            Ok(()) => {
                temporary = Some(candidate);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let temporary = temporary.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no temporary tmux shim path was available",
        )
    })?;

    if !fs::read_link(shim).is_ok_and(|current| current == previous) {
        let _ = fs::remove_file(&temporary);
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "tmux shim changed while it was being refreshed",
        ));
    }
    if let Err(error) = fs::rename(&temporary, shim) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

/// Where the symlink at `link` points, with a relative target resolved against its directory.
fn resolved_link_target(link: &Path) -> Option<PathBuf> {
    let target = fs::read_link(link).ok()?;
    Some(if target.is_absolute() {
        target
    } else {
        link.parent().unwrap_or_else(|| Path::new(".")).join(target)
    })
}

/// Reports whether the symlink at `shim` resolves to the same file as `target`.
pub(super) fn symlink_points_to(shim: &Path, target: &Path) -> bool {
    resolved_link_target(shim).is_some_and(|resolved| paths_resolve_to_same_file(&resolved, target))
}

/// Reports whether both paths canonicalize to the same file.
fn paths_resolve_to_same_file(left: &Path, right: &Path) -> bool {
    matches!(
        (fs::canonicalize(left), fs::canonicalize(right)),
        (Ok(left), Ok(right)) if left == right
    )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::{DropinInvocation, same_packaged_rmux_lineage};
    use crate::cli::aux_command::{AuxCommand, args};
    use std::path::Path;

    #[test]
    fn parses_drop_in_subcommands_after_top_level_flags() {
        for (argv, expected) in [
            (
                &["-Ldemo", "doctor", "tmux-dropin"][..],
                DropinInvocation::DoctorTmuxDropin,
            ),
            (
                &["-c/tmp", "doctor", "tmux-dropin"][..],
                DropinInvocation::DoctorTmuxDropin,
            ),
            (&["setup", "tmux-shim"][..], DropinInvocation::SetupTmuxShim),
        ] {
            let invocation = DropinInvocation::parse(&args(argv))
                .expect("parse succeeds")
                .expect("drop-in invocation");

            assert_eq!(invocation, expected, "{argv:?}");
        }
    }

    #[test]
    fn ignores_other_commands() {
        assert!(
            DropinInvocation::parse(&args(&["list-sessions"]))
                .expect("parse succeeds")
                .is_none()
        );
    }

    #[test]
    fn recognizes_versions_of_the_same_homebrew_formula() {
        assert!(same_packaged_rmux_lineage(
            Path::new("/opt/homebrew/Cellar/rmux/0.8.0/bin/rmux"),
            Path::new("/opt/homebrew/Cellar/rmux/0.9.0/bin/rmux")
        ));
        assert!(!same_packaged_rmux_lineage(
            Path::new("/tmp/attacker/rmux"),
            Path::new("/opt/homebrew/Cellar/rmux/0.9.0/bin/rmux")
        ));
    }

    #[test]
    fn recognizes_versions_from_the_same_nix_store() {
        assert!(same_packaged_rmux_lineage(
            Path::new("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-rmux-0.8.0/bin/rmux"),
            Path::new("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-rmux-0.9.0/bin/rmux")
        ));
        assert!(!same_packaged_rmux_lineage(
            Path::new("/tmp/attacker/rmux"),
            Path::new("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-rmux-0.9.0/bin/rmux")
        ));
        assert!(!same_packaged_rmux_lineage(
            Path::new("/tmp/fake/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-rmux-0.8.0/bin/rmux"),
            Path::new("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-rmux-0.9.0/bin/rmux")
        ));
    }
}
