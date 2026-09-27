//! `rmux claude`: launching Claude Code with rmux as its multiplexer.
//!
//! Claude Code's teammate mode drives a `tmux` binary. This module puts a private `tmux` shim
//! — a symlink to this executable — first on Claude's `PATH`, so those calls come back here
//! and are served by rmux rather than a real tmux.
//!
//! Upstream ran Claude by `exec`ing it in the client process, against a **private** rmux daemon
//! it started on its own socket label. That meant a second seed lease competing with the user's
//! daemon for the same seed, and a workload that never passed a policy gate. Claude now runs as
//! an ordinary managed pane on the existing shared daemon (see [`super::managed_io`]), in a
//! session this invocation owns, with its teammate session beside it. The logical session names
//! Claude knows about are mapped onto that owned pair by [`super::claude_namespace`].
//!
//! Nothing here spawns or `exec`s the workload locally, and there is no hidden CLI entrypoint
//! that would: the internal runner command, its pid file and the private-daemon lifecycle all
//! went away with the local launch they existed to support.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

use rmux_proto::ProcessCommand;

use super::aux_command::{AuxCommand, failure, user_home};
use super::claude_skill::ClaudeSkillInvocation;
use super::managed_io::{
    ManagedPaneCommand, ManagedPaneDisplay, ManagedPaneKind, run_managed_pane_command,
};
use super::tmux_dropin::{PUBLIC_BINARY_OVERRIDE_ENV, symlink_points_to};
use super::top_level::{scan_claude_top_level_invocation, validate_claude_top_level_invocation};
use super::{ExitFailure, StartupOptions, top_level_startup};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

const TEAMMATE_MODE_FLAG: &str = "--teammate-mode";
const TEAMMATE_MODE: &str = "tmux";
const AGENT_TEAMS_ENV: &str = "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS";
const DISABLE_TMUX_SHIM_ENV: &str = "RMUX_DISABLE_TMUX_SHIM";
const DIRECT_LAUNCH_ENV: &str = "RMUX_CLAUDE_DIRECT";

/// The program name the managed pane resolves through its own `PATH`.
///
/// Resolution is the pane shell's, not this client's: the pane shell runs `claude` the same way
/// an interactive user would, including any shell function or alias wrapping it.
const CLAUDE_PROGRAM: &str = "claude";

/// One `rmux claude` invocation's pass-through arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClaudeInvocation {
    args: Vec<OsString>,
}

impl AuxCommand for ClaudeInvocation {
    /// Recognizes `rmux claude`, rejecting top-level modes the managed launcher cannot honor.
    fn parse(arguments: &[OsString]) -> Result<Option<Self>, ExitFailure> {
        let Some(invocation) = scan_claude_top_level_invocation(arguments) else {
            return Ok(None);
        };
        validate_claude_top_level_invocation(&invocation)?;
        Ok(Some(Self {
            args: invocation.into_arguments(),
        }))
    }

    /// Installs the bundled skill for `rmux claude install-skill`, and otherwise launches Claude.
    ///
    /// Claude's arguments belong to Claude rather than to rmux, so the typed parse never runs
    /// for them. The launch still needs the daemon this invocation would otherwise resolve, so
    /// the top-level `-L`/`-S` selection is recovered from raw argv — the same recovery the
    /// unknown-command path uses.
    fn run(self, argv: &[OsString]) -> Result<i32, ExitFailure> {
        if let Some(skill) = ClaudeSkillInvocation::parse(&self.args)? {
            return skill.run(argv);
        }
        let (socket_path, startup) = top_level_startup(argv)?;
        launch(self, &socket_path, startup).map_err(|error| error.with_socket_context(&socket_path))
    }
}

/// Runs Claude Code as a managed pane on the daemon `socket_path` selects.
///
/// The attached/direct decision is unchanged from upstream — a real terminal gets the
/// interactive UI, redirected input does not — but both now reach the same managed pane. The
/// only difference between them is how this process presents that pane: an attached rmux client
/// or a byte relay.
///
/// # Errors
///
/// Fails when the private tmux shim cannot be installed, when the daemon cannot be reached, and
/// with Claude's own nonzero gated exit status.
fn launch(
    invocation: ClaudeInvocation,
    socket_path: &Path,
    startup: StartupOptions,
) -> Result<i32, ExitFailure> {
    let attached = should_launch_attached();
    if !attached {
        report_unrequested_direct_launch();
    }

    let workload = build_claude_workload(invocation.args)?;
    let directory = env::current_dir().map_err(|error| {
        failure(
            "claude",
            format_args!("failed to resolve the current directory: {error}"),
        )
    })?;

    let result = run_managed_pane_command(
        ManagedPaneCommand {
            process: ProcessCommand::Argv(workload.argv),
            directory,
            environment: workload.environment,
            display: if attached {
                ManagedPaneDisplay::Attach
            } else {
                ManagedPaneDisplay::Relay
            },
            kind: ManagedPaneKind::Claude,
        },
        socket_path,
        startup,
    );
    // The shim directory must outlive the pane: Claude resolves `tmux` through it for as long
    // as it runs, which on the attached path is the whole time this call blocks.
    drop(workload.shim);
    result
}

