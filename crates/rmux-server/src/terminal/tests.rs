use super::environment_from_os_pairs;
use super::{parse_environment_assignments, validate_process_command, TerminalProfile};
use crate::test_fixtures::{option_store, unique_temp_path};
use rmux_core::{EnvironmentStore, OptionStore, PaneId};
use rmux_proto::{OptionName, ProcessCommand, ScopeSelector, SessionName, SetOptionMode};
use std::collections::HashMap;
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

const DEFAULT_SHELL: &str = "/bin/sh";
/// `default-shell` set globally to [`DEFAULT_SHELL`].
const SHELL_OPTION: (ScopeSelector, OptionName, &str, SetOptionMode) = (
    ScopeSelector::Global,
    OptionName::DefaultShell,
    DEFAULT_SHELL,
    SetOptionMode::Replace,
);
/// `default-terminal` set globally to `tmux-256color`.
const DEFAULT_TERMINAL: (ScopeSelector, OptionName, &str, SetOptionMode) = (
    ScopeSelector::Global,
    OptionName::DefaultTerminal,
    "tmux-256color",
    SetOptionMode::Replace,
);

/// The session every profile is spawned for.
fn alpha() -> SessionName {
    SessionName::new("alpha").expect("valid session name")
}

/// The arguments of one [`TerminalProfile::for_session`] call for session [`alpha`], without a
/// base environment. The default is session 7 on `/tmp/rmux.sock` with terminal defaults, only
/// `default-shell` set, and no spawn environment, overrides, pane or requested directory.
struct SessionSpawn<'a> {
    environment: EnvironmentStore,
    options: OptionStore,
    session_id: u32,
    socket_path: PathBuf,
    spawn_environment: Option<&'a HashMap<String, String>>,
    include_terminal_defaults: bool,
    overrides: Option<&'a [String]>,
    pane_id: Option<PaneId>,
    requested_cwd: Option<&'a Path>,
}

impl Default for SessionSpawn<'_> {
    fn default() -> Self {
        Self {
            environment: EnvironmentStore::new(),
            options: option_store([SHELL_OPTION]),
            session_id: 7,
            socket_path: PathBuf::from("/tmp/rmux.sock"),
            spawn_environment: None,
            include_terminal_defaults: true,
            overrides: None,
            pane_id: None,
            requested_cwd: None,
        }
    }
}

impl SessionSpawn<'_> {
    fn profile(&self) -> TerminalProfile {
        TerminalProfile::for_session(
            &self.environment,
            &self.options,
            &alpha(),
            self.session_id,
            &self.socket_path,
            None,
            self.spawn_environment,
            self.include_terminal_defaults,
            self.overrides,
            self.pane_id,
            self.requested_cwd,
        )
        .expect("profile")
    }
}

/// A `run-shell` profile over empty stores, for `target`'s session name and id when given.
fn run_shell_profile(
    target: Option<(&SessionName, u32)>,
    socket_path: &Path,
    requested_cwd: &Path,
) -> TerminalProfile {
    TerminalProfile::for_run_shell(
        &EnvironmentStore::new(),
        &OptionStore::new(),
        target.map(|(session_name, _)| session_name),
        target.map(|(_, session_id)| session_id),
        socket_path,
        None,
        false,
        None,
        Some(requested_cwd),
    )
    .expect("run-shell profile")
}

/// Asserts `profile` exports `expected` as both `RMUX` and `TMUX`.
fn assert_mux_env(profile: &TerminalProfile, expected: &str) {
    assert_eq!(profile.environment_value("RMUX"), Some(expected));
    assert_eq!(profile.environment_value("TMUX"), Some(expected));
}

#[test]
fn base_environment_snapshot_skips_non_utf8_pairs() {
    let environment = environment_from_os_pairs([
        (
            OsString::from_vec(b"INVALID_NAME_\xff".to_vec()),
            OsString::from("value"),
        ),
        (
            OsString::from("INVALID_VALUE"),
            OsString::from_vec(b"value_\xff".to_vec()),
        ),
        (OsString::from("VALID"), OsString::from("value")),
    ]);

    assert_eq!(environment.get("VALID").map(String::as_str), Some("value"));
    assert_eq!(environment.len(), 1);
}

