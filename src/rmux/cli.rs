//! Public CLI dispatch for the RMUX binary.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Resolving commands the typed parser rejected through the daemon's runtime command aliases.
#[path = "cli/alias_fallback.rs"]
mod alias_fallback;
/// Terminal requirements and connection handover for `attach-session`.
#[path = "cli/attach_transport.rs"]
mod attach_transport;
/// Scripted automation commands: pane discovery, snapshots, output streaming, and waits.
#[path = "cli/automation/mod.rs"]
mod automation;
/// The trait and helpers rmux's auxiliary top-level commands share.
#[path = "cli/aux_command.rs"]
mod aux_command;
/// The `load-buffer` and `save-buffer` paste-buffer transfer commands.
#[path = "cli/buffer_commands.rs"]
mod buffer_commands;
/// The `capabilities` command reporting wire version, JSON commands, and format names.
#[path = "cli/capabilities.rs"]
mod capabilities;
/// Building and sending `capture-pane` requests, including the target-action form.
#[path = "cli/capture_pane.rs"]
mod capture_pane;
#[path = "cli/claude_launcher.rs"]
mod claude_launcher;
#[path = "cli/claude_namespace.rs"]
mod claude_namespace;
/// The `rmux claude install-skill` invocation writing the bundled Claude skill file.
#[path = "cli/claude_skill.rs"]
mod claude_skill;
/// Client-scoped commands: attach, detach, switch, refresh, suspend, and `list-clients`.
#[path = "cli/client_commands.rs"]
mod client_commands;
/// The `list-commands` inventory rendered for a socket.
#[path = "cli/command_inventory.rs"]
mod command_inventory;
/// Sending parsed commands over a connection and writing their responses.
#[path = "cli/command_runner.rs"]
mod command_runner;
/// Option and environment commands: `set-option`, `set-environment`, and their `show-` pairs.
#[path = "cli/config_commands.rs"]
mod config_commands;
/// Rendering a `clap` parse failure inside control mode's `%begin`/`%error` framing.
#[path = "cli/control_mode_error.rs"]
mod control_mode_error;
/// The `rmux diagnose` top-level invocation.
#[path = "cli/diagnose.rs"]
mod diagnose;
/// Running the parsed command queue, sharing one connection across the queued commands.
#[path = "cli/dispatch.rs"]
mod dispatch;
/// The CLI's typed exit failure: exit code, message, output stream, and classification.
#[path = "cli/error.rs"]
mod error;
/// Printing a `display-message` format template resolved against a target.
#[path = "cli/format_print.rs"]
mod format_print;
/// JSON rendering for `list-clients`, `list-panes`, `list-sessions`, and `list-windows`.
#[path = "cli/json_output.rs"]
mod json_output;
/// Key commands: `send-keys`, `send-prefix`, `bind-key`, `unbind-key`, and `list-keys`.
#[path = "cli/key_commands.rs"]
mod key_commands;
#[path = "cli/managed_io.rs"]
mod managed_io;
/// The `display-message` command and its JSON variant.
#[path = "cli/message_commands.rs"]
mod message_commands;
/// Pane commands: `select-pane`, `resize-pane`, `respawn-pane`, `pipe-pane`, `list-panes`.
#[path = "cli/pane_commands.rs"]
mod pane_commands;
/// Constants describing the stable scripting surface: contract version, JSON commands, tags.
#[path = "cli/scripting_contract.rs"]
mod scripting_contract;
/// Server-scoped commands: `start-server`, `kill-server`, access control, and locking.
#[path = "cli/server_commands.rs"]
mod server_commands;
/// Session commands: `new-session`, `has-session`, `kill-session`, `rename-session`, listing.
#[path = "cli/session_commands.rs"]
mod session_commands;
#[path = "cli/shell_startup.rs"]
mod shell_startup;
/// Auto-start configuration, startup endpoints, and the foreground-server entrypoint.
#[path = "cli/startup.rs"]
mod startup;
/// Resolving session, window, and pane target specifiers against the daemon's current state.
#[path = "cli/target_resolution.rs"]
mod target_resolution;
/// The client terminal's size, from explicit flags or the controlling terminal.
#[path = "cli/terminal_size.rs"]
mod terminal_size;
/// Querying the controlling terminal for its color palette.
#[path = "cli/terminal_theme.rs"]
mod terminal_theme;
/// The tmux drop-in `doctor` and `setup` subcommands for the tmux shim.
#[path = "cli/tmux_dropin.rs"]
mod tmux_dropin;
/// Validation and scanning of top-level flags before any command is parsed.
#[path = "cli/top_level.rs"]
mod top_level;
/// The `web-share` command family exposing panes and sessions over the web frontend.
#[path = "cli/web_commands.rs"]
mod web_commands;
/// Rendering the human-readable banner printed after a web share is created.
#[path = "cli/web_share_display.rs"]
mod web_share_display;
/// Window commands: create, kill, select, rename, move, link, swap, resize, and rotate.
#[path = "cli/window_commands.rs"]
mod window_commands;

