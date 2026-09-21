//! Managed terminal IPC for the CLI workloads that used to run as local processes.
//!
//! Two invocation families — `rmux -c <shell-command>` and `rmux claude` — historically
//! resolved a host shell (or `claude` itself) and `exec`'d it in the invoking process. Neither
//! ever reached a policy gate, which made them the two ungated workload entrypoints in an
//! otherwise gated multiplexer. Both now run as an ordinary one-shot `ShellMux` pane on the
//! **existing** daemon, created in a private session this invocation owns and destroys.
//!
//! # These modes have PTY semantics, not Unix-pipe semantics
//!
//! A managed pane is a terminal. That is a deliberate compatibility choice and it is visible:
//!
//! * stdout and stderr are **merged** — the workload can still redirect them apart inside the
//!   pane, but the relay below sees one stream and cannot separate them;
//! * input is **terminal input**, so there is no half-close: [`ManagedPaneDisplay::Relay`]
//!   translates stdin EOF into terminal EOF keys, which a canonical-mode reader observes as
//!   end-of-input and a raw-mode application does not;
//! * the workload sees a TTY on all three descriptors even when this process' stdin is a pipe.
//!
//! Callers that need real separate pipes and a real half-close use the native `ShellIo::execute`
//! API instead. Nothing here claims pipe-transparent CLI compatibility.
//!
//! # Why the workload cannot start before the controller is ready
//!
//! The pane is created before this process can subscribe to its output, so a short workload
//! could finish — and its bytes age out — before anyone is listening. The controller therefore
//! locks a nonce-derived `wait-for` channel *before* creating the pane, and the pane's command
//! line acquires and releases that same channel before it runs anything else. While the pane is
//! parked on that lock the controller sets pane-local `remain-on-exit`, resolves the pane's
//! stable `%id` and arms output/exit observation; unlocking the channel is what starts the
//! workload. These are ordinary `wait-for` requests, not a private wire extension.
//!
//! # Exit status is the gated status
//!
//! The reported exit code is the pane's process status as the daemon retained it, which for a
//! marsh pane is the *gated* status: a command that exited zero but whose staged writes were
//! refused fails here too. That is the whole point of routing these modes through the mux.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rmux_client::{connect, AutoStartConfig, Connection};
use rmux_proto::request::{DetachClientExtRequest, NewSessionExtRequest, SendKeysExtRequest};
use rmux_proto::{
    KillSessionRequest, OptionScopeSelector, PaneTarget, PaneTargetRef, ProcessCommand, Response,
    SessionName, SetOptionMode, WaitForMode, WindowTarget,
};

use super::automation::{
    pane_process_state, stable_pane_ref_for_slot, PaneExitStatus, PaneProcessState,
};
use super::client_commands::run_attach_session;
use super::startup::StartupEndpoint;
use super::{
    connect_with_startserver, current_terminal_size, expect_command_output,
    expect_command_success, infer_client_utf8_from_env, resolve_pane_target_spec, ExitFailure,
    StartupOptions,
};
use crate::cli_args::{parse_target_spec, AttachSessionArgs};
use crate::client_terminal::client_terminal_context_from_parts;

/// Prefix of every session this module owns.
///
/// Deliberately distinct from the `rmux-claude` / `claude-swarm` names Claude Code's tmux
/// teammate mode knows about: those stay logical names that [`super::claude_namespace`]
/// rewrites onto the owned pair below, so two concurrent invocations sharing one daemon never
/// collide on a session name.
const OWNED_SESSION_PREFIX: &str = "marsh-io-";

/// Suffix of the companion session a Claude invocation's teammates appear in.
const SWARM_SESSION_SUFFIX: &str = "-swarm";

/// How long the teammate monitor waits for the companion session to appear.
const VIEWER_WAIT_TIMEOUT: Duration = Duration::from_mins(10);

/// How often the teammate monitor and the attach exit observer re-check the daemon.
const VIEWER_POLL_INTERVAL: Duration = Duration::from_millis(300);

/// How often the relay re-checks pane output and pane liveness.
const RELAY_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Largest stdin chunk the relay forwards in one `send-keys` frame.
const RELAY_STDIN_CHUNK: usize = 4096;

/// How long a pane may be reported dead without a retained exit status before the relay gives up.
///
/// The daemon marks a pane dead and records how it died in that order, so a fast command can be
/// observed dead with nothing to report yet. This is the window for that gap to close; past it
/// the missing status is reported as missing rather than guessed at.
const EXIT_STATUS_GRACE: Duration = Duration::from_secs(2);

/// Command name used in this module's daemon-error messages.
const COMMAND_NAME: &str = "rmux";

/// Resolved absolute endpoint of the daemon carrying this invocation's sessions.
pub(super) const CLAUDE_ENDPOINT_ENV: &str = "RMUX_INTERNAL_CLAUDE_ENDPOINT";

/// Owned session name that stands in for Claude's logical `rmux-claude` session.
pub(super) const CLAUDE_MAIN_SESSION_ENV: &str = "RMUX_INTERNAL_CLAUDE_MAIN_SESSION";

/// Owned session name that stands in for Claude's logical `claude-swarm` session.
pub(super) const CLAUDE_SWARM_SESSION_ENV: &str = "RMUX_INTERNAL_CLAUDE_SWARM_SESSION";

/// How the controlling invocation presents the managed pane to its user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ManagedPaneDisplay {
    /// Attach this process' real terminal to the owned session.
    ///
    /// Chosen when stdin is a terminal, so the workload gets the interactive rendering, key
    /// handling and resize behaviour of a normal rmux client.
    Attach,
    /// Proxy bytes between this process' stdin/stdout and the pane.
    ///
    /// Chosen when stdin is redirected. No terminal is entered and no emulator is run: stdin
    /// bytes become pane input and pane output bytes are written to stdout undecoded.
    Relay,
}