#[test]
fn terminal_profile_sets_rmux_term_shell_and_pane_context() {
    let socket_path = temp_socket_path();
    let profile = SessionSpawn {
        options: option_store([DEFAULT_TERMINAL, SHELL_OPTION]),
        socket_path: socket_path.clone(),
        overrides: Some(&["FOO=bar".to_owned()]),
        pane_id: Some(PaneId::new(3)),
        requested_cwd: Some(std::env::temp_dir().as_path()),
        ..SessionSpawn::default()
    }
    .profile();
    assert_eq!(profile.environment_value("TERM"), Some("tmux-256color"));
    assert_eq!(profile.environment_value("TERM_PROGRAM"), Some("rmux"));
    assert_eq!(
        profile.environment_value("TERM_PROGRAM_VERSION"),
        Some(env!("CARGO_PKG_VERSION"))
    );
    let ambient_colorterm = std::env::var("COLORTERM").ok();
    assert_eq!(
        profile.environment_value("COLORTERM"),
        ambient_colorterm.as_deref()
    );
    assert_mux_env(&profile, &expected_mux_env(&socket_path, 7));
    assert_eq!(profile.environment_value("RMUX_PANE"), Some("%3"));
    assert_eq!(profile.environment_value("TMUX_PANE"), Some("%3"));
    assert_eq!(profile.environment_value("FOO"), Some("bar"));
    let expected_cwd = std::env::temp_dir();
    assert_eq!(profile.environment_value("SHELL"), Some(DEFAULT_SHELL));
    assert_eq!(
        profile.environment_value("PWD"),
        Some(expected_cwd.to_string_lossy().as_ref())
    );
    assert_eq!(profile.cwd(), expected_cwd.as_path());
}

#[test]
fn terminal_profile_applies_spawn_environment_before_explicit_overrides() {
    let spawn_environment = HashMap::from([
        ("PATH".to_owned(), "/client/bin:/usr/bin".to_owned()),
        ("RMUX_CLIENT_ONLY".to_owned(), "present".to_owned()),
    ]);

    let profile = SessionSpawn {
        socket_path: temp_socket_path(),
        spawn_environment: Some(&spawn_environment),
        overrides: Some(&["RMUX_CLIENT_ONLY=override".to_owned()]),
        ..SessionSpawn::default()
    }
    .profile();

    assert_eq!(
        profile.environment_value("PATH"),
        Some("/client/bin:/usr/bin")
    );
    assert_eq!(
        profile.environment_value("RMUX_CLIENT_ONLY"),
        Some("override")
    );
}

#[test]
fn terminal_profile_uses_client_shell_when_default_shell_is_unset() {
    let client_shell = std::env::current_exe().expect("test executable path");
    let client_shell_value = client_shell
        .to_str()
        .expect("test executable path is UTF-8")
        .to_owned();
    let spawn_environment = HashMap::from([
        ("SHELL".to_owned(), client_shell_value.clone()),
        ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
    ]);

    let profile = SessionSpawn {
        options: OptionStore::new(),
        socket_path: temp_socket_path(),
        spawn_environment: Some(&spawn_environment),
        ..SessionSpawn::default()
    }
    .profile();

    assert_eq!(profile.shell(), client_shell);
    assert_eq!(
        profile.environment_value("SHELL"),
        Some(client_shell_value.as_str())
    );
}