use crate::cli_args::{Cli, parse, parse_with_runtime_command_groups, scan_top_level_command};
use crate::cli_response::{expect_command_output, expect_command_success, unexpected_response};
use attach_transport::{attach_with_connection, require_attach_terminal};
use aux_command::AuxCommand;
use client_commands::{
    client_terminal_context_from_cli, optional_client_flags, run_control_mode, run_detach_client,
    run_list_clients, run_refresh_client, run_suspend_client, run_switch_client,
};
use client_commands::{run_switch_client_on_connection, validate_nested_attach_before_connect};
#[cfg(test)]
use command_inventory::render_list_commands_line;
pub(crate) use command_runner::{
    capture_target_action_needs_legacy_retry, cli_target_actions_enabled, run_command,
    run_command_resolved, run_payload_command, run_payload_command_resolved,
    target_action_needs_legacy_retry,
};
use command_runner::{finish_command_success, write_command_output, write_lines_output};
use control_mode_error::parse_failure as control_mode_parse_failure;
#[cfg(test)]
use dispatch::default_client_command;
use dispatch::{command_has_start_server_flag, dispatch_command_queue};
pub(crate) use error::{ExitFailure, ExitMessageTermination};
use rmux_client::{
    Connection, connect, ensure_server_running_with_config_outcome, resolve_socket_path,
    resolve_tmux_compatible_socket_path,
};
use shell_startup::run_shell_startup;
#[cfg(test)]
use startup::ServerStartupConfig;
use startup::{
    StartServerConnection, StartupEndpoint, StartupOptions, run_foreground_server,
    startup_config_from_cli, startup_config_from_top_level_scan,
};
use target_resolution::{
    CommandTarget, list_session_names, listed_pane_index_matches_target,
    resolve_current_pane_target, resolve_current_session_target,
    resolve_existing_window_target_or_current, resolve_pane_target_spec,
    resolve_session_target_spec, resolve_target_spec,
    resolve_window_index_target_or_current_session, resolve_window_target_spec,
};
use terminal_size::{build_terminal_size, current_terminal_size};
use top_level::{
    accept_compatibility_options, infer_client_utf8_from_env, top_level_parse_failure,
    top_level_version_output, top_level_version_requested, validate_top_level_invocation,
};

const TMUX_COMPAT_OVERRIDE_ENV: &str = "RMUX_INTERNAL_INVOKED_AS_TMUX";