/// Which workload family a managed pane carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ManagedPaneKind {
    /// A plain one-shot command with no companion session.
    Command,
    /// A Claude Code launch, which also owns a companion teammate session and its monitor.
    Claude,
}

/// One request to run a process as a managed, app-owned pane.
#[derive(Debug)]
pub(super) struct ManagedPaneCommand {
    /// The workload itself, exactly as the caller expressed it.
    pub(super) process: ProcessCommand,
    /// The directory the pane starts in. An unusable directory is an error, never a fallback.
    pub(super) directory: PathBuf,
    /// Extra environment for the workload. A `None` value means "unset", which the pane-spawn
    /// wire cannot express and which is therefore rejected rather than silently dropped.
    pub(super) environment: Vec<(String, Option<String>)>,
    /// How this invocation presents the pane.
    pub(super) display: ManagedPaneDisplay,
    /// Which workload family this is.
    pub(super) kind: ManagedPaneKind,
}

/// The session and channel names one invocation owns.
#[derive(Debug, Clone)]
struct ManagedPaneIdentity {
    /// The session carrying the workload pane.
    main: SessionName,
    /// The companion session teammates appear in, for [`ManagedPaneKind::Claude`] only.
    swarm: Option<SessionName>,
    /// The `wait-for` channel the start barrier uses.
    channel: String,
}

impl ManagedPaneIdentity {
    /// Allocates a fresh identity from OS randomness.
    ///
    /// The nonce is the same 128-bit `/dev/urandom` value the Claude runtime directory has
    /// always used, so two invocations cannot collide on the shared daemon even when they start
    /// in the same millisecond.
    fn allocate(kind: ManagedPaneKind) -> Result<Self, ExitFailure> {
        let nonce = random_hex_128()?;
        let main = owned_session_name(&format!("{OWNED_SESSION_PREFIX}{nonce}"))?;
        let swarm = match kind {
            ManagedPaneKind::Command => None,
            ManagedPaneKind::Claude => Some(owned_session_name(&format!(
                "{OWNED_SESSION_PREFIX}{nonce}{SWARM_SESSION_SUFFIX}"
            ))?),
        };
        Ok(Self {
            main,
            swarm,
            channel: format!("{OWNED_SESSION_PREFIX}{nonce}-start"),
        })
    }

    /// Returns every session this invocation owns, companion first.
    fn owned_sessions(&self) -> impl Iterator<Item = &SessionName> {
        self.swarm.iter().chain(std::iter::once(&self.main))
    }
}

/// Reads 128 bits of OS randomness as lowercase hex.
fn random_hex_128() -> Result<String, ExitFailure> {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut bytes = [0u8; 16];
    // The OS generator through `getrandom`, not `/dev/urandom` directly. Opening that path is a
    // unix assumption in code that has no other one: every managed `rmux -c` and every Claude
    // invocation allocates a nonce here, so a hard-coded device node would make both fail outright
    // on Windows. `getrandom` reaches the same entropy source on unix and `BCryptGenRandom` on
    // Windows, and needs no descriptor, so it also works where the process has exhausted its file
    // handles.
    getrandom::fill(&mut bytes).map_err(|error| {
        ExitFailure::new(1, format!("rmux: failed to read OS randomness: {error}"))
    })?;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(output)
}

/// Validates a generated owned-session name.
fn owned_session_name(value: &str) -> Result<SessionName, ExitFailure> {
    SessionName::new(value.to_owned()).map_err(|error| {
        ExitFailure::new(1, format!("rmux: invalid managed session name: {error}"))
    })
}

/// Runs `request` as a managed pane on the daemon `socket_path` selects.
///
/// Resolves (or auto-starts, when `startup` permits it) the one daemon this invocation will
/// use, creates its own session there, runs the workload as that session's single one-shot
/// pane, and disposes of the session before returning. The shared daemon, its seed lease and
/// every other client survive success, failure and cancellation alike; nothing here ever falls
/// back to spawning the workload as a local process.
pub(super) fn run_managed_pane_command(
    request: ManagedPaneCommand,
    socket_path: &Path,
    startup: StartupOptions,
) -> Result<i32, ExitFailure> {
    let ManagedPaneCommand {
        process,
        directory,
        environment,
        display,
        kind,
    } = request;
    let identity = ManagedPaneIdentity::allocate(kind)?;
    let directory = usable_directory(&directory)?;

    // Clone the shared endpoint before the connection consumes `startup`: auto-start may move
    // the daemon to a different socket, and the workload must be told the one actually serving.
    let endpoint = startup.endpoint.clone();
    let mut connection = connect_with_startserver(socket_path, startup)?;
    let endpoint = absolute_endpoint(&endpoint.socket_path());

    let spawn = OwnedPaneSpawn {
        directory,
        environment: workload_environment(environment, &identity, &endpoint)?,
        command: barrier_command(&endpoint, &identity.channel, &process)?,
        display,
        kind,
    };

    let outcome = run_owned_pane(&mut connection, &identity, &endpoint, spawn);
    dispose(&mut connection, &identity);
    outcome
}

/// Everything the owned session needs at creation time.
struct OwnedPaneSpawn {
    /// Validated start directory.
    directory: String,
    /// Complete `NAME=VALUE` spawn environment.
    environment: Vec<String>,
    /// Barrier-wrapped shell command line.
    command: String,
    /// How the invocation presents the pane.
    display: ManagedPaneDisplay,
    /// Which workload family this is.
    kind: ManagedPaneKind,
}

