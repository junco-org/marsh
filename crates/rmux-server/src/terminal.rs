use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmux_core::{EnvironmentStore, OptionStore, PaneId};
use rmux_proto::{AttachShellCommand, OptionName, ProcessCommand, RmuxError, SessionName};

mod shell_resolver;
mod shell_spec;

#[cfg(unix)]
pub(crate) use shell_resolver::is_suitable_shell;
#[cfg(windows)]
use shell_resolver::CLIENT_SHELL_ENV;
use shell_resolver::{configured_pane_shell, resolve_shell_path};
use shell_spec::ShellSpec;

#[cfg(windows)]
const WINDOWS_BATCH_ARGV_UNSUPPORTED_MESSAGE: &str =
    "process command argv cannot target Windows .cmd or .bat scripts; use shell command mode";

/// How a pane's terminal runs what the user types or asks for.
///
/// Recorded once, when the pane is created, and carried through every respawn. It is deliberately
/// an explicit decision rather than something re-derived from a path, because the two modes are
/// not two spellings of the same thing:
///
/// * [`Embedded`](Self::Embedded) is the daemon's own interpreter, reading the pane's terminal
///   through an idle lease. Each submitted line becomes one gated workload, so an approval
///   decision covers exactly the command that was run.
/// * [`External`](Self::External) is a real child process — the shell an operator deliberately
///   named in `default-shell`. The whole session is then one workload, which is a coarser gate,
///   and is chosen only because it was asked for.
///
/// Inferring the mode from [`TerminalProfile::shell`] would silently pick the second: that path is
/// the *helper* shell, resolved from `$SHELL` or the passwd entry for `run-shell` and friends, and
/// it is never empty. Every unconfigured pane would become a long-lived `sh` subprocess gating an
/// entire interactive session instead of each line in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaneShell {
    /// The embedded brush interpreter: no `default-shell` was configured.
    Embedded,
    /// A deliberately configured `default-shell`, run as this pane's process.
    External(PathBuf),
}

impl PaneShell {
    /// The pane mode `default-shell` currently asks for, in this scope.
    pub(crate) fn resolve(options: &OptionStore, session_name: Option<&SessionName>) -> Self {
        configured_pane_shell(options, session_name).map_or(Self::Embedded, Self::External)
    }
}

/// Immutable pane-spawn metadata captured when a pane terminal is created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalProfile {
    cwd: PathBuf,
    /// Whether [`cwd`](Self::cwd) is a directory the caller *named*, or one that was inherited.
    ///
    /// The distinction decides what happens when it turns out to lie outside the seed. A named
    /// directory is a request this daemon cannot honour, and starting somewhere else would run the
    /// caller's command against the wrong tree — so it is refused. An inherited one is not a
    /// request at all: it is wherever the daemon happened to be started, and refusing it would
    /// make a server launched from outside its own seed unable to open a single pane.
    requested_cwd: bool,
    /// The shell helper commands run through, and the value exported as `SHELL`.
    ///
    /// Always a real path, and never on its own evidence of what the pane is running — see
    /// [`PaneShell`].
    shell: PathBuf,
    /// How this pane runs its work.
    pane_shell: PaneShell,
    raw_environment: Arc<Vec<(OsString, OsString)>>,
}

/// Session-level environment captured from the pane that created a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionBaseEnvironment {
    raw_environment: Vec<(OsString, OsString)>,
}

impl SessionBaseEnvironment {
    pub(crate) fn from_profile(profile: &TerminalProfile) -> Self {
        Self {
            raw_environment: profile.raw_environment.as_ref().clone(),
        }
    }

    fn environment_map(&self) -> HashMap<String, String> {
        environment_from_os_pairs(self.raw_environment.iter().cloned())
    }

    fn raw_environment(&self) -> &[(OsString, OsString)] {
        &self.raw_environment
    }
}