/// Runs one `rmux` invocation from its argument vector and returns its process exit code.
#[allow(
    clippy::too_many_lines,
    reason = "one linear invocation pipeline whose ordered early-exit branches are the CLI \
              contract; splitting it would hide that order behind helper names"
)]
pub(crate) fn run<I, T>(args: I) -> Result<i32, ExitFailure>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let arguments = args.get(1..).unwrap_or(&[]);
    if let Some(error) = top_level_parse_failure(arguments) {
        return Err(error);
    }
    if top_level_version_requested(arguments) {
        return Err(ExitFailure::new_stdout(
            0,
            top_level_version_output(invoked_as_tmux(&args)),
        ));
    }
    // Extensions whose arguments are not tmux commands are served from raw argv, in this order,
    // before the typed queue is parsed.
    if let Some(exit) = diagnose::DiagnoseInvocation::dispatch(&args)
        .or_else(|| tmux_dropin::DropinInvocation::dispatch(&args))
        .or_else(|| claude_launcher::ClaudeInvocation::dispatch(&args))
        .or_else(|| capabilities::CapabilitiesInvocation::dispatch(&args))
    {
        return exit;
    }
    let runtime_resolution =
        alias_fallback::runtime_command_resolution_for_invocation(&args, invoked_as_tmux(&args))?;
    if let Some(alias_fallback::RuntimeCommandResolution::LegacyServerDispatch(exit_code)) =
        runtime_resolution.as_ref()
    {
        return Ok(*exit_code);
    }
    let parsed_cli = parse_with_runtime_resolution(&args, runtime_resolution.as_ref());
    let mut prestarted_endpoint = None;
    let mut cli = match parsed_cli {
        Ok(cli) => cli,
        Err(error) if runtime_resolution.is_some() => {
            let control_mode =
                scan_top_level_command(arguments).map_or(0, |scan| scan.control_mode);
            if control_mode != 0 {
                return Err(control_mode_parse_failure(error, control_mode));
            }
            return Err(ExitFailure::from(error));
        }
        Err(error) if error.kind() == clap::error::ErrorKind::InvalidSubcommand => {
            match parse_cold_alias_queue_after_startup(&args, error)? {
                ColdAliasParseOutcome::NotApplicable(error) => {
                    return parse_failure_or_absent_server(&args, error);
                }
                ColdAliasParseOutcome::Parsed(cold_cli, endpoint) => {
                    prestarted_endpoint = Some(endpoint);
                    *cold_cli
                }
                ColdAliasParseOutcome::Dispatched(exit_code) => return Ok(exit_code),
            }
        }
        Err(error) => return parse_failure_or_absent_server(&args, error),
    };
    cli.utf8 |= infer_client_utf8_from_env();
    let command_was_provided = cli.command.is_some();
    validate_top_level_invocation(&cli, command_was_provided)?;
    accept_compatibility_options(&cli);
    let mut startup_config = startup_config_from_cli(&cli);

    let mut socket_path = resolve_invocation_socket_path(
        invoked_as_tmux(&args),
        cli.socket_name(),
        cli.socket_path(),
    )
    .map_err(|error| error.with_startup_context("resolve socket", None))?;

    if let Some(crate::cli_args::Command::AttachSession(args)) = cli.command.as_ref() {
        validate_nested_attach_before_connect(args, &socket_path)?;
    }

    // A normal application startup replaces the daemon on the selected endpoint. Every
    // implicit launch takes this route — `rmux`, `cargo run`, and a foreground `rmux -D` —
    // so the executable that was just built always becomes the daemon, and a stale one never
    // keeps serving the socket. Stopping it closes its panes and terminates their commands,
    // which is why explicit subcommands, `-c` workloads, control mode, and `-N` are excluded:
    // those are control requests against sessions that must survive. `-D` is not excluded,
    // because a foreground server is an application startup that has to bind this endpoint.
    if !command_was_provided
        && cli.shell_command.is_none()
        && cli.control_mode == 0
        && !cli.no_start_server
    {
        // A cold endpoint is the expected case on a first launch, so only an absent daemon is
        // tolerated. A permission error, protocol failure, or endpoint-cleanup timeout means
        // the old daemon may still own the socket, and starting over it would race it.
        if let Err(error) = server_commands::run_kill_server(&socket_path)
            && !error.is_server_absent()
        {
            return Err(error
                .with_startup_context("stop previous daemon", Some(&socket_path))
                .with_socket_context(&socket_path));
        }
    }

    // A start-server command may create the daemon that loads command-alias
    // definitions from `-f`. Resolve the original argv only after that config
    // is ready, while retaining the startup connection so the empty daemon
    // cannot exit between alias resolution and typed dispatch.
    if prestarted_endpoint.is_none()
        && runtime_resolution.is_none()
        && cli.control_mode == 0
        && !cli.no_fork
        && !cli.no_start_server
        && cli.shell_command.is_none()
        && cli
            .command
            .as_ref()
            .is_some_and(command_has_start_server_flag)
    {
        let outcome = ensure_server_running_with_config_outcome(
            &socket_path,
            startup_config.auto_start.clone(),
        )
        .map_err(ExitFailure::from)
        .map_err(|error| {
            error
                .with_startup_context("start or connect to daemon", Some(&socket_path))
                .with_socket_context(&socket_path)
        })?;
        let endpoint = StartupEndpoint::prestarted(outcome);
        let selected_socket_path = endpoint.socket_path();
        let cold_resolution = endpoint.with_connection_mut(|connection| {
            alias_fallback::runtime_command_resolution_after_startup(
                &args,
                &selected_socket_path,
                connection,
            )
        })?;
        prestarted_endpoint = Some(endpoint);
        if let Some(alias_fallback::RuntimeCommandResolution::LegacyServerDispatch(exit_code)) =
            cold_resolution.as_ref()
        {
            return Ok(*exit_code);
        }
        if cold_resolution.is_some() {
            cli = parse_with_runtime_resolution(&args, cold_resolution.as_ref())
                .map_err(ExitFailure::from)?;
            cli.utf8 |= infer_client_utf8_from_env();
            let command_was_provided = cli.command.is_some();
            validate_top_level_invocation(&cli, command_was_provided)?;
            accept_compatibility_options(&cli);
            startup_config = startup_config_from_cli(&cli);
        }
    }

    let startup_endpoint =
        prestarted_endpoint.unwrap_or_else(|| StartupEndpoint::resolved(socket_path.clone()));
    socket_path = startup_endpoint.socket_path();

    if let Some(shell_command) = cli.shell_command.as_deref() {
        return run_shell_startup(
            &socket_path,
            StartupOptions::new(
                cli.no_start_server,
                startup_config.auto_start,
                startup_endpoint,
            ),
            shell_command,
            cli.login_shell,
        )
        .map_err(|error| error.with_socket_context(&socket_path));
    }

    if cli.no_fork {
        return run_foreground_server(&socket_path, &startup_config);
    }

    let startup = StartupOptions::new(
        cli.no_start_server,
        startup_config.auto_start,
        startup_endpoint,
    );
    if cli.control_mode != 0 {
        return run_control_mode(&cli, &socket_path, startup)
            .map_err(|error| error.with_socket_context(&socket_path));
    }
    let client_terminal = client_terminal_context_from_cli(&cli);
    let commands = cli.into_command_queue();
    dispatch_command_queue(commands, &socket_path, &startup, &client_terminal)
        .map_err(|error| error.with_socket_context(&socket_path))
}