/// Creates the owned session, arms observation behind the barrier, then runs the display.
fn run_owned_pane(
    connection: &mut Connection,
    identity: &ManagedPaneIdentity,
    endpoint: &Path,
    spawn: OwnedPaneSpawn,
) -> Result<i32, ExitFailure> {
    let OwnedPaneSpawn {
        directory,
        environment,
        command,
        display,
        kind,
    } = spawn;

    // Hold the barrier before the pane exists. Everything up to the matching unlock happens
    // while the workload is parked, so no output can be produced before anyone observes it.
    wait_for(connection, &identity.channel, WaitForMode::Lock)?;
    let pane = match arm_owned_pane(connection, identity, directory, environment, command) {
        Ok(pane) => pane,
        Err(error) => {
            // Release the server-local lock even on failure; it outlives this process otherwise.
            let _ = wait_for(connection, &identity.channel, WaitForMode::Unlock);
            return Err(error);
        }
    };

    // The main pane exists now, which is the point upstream's pid file used to mark.
    let monitor = match (kind, identity.swarm.as_ref()) {
        (ManagedPaneKind::Claude, Some(swarm)) => Some(TeammateMonitor::start(
            endpoint,
            identity.main.clone(),
            swarm.clone(),
        )),
        _ => None,
    };

    let result = release_and_display(connection, identity, endpoint, &pane, display);
    if let Some(monitor) = monitor {
        monitor.stop();
    }
    result
}

/// Creates the session and prepares the pane while the workload is still parked.
///
/// Returns the pane's stable identity: every later operation addresses that `%id` rather than a
/// slot index, so a layout change cannot redirect input or observation at another pane.
fn arm_owned_pane(
    connection: &mut Connection,
    identity: &ManagedPaneIdentity,
    directory: String,
    environment: Vec<String>,
    command: String,
) -> Result<PaneTargetRef, ExitFailure> {
    let response = connection
        .new_session_extended(NewSessionExtRequest {
            session_name: Some(identity.main.clone()),
            working_directory: Some(directory),
            detached: true,
            size: current_terminal_size(),
            environment: Some(environment),
            group_target: None,
            attach_if_exists: false,
            detach_other_clients: false,
            kill_other_clients: false,
            flags: None,
            window_name: None,
            print_session_info: false,
            print_format: None,
            command: None,
            process_command: Some(ProcessCommand::Shell(command)),
            // The caller's environment is already in `environment` above; a second update from
            // the invoking client would only re-apply the same values.
            client_environment: None,
            skip_environment_update: true,
        })
        .map_err(ExitFailure::from)?;
    expect_command_success(response, "new-session")?;

    let slot = PaneTarget::with_window(identity.main.clone(), 0, 0);
    let pane = stable_pane_ref_for_slot(connection, &slot, "new-session")?;

    // Retain the pane after the workload exits: its exit status is this invocation's exit
    // status, and a pane that vanished on exit would take that status with it.
    set_remain_on_exit(connection, &slot)?;

    Ok(pane)
}

/// Releases the barrier and runs the selected display to completion.
fn release_and_display(
    connection: &mut Connection,
    identity: &ManagedPaneIdentity,
    endpoint: &Path,
    pane: &PaneTargetRef,
    display: ManagedPaneDisplay,
) -> Result<i32, ExitFailure> {
    match display {
        ManagedPaneDisplay::Relay => {
            // The SDK stream is armed before the barrier is released, so nothing the workload
            // writes can predate the subscription. It is built and dropped inside one runtime
            // because dropping it emits an unsubscribe request.
            let pane_id = stable_pane_id(pane)?;
            let runtime = relay_runtime()?;
            let armed = runtime.block_on(arm_output(endpoint, identity.main.clone(), pane_id))?;
            // The exit watcher is armed with the stream and before the unlock, for the same
            // reason: a short workload can be gone before the first poll.
            let watcher = PaneExitWatcher::start(endpoint, pane.clone());
            wait_for(connection, &identity.channel, WaitForMode::Unlock)?;
            start_stdin_forwarder(endpoint, pane_id);
            let pumped = runtime.block_on(pump_output(armed, &watcher));
            watcher.stop();
            PaneExitStatus::resolved_exit_code(pumped?, COMMAND_NAME)
        }
        ManagedPaneDisplay::Attach => {
            let observer =
                AttachExitObserver::start(endpoint, identity.main.clone(), pane.clone());
            wait_for(connection, &identity.channel, WaitForMode::Unlock)?;
            let attached = attach(endpoint, &identity.main);
            observer.stop();
            let attached = attached?;
            // The same settle window the relay watcher uses: the pane can be reported dead a
            // moment before its status is retained.
            match settled_exit_state(connection, pane)? {
                Some(status) => PaneExitStatus::resolved_exit_code(Some(status), COMMAND_NAME),
                // The user detached while the workload was still running. Its status is not
                // available and inventing one would be a lie, so report the client's own.
                None => Ok(attached),
            }
        }
    }
}

/// Reads the pane's exit status once it has settled, or `None` while it is still running.
///
/// Returns as soon as the reading is conclusive; a pane already reported dead but without a
/// retained status is re-read for [`EXIT_STATUS_GRACE`] before the missing status is accepted
/// as missing. A pane that is simply still alive returns immediately.
fn settled_exit_state(
    connection: &mut Connection,
    pane: &PaneTargetRef,
) -> Result<Option<PaneExitStatus>, ExitFailure> {
    let deadline = Instant::now() + EXIT_STATUS_GRACE;
    loop {
        match pane_process_state(connection, pane)? {
            PaneProcessState::Alive => return Ok(None),
            PaneProcessState::Exited(status) => {
                if status.is_conclusive() || Instant::now() >= deadline {
                    return Ok(Some(status));
                }
            }
        }
        thread::sleep(RELAY_POLL_INTERVAL);
    }
}