/// The argv, environment and shim lifetime one Claude launch needs.
struct ClaudeWorkload {
    /// Complete argv for the pane process.
    argv: Vec<String>,
    /// Pane environment overrides.
    environment: Vec<(String, Option<String>)>,
    /// Keeps a temporary shim directory alive while the pane runs.
    shim: Option<PrivateTmuxShim>,
}

/// Builds the Claude workload, installing the private tmux shim unless it is disabled.
fn build_claude_workload(args: Vec<OsString>) -> Result<ClaudeWorkload, ExitFailure> {
    let mut argv = vec![
        CLAUDE_PROGRAM.to_owned(),
        TEAMMATE_MODE_FLAG.to_owned(),
        TEAMMATE_MODE.to_owned(),
    ];
    for argument in args {
        argv.push(argument.into_string().map_err(|value| {
            failure(
                "claude",
                format_args!("argument is not valid UTF-8: {}", value.to_string_lossy()),
            )
        })?);
    }

    let mut environment = vec![(AGENT_TEAMS_ENV.to_owned(), Some("1".to_owned()))];
    let shim = if private_tmux_shim_enabled() {
        let shim = ensure_private_tmux_shim()?;
        let path = path_with_shim_first(shim.path())?;
        environment.push((
            "PATH".to_owned(),
            Some(path.into_string().map_err(|value| {
                failure(
                    "claude",
                    format_args!("PATH is not valid UTF-8: {}", value.to_string_lossy()),
                )
            })?),
        ));
        Some(shim)
    } else {
        None
    };

    Ok(ClaudeWorkload {
        argv,
        environment,
        shim,
    })
}

/// Reports whether the private `tmux` shim should be installed for this launch.
fn private_tmux_shim_enabled() -> bool {
    !env_flag_enabled(DISABLE_TMUX_SHIM_ENV)
}

/// Treats `name` as set when its value is non-empty and neither `0` nor `false`.
fn env_flag_enabled(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| {
        let value = value.to_string_lossy();
        let value = value.trim();
        !value.is_empty()
            && !value.eq_ignore_ascii_case("0")
            && !value.eq_ignore_ascii_case("false")
    })
}

/// A directory holding the private `tmux` shim Claude resolves through.
///
/// The directory is per-user and persistent, so it is never removed here: a concurrent
/// `rmux claude` may be resolving the same symlink. The value exists to tie the shim's
/// readiness to the lifetime of the pane that depends on it.
struct PrivateTmuxShim {
    dir: PathBuf,
}