/// Result of retrying an unknown-subcommand parse after a cold `start-server` auto-start.
enum ColdAliasParseOutcome {
    NotApplicable(clap::Error),
    Parsed(Box<Cli>, StartupEndpoint),
    Dispatched(i32),
}

/// Starts the daemon for a cold `start-server` command so its runtime aliases can parse argv.
fn parse_cold_alias_queue_after_startup(
    args: &[OsString],
    original_error: clap::Error,
) -> Result<ColdAliasParseOutcome, ExitFailure> {
    let Ok(scan) = scan_top_level_command(args.get(1..).unwrap_or(&[])) else {
        return Ok(ColdAliasParseOutcome::NotApplicable(original_error));
    };
    if scan.control_mode != 0
        || scan.no_fork
        || scan.no_start_server
        || scan.shell_command.is_some()
    {
        return Ok(ColdAliasParseOutcome::NotApplicable(original_error));
    }
    let Some(first_command) = alias_fallback::first_cold_start_command(args) else {
        return Ok(ColdAliasParseOutcome::NotApplicable(original_error));
    };
    if !command_has_start_server_flag(&first_command) {
        return Ok(ColdAliasParseOutcome::NotApplicable(original_error));
    }

    let socket_path = resolve_invocation_socket_path(
        invoked_as_tmux(args),
        scan.socket_name.as_deref(),
        scan.socket_path.as_deref().map(Path::new),
    )?;
    let startup_config = startup_config_from_top_level_scan(&scan, &first_command);
    let outcome =
        ensure_server_running_with_config_outcome(&socket_path, startup_config.auto_start)
            .map_err(ExitFailure::from)
            .map_err(|error| {
                error
                    .with_startup_context("start or connect to daemon", Some(&socket_path))
                    .with_socket_context(&socket_path)
            })?;
    let endpoint = StartupEndpoint::prestarted(outcome);
    let selected_socket_path = endpoint.socket_path();
    let resolution = endpoint.with_connection_mut(|connection| {
        alias_fallback::runtime_command_resolution_after_startup(
            args,
            &selected_socket_path,
            connection,
        )
    })?;
    let Some(resolution) = resolution else {
        return Err(ExitFailure::from(original_error));
    };
    if let alias_fallback::RuntimeCommandResolution::LegacyServerDispatch(exit_code) = resolution {
        return Ok(ColdAliasParseOutcome::Dispatched(exit_code));
    }
    let cli = parse_with_runtime_resolution(args, Some(&resolution)).map_err(ExitFailure::from)?;
    Ok(ColdAliasParseOutcome::Parsed(Box::new(cli), endpoint))
}

/// Parses argv, substituting daemon-resolved command groups when an alias resolution exists.
fn parse_with_runtime_resolution(
    args: &[OsString],
    resolution: Option<&alias_fallback::RuntimeCommandResolution>,
) -> Result<Cli, clap::Error> {
    match resolution {
        Some(alias_fallback::RuntimeCommandResolution::Canonical(groups)) => {
            parse_with_runtime_command_groups(args.to_vec(), groups)
        }
        Some(alias_fallback::RuntimeCommandResolution::LegacyDirect) | None => parse(args.to_vec()),
        Some(alias_fallback::RuntimeCommandResolution::LegacyServerDispatch(_)) => unreachable!(),
    }
}

/// Resolves this invocation's daemon endpoint.
///
/// Every endpoint-resolution branch goes through here so that a Claude workload's tmux-shim
/// call is adapted the same way regardless of which branch reached it: the shim's
/// `-L claude-swarm-*` label is redirected onto the shared endpoint, and the invocation's
/// owned session namespace is installed for the command and target resolution that follow.
/// An ordinary invocation — anything not reached through the shim, or carrying no namespace —
/// resolves exactly as it always did.
fn resolve_invocation_socket_path(
    invoked_as_tmux: bool,
    socket_name: Option<&std::ffi::OsStr>,
    socket_path: Option<&Path>,
) -> Result<PathBuf, ExitFailure> {
    if let Some(redirect) =
        claude_namespace::select_endpoint(invoked_as_tmux, socket_name, socket_path)?
    {
        return Ok(redirect);
    }
    if invoked_as_tmux {
        resolve_tmux_compatible_socket_path(socket_name, socket_path)
    } else {
        resolve_socket_path(socket_name, socket_path)
    }
    .map_err(ExitFailure::from)
}