/// Disposes of exactly this invocation's sessions.
///
/// Never touches the daemon, another Claude invocation's sessions, or a window some unrelated
/// live alias still owns: `kill-session` removes this session's own links only, and a window
/// linked elsewhere survives on its remaining alias.
fn dispose(connection: &mut Connection, identity: &ManagedPaneIdentity) {
    for session in identity.owned_sessions() {
        let _ = connection.kill_session(KillSessionRequest {
            target: session.clone(),
            kill_all_except_target: false,
            clear_alerts: false,
            kill_group: false,
        });
    }
}

/// Sends one `wait-for` operation and requires it to succeed.
fn wait_for(
    connection: &mut Connection,
    channel: &str,
    mode: WaitForMode,
) -> Result<(), ExitFailure> {
    let response = connection
        .wait_for(channel.to_owned(), mode)
        .map_err(ExitFailure::from)?;
    expect_command_success(response, "wait-for")
}

/// Sets pane-local `remain-on-exit` on the workload pane.
fn set_remain_on_exit(connection: &mut Connection, slot: &PaneTarget) -> Result<(), ExitFailure> {
    let response = connection
        .set_option_by_name(
            OptionScopeSelector::Pane(slot.clone()),
            "remain-on-exit".to_owned(),
            Some("on".to_owned()),
            SetOptionMode::Replace,
            false,
            false,
            false,
        )
        .map_err(ExitFailure::from)?;
    expect_command_success(response, "set-option")
}

/// Validates the requested start directory.
///
/// An unusable directory is an error: starting somewhere else would run the workload in a
/// directory the caller did not ask for, and falling back to a local process would run it
/// outside the gate entirely.
fn usable_directory(directory: &Path) -> Result<String, ExitFailure> {
    if !directory.is_dir() {
        return Err(ExitFailure::new(
            1,
            format!(
                "rmux: cannot start a managed command in '{}': not a usable directory",
                directory.display()
            ),
        ));
    }
    directory
        .to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| ExitFailure::new(1, "rmux: managed command directory is not valid UTF-8"))
}

/// Resolves the daemon endpoint to an absolute path.
///
/// The workload is told this path through `RMUX_INTERNAL_CLAUDE_ENDPOINT` and runs with its own
/// working directory, so a relative endpoint would not resolve the same way for it.
fn absolute_endpoint(socket_path: &Path) -> PathBuf {
    std::path::absolute(socket_path).unwrap_or_else(|_| socket_path.to_path_buf())
}

/// Builds the complete `NAME=VALUE` spawn environment for the workload.
///
/// The caller's environment is carried explicitly rather than inherited from the daemon: these
/// modes replace a local `exec`, and a command that used to see the invoking shell's variables
/// must still see them.
fn workload_environment(
    overrides: Vec<(String, Option<String>)>,
    identity: &ManagedPaneIdentity,
    endpoint: &Path,
) -> Result<Vec<String>, ExitFailure> {
    let mut environment = caller_environment()?;
    for (name, value) in overrides {
        let value = value.as_deref().ok_or_else(|| {
            ExitFailure::new(
                1,
                format!(
                    "rmux: cannot unset '{name}' for a managed command: the pane spawn \
                     environment can only assign values"
                ),
            )
        })?;
        environment.push(format!("{name}={value}"));
    }
    if let Some(swarm) = identity.swarm.as_ref() {
        let endpoint = endpoint
            .to_str()
            .ok_or_else(|| ExitFailure::new(1, "rmux: daemon socket path is not valid UTF-8"))?;
        environment.push(format!("{CLAUDE_ENDPOINT_ENV}={endpoint}"));
        environment.push(format!("{CLAUDE_MAIN_SESSION_ENV}={}", identity.main));
        environment.push(format!("{CLAUDE_SWARM_SESSION_ENV}={swarm}"));
    }
    Ok(environment)
}

/// Collects the invoking process' environment as `NAME=VALUE` assignments.
///
/// A non-UTF-8 name or value is rejected rather than lossily transcoded, matching how the
/// multiplexer core refuses a non-UTF-8 pane environment.
fn caller_environment() -> Result<Vec<String>, ExitFailure> {
    let mut environment = Vec::new();
    for (name, value) in std::env::vars_os() {
        environment.push(format!("{}={}", os_text(&name)?, os_text(&value)?));
    }
    Ok(environment)
}

/// Requires an OS string to be UTF-8.
fn os_text(value: &OsStr) -> Result<&str, ExitFailure> {
    value.to_str().ok_or_else(|| {
        ExitFailure::new(
            1,
            format!(
                "rmux: managed command carries non-UTF-8 data: {}",
                value.to_string_lossy()
            ),
        )
    })
}