#[test]
fn terminal_profile_honors_explicit_color_environment_overrides() {
    let mut environment = EnvironmentStore::new();
    for (name, value) in [("NO_COLOR", "1"), ("COLORTERM", "truecolor")] {
        environment.set(
            ScopeSelector::Session(alpha()),
            name.to_owned(),
            value.to_owned(),
        );
    }

    let profile = SessionSpawn {
        environment,
        options: option_store([DEFAULT_TERMINAL]),
        socket_path: temp_socket_path(),
        overrides: Some(&["NODE_DISABLE_COLORS=1".to_owned(), "CLICOLOR=0".to_owned()]),
        pane_id: Some(PaneId::new(3)),
        requested_cwd: Some(std::env::temp_dir().as_path()),
        ..SessionSpawn::default()
    }
    .profile();

    assert_eq!(profile.environment_value("NO_COLOR"), Some("1"));
    assert_eq!(profile.environment_value("COLORTERM"), Some("truecolor"));
    assert_eq!(profile.environment_value("NODE_DISABLE_COLORS"), Some("1"));
    assert_eq!(profile.environment_value("CLICOLOR"), Some("0"));
}

#[test]
fn terminal_profile_applies_default_terminal_before_per_command_term_override() {
    let mut environment = EnvironmentStore::new();
    environment.set(
        ScopeSelector::Session(alpha()),
        "TERM".to_owned(),
        "screen-256color".to_owned(),
    );
    let spawn = SessionSpawn {
        environment,
        options: option_store([DEFAULT_TERMINAL]),
        session_id: 2,
        ..SessionSpawn::default()
    };

    let profile = spawn.profile();
    assert_eq!(profile.environment_value("TERM"), Some("tmux-256color"));

    let override_profile = SessionSpawn {
        overrides: Some(&["TERM=screen-256color".to_owned()]),
        ..spawn
    }
    .profile();
    assert_eq!(
        override_profile.environment_value("TERM"),
        Some("screen-256color")
    );
}

#[test]
fn run_shell_profile_exports_tmux_env_for_plugin_children() {
    let socket_path = temp_socket_path();
    let temp_dir = std::env::temp_dir();

    let detached_profile = run_shell_profile(None, &socket_path, &temp_dir);
    assert_mux_env(&detached_profile, &expected_mux_env(&socket_path, 0));

    let targeted_profile = run_shell_profile(Some((&alpha(), 7)), &socket_path, &temp_dir);
    assert_mux_env(&targeted_profile, &expected_mux_env(&socket_path, 7));
}

#[test]
fn run_shell_profile_exports_absolute_mux_env_for_relative_socket() -> Result<(), Box<dyn Error>> {
    let socket_path = PathBuf::from("relative-rmux.sock");
    let run_cwd = unique_temp_path("server-terminal-run-shell-relative-socket-cwd");
    fs::create_dir_all(&run_cwd)?;

    let profile = run_shell_profile(None, &socket_path, &run_cwd);

    assert_mux_env(&profile, &expected_mux_env(&socket_path, 0));
    assert_eq!(profile.cwd(), run_cwd.as_path());

    Ok(())
}

#[test]
fn terminal_profile_prefers_rmux_term_program_for_default_window_name() {
    let profile = SessionSpawn::default().profile();

    assert_eq!(profile.default_window_name().as_deref(), Some("rmux"));
}

#[test]
fn terminal_profile_initial_pane_title_uses_host_short() {
    let home = std::env::current_dir().expect("current dir");
    let home_text = home.to_string_lossy().into_owned();

    let profile = SessionSpawn {
        overrides: Some(&[
            "USER=alice".to_owned(),
            format!("HOME={home_text}"),
            "PWD=/ignored".to_owned(),
        ]),
        requested_cwd: Some(&home),
        ..SessionSpawn::default()
    }
    .profile();

    let title = profile.initial_pane_title().expect("initial title");
    let host = crate::host_name::local_hostname().expect("host name");
    assert_eq!(title, host.split('.').next().unwrap_or(&host));
}

#[test]
fn terminal_profile_falls_back_to_shell_name_without_term_program() {
    let profile = SessionSpawn {
        include_terminal_defaults: false,
        ..SessionSpawn::default()
    }
    .profile();

    let shell_name = default_shell_name();
    assert_eq!(profile.default_window_name().as_deref(), Some(&*shell_name));
}