impl PrivateTmuxShim {
    /// Adopts an existing per-user shim directory that outlives this process.
    const fn persistent(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Directory to put ahead of the pane's `PATH` so its `tmux` resolves to the shim.
    fn path(&self) -> &Path {
        &self.dir
    }
}

/// Decides from the environment and stdin whether Claude gets an attached rmux client.
fn should_launch_attached() -> bool {
    launch_attached_decision(
        env::var_os(DIRECT_LAUNCH_ENV).is_some(),
        io::stdin().is_terminal(),
    )
}

/// Attach only for a terminal stdin with no explicit direct-launch request.
const fn launch_attached_decision(direct_launch_requested: bool, stdin_is_terminal: bool) -> bool {
    !direct_launch_requested && stdin_is_terminal
}

/// Whether a fall-through to the relayed path should be explained on stderr.
const fn should_report_direct_launch(
    direct_launch_requested: bool,
    stderr_is_terminal: bool,
) -> bool {
    !direct_launch_requested && stderr_is_terminal
}

/// Explains a fall-through to the relayed launch path when the user did not ask for it.
///
/// Claude's interactive UI needs a terminal; reaching this point means stdin was not recognized
/// as one (issue #77: VS Code tasks, plain pipes), and launching silently used to look like
/// "rmux claude does nothing".
fn report_unrequested_direct_launch() {
    if should_report_direct_launch(
        env::var_os(DIRECT_LAUNCH_ENV).is_some(),
        io::stderr().is_terminal(),
    ) {
        eprintln!(
            "rmux claude: stdin is not a terminal, so the interactive UI is skipped \
             and claude's output is relayed; run from a terminal, or set \
             {DIRECT_LAUNCH_ENV}=1 to make the relayed launch explicit."
        );
    }
}

/// Requires `path` to be a plain directory owned by this uid, tightening group/world bits.
fn validate_secure_owner_directory(path: &Path, label: &str) -> Result<(), ExitFailure> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        failure(
            "claude",
            format_args!("failed to inspect {label} '{}': {error}", path.display()),
        )
    })?;
    if metadata.file_type().is_symlink() {
        return Err(failure(
            "claude",
            format_args!("refusing symlinked {label} '{}'", path.display()),
        ));
    }
    if !metadata.is_dir() {
        return Err(failure(
            "claude",
            format_args!("refusing non-directory {label} '{}'", path.display()),
        ));
    }
    // SAFETY: `geteuid` reads the effective uid of the current process and has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid {
        return Err(failure(
            "claude",
            format_args!(
                "refusing {label} '{}' owned by uid {}",
                path.display(),
                metadata.uid()
            ),
        ));
    }

    let mode = metadata.mode() & 0o777;
    if mode.trailing_zeros() >= 6 {
        return Ok(());
    }

    fs::set_permissions(path, fs::Permissions::from_mode(mode & !0o077)).map_err(|error| {
        failure(
            "claude",
            format_args!(
                "failed to tighten permissions on {label} '{}': {error}",
                path.display()
            ),
        )
    })?;
    let tightened = fs::symlink_metadata(path).map_err(|error| {
        failure(
            "claude",
            format_args!("failed to re-inspect {label} '{}': {error}", path.display()),
        )
    })?;
    if tightened.mode().trailing_zeros() >= 6 {
        Ok(())
    } else {
        Err(failure(
            "claude",
            format_args!(
                "refusing group/world-accessible {label} '{}'",
                path.display()
            ),
        ))
    }
}

/// Creates or repairs the per-user `tmux` symlink that points back at this executable.
fn ensure_private_tmux_shim() -> Result<PrivateTmuxShim, ExitFailure> {
    let dir = user_home("claude")?.join(".local/share/rmux/claude-tmux-shim");
    fs::create_dir_all(&dir).map_err(|error| {
        failure(
            "claude",
            format_args!(
                "failed to create private tmux shim directory '{}': {error}",
                dir.display()
            ),
        )
    })?;
    validate_secure_owner_directory(&dir, "private tmux shim directory")?;
    let target = private_tmux_shim_target_binary()?;
    let shim = dir.join("tmux");
    let create = || {
        symlink(&target, &shim).map_err(|error| {
            failure(
                "claude",
                format_args!(
                    "failed to create private tmux shim '{}': {error}",
                    shim.display()
                ),
            )
        })
    };
    match fs::symlink_metadata(&shim) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if !symlink_points_to(&shim, &target) {
                fs::remove_file(&shim).map_err(|error| {
                    failure(
                        "claude",
                        format_args!(
                            "failed to replace private tmux shim '{}': {error}",
                            shim.display()
                        ),
                    )
                })?;
                create()?;
            }
        }
        Ok(_) => {
            return Err(failure(
                "claude",
                format_args!(
                    "'{}' exists and is not a symlink; refusing to overwrite it",
                    shim.display()
                ),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => create()?,
        Err(error) => {
            return Err(failure(
                "claude",
                format_args!(
                    "failed to inspect private tmux shim '{}': {error}",
                    shim.display()
                ),
            ));
        }
    }
    Ok(PrivateTmuxShim::persistent(dir))
}

/// Resolves the binary that the private `tmux` symlink should point to.
fn private_tmux_shim_target_binary() -> Result<PathBuf, ExitFailure> {
    let public = public_rmux_binary()?;
    Ok(private_tmux_shim_target_for_public_binary(&public))
}

/// Prefers a `libexec` full helper beside `public`, falling back to `public` itself.
fn private_tmux_shim_target_for_public_binary(public: &Path) -> PathBuf {
    FULL_HELPER_PATHS
        .into_iter()
        .filter_map(|helper| Some(public.parent()?.join(helper)))
        .find(|candidate| candidate.is_file() && candidate.as_path() != public)
        .unwrap_or_else(|| public.to_path_buf())
}

/// Where a full `rmux` helper may sit relative to the public binary's directory, preferred first.
const FULL_HELPER_PATHS: [&str; 3] = [
    "libexec/rmux/rmux",
    "../libexec/rmux/rmux",
    "../lib/rmux/libexec/rmux",
];

/// Locates this rmux executable, honoring the internal public-binary override variable.
fn public_rmux_binary() -> Result<PathBuf, ExitFailure> {
    if let Some(path) = env::var_os(PUBLIC_BINARY_OVERRIDE_ENV) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
    }

    env::current_exe().map_err(|error| {
        failure(
            "claude",
            format_args!("failed to resolve current rmux binary: {error}"),
        )
    })
}