/// Wraps the workload in the start barrier.
///
/// The pane first acquires the controller-held channel, then releases it, then runs the
/// workload — three commands joined with `&&`, so a workload that never gets its turn never
/// runs. The workload is quoted as a single unit for the same reason: a user `||` inside the
/// command text must not be able to bind to a failed barrier and run anyway.
fn barrier_command(
    endpoint: &Path,
    channel: &str,
    process: &ProcessCommand,
) -> Result<String, ExitFailure> {
    let binary = std::env::current_exe().map_err(|error| {
        ExitFailure::new(
            1,
            format!("rmux: failed to resolve the rmux binary: {error}"),
        )
    })?;
    let binary = shell_quote(binary.as_os_str())?;
    let endpoint = shell_quote(endpoint.as_os_str())?;
    let channel = shell_quote(OsStr::new(channel))?;

    let mut command = String::new();
    write!(
        command,
        "{binary} -S {endpoint} wait-for -L {channel} && \
         {binary} -S {endpoint} wait-for -U {channel} && "
    )
    .map_err(|error| ExitFailure::new(1, format!("rmux: failed to build pane command: {error}")))?;
    command.push_str(&workload_command(process)?);
    Ok(command)
}

/// Renders one workload as a single shell command.
fn workload_command(process: &ProcessCommand) -> Result<String, ExitFailure> {
    match process {
        // `eval` keeps the caller's shell text one command: its own operators still apply
        // inside it, but none of them can reach across the `&&` chain above.
        ProcessCommand::Shell(text) => Ok(format!("eval {}", shell_quote(OsStr::new(text))?)),
        // Quoting every element preserves argv boundaries exactly through the shell's
        // re-splitting, so a program name containing spaces stays one argument.
        ProcessCommand::Argv(argv) => {
            let mut command = String::new();
            for argument in argv {
                if !command.is_empty() {
                    command.push(' ');
                }
                command.push_str(&shell_quote(OsStr::new(argument))?);
            }
            if command.is_empty() {
                return Err(ExitFailure::new(1, "rmux: managed command is empty"));
            }
            Ok(command)
        }
        other => Err(ExitFailure::new(
            1,
            format!("rmux: unsupported managed command form: {other:?}"),
        )),
    }
}

/// Quotes one argument for POSIX shell evaluation.
fn shell_quote(value: &OsStr) -> Result<String, ExitFailure> {
    let value = os_text(value)?;
    if value.is_empty() {
        return Ok("''".to_owned());
    }
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(byte, b'/' | b'.' | b'_' | b'-' | b':' | b'=' | b',' | b'%')
    }) {
        return Ok(value.to_owned());
    }
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}

/// Attaches this process' terminal to the owned session.
fn attach(endpoint: &Path, session: &SessionName) -> Result<i32, ExitFailure> {
    let target = parse_target_spec(session.as_str())
        .map_err(|error| ExitFailure::new(1, format!("rmux: invalid managed session: {error}")))?;
    let args = AttachSessionArgs {
        working_directory: None,
        detach_other_clients: false,
        skip_environment_update: false,
        flags: Vec::new(),
        read_only: false,
        target: Some(target),
        kill_other_clients: false,
    };
    // The daemon is already running — this invocation created a session on it moments ago — so
    // the attach must never be allowed to start a second one.
    let startup = StartupOptions::new(
        true,
        AutoStartConfig::disabled(),
        StartupEndpoint::resolved(endpoint.to_path_buf()),
    );
    run_attach_session(
        args,
        endpoint,
        startup,
        // `-2` and `-T` are top-level client flags of an rmux *command* invocation; neither
        // `rmux -c` nor `rmux claude` has ever forwarded them to the terminal it opened.
        client_terminal_context_from_parts(Vec::new(), infer_client_utf8_from_env()),
    )
}

/// Detaches this invocation's client once the retained workload pane exits.
///
/// Without it an attached client would sit in front of a dead `remain-on-exit` pane forever.
/// It detaches by owned session, which holds exactly one client — this one — so no other client
/// of the shared daemon is affected.
struct AttachExitObserver {
    /// Cleared to ask the observer thread to stop.
    running: Arc<AtomicBool>,
    /// The observer thread.
    handle: JoinHandle<()>,
}

impl AttachExitObserver {
    /// Starts the observer on its own connection to the shared daemon.
    fn start(endpoint: &Path, session: SessionName, pane: PaneTargetRef) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let endpoint = endpoint.to_path_buf();
        let thread_running = Arc::clone(&running);
        let handle = thread::spawn(move || {
            let Ok(mut connection) = connect(&endpoint) else {
                return;
            };
            while thread_running.load(Ordering::Relaxed) {
                if matches!(
                    pane_process_state(&mut connection, &pane),
                    Ok(PaneProcessState::Exited(_))
                ) {
                    let _ = connection.detach_client_extended(DetachClientExtRequest {
                        target_client: None,
                        all_other_clients: false,
                        target_session: Some(session),
                        kill_on_detach: false,
                        exec_command: None,
                    });
                    return;
                }
                thread::sleep(VIEWER_POLL_INTERVAL);
            }
        });
        Self { running, handle }
    }

    /// Stops and joins the observer.
    fn stop(self) {
        self.running.store(false, Ordering::Relaxed);
        let _ = self.handle.join();
    }
}

/// Links Claude's teammate window into the owned main session when it appears.
///
/// Upstream started a private daemon for this and polled a pid file. The companion session now
/// lives on the same shared daemon under a name only this invocation uses, so the monitor is
/// ordinary `list-panes` / `link-window` / `select-window` IPC against that name.
struct TeammateMonitor {
    /// Cleared to ask the monitor thread to stop.
    running: Arc<AtomicBool>,
    /// The monitor thread.
    handle: JoinHandle<()>,
}