/// Recovers the daemon endpoint and auto-start policy of an invocation dispatched before the
/// typed command queue is parsed.
///
/// `rmux claude` is the only such invocation that needs a daemon: its arguments belong to
/// Claude, so the typed parse never runs for them, but the workload still has to reach the one
/// daemon the user selected with `-L`/`-S`.
fn top_level_startup(args: &[OsString]) -> Result<(PathBuf, StartupOptions), ExitFailure> {
    let arguments = args.get(1..).unwrap_or(&[]);
    let scan = scan_top_level_command(arguments).ok();
    // The scan carries `-f`, `-L`, `-S` and `-N` when it succeeds. It only fails on argument
    // shapes the top level cannot describe at all, and even then the socket selection is
    // recoverable from raw argv the same way an unknown command's is.
    let (socket_name, socket_path) = scan.as_ref().map_or_else(
        || {
            let (name, path) = recover_socket_selection(arguments).unwrap_or_default();
            (name, path.map(PathBuf::into_os_string))
        },
        |scan| (scan.socket_name.clone(), scan.socket_path.clone()),
    );
    let socket_path = resolve_invocation_socket_path(
        invoked_as_tmux(args),
        socket_name.as_deref(),
        socket_path.as_deref().map(Path::new),
    )?;

    let cwd = env::current_dir().ok();
    let auto_start = match scan.as_ref().map(|scan| scan.config_files.as_slice()) {
        Some([]) | None => rmux_client::AutoStartConfig::default_files(true, cwd),
        Some(files) => rmux_client::AutoStartConfig::custom_files(files.to_vec(), false, cwd),
    };
    let startup = StartupOptions::new(
        scan.is_some_and(|scan| scan.no_start_server),
        auto_start,
        StartupEndpoint::resolved(socket_path.clone()),
    );
    Ok((socket_path, startup))
}

/// Whether this invocation acts as tmux, by argv0 or the internal override variable.
fn invoked_as_tmux(args: &[OsString]) -> bool {
    invoked_as_tmux_argv0(args) || internal_tmux_compat_override()
}

/// Whether argv0's file stem is `tmux`, ignoring case.
fn invoked_as_tmux_argv0(args: &[OsString]) -> bool {
    args.first()
        .and_then(|arg| std::path::Path::new(arg).file_stem())
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case("tmux"))
}

/// Whether `RMUX_INTERNAL_INVOKED_AS_TMUX` is set to `1`.
fn internal_tmux_compat_override() -> bool {
    env::var_os(TMUX_COMPAT_OVERRIDE_ENV)
        .as_deref()
        .is_some_and(|value| value == "1")
}

/// Turns a parse failure into an alias retry against a running daemon, or a plain exit.
fn parse_failure_or_absent_server(
    args: &[OsString],
    error: clap::Error,
) -> Result<i32, ExitFailure> {
    if !parse_failure_should_probe_server(args, &error) {
        return Err(ExitFailure::from(error));
    }

    let Some((socket_name, socket_path)) = recover_socket_selection(args.get(1..).unwrap_or(&[]))
    else {
        return Err(ExitFailure::from(error));
    };
    let resolved = resolve_invocation_socket_path(
        invoked_as_tmux(args),
        socket_name.as_deref(),
        socket_path.as_deref(),
    )?;

    match connect(&resolved) {
        Ok(mut connection) if error.kind() == clap::error::ErrorKind::InvalidSubcommand => {
            alias_fallback::run_unknown_command_through_server_aliases(
                args,
                &resolved,
                &mut connection,
            )
            .map_err(|error| error.with_socket_context(&resolved))
        }
        Ok(_) => Err(ExitFailure::from(error)),
        Err(connect_error) => Err(ExitFailure::from_client_connect(&resolved, connect_error)),
    }
}

/// Whether this parse failure is one a running daemon could still resolve as an alias.
fn parse_failure_should_probe_server(args: &[OsString], error: &clap::Error) -> bool {
    if matches!(
        error.kind(),
        clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
    ) {
        return false;
    }

    if error.kind() == clap::error::ErrorKind::InvalidSubcommand {
        return first_command_token(args.get(1..).unwrap_or(&[])).is_some();
    }

    if error.kind() == clap::error::ErrorKind::ValueValidation {
        let message = error.to_string();
        if message.contains("command too long") {
            return first_command_token(args.get(1..).unwrap_or(&[])).is_some();
        }
        if message.contains("expects an argument") {
            return first_command_token(args.get(1..).unwrap_or(&[]))
                .is_some_and(|command| matches!(command.as_str(), "new-session" | "new"));
        }
        return first_command_token(args.get(1..).unwrap_or(&[]))
            .is_some_and(|command| matches!(command.as_str(), "resize-pane" | "resizep"));
    }

    first_command_token(args.get(1..).unwrap_or(&[])).is_some_and(|command| {
        matches!(
            command.as_str(),
            "new-session" | "new" | "resize-pane" | "resizep"
        )
    })
}