impl TerminalProfile {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_session(
        environment: &EnvironmentStore,
        options: &OptionStore,
        session_name: &SessionName,
        session_id: u32,
        socket_path: &Path,
        base_environment: Option<&SessionBaseEnvironment>,
        spawn_environment: Option<&HashMap<String, String>>,
        include_terminal_defaults: bool,
        overrides: Option<&[String]>,
        pane_id: Option<PaneId>,
        requested_cwd: Option<&Path>,
    ) -> Result<Self, RmuxError> {
        let base_environment_map = base_environment.map(SessionBaseEnvironment::environment_map);
        Self::for_session_with_environment(
            environment,
            options,
            session_name,
            session_id,
            socket_path,
            base_environment_map.as_ref(),
            spawn_environment,
            base_environment.map(SessionBaseEnvironment::raw_environment),
            include_terminal_defaults,
            overrides,
            pane_id,
            requested_cwd,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_initial_session_pane(
        environment: &EnvironmentStore,
        options: &OptionStore,
        session_name: &SessionName,
        session_id: u32,
        socket_path: &Path,
        spawn_environment: Option<&HashMap<String, String>>,
        raw_base_environment: Option<&[(OsString, OsString)]>,
        include_terminal_defaults: bool,
        overrides: Option<&[String]>,
        pane_id: Option<PaneId>,
        requested_cwd: Option<&Path>,
    ) -> Result<Self, RmuxError> {
        Self::for_session_with_environment(
            environment,
            options,
            session_name,
            session_id,
            socket_path,
            spawn_environment,
            None,
            raw_base_environment,
            include_terminal_defaults,
            overrides,
            pane_id,
            requested_cwd,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn for_session_with_environment(
        environment: &EnvironmentStore,
        options: &OptionStore,
        session_name: &SessionName,
        session_id: u32,
        socket_path: &Path,
        base_environment: Option<&HashMap<String, String>>,
        spawn_environment: Option<&HashMap<String, String>>,
        raw_base_environment: Option<&[(OsString, OsString)]>,
        include_terminal_defaults: bool,
        overrides: Option<&[String]>,
        pane_id: Option<PaneId>,
        requested_cwd: Option<&Path>,
    ) -> Result<Self, RmuxError> {
        let mut resolved = base_environment
            .cloned()
            .unwrap_or_else(base_process_environment);
        let include_implicit_globals = base_environment.is_none();
        if base_environment.is_some() {
            environment.apply_to_process_environment_without_implicit_globals(
                Some(session_name),
                &mut resolved,
            );
        } else {
            environment.apply_to_process_environment(Some(session_name), &mut resolved);
        }
        if let Some(spawn_environment) = spawn_environment {
            for (name, value) in spawn_environment {
                set_environment_value(&mut resolved, name.clone(), value.clone());
            }
        }

        Self::from_resolved_environment(
            resolved,
            raw_base_environment,
            environment
                .suppressed_process_environment_names(Some(session_name), include_implicit_globals),
            options,
            session_name,
            session_id,
            socket_path,
            include_terminal_defaults,
            overrides,
            pane_id,
            requested_cwd,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_resolved_environment(
        mut resolved: HashMap<String, String>,
        raw_base_environment: Option<&[(OsString, OsString)]>,
        mut suppressed_raw_names: HashSet<String>,
        options: &OptionStore,
        session_name: &SessionName,
        session_id: u32,
        socket_path: &Path,
        include_terminal_defaults: bool,
        overrides: Option<&[String]>,
        pane_id: Option<PaneId>,
        requested_cwd: Option<&Path>,
    ) -> Result<Self, RmuxError> {
        if include_terminal_defaults {
            if let Some(default_terminal) = options
                .resolve(Some(session_name), OptionName::DefaultTerminal)
                .or_else(|| options.resolve(None, OptionName::DefaultTerminal))
            {
                set_environment_value(
                    &mut resolved,
                    "TERM".to_owned(),
                    default_terminal.to_owned(),
                );
            }
            set_environment_value(&mut resolved, "TERM_PROGRAM".to_owned(), "rmux".to_owned());
            set_environment_value(
                &mut resolved,
                "TERM_PROGRAM_VERSION".to_owned(),
                env!("CARGO_PKG_VERSION").to_owned(),
            );
        } else {
            remove_environment_value(&mut resolved, "TERM_PROGRAM");
            remove_environment_value(&mut resolved, "TERM_PROGRAM_VERSION");
            suppress_terminal_program_environment(&mut suppressed_raw_names);
        }

        let mux_socket_path = mux_environment_socket_path(socket_path);
        let mux_env = format!(
            "{},{},{}",
            mux_socket_path.display(),
            std::process::id(),
            session_id
        );
        set_environment_value(&mut resolved, "RMUX".to_owned(), mux_env.clone());
        set_environment_value(&mut resolved, "TMUX".to_owned(), mux_env);
        crate::tmux_shim::apply_tmux_shim_environment(&mut resolved, socket_path);

        if let Some(overrides) = overrides {
            for (name, value) in parse_environment_assignments(overrides)? {
                set_environment_value(&mut resolved, name, value);
            }
        }

        let cwd = resolve_working_directory(requested_cwd)?;
        let shell = resolve_shell_path(options, Some(session_name), &resolved);
        let pane_shell = PaneShell::resolve(options, Some(session_name));
        let suppressed_raw_names =
            suppress_client_shell_environment(&mut resolved, suppressed_raw_names);
        set_environment_value(
            &mut resolved,
            "SHELL".to_owned(),
            shell.to_string_lossy().into_owned(),
        );

        if let Some(pane_id) = pane_id {
            let pane_env = format!("%{}", pane_id.as_u32());
            set_environment_value(&mut resolved, "RMUX_PANE".to_owned(), pane_env.clone());
            set_environment_value(&mut resolved, "TMUX_PANE".to_owned(), pane_env);
        }

        set_environment_value(
            &mut resolved,
            "PWD".to_owned(),
            cwd.to_string_lossy().into_owned(),
        );

        Ok(Self {
            cwd,
            requested_cwd: requested_cwd.is_some(),
            shell,
            pane_shell,
            raw_environment: raw_process_environment(
                raw_base_environment,
                &resolved,
                &suppressed_raw_names,
            )
            .into(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_run_shell(
        environment: &EnvironmentStore,
        options: &OptionStore,
        session_name: Option<&SessionName>,
        session_id: Option<u32>,
        socket_path: &Path,
        base_environment: Option<&SessionBaseEnvironment>,
        include_terminal_defaults: bool,
        pane_id: Option<PaneId>,
        requested_cwd: Option<&Path>,
    ) -> Result<Self, RmuxError> {
        let mut resolved = base_environment
            .map(SessionBaseEnvironment::environment_map)
            .unwrap_or_else(base_process_environment);
        let include_implicit_globals = base_environment.is_none();
        if base_environment.is_some() {
            environment
                .apply_to_process_environment_without_implicit_globals(session_name, &mut resolved);
        } else {
            environment.apply_to_process_environment(session_name, &mut resolved);
        }
        remove_environment_value(&mut resolved, "RMUX_PANE");
        remove_environment_value(&mut resolved, "TMUX_PANE");

        let mut suppressed = environment
            .suppressed_process_environment_names(session_name, include_implicit_globals);

        if include_terminal_defaults {
            if let Some(default_terminal) = session_name
                .and_then(|session_name| {
                    options.resolve(Some(session_name), OptionName::DefaultTerminal)
                })
                .or_else(|| options.resolve(None, OptionName::DefaultTerminal))
            {
                set_environment_value(
                    &mut resolved,
                    "TERM".to_owned(),
                    default_terminal.to_owned(),
                );
            }
            set_environment_value(&mut resolved, "TERM_PROGRAM".to_owned(), "rmux".to_owned());
            set_environment_value(
                &mut resolved,
                "TERM_PROGRAM_VERSION".to_owned(),
                env!("CARGO_PKG_VERSION").to_owned(),
            );
        } else {
            remove_environment_value(&mut resolved, "TERM_PROGRAM");
            remove_environment_value(&mut resolved, "TERM_PROGRAM_VERSION");
            suppress_terminal_program_environment(&mut suppressed);
        }

        let mux_session_id = session_id.map_or(0_i32, |id| i32::try_from(id).unwrap_or(i32::MAX));
        let mux_socket_path = mux_environment_socket_path(socket_path);
        let mux_env = format!(
            "{},{},{}",
            mux_socket_path.display(),
            std::process::id(),
            mux_session_id
        );
        set_environment_value(&mut resolved, "RMUX".to_owned(), mux_env.clone());
        set_environment_value(&mut resolved, "TMUX".to_owned(), mux_env);
        crate::tmux_shim::apply_tmux_shim_environment(&mut resolved, socket_path);
        if let Some(pane_id) = pane_id {
            let pane_env = format!("%{}", pane_id.as_u32());
            set_environment_value(&mut resolved, "RMUX_PANE".to_owned(), pane_env.clone());
            set_environment_value(&mut resolved, "TMUX_PANE".to_owned(), pane_env);
        }

        let cwd = resolve_working_directory(requested_cwd)?;
        let shell = resolve_shell_path(options, session_name, &resolved);
        #[cfg(windows)]
        {
            remove_environment_value(&mut resolved, CLIENT_SHELL_ENV);
        }
        set_environment_value(
            &mut resolved,
            "SHELL".to_owned(),
            shell.to_string_lossy().into_owned(),
        );
        set_environment_value(
            &mut resolved,
            "PWD".to_owned(),
            cwd.to_string_lossy().into_owned(),
        );

        suppressed.insert("RMUX_PANE".to_owned());
        suppressed.insert("TMUX_PANE".to_owned());
        #[cfg(windows)]
        suppressed.insert(CLIENT_SHELL_ENV.to_owned());
        Ok(Self {
            cwd,
            requested_cwd: requested_cwd.is_some(),
            shell,
            pane_shell: PaneShell::resolve(options, session_name),
            raw_environment: raw_process_environment(
                base_environment.map(SessionBaseEnvironment::raw_environment),
                &resolved,
                &suppressed,
            )
            .into(),
        })
    }

    pub(crate) fn raw_environment(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.raw_environment
            .iter()
            .map(|(name, value)| (name.as_os_str(), value.as_os_str()))
    }

    /// This profile's environment, as the shell variables a managed job is built with.
    ///
    /// The complete resolved environment, not a selected few names: `TERM`, `RMUX`, `TMUX`,
    /// `RMUX_PANE`, `TMUX_PANE` and every override and removal the option store decided are
    /// already folded into it, and handing the job a subset would leave a pane running with an
    /// environment its own client cannot explain.
    ///
    /// # Errors
    ///
    /// Fails when a name or value is not UTF-8.
    pub(crate) fn shell_environment(
        &self,
    ) -> Result<brush_core::env::ShellEnvironment, RmuxError> {
        shell_environment_from_pairs(self.raw_environment())
    }

    /// The seed-relative directory a job for this profile is opened over.
    ///
    /// An inherited directory outside the seed falls back to the seed root, which is the same
    /// answer [`ShellMux::default_dir`](marsh_core::shellmux::ShellMux::default_dir) gives and for
    /// the same reason: nobody asked for it, so there is nothing to refuse. A directory the caller
    /// *named* is refused instead — see [`requested_cwd`](Self::requested_cwd).
    ///
    /// # Errors
    ///
    /// Fails when the caller named a directory outside the seed and outside every snapshot of it.
    pub(crate) fn seed_relative_dir(
        &self,
        executor: &marsh_core::shellmux::ExecutorInfo,
    ) -> Result<String, RmuxError> {
        match seed_relative_path(executor, self.cwd()) {
            Ok(directory) => Ok(directory),
            Err(error) if self.requested_cwd => Err(error),
            Err(_) => Ok(String::new()),
        }
    }

    /// Marks this profile's directory as inherited rather than named by the caller.
    ///
    /// For one caller: a respawn. `plan_window_terminal` records the *resolved* directory in a
    /// pane's provenance, which for an unnamed pane is whatever the daemon's own process cwd was.
    /// Replaying that on respawn hands it back as `requested_cwd`, so a directory nobody ever
    /// asked for arrives looking like a request — and [`seed_relative_dir`](Self::seed_relative_dir)
    /// then refuses it. The visible symptom is a server started outside its own seed where
    /// `new-window` works and `respawn-pane` fails.
    ///
    /// A respawn whose directory *was* named keeps its named-ness and keeps failing loudly, which
    /// is what plan line 355's "a command-less respawn preserves its original mode" asks for.
    pub(crate) const fn inherit_cwd(mut self) -> Self {
        self.requested_cwd = false;
        self
    }

    pub(crate) fn with_source_depth(mut self, depth: usize) -> Self {
        let value = depth.to_string();
        set_raw_environment_value(
            Arc::make_mut(&mut self.raw_environment),
            OsString::from("RMUX_SOURCE_DEPTH"),
            OsString::from(value),
        );
        self
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub(crate) fn shell(&self) -> &Path {
        &self.shell
    }

    /// How this pane runs its work, for the caller that records it against a respawn.
    pub(crate) const fn pane_shell(&self) -> &PaneShell {
        &self.pane_shell
    }

    /// Restores the shell decision an existing pane was created with, for its respawn.
    ///
    /// tmux binds a pane to its original shell even when `default-shell` changes later, and this
    /// preserves that for both modes: a pane that was the embedded interpreter stays embedded
    /// after the option is pointed at `/bin/zsh`, and a pane launched into a configured shell
    /// keeps that executable — with `SHELL` realigned to it, so the pane's own environment does
    /// not contradict the program it is running.
    ///
    /// [`PaneShell::Embedded`] deliberately leaves the helper shell alone. There is no path to
    /// restore: the embedded interpreter is not an executable, and `SHELL` still has to name
    /// something real for `run-shell` and `$SHELL`-reading programs inside the pane.
    pub(crate) fn with_respawn_shell(mut self, shell: PaneShell) -> Self {
        if let PaneShell::External(path) = &shell {
            set_raw_environment_value(
                Arc::make_mut(&mut self.raw_environment),
                OsString::from("SHELL"),
                path.as_os_str().to_os_string(),
            );
            self.shell = path.clone();
        }
        self.pane_shell = shell;
        self
    }

    /// The managed line this pane starts, or `None` when it starts an idle prompt instead.
    ///
    /// The four combinations are genuinely four different compositions, and collapsing any pair
    /// would run something other than what was asked for:
    ///
    /// * **Embedded, no command** — no line at all. The pane is a prompt waiting for one, and
    ///   reserving a command for it would close the pane as soon as that command finished.
    /// * **Embedded, shell text** — the text verbatim. The embedded interpreter *is* the point;
    ///   wrapping it in an implicit `sh -c` would run it in a shell with none of marsh's builtins
    ///   and none of its instrumentation.
    /// * **External, no command** — the configured shell's interactive plan, including the login
    ///   `argv0` rewrite, so it reads the login files a user expects it to.
    /// * **External, shell text** — the configured shell's *command* plan, so that shell's own
    ///   dialect decides how the text is read rather than brush's.
    ///
    /// An argv workload is `exec -- <quoted…>` in both modes. It names a program directly, so
    /// there is no dialect to preserve and nothing for an intermediate shell to reinterpret.
    ///
    /// # Errors
    ///
    /// Fails for an empty argv, for a workload shape this daemon cannot compose, and when a
    /// configured shell's plan cannot be written as a managed command line.
    pub(crate) fn pane_workload_line(
        &self,
        command: Option<&ProcessCommand>,
    ) -> Result<Option<String>, RmuxError> {
        match (&self.pane_shell, command) {
            (PaneShell::Embedded, None) => Ok(None),
            (PaneShell::External(path), None) => {
                ShellSpec::new(path).interactive_line(&self.cwd).map(Some)
            }
            (PaneShell::External(path), Some(ProcessCommand::Shell(text))) => {
                ShellSpec::new(path).command_line(&self.cwd, text).map(Some)
            }
            (_, Some(command)) => crate::io::protocol::workload_line(command)
                .map(Some)
                .map_err(|error| RmuxError::spawn_failed(error.to_string())),
        }
    }

    pub(crate) fn resolved_default_shell(
        environment: &EnvironmentStore,
        options: &OptionStore,
        session_name: Option<&SessionName>,
    ) -> PathBuf {
        let mut resolved = base_process_environment();
        environment.apply_to_process_environment(session_name, &mut resolved);
        resolve_shell_path(options, session_name, &resolved)
    }

    pub(crate) fn attach_shell_command(&self, command: String) -> AttachShellCommand {
        AttachShellCommand::new(
            command,
            self.shell.to_string_lossy().into_owned(),
            self.cwd.to_string_lossy().into_owned(),
        )
    }

    pub(crate) fn environment_value(&self, name: &str) -> Option<&str> {
        self.raw_environment
            .iter()
            .find(|(candidate, _)| os_environment_name_eq(candidate, name))
            .and_then(|(_, value)| value.to_str())
    }

    #[cfg(test)]
    pub(crate) fn with_test_environment(mut self, environment: HashMap<String, String>) -> Self {
        for (name, value) in environment {
            set_raw_environment_value(
                Arc::make_mut(&mut self.raw_environment),
                OsString::from(name),
                OsString::from(value),
            );
        }
        self
    }

    pub(crate) fn default_window_name(&self) -> Option<String> {
        self.environment_value("TERM_PROGRAM")
            .filter(|value| *value == "rmux")
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| shell_program_name(&self.shell))
    }

    pub(crate) fn initial_pane_title(&self) -> Option<String> {
        let host = crate::host_name::local_hostname()?;
        Some(host.split('.').next().unwrap_or(&host).to_owned())
    }

    pub(crate) fn automatic_window_name(&self, command: Option<&ProcessCommand>) -> Option<String> {
        if command.is_some() {
            self.runtime_window_name(command)
        } else {
            self.default_window_name()
        }
    }

    pub(crate) fn runtime_window_name(&self, command: Option<&ProcessCommand>) -> Option<String> {
        match command {
            Some(ProcessCommand::Shell(command)) => {
                shell_command_window_name(command).or_else(|| shell_program_name(&self.shell))
            }
            Some(ProcessCommand::Argv(argv)) if !argv.is_empty() => executable_name(&argv[0]),
            None => shell_program_name(&self.shell),
            Some(ProcessCommand::Argv(_)) | Some(_) => shell_program_name(&self.shell),
        }
    }

}

#[cfg(windows)]
fn suppress_client_shell_environment(
    resolved: &mut HashMap<String, String>,
    mut suppressed_raw_names: HashSet<String>,
) -> HashSet<String> {
    remove_environment_value(resolved, CLIENT_SHELL_ENV);
    suppressed_raw_names.insert(CLIENT_SHELL_ENV.to_owned());
    suppressed_raw_names
}

#[cfg(not(windows))]
fn suppress_client_shell_environment(
    _resolved: &mut HashMap<String, String>,
    suppressed_raw_names: HashSet<String>,
) -> HashSet<String> {
    suppressed_raw_names
}

fn suppress_terminal_program_environment(suppressed_raw_names: &mut HashSet<String>) {
    suppressed_raw_names.insert("TERM_PROGRAM".to_owned());
    suppressed_raw_names.insert("TERM_PROGRAM_VERSION".to_owned());
}

pub(crate) fn base_process_environment() -> HashMap<String, String> {
    environment_from_os_pairs(std::env::vars_os())
}

pub(crate) fn base_process_environment_display_only() -> HashMap<String, String> {
    display_environment_from_os_pairs(std::env::vars_os())
}

fn raw_process_environment(
    raw_base_environment: Option<&[(OsString, OsString)]>,
    resolved: &HashMap<String, String>,
    suppressed_names: &HashSet<String>,
) -> Vec<(OsString, OsString)> {
    let mut raw = match raw_base_environment {
        Some(environment) => environment.to_vec(),
        None => std::env::vars_os().collect(),
    };
    raw.retain(|(name, _)| {
        !suppressed_names
            .iter()
            .any(|suppressed| os_environment_name_eq(name, suppressed))
            && !resolved
                .keys()
                .any(|resolved_name| os_environment_name_eq(name, resolved_name))
    });
    raw.extend(
        resolved
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value))),
    );
    raw
}

fn environment_from_os_pairs<I>(pairs: I) -> HashMap<String, String>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    pairs
        .into_iter()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

fn display_environment_from_os_pairs<I>(pairs: I) -> HashMap<String, String>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    pairs
        .into_iter()
        .filter_map(|(name, value)| {
            let name = name.into_string().ok()?;
            if value.clone().into_string().is_ok() {
                return None;
            }
            Some((name, display_os_environment_value(&value)))
        })
        .collect()
}

#[cfg(unix)]
fn display_os_environment_value(value: &OsStr) -> String {
    use std::os::unix::ffi::OsStrExt;

    value
        .as_bytes()
        .iter()
        .map(|byte| match *byte {
            b'\\' => "\\\\".to_owned(),
            0x20..=0x7e => char::from(*byte).to_string(),
            other => format!("\\{other:03o}"),
        })
        .collect()
}

#[cfg(windows)]
fn display_os_environment_value(value: &OsStr) -> String {
    value.to_string_lossy().into_owned()
}

#[cfg(windows)]
fn os_environment_name_eq(left: &OsStr, right: &str) -> bool {
    left.to_string_lossy().eq_ignore_ascii_case(right)
}

#[cfg(not(windows))]
fn os_environment_name_eq(left: &OsStr, right: &str) -> bool {
    left == OsStr::new(right)
}

fn shell_command_window_name(command: &str) -> Option<String> {
    let first = command.split_whitespace().next()?;
    executable_name(first)
}

pub(crate) fn validate_process_command(command: Option<&ProcessCommand>) -> Result<(), RmuxError> {
    let empty_argv = matches!(
        command,
        Some(ProcessCommand::Argv(argv)) if argv.is_empty() || argv.first().is_some_and(String::is_empty)
    );
    if empty_argv {
        return Err(RmuxError::empty_process_command());
    }
    #[cfg(windows)]
    if let Some(ProcessCommand::Argv(argv)) = command {
        if let Some(program) = argv.first() {
            reject_windows_batch_argv(Path::new(program))?;
        }
    }
    Ok(())
}

/// Rejects a workload this profile's configured shell cannot express, before a pane is created.
///
/// Composing the line is the check. A Windows `default-shell` reached through a verbatim
/// `cmd.exe` tail, a batch script named as an argv program, and a non-UTF-8 word all fail here
/// rather than after the layout has already been mutated for a pane that can never start.
///
/// # Errors
///
/// Fails for the reasons [`TerminalProfile::pane_workload_line`] fails.
#[cfg(windows)]
pub(crate) fn validate_windows_process_command_for_profile(
    profile: &TerminalProfile,
    command: Option<&ProcessCommand>,
) -> Result<(), RmuxError> {
    validate_process_command(command)?;
    let _ = profile.pane_workload_line(command)?;
    Ok(())
}

#[cfg(windows)]
fn reject_windows_batch_argv(program: &Path) -> Result<(), RmuxError> {
    if is_windows_batch_script(program) {
        return Err(RmuxError::spawn_failed(
            WINDOWS_BATCH_ARGV_UNSUPPORTED_MESSAGE,
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn is_windows_batch_script(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|extension| matches!(extension.to_ascii_lowercase().as_str(), "bat" | "cmd"))
        .unwrap_or(false)
}

pub(crate) fn parse_environment_assignments(
    values: &[String],
) -> Result<HashMap<String, String>, RmuxError> {
    let mut environment = HashMap::new();

    for value in values {
        #[cfg(windows)]
        if value.starts_with('=') {
            continue;
        }

        let Some((name, value)) = value.split_once('=') else {
            return Err(RmuxError::Server(format!(
                "environment assignment must be NAME=VALUE: {value}"
            )));
        };
        if name.is_empty() {
            return Err(RmuxError::Server(
                "environment assignment name must not be empty".to_owned(),
            ));
        }
        environment.insert(name.to_owned(), value.to_owned());
    }

    Ok(environment)
}

fn set_environment_value(environment: &mut HashMap<String, String>, name: String, value: String) {
    remove_environment_value(environment, &name);

    environment.insert(name, value);
}

fn remove_environment_value(environment: &mut HashMap<String, String>, name: &str) {
    #[cfg(windows)]
    if let Some(existing) = environment
        .keys()
        .find(|key| key.eq_ignore_ascii_case(name))
        .cloned()
    {
        environment.remove(&existing);
        return;
    }

    environment.remove(name);
}

fn set_raw_environment_value(
    environment: &mut Vec<(OsString, OsString)>,
    name: OsString,
    value: OsString,
) {
    let name_string = name.to_string_lossy().into_owned();
    environment.retain(|(existing, _)| !os_environment_name_eq(existing, &name_string));
    environment.push((name, value));
}

fn resolve_working_directory(requested_cwd: Option<&Path>) -> Result<PathBuf, RmuxError> {
    let requested = requested_cwd
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    for candidate in requested
        .into_iter()
        .chain(std::env::var_os("USERPROFILE").map(PathBuf::from))
        .chain(std::env::var_os("HOME").map(PathBuf::from))
        .chain(std::iter::once(default_working_directory()))
    {
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }

    Err(RmuxError::Server(
        "failed to resolve a working directory".to_owned(),
    ))
}

fn mux_environment_socket_path(socket_path: &Path) -> PathBuf {
    let absolute = if socket_path.is_absolute() {
        socket_path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(socket_path))
            .unwrap_or_else(|_| socket_path.to_path_buf())
    };
    canonical_path_or_parent(absolute)
}

fn canonical_path_or_parent(path: PathBuf) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(&path) {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(file_name)) => std::fs::canonicalize(parent)
            .map(|canonical_parent| canonical_parent.join(file_name))
            .unwrap_or(path),
        _ => path,
    }
}

fn default_working_directory() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from("/")
    }
    #[cfg(windows)]
    {
        PathBuf::from(r"C:\")
    }
}

fn shell_program_name(path: &Path) -> Option<String> {
    executable_name(path.as_os_str())
}

fn executable_name(path: impl AsRef<std::ffi::OsStr>) -> Option<String> {
    let name = Path::new(path.as_ref()).file_name()?.to_string_lossy();
    let trimmed = name.trim_start_matches('-');
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Builds the exported environment a managed job starts with, from raw OS pairs.
///
/// Every pair becomes an *exported* shell variable, because that is what an environment is: a
/// pane's `TERM`, `RMUX_PANE` and the rest are only visible to the programs the shell runs if
/// they are marked for export, and a pane whose `TERM` never reached its child would render
/// against the wrong terminfo.
///
/// # Errors
///
/// Fails when a name or value is not UTF-8. A shell variable is a `String`, so the alternative
/// would be a lossy conversion: the workload would silently receive a *different* value from the
/// one the client sent, which is worse than refusing to start it.
pub(crate) fn shell_environment_from_pairs<'a, I>(
    pairs: I,
) -> Result<brush_core::env::ShellEnvironment, RmuxError>
where
    I: IntoIterator<Item = (&'a OsStr, &'a OsStr)>,
{
    let mut environment = brush_core::env::ShellEnvironment::new();
    for (name, value) in pairs {
        let (Some(name), Some(value)) = (name.to_str(), value.to_str()) else {
            return Err(RmuxError::spawn_failed(
                "pane environment contains non-UTF-8 data",
            ));
        };
        let mut variable = brush_core::variables::ShellVariable::new(value);
        variable.export();
        environment.set_global(name, variable).map_err(|error| {
            RmuxError::spawn_failed(format!("pane environment variable {name} is unusable: {error}"))
        })?;
    }
    Ok(environment)
}

/// Rewrites a host path as the seed-relative directory a job is opened over.
///
/// Two prefixes are stripped, in this order, because both name the same logical place:
///
/// * the seed itself — the directory this daemon leased and every job publishes into;
/// * `<snapshot root>/<one component>` — a path *inside* some job's snapshot. The first component
///   under the snapshot root names that snapshot, and everything after it is the same relative
///   position in the seed. A client whose shell has already moved into a pane's snapshot would
///   otherwise hand over a path this daemon has no seed-relative name for.
///
/// # Errors
///
/// Fails when `path` is under neither. That is an ordinary spawn error and deliberately not a
/// silent jump to the seed root: a caller that asked for a directory outside this singleton's
/// seed asked for something this daemon cannot give it, and starting somewhere else instead would
/// run their command against the wrong tree.
pub(crate) fn seed_relative_path(
    executor: &marsh_core::shellmux::ExecutorInfo,
    path: &Path,
) -> Result<String, RmuxError> {
    if let Some(seed) = executor.seed.as_deref() {
        if let Ok(relative) = path.strip_prefix(seed) {
            return job_directory(path, relative);
        }
    }
    if let Some(root) = executor.snapshot_parent.as_deref() {
        if let Ok(relative) = path.strip_prefix(root) {
            let mut components = relative.components();
            if components.next().is_some() {
                let inside = components.as_path().to_path_buf();
                return job_directory(path, &inside);
            }
            return Ok(String::new());
        }
    }
    Err(RmuxError::spawn_failed(format!(
        "{} directory {} is outside this server's seed",
        rmux_proto::SPAWN_FAILED_MESSAGE_PREFIX,
        path.display()
    )))
}

/// Joins `relative`'s components with `/`, the separator a job directory is spelled with.
///
/// `original` is only for the diagnostic: a caller needs the path it actually asked for, not the
/// suffix that failed to convert.
fn job_directory(original: &Path, relative: &Path) -> Result<String, RmuxError> {
    let mut segments = Vec::new();
    for component in relative.components() {
        let Some(segment) = component.as_os_str().to_str() else {
            return Err(RmuxError::spawn_failed(format!(
                "{} directory {} is not UTF-8",
                rmux_proto::SPAWN_FAILED_MESSAGE_PREFIX,
                original.display()
            )));
        };
        segments.push(segment);
    }
    Ok(segments.join("/"))
}

#[cfg(test)]
#[path = "terminal/profile_env_tests.rs"]
mod profile_env_tests;
#[cfg(test)]
#[path = "terminal/tests.rs"]
mod tests;