impl TeammateMonitor {
    /// Starts the monitor. It begins immediately: the main pane already exists by this point.
    fn start(endpoint: &Path, main: SessionName, swarm: SessionName) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let endpoint = endpoint.to_path_buf();
        let thread_running = Arc::clone(&running);
        let handle = thread::spawn(move || {
            let Ok(mut connection) = connect(&endpoint) else {
                return;
            };
            let deadline = Instant::now() + VIEWER_WAIT_TIMEOUT;
            while thread_running.load(Ordering::Relaxed) && Instant::now() < deadline {
                if swarm_session_has_panes(&mut connection, &swarm)
                    && show_teammate_window(&mut connection, &main, &swarm).is_ok()
                {
                    return;
                }
                thread::sleep(VIEWER_POLL_INTERVAL);
            }
        });
        Self { running, handle }
    }

    /// Stops and joins the monitor.
    fn stop(self) {
        self.running.store(false, Ordering::Relaxed);
        let _ = self.handle.join();
    }
}

/// Reports whether the companion session exists and already has a pane.
fn swarm_session_has_panes(connection: &mut Connection, swarm: &SessionName) -> bool {
    let Ok(response) = connection.list_panes(swarm.clone(), Some("#{pane_id}\n".to_owned())) else {
        return false;
    };
    expect_command_output(&response, "list-panes")
        .is_ok_and(|output| !String::from_utf8_lossy(output.stdout()).trim().is_empty())
}

/// Links the companion window into the main session and selects the linked slot.
///
/// The linked window keeps normal shared-window alias semantics: it stays alive in the
/// companion session too, and removing one link does not destroy the other.
fn show_teammate_window(
    connection: &mut Connection,
    main: &SessionName,
    swarm: &SessionName,
) -> Result<(), ExitFailure> {
    let response = connection
        .link_window(
            WindowTarget::with_window(swarm.clone(), 0),
            WindowTarget::with_window(main.clone(), 0),
            true,
            false,
            false,
            false,
        )
        .map_err(ExitFailure::from)?;
    let linked = match response {
        Response::LinkWindow(response) => response.target,
        other => {
            expect_command_success(other, "link-window")?;
            return Err(ExitFailure::new(
                1,
                "rmux: link-window succeeded without reporting the linked window",
            ));
        }
    };

    let response = connection
        .select_window(linked)
        .map_err(ExitFailure::from)?;
    expect_command_success(response, "select-window")
}

/// The SDK handles one relay holds open for the life of the workload.
///
/// Armed before the barrier is released and dropped inside the runtime that built them, because
/// [`PaneOutputStream`](rmux_sdk::PaneOutputStream) emits a best-effort unsubscribe on drop.
struct ArmedOutput {
    /// Kept alive: the stream borrows this facade's transport.
    _rmux: rmux_sdk::Rmux,
    /// The live raw-output subscription, pinned to the workload pane's stable id.
    stream: rmux_sdk::PaneOutputStream,
}

/// Builds the runtime the SDK output pump runs on.
///
/// `rmux -c` and direct `rmux claude` are short-lived foreground processes that do exactly one
/// thing, so a single-threaded runtime created at this boundary is the whole cost. The control
/// connection stays synchronous and the stdin forwarder stays on its own OS thread; only the
/// output side is async, because only the output side has an async contract to honour.
fn relay_runtime() -> Result<tokio::runtime::Runtime, ExitFailure> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            ExitFailure::new(
                1,
                format!("rmux: failed to start the managed output runtime: {error}"),
            )
        })
}

/// Opens the SDK output stream for the workload pane.
///
/// Uses [`rmux_sdk::Pane::output_stream_starting_at`] with
/// [`PaneOutputStart::Oldest`](rmux_sdk::PaneOutputStart) so that anything the pane produced
/// between session creation and this call is still in the subscription's retained range.
async fn arm_output(
    endpoint: &Path,
    session: SessionName,
    pane_id: rmux_proto::PaneId,
) -> Result<ArmedOutput, ExitFailure> {
    let rmux = rmux_sdk::Rmux::connect(rmux_sdk::RmuxEndpoint::UnixSocket(endpoint.to_path_buf()))
        .await
        .map_err(|error| sdk_failure(&error))?;
    let pane = rmux
        .pane_by_id(session, pane_id)
        .await
        .map_err(|error| sdk_failure(&error))?;
    let stream = pane
        .output_stream_starting_at(rmux_sdk::PaneOutputStart::Oldest)
        .await
        .map_err(|error| sdk_failure(&error))?;
    Ok(ArmedOutput {
        _rmux: rmux,
        stream,
    })
}

/// Converts an SDK failure into this CLI's exit failure.
fn sdk_failure(error: &rmux_sdk::RmuxError) -> ExitFailure {
    ExitFailure::new(1, format!("rmux: managed output failed: {error}"))
}

/// Watches for the workload pane's exit on its own thread and connection.
///
/// # Why this is not the SDK's exit probe
///
/// The SDK's own collect helper observes exit through `Pane::info()`, and the public
/// [`rmux_sdk::Pane::wait_exit`] wraps the same thing behind the SDK's default operation
/// timeout — which alone would rule it out here, since a managed workload may run for hours.
///
/// The disqualifying reason is sharper than that. `Pane::info()` requests
/// `#{pane_start_command}` and runs the answer through `decode_command_field`, which
/// percent-decodes every element. The daemon only percent-encodes the *multi-element* argv form:
/// a single-string command goes through `quote_single_shell_command`, which escapes quotes and
/// backslashes and nothing else. So a pane whose start command contains a bare `%` — every
/// `rmux -c "printf '%s\n' ..."`, which is the most ordinary thing a user can type — makes
/// `Pane::info()` fail to parse a field this relay never asked for. The asymmetry is upstream's,
/// identical in the vendored server and in the pinned SDK, and neither side is ours to change.
///
/// The output *stream* is unaffected and stays on the SDK: it never parses that field, and it is
/// where the lag, end-marker and trailing-byte contracts actually live. Only the exit probe moves
/// to the CLI's existing `list-panes` reading, whose format does not mention the start command.
struct PaneExitWatcher {
    /// Cleared to ask the watcher thread to stop.
    running: Arc<AtomicBool>,
    /// The observed exit, once there is one.
    exited: Arc<Mutex<Option<PaneExitStatus>>>,
    /// Set when the watcher can no longer reach the daemon, so the pump stops waiting on it.
    failed: Arc<AtomicBool>,
    /// The watcher thread.
    handle: JoinHandle<()>,
}