/// Recovers the `-L`/`-S` socket selection from raw argv when the typed parse failed.
fn recover_socket_selection(arguments: &[OsString]) -> Option<(Option<OsString>, Option<PathBuf>)> {
    let (mut socket_name, mut socket_path) = (None, None);
    command_index(arguments, |flag, value| match flag {
        "-L" => socket_name = value.cloned(),
        "-S" => socket_path = value.map(PathBuf::from),
        _ if flag.len() > 2 && flag.starts_with("-L") => {
            socket_name = flag.get(2..).map(OsString::from);
        }
        _ if flag.len() > 2 && flag.starts_with("-S") => {
            socket_path = flag.get(2..).map(PathBuf::from);
        }
        _ => {}
    })?;
    Some((socket_name, socket_path))
}

/// Returns the first non-flag argument, the command name, skipping top-level flag values.
fn first_command_token(arguments: &[OsString]) -> Option<String> {
    let index = command_index(arguments, |_, _| {})?;
    arguments.get(index)?.to_str().map(str::to_owned)
}

/// The index of the command word after the top-level flags, skipping every flag word.
///
/// `-c`, `-f`, `-L`, `-S` and `-T` take the next argument as their value, and `visit` sees each
/// flag word with that value. The index is past the end when argv runs out first, and `None`
/// means a flag word was not UTF-8.
fn command_index(
    arguments: &[OsString],
    mut visit: impl FnMut(&str, Option<&OsString>),
) -> Option<usize> {
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        let argument = argument.to_str()?;
        if argument == "--" {
            return Some(index + 1);
        }
        if !argument.starts_with('-') || argument == "-" {
            return Some(index);
        }
        let value = if matches!(argument, "-c" | "-f" | "-L" | "-S" | "-T") {
            index += 1;
            arguments.get(index)
        } else {
            None
        };
        visit(argument, value);
        index += 1;
    }
    Some(index)
}

/// Connects for the next queued command, discarding the connection's startup provenance.
fn connect_with_startserver(
    socket_path: &Path,
    startup: StartupOptions,
) -> Result<Connection, ExitFailure> {
    connect_with_startserver_outcome(socket_path, startup)
        .map(StartServerConnection::into_connection)
}

impl StartServerConnection {
    /// Discards the recorded startup provenance and keeps only the open connection.
    fn into_connection(self) -> Connection {
        self.connection
    }
}

/// Connects the next queued command, auto-starting the daemon when allowed.
///
/// The startup connection opened before dispatch is handed to the first
/// command that needs one; leaving it open in parallel would register a second
/// idle client and permanently cancel the daemon's exit-empty shutdown. Its
/// provenance is kept on the shared startup endpoint, so a later attach still
/// knows that this invocation started the daemon.
fn connect_with_startserver_outcome(
    socket_path: &Path,
    startup: StartupOptions,
) -> Result<StartServerConnection, ExitFailure> {
    let StartupOptions {
        no_start_server,
        config,
        endpoint,
    } = startup;
    if no_start_server {
        let connection = connect(socket_path)
            .map_err(|error| ExitFailure::from_client_connect(socket_path, error))
            .map_err(|error| error.with_startup_context("connect to daemon", Some(socket_path)))?;
        return Ok(StartServerConnection {
            connection,
            provenance: endpoint.provenance(),
        });
    }
    if let Some(connection) = endpoint.take_connection() {
        return Ok(StartServerConnection {
            connection,
            provenance: endpoint.provenance(),
        });
    }
    let outcome = ensure_server_running_with_config_outcome(socket_path, config)
        .map_err(ExitFailure::from)
        .map_err(|error| {
            error.with_startup_context("start or connect to daemon", Some(socket_path))
        })?;
    endpoint.record_ensured(outcome.socket_path(), outcome.provenance());
    Ok(StartServerConnection {
        connection: outcome.into_connection(),
        provenance: endpoint.provenance(),
    })
}