#[test]
fn terminal_profile_ignores_non_rmux_term_program_for_default_window_name() {
    let profile = SessionSpawn {
        overrides: Some(&["TERM_PROGRAM=tmux".to_owned()]),
        ..SessionSpawn::default()
    }
    .profile();

    let shell_name = default_shell_name();
    assert_eq!(profile.default_window_name().as_deref(), Some(&*shell_name));
}

#[test]
fn terminal_profile_runtime_window_name_tracks_spawned_command_shape() {
    let profile = SessionSpawn::default().profile();

    let shell_name = default_shell_name();
    for (command, expected) in [
        (None, shell_name.as_str()),
        (
            Some(ProcessCommand::Shell("printf hi".to_owned())),
            "printf",
        ),
        (Some(ProcessCommand::Shell("exit 0".to_owned())), "exit"),
        (
            Some(ProcessCommand::Argv(vec![
                "/usr/bin/top".to_owned(),
                "-H".to_owned(),
            ])),
            "top",
        ),
    ] {
        assert_eq!(
            profile.runtime_window_name(command.as_ref()).as_deref(),
            Some(expected),
            "{command:?}"
        );
    }
    assert_eq!(profile.automatic_window_name(None).as_deref(), Some("rmux"));
    assert_eq!(
        profile
            .automatic_window_name(Some(&ProcessCommand::Shell("sleep 30".to_owned())))
            .as_deref(),
        Some("sleep")
    );
}

#[test]
fn explicit_empty_process_commands_are_rejected() {
    for command in [
        ProcessCommand::Argv(Vec::new()),
        ProcessCommand::Argv(vec![String::new()]),
    ] {
        let error = validate_process_command(Some(&command))
            .expect_err("explicit empty process commands must be rejected");
        assert!(
            error
                .to_string()
                .contains("process command must not be empty"),
            "unexpected validation error: {error}"
        );
    }
}

#[test]
fn unix_structured_argv_does_not_treat_file_suffixes_specially() {
    for extension in ["cmd", "bat"] {
        validate_process_command(Some(&ProcessCommand::Argv(vec![format!(
            "/tmp/native-name.{extension}"
        )])))
        .expect("Unix argv targets remain direct regardless of their file suffix");
    }
}

#[test]
fn empty_shell_process_command_is_allowed_for_empty_tmux_panes() {
    validate_process_command(Some(&ProcessCommand::Shell(String::new())))
        .expect("empty shell command creates a tmux-style empty pane");
}

/// The shell resolved for session [`alpha`] whose `default-shell` is `default_shell`, with
/// `SHELL=/bin/sh` in the spawn environment.
fn resolved_shell(default_shell: &str) -> PathBuf {
    let session_name = alpha();
    let environment = HashMap::from([("SHELL".to_owned(), "/bin/sh".to_owned())]);
    let options = option_store([(
        ScopeSelector::Session(session_name.clone()),
        OptionName::DefaultShell,
        default_shell,
        SetOptionMode::Replace,
    )]);

    super::resolve_shell_path(&options, Some(&session_name), &environment)
}

#[test]
fn resolve_shell_path_prefers_explicit_default_shell_option_before_shell_env_fallback() {
    assert_eq!(
        resolved_shell("/bin/bash"),
        super::shell_resolver::normalize_shell_path(PathBuf::from("/bin/bash"))
    );
}

#[test]
fn resolve_shell_path_uses_shell_env_when_default_shell_is_explicitly_empty() {
    assert_eq!(
        resolved_shell(""),
        super::shell_resolver::normalize_shell_path(PathBuf::from("/bin/sh"))
    );
}

#[test]
fn parse_environment_assignments_rejects_missing_equals() {
    let error = parse_environment_assignments(&["INVALID".to_owned()])
        .expect_err("invalid environment assignment");
    assert_eq!(
        error,
        rmux_proto::RmuxError::Server(
            "environment assignment must be NAME=VALUE: INVALID".to_owned()
        )
    );
}