impl PaneExitWatcher {
    /// Starts the watcher against the shared daemon.
    fn start(endpoint: &Path, pane: PaneTargetRef) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let exited = Arc::new(Mutex::new(None));
        let failed = Arc::new(AtomicBool::new(false));
        let endpoint = endpoint.to_path_buf();
        let thread_running = Arc::clone(&running);
        let thread_exited = Arc::clone(&exited);
        let thread_failed = Arc::clone(&failed);
        let handle = thread::spawn(move || {
            let Ok(mut connection) = connect(&endpoint) else {
                thread_failed.store(true, Ordering::Release);
                return;
            };
            // A pane is reported dead a moment before the daemon has retained *how* it died, so
            // the first dead reading is not necessarily the answer. Keep polling until the
            // reading settles, and only give up after a bounded grace — at which point the
            // fail-closed path reports "could not determine" rather than inventing a zero.
            let mut dead_since = None;
            while thread_running.load(Ordering::Acquire) {
                match pane_process_state(&mut connection, &pane) {
                    Ok(PaneProcessState::Exited(status)) => {
                        let settled = status.is_conclusive()
                            || dead_since.is_some_and(|since: Instant| {
                                since.elapsed() >= EXIT_STATUS_GRACE
                            });
                        if settled {
                            if let Ok(mut slot) = thread_exited.lock() {
                                *slot = Some(status);
                            }
                            return;
                        }
                        dead_since.get_or_insert_with(Instant::now);
                    }
                    Ok(PaneProcessState::Alive) => dead_since = None,
                    Err(_) => {
                        thread_failed.store(true, Ordering::Release);
                        return;
                    }
                }
                thread::sleep(RELAY_POLL_INTERVAL);
            }
        });
        Self {
            running,
            exited,
            failed,
            handle,
        }
    }

    /// Reports the observed exit, or an error once the watcher can no longer answer.
    fn observed(&self) -> Result<Option<PaneExitStatus>, ExitFailure> {
        if let Ok(slot) = self.exited.lock() {
            if let Some(status) = *slot {
                return Ok(Some(status));
            }
        }
        if self.failed.load(Ordering::Acquire) {
            return Err(ExitFailure::new(
                1,
                "rmux: lost contact with the daemon while waiting for the managed command",
            ));
        }
        Ok(None)
    }

    /// Stops and joins the watcher.
    fn stop(self) {
        self.running.store(false, Ordering::Release);
        let _ = self.handle.join();
    }
}

/// What the output pump observed while consuming one batch of chunks.
#[derive(Debug, Clone, Copy, Default)]
struct OutputProgress {
    /// Whether the daemon's empty-byte end marker arrived.
    saw_end: bool,
    /// Whether any chunk was ready at all.
    saw_output: bool,
    /// Events the daemon reported as dropped.
    missed_events: u64,
}

/// Writes one chunk to stdout and records what it proves.
///
/// The three properties this has to preserve are the three the plan names: a
/// [`Lag`](rmux_sdk::PaneOutputChunk::Lag) notice is an *explicit gap* rather than silent loss,
/// an empty byte payload is the *end marker* rather than an empty write, and everything else is
/// forwarded byte for byte with no decoding.
fn consume_chunk(
    chunk: rmux_sdk::PaneOutputChunk,
    progress: &mut OutputProgress,
) -> Result<(), ExitFailure> {
    match chunk {
        rmux_sdk::PaneOutputChunk::Bytes { bytes, .. } => {
            if bytes.is_empty() {
                progress.saw_end = true;
            } else {
                write_stdout(&bytes)?;
            }
        }
        rmux_sdk::PaneOutputChunk::Lag(notice) => {
            progress.missed_events = progress.missed_events.saturating_add(notice.missed_events);
        }
        // `PaneOutputChunk` is `#[non_exhaustive]`. A variant this build does not understand
        // carries output whose meaning is unknown, and guessing at it would be the silent loss
        // the lag arm exists to prevent — so it is counted as a gap and fails the relay.
        _ => {
            progress.missed_events = progress.missed_events.saturating_add(1);
        }
    }
    Ok(())
}

/// Drains the stream through its end marker or closure.
///
/// Called only after the pane has been observed exited, which is precisely when truncation is
/// tempting and wrong: the bytes a program wrote immediately before exiting are still in flight,
/// and an exit observation is not permission to drop them.
async fn drain_to_end(
    stream: &mut rmux_sdk::PaneOutputStream,
    progress: &mut OutputProgress,
) -> Result<(), ExitFailure> {
    loop {
        match stream.next().await.map_err(|error| sdk_failure(&error))? {
            Some(chunk) => {
                consume_chunk(chunk, progress)?;
                if progress.saw_end {
                    return Ok(());
                }
            }
            None => return Ok(()),
        }
    }
}