/// Joins a command's tokens into one shell line, quoting each when there is more than one.
fn shell_command_text(command: Vec<String>) -> String {
    if command.len() == 1 {
        return command.into_iter().next().unwrap_or_default();
    }

    command
        .into_iter()
        .map(|token| shell_command_token(&token))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Single-quotes one shell token, escaping any embedded single quote.
fn shell_command_token(token: &str) -> String {
    format!("'{}'", token.replace('\'', "'\\''"))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::aux_command::args;
    use super::{
        ServerStartupConfig, command_has_start_server_flag, default_client_command,
        render_list_commands_line, run, startup_config_from_cli, top_level_parse_failure,
    };
    use crate::cli_args::{
        AttachSessionArgs, Command, ListSessionsArgs, NewWindowArgs, StartServerArgs,
        parse as parse_cli, parse_target_spec,
    };
    use std::path::PathBuf;

    #[test]
    fn top_level_preparse_accepts_tmux_short_help() {
        for values in [&["-h"][..], &["-Nh"][..], &["-hV"][..]] {
            let error = top_level_parse_failure(&args(values)).expect("expected short help exit");
            assert_eq!(error.exit_code(), 0);
            assert!(!error.use_stderr());
            assert_eq!(
                error.message(),
                "usage: rmux [-2CDhlNuVv] [-c shell-command] [-f file] [-L socket-name]\n            [-S socket-path] [-T features] [command [flags]]"
            );
        }
    }

    #[test]
    fn top_level_preparse_rejects_long_options_with_tmux_usage() {
        assert_eq!(
            top_level_parse_failure(&args(&["--help"]))
                .expect("expected --help to fail before clap")
                .message(),
            "usage: rmux [-2CDlNuVv] [-c shell-command] [-f file] [-L socket-name]\n            [-S socket-path] [-T features] [command [flags]]\n\nRMUX extensions:\n  capabilities [--human|--json]\n  claude [install-skill|claude-args...]\n  diagnose [--human|--json]\n  doctor tmux-dropin\n  setup tmux-shim\n  wait-pane [flags]\n  pane-snapshot [flags]\n  stream-pane [--raw|--lines]\n  collect-pane-output --until-pane-exit --max-bytes bytes\n  locator|expect-pane [flags]\n  find-panes|find-sessions [flags]\n  broadcast-keys -t target... -- key ...\n  with-session session-name -- command ...\n  web-share [flags]\n  web-share list|lookup|stop|disconnect|off|config\n\nUse `rmux list-commands` for the tmux-compatible command surface."
        );
        assert_eq!(
            top_level_parse_failure(&args(&["--not-a-tmux-flag", "-h"]))
                .expect("expected long top-level option to fail before clap")
                .message(),
            "usage: rmux [-2CDlNuVv] [-c shell-command] [-f file] [-L socket-name]\n            [-S socket-path] [-T features] [command [flags]]"
        );
        assert!(top_level_parse_failure(&args(&["split-window", "-h"])).is_none());
    }

    #[test]
    fn top_level_preparse_rejects_invalid_clusters_with_tmux_unknown_option() {
        assert!(
            top_level_parse_failure(&args(&["-xh"]))
                .expect("expected invalid cluster to fail before clap")
                .message()
                .contains("unknown option -- x")
        );
        assert!(
            top_level_parse_failure(&args(&["-Nxh"]))
                .expect("expected invalid cluster to fail before clap")
                .message()
                .contains("unknown option -- x")
        );
    }

    #[test]
    fn top_level_preparse_leaves_version_first_clusters_for_clap() {
        assert!(top_level_parse_failure(&args(&["-Vh"])).is_none());
        assert!(top_level_parse_failure(&args(&["-lVh"])).is_none());
    }

    #[test]
    fn top_level_preparse_does_not_parse_option_values_as_flags() {
        assert!(top_level_parse_failure(&args(&["-L", "-h", "list-sessions",])).is_none());
        assert!(top_level_parse_failure(&args(&["-Lhas-h", "list-sessions"])).is_none());
    }

    #[test]
    fn claude_dispatch_rejects_top_level_modes_it_cannot_honor() {
        for flag in ["-h", "-V"] {
            let exit = run(args(&["rmux", flag, "claude"]))
                .expect_err("top-level display option exits before extension dispatch");
            assert_eq!(exit.exit_code(), 0, "{flag}");
            assert!(
                !exit.message().contains("not supported by the managed"),
                "{flag} keeps top-level priority"
            );
        }

        for values in [
            &["rmux", "-D", "claude"][..],
            &["rmux", "-c", "echo ignored", "claude", "install-skill"][..],
            &["rmux", "-cecho ignored", "claude"][..],
            &["rmux", "-u", "-D", "-N", "claude"][..],
        ] {
            let error = run(args(values)).expect_err("managed launcher mode must be rejected");
            assert_eq!(error.exit_code(), 1, "{values:?}");
            assert!(
                error.message().contains("usage: rmux"),
                "the scanner must reject the mode before external dispatch: {values:?}"
            );
        }

        let no_start = run(args(&["rmux", "-N", "claude"]))
            .expect_err("-N must not silently start a private server");
        assert!(no_start.message().contains("-N is incompatible"));

        let control = run(args(&["rmux", "-C", "claude"]))
            .expect_err("-C must not silently launch an attached client");
        assert!(control.message().contains("-C control mode"));

        for (values, option) in [
            (&["rmux", "-2", "claude"][..], "-2"),
            (&["rmux", "-f", "config", "claude"][..], "-f"),
            (&["rmux", "-f", "--unknown", "claude"][..], "-f"),
            (&["rmux", "-l", "claude"][..], "-l"),
            (&["rmux", "-Ldemo", "claude"][..], "-L"),
            (&["rmux", "-S/path", "claude"][..], "-S"),
            (&["rmux", "-TRGB", "claude"][..], "-T"),
            (&["rmux", "-u", "claude"][..], "-u"),
            (&["rmux", "-v", "claude"][..], "-v"),
            (&["rmux", "-v", "-fconfig", "claude"][..], "-f"),
            (&["rmux", "-u", "-v", "-fconfig", "claude"][..], "-f"),
            (&["rmux", "-v", "-Ldemo", "claude"][..], "-L"),
            (&["rmux", "-L", "-f", "claude"][..], "-L"),
        ] {
            let error = run(args(values)).expect_err("ignored option must be rejected");
            assert!(
                error.message().contains(option),
                "diagnostic must name {option}: {error:?}"
            );
        }
    }

    #[test]
    fn command_too_long_parse_errors_probe_for_absent_server_first() {
        let error = clap::Error::raw(clap::error::ErrorKind::ValueValidation, "command too long");

        assert!(super::parse_failure_should_probe_server(
            &args(&["rmux", "-S", "/tmp/missing.sock", "aaaaaaaa"]),
            &error
        ));
    }

    #[test]
    fn start_server_inventory_matches_supported_frozen_commands() {
        assert!(command_has_start_server_flag(&default_client_command()));
        assert!(command_has_start_server_flag(&Command::StartServer(
            StartServerArgs::default()
        )));
        assert!(command_has_start_server_flag(&Command::AttachSession(
            AttachSessionArgs {
                detach_other_clients: false,
                skip_environment_update: false,
                flags: Vec::new(),
                read_only: false,
                target: Some(parse_target_spec("alpha").expect("valid target")),
                kill_other_clients: false,
                working_directory: None,
            }
        )));
        assert!(!command_has_start_server_flag(&Command::KillServer));
        assert!(!command_has_start_server_flag(&Command::ListSessions(
            ListSessionsArgs {
                format: None,
                filter: None,
                json: false,
                sort_order: None,
                reversed: false,
            }
        )));
        assert!(!command_has_start_server_flag(&Command::NewWindow(
            NewWindowArgs {
                after: false,
                before: false,
                target: Some(parse_target_spec("alpha").expect("valid target")),
                name: None,
                detached: false,
                format: None,
                print_target: false,
                kill_existing: false,
                select_existing: false,
                start_directory: None,
                environment: Vec::new(),
                command: Vec::new(),
                queue_command: String::new(),
            }
        )));
        let web_create = parse_cli(["rmux", "web-share", "-t", "alpha"])
            .expect("web-share create parses")
            .command
            .expect("parsed command");
        assert!(command_has_start_server_flag(&web_create));
        for args in [
            &["rmux", "web-share", "-l"][..],
            &["rmux", "web-share", "-K", "abc12345"][..],
            &["rmux", "web-share", "disconnect", "abc12345"][..],
            &["rmux", "web-share", "-X"][..],
            &["rmux", "web-share", "--lookup", "abc12345"][..],
            &["rmux", "web-share", "--config"][..],
        ] {
            let command = parse_cli(args.iter().copied())
                .expect("web-share lifecycle command parses")
                .command
                .expect("parsed command");
            assert!(!command_has_start_server_flag(&command));
        }
    }

    #[test]
    fn explicit_config_files_disable_quiet_startup_loading() {
        let cli = parse_cli(["rmux", "-f", "one.conf", "-f", "two.conf"]).expect("cli parses");
        let startup = startup_config_from_cli(&cli);

        match startup.server {
            ServerStartupConfig::Files { files, quiet, .. } => {
                assert!(!quiet);
                assert_eq!(
                    files,
                    vec![PathBuf::from("one.conf"), PathBuf::from("two.conf")]
                );
            }
            ServerStartupConfig::Default { .. } => panic!("expected explicit config files"),
        }
    }

    #[test]
    fn start_server_rejects_zero_web_port() {
        assert!(parse_cli(["rmux", "start-server", "--web-port", "0"]).is_err());
    }

    #[test]
    fn list_commands_tmux_format_variables_expand() {
        assert_eq!(
            render_list_commands_line(
                Some("#{command_list_name}|#{command_list_alias}|#{command_name}|#{command_alias}"),
                "attach-session",
                Some("attach"),
            ),
            "attach-session|attach||"
        );
    }

    #[test]
    fn list_commands_default_output_matches_tmux_signature_shape() {
        assert_eq!(
            render_list_commands_line(None, "attach-session", Some("attach")),
            "attach-session (attach) [-dErx] [-c working-directory] [-f flags] [-t target-session]"
        );
        assert_eq!(
            render_list_commands_line(None, "kill-server", None),
            "kill-server "
        );
    }

    #[test]
    fn list_commands_usage_variable_expands_tmux_signature_suffix() {
        assert_eq!(
            render_list_commands_line(
                Some("#{command_list_name}|#{command_list_usage}"),
                "attach-session",
                Some("attach"),
            ),
            "attach-session|[-dErx] [-c working-directory] [-f flags] [-t target-session]"
        );
    }
}