fn temp_socket_path() -> PathBuf {
    std::env::temp_dir().join("rmux.sock")
}

fn expected_mux_env(socket_path: &Path, index: u32) -> String {
    let absolute = if socket_path.is_absolute() {
        socket_path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(socket_path))
            .unwrap_or_else(|_| socket_path.to_path_buf())
    };
    let socket_path = if let Ok(canonical) = fs::canonicalize(&absolute) {
        canonical
    } else {
        match (absolute.parent(), absolute.file_name()) {
            (Some(parent), Some(file_name)) => fs::canonicalize(parent)
                .map(|canonical_parent| canonical_parent.join(file_name))
                .unwrap_or(absolute),
            _ => absolute,
        }
    };
    format!("{},{},{}", socket_path.display(), std::process::id(), index)
}

fn default_shell_name() -> String {
    Path::new(DEFAULT_SHELL)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.trim_start_matches('-').to_owned())
        .filter(|name| !name.is_empty())
        .expect("test default shell has a file name")
}

/// A job's public logical cwd opens another shell on the same source and directory.
#[test]
fn a_snapshot_directory_opens_a_shell_on_its_original_seed() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let root = scratch.path().canonicalize().expect("canonical scratch");
    let seed = root.join("seed");
    fs::create_dir_all(seed.join("src")).expect("seed tree");
    let filesystem = std::sync::Arc::new(marsh_btrfs::fake::CopyTree::new());
    filesystem.register(&seed);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime for the engine");
    let (pane_seed, mapped_seed) = runtime.block_on(async {
        // The receiver is held until teardown rather than drained: the command below produces no
        // output, so nothing is waiting on an output receipt, but dropping the queue's consumer
        // end early would be a different engine from the one production builds.
        let (io, _events) = crate::io::ShellIo::new(
            &seed,
            brush_core::env::ShellEnvironment::new(),
            marsh_core::shellmux::TerminalGeometry { rows: 24, cols: 80 },
            tokio::runtime::Handle::current(),
            root.join("rmux.sock"),
            // A managed job, so its snapshot is one the daemon actually took.
            |mut profile, frontend| {
                profile.sandbox_policy = marsh_core::SandboxPolicy::allow();
                marsh_core::test_support::mux(profile, frontend, filesystem)
            },
        )
        .expect("open the test engine");
        // A real job, so the snapshot the daemon has to recognize is one it actually took.
        let pane = io
            .open_shell(
                &seed.join("src"),
                Some(marsh_core::shellmux::ShellId::from("pane")),
                marsh_core::shellmux::SpawnOptions::default(),
            )
            .await
            .expect("open a shell");
        let logical_directory = io
            .jobs()
            .into_iter()
            .find(|view| view.id == *pane.id())
            .map(|view| view.working_directory)
            .expect("logical working directory");
        assert_eq!(logical_directory, seed.join("src"));

        let mapped = io
            .open_shell(
                &logical_directory,
                Some(marsh_core::shellmux::ShellId::from("mapped")),
                marsh_core::shellmux::SpawnOptions::default(),
            )
            .await
            .expect("open a shell from inside the pane's snapshot");
        // The completed verdict: `Ok` is an approved publication and nothing else.
        let completion = mapped
            .run_command(
                "printf mapped > mapped.txt",
                marsh_core::shellmux::CommandOptions::default(),
            )
            .await
            .expect("the mapped shell publishes into the original seed");
        assert!(completion.is_published(), "{:?}", completion);

        let answer = (pane.sandbox().seed.clone(), mapped.sandbox().seed.clone());
        io.shutdown().await.expect("shut the engine down");
        answer
    });

    assert_eq!(pane_seed, seed);
    assert_eq!(
        mapped_seed, seed,
        "a path inside a job's snapshot names the seed that snapshot was taken from"
    );
    assert_eq!(
        fs::read(seed.join("src/mapped.txt")).expect("the published file"),
        b"mapped",
        "the write landed in the original seed, under the directory the snapshot stood for"
    );
}