/// Runs the output pump until the workload has exited and its output has ended.
///
/// Ordering is the SDK collect helper's, with stdout as the sink instead of a byte buffer:
/// check for an already-exited pane first, otherwise poll ready output, then re-check, and
/// drain to the end before trusting the retained exit status. The exit *observation* comes from
/// `watcher` rather than the SDK, for the reason documented on [`PaneExitWatcher`]; everything
/// the plan's three output contracts depend on — explicit lag notices, the empty-byte end
/// marker, and trailing bytes surviving an exit that lands first — comes from the SDK stream.
///
/// Lag is fatal: a short result reported as complete is worse than a failure, and the workload
/// is never re-run to recover it.
async fn pump_output(
    mut armed: ArmedOutput,
    watcher: &PaneExitWatcher,
) -> Result<Option<PaneExitStatus>, ExitFailure> {
    let mut progress = OutputProgress::default();
    let exited = loop {
        if let Some(status) = watcher.observed()? {
            drain_to_end(&mut armed.stream, &mut progress).await?;
            break Some(status);
        }
        let chunks = armed
            .stream
            .poll_once()
            .await
            .map_err(|error| sdk_failure(&error))?;
        progress.saw_output = !chunks.is_empty();
        for chunk in chunks {
            consume_chunk(chunk, &mut progress)?;
        }
        if progress.saw_end {
            // The stream ended before the exit watcher caught up. The retained status is still
            // authoritative, so wait for it rather than report an unknown exit.
            break loop {
                if let Some(status) = watcher.observed()? {
                    break Some(status);
                }
                tokio::time::sleep(RELAY_POLL_INTERVAL).await;
            };
        }
        if !progress.saw_output {
            tokio::time::sleep(RELAY_POLL_INTERVAL).await;
        }
    };

    if progress.missed_events > 0 {
        return Err(ExitFailure::new(
            1,
            format!(
                "rmux: lost managed command output due to lag; missed {} events",
                progress.missed_events
            ),
        ));
    }
    Ok(exited)
}

/// Returns the workload pane's stable id.
fn stable_pane_id(pane: &PaneTargetRef) -> Result<rmux_proto::PaneId, ExitFailure> {
    pane.pane_id()
        .ok_or_else(|| ExitFailure::new(1, "rmux: managed pane has no stable identity"))
}

/// Writes raw pane bytes to stdout without decoding them.
fn write_stdout(bytes: &[u8]) -> Result<(), ExitFailure> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(bytes)
        .and_then(|()| stdout.flush())
        .map_err(|error| ExitFailure::new(1, format!("rmux: failed to write output: {error}")))
}

/// Forwards stdin to the workload pane on its own thread and connection.
///
/// Detached rather than joined: it is blocked in `read` on a descriptor nobody else can
/// interrupt, so waiting for it would outlive the workload. It owns a second client connection
/// to the shared daemon — the same thing the attach observer and teammate monitor do — which
/// leaves the control connection free for the barrier and for cleanup.
fn start_stdin_forwarder(endpoint: &Path, pane_id: rmux_proto::PaneId) {
    let endpoint = endpoint.to_path_buf();
    let pane_spec = pane_id.to_string();
    thread::spawn(move || {
        let Ok(mut connection) = connect(&endpoint) else {
            return;
        };
        let Ok(pane_spec) = parse_target_spec(&pane_spec) else {
            return;
        };
        let mut stdin = io::stdin().lock();
        let mut buffer = [0u8; RELAY_STDIN_CHUNK];
        loop {
            let read = match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            // Resolve the stable `%id` every time rather than remember a slot index: a layout
            // change moves indices, and input must never land in another pane.
            let Ok(target) = resolve_pane_target_spec(&mut connection, &pane_spec) else {
                return;
            };
            if send_bytes(&mut connection, target, &buffer[..read]).is_err() {
                return;
            }
        }
        // Nothing reads stdin after the loop, so release the process-wide lock before the
        // final round trip instead of holding it for the thread's lifetime.
        drop(stdin);
        if let Ok(target) = resolve_pane_target_spec(&mut connection, &pane_spec) {
            let _ = send_terminal_eof(&mut connection, target);
        }
    });
}

/// Forwards raw stdin bytes as hex key tokens.
///
/// The hex path is the only `send-keys` form that preserves arbitrary bytes: every other form
/// interprets its argument as key names or as text to be re-encoded.
fn send_bytes(
    connection: &mut Connection,
    target: PaneTarget,
    bytes: &[u8],
) -> Result<(), ExitFailure> {
    let keys = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let response = connection
        .send_keys_extended(SendKeysExtRequest {
            target: Some(target),
            keys,
            expand_formats: false,
            hex: true,
            literal: false,
            dispatch_key_table: false,
            copy_mode_command: false,
            forward_mouse_event: false,
            reset_terminal: false,
            repeat_count: None,
        })
        .map_err(ExitFailure::from)?;
    expect_command_success(response, "send-keys")
}

/// Sends terminal EOF after stdin closes.
///
/// Two `C-d` tokens, because a canonical-mode reader treats the first as "flush the current
/// partial line" and only an immediately following one as end-of-input. This cannot half-close
/// a raw-mode application: a terminal has no half-close, and pretending otherwise would
/// fabricate an EOF the workload never saw.
fn send_terminal_eof(connection: &mut Connection, target: PaneTarget) -> Result<(), ExitFailure> {
    let response = connection
        .send_keys_extended(SendKeysExtRequest {
            target: Some(target),
            keys: vec!["C-d".to_owned(), "C-d".to_owned()],
            expand_formats: false,
            hex: false,
            literal: false,
            dispatch_key_table: false,
            copy_mode_command: false,
            forward_mouse_event: false,
            reset_terminal: false,
            repeat_count: None,
        })
        .map_err(ExitFailure::from)?;
    expect_command_success(response, "send-keys")
}
