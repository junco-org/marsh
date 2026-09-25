use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmux_core::{EnvironmentStore, OptionStore, PaneId};
use rmux_proto::{AttachShellCommand, OptionName, ProcessCommand, RmuxError, SessionName};

mod shell_resolver;
mod shell_spec;

pub(crate) use shell_resolver::is_suitable_shell;
use shell_resolver::{configured_pane_shell, resolve_shell_path};
use shell_spec::ShellSpec;

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
    /// The directory a job for this profile starts in.
    ///
    /// A host path, authoritative whether it was named or inherited: the seed it lies in is
    /// discovered from it when the job is opened, so there is nothing left for this type to
    /// decide about it.
    cwd: PathBuf,
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
        Ok(Self {
            cwd,
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
    pub(crate) fn shell_environment(&self) -> Result<brush_core::env::ShellEnvironment, RmuxError> {
        shell_environment_from_pairs(self.raw_environment())
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
            (PaneShell::External(path), None) => ShellSpec::new(path).interactive_line().map(Some),
            (PaneShell::External(path), Some(ProcessCommand::Shell(text))) => {
                ShellSpec::new(path).command_line(text).map(Some)
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
    Ok(())
}

pub(crate) fn parse_environment_assignments(
    values: &[String],
) -> Result<HashMap<String, String>, RmuxError> {
    let mut environment = HashMap::new();

    for value in values {
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

/// The directory a profile starts its job in.
///
/// A caller who named one gets that one or an error: silently substituting `HOME` would run their
/// command against a tree they never asked for. Only a caller who named *nothing* gets the search
/// — the process directory, then the usual home candidates, then the OS root — because there is
/// no request to honour and refusing would leave a host unable to build a profile at all.
fn resolve_working_directory(requested_cwd: Option<&Path>) -> Result<PathBuf, RmuxError> {
    if let Some(requested) = requested_cwd {
        if requested.as_os_str().is_empty() || !requested.is_dir() {
            return Err(RmuxError::Server(format!(
                "{}: no such directory",
                requested.display()
            )));
        }
        return Ok(requested.to_path_buf());
    }
    for candidate in std::env::current_dir()
        .ok()
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
    PathBuf::from("/")
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
            RmuxError::spawn_failed(format!(
                "pane environment variable {name} is unusable: {error}"
            ))
        })?;
    }
    Ok(environment)
}

#[cfg(test)]
#[path = "terminal/tests.rs"]
mod tests;