/// Builds a `PATH` with `shim_dir` ahead of the inherited one.
fn path_with_shim_first(shim_dir: &Path) -> Result<OsString, ExitFailure> {
    path_with_shim_first_from(shim_dir, env::var_os("PATH"))
}

/// Joins `shim_dir` ahead of `original`, tolerating an unset or empty inherited `PATH`.
fn path_with_shim_first_from(
    shim_dir: &Path,
    original: Option<OsString>,
) -> Result<OsString, ExitFailure> {
    let original = original.unwrap_or_default();
    let mut paths = vec![shim_dir.to_path_buf()];
    if !original.is_empty() {
        paths.extend(env::split_paths(&original));
    }
    env::join_paths(paths).map_err(|error| {
        failure(
            "claude",
            format_args!("failed to build PATH with private tmux shim: {error}"),
        )
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::{
        ClaudeInvocation, launch_attached_decision, path_with_shim_first_from,
        private_tmux_shim_target_for_public_binary, should_report_direct_launch,
    };
    use crate::cli::aux_command::{AuxCommand, args};
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};

    #[test]
    fn unrequested_direct_launch_notice_distinguishes_human_stderr_from_redirects() {
        assert!(should_report_direct_launch(false, true));
        assert!(!should_report_direct_launch(false, false));
        assert!(!should_report_direct_launch(true, true));
    }

    #[test]
    fn attached_launch_rejects_direct_and_nonterminal_inputs() {
        assert!(launch_attached_decision(false, true));
        assert!(!launch_attached_decision(true, true));
        assert!(!launch_attached_decision(false, false));
    }

    #[test]
    fn invocation_preserves_claude_arguments() {
        let invocation = ClaudeInvocation::parse(&args(&[
            "claude",
            "--dangerously-skip-permissions",
            "--teammate-mode",
            "in-process",
        ]))
        .expect("parse succeeds")
        .expect("claude invocation");

        assert_eq!(
            invocation.args,
            args(&[
                "--dangerously-skip-permissions",
                "--teammate-mode",
                "in-process"
            ])
        );
    }

    #[test]
    fn shim_path_precedes_existing_path() {
        let shim = Path::new("rmux-shim");
        let existing =
            env::join_paths([Path::new("usr-bin"), Path::new("bin")]).expect("joined path");
        let path = path_with_shim_first_from(shim, Some(existing)).expect("joined path");
        let mut entries = env::split_paths(&path);
        assert_eq!(entries.next(), Some(shim.to_path_buf()));
        assert_eq!(entries.next(), Some(PathBuf::from("usr-bin")));
        assert_eq!(entries.next(), Some(PathBuf::from("bin")));
    }

    #[test]
    fn private_tmux_shim_prefers_packaged_full_helpers_in_layout_order() {
        const STANDARD: &str = "libexec/rmux/rmux";
        const PREFIX_LIB: &str = "lib/rmux/libexec/rmux";
        for (label, helpers, expected) in [
            ("full-helper", &[STANDARD][..], Some(STANDARD)),
            ("prefix-lib-helper", &[PREFIX_LIB][..], Some(PREFIX_LIB)),
            (
                "helper-precedence",
                &[STANDARD, PREFIX_LIB][..],
                Some(STANDARD),
            ),
            ("no-helper", &[][..], None),
        ] {
            let root = env::temp_dir().join(format!(
                "rmux-claude-launcher-{label}-{}",
                std::process::id()
            ));
            let public = root.join("bin").join("rmux");
            fs::create_dir_all(root.join("bin")).expect("bin dir");
            fs::write(&public, b"tiny").expect("public rmux");
            for helper in helpers {
                let helper = root.join(helper);
                fs::create_dir_all(helper.parent().expect("helper parent")).expect("helper dir");
                fs::write(&helper, b"full").expect("full helper");
            }

            let target = private_tmux_shim_target_for_public_binary(&public);
            match expected {
                Some(helper) => assert_eq!(
                    fs::canonicalize(target).expect("canonical target"),
                    fs::canonicalize(root.join(helper)).expect("canonical helper"),
                    "{label}"
                ),
                None => assert_eq!(target, public, "{label}"),
            }

            let _ = fs::remove_dir_all(root);
        }
    }
}
