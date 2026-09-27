use std::path::Path;

/// The `set-hook` and `show-hooks` command implementations.
#[path = "config_commands/hooks.rs"]
mod hooks;
/// Argument and scope resolution shared by the option commands.
#[path = "config_commands/options.rs"]
mod options;

use rmux_client::ClientError;
use rmux_proto::{
    ErrorResponse, Request, Response, RmuxError, ScopeSelector, SetEnvironmentMode,
    SetOptionByNameRequest,
};

use crate::cli::target_resolution::{connect_cli, resolve_session_target_spec};
use crate::cli::{
    ExitFailure, expect_command_output, expect_command_success, resolve_current_session_target,
    run_command_resolved, run_payload_command_resolved, write_command_output,
};
use crate::cli_args::{
    SetEnvironmentArgs, SetOptionArgs, SetOptionCommandKind, ShowEnvironmentArgs, ShowOptionsArgs,
    ShowOptionsCommandKind, TargetSpec,
};
pub(crate) use hooks::{run_set_hook, run_show_hooks};
use options::{ResolvedSetOptionCommand, resolve_set_option_args, resolve_show_options_scope};

/// Runs `set-option` and its variants, silently succeeding on option errors when `-q` is given.
pub(crate) fn run_set_option(
    command: SetOptionCommandKind,
    args: SetOptionArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    let quiet = args.quiet;
    let mut connection = connect_cli(socket_path)?;
    let Some(ResolvedSetOptionCommand::Request(request)) = unless_quiet(
        resolve_set_option_args(&mut connection, command, args),
        quiet,
    )?
    else {
        return Ok(0);
    };

    let response = connection
        .roundtrip(&Request::SetOptionByName(Box::new(
            SetOptionByNameRequest {
                scope: request.scope,
                name: request.option,
                value: request.value,
                mode: request.mode,
                only_if_unset: request.only_if_unset,
                unset: request.unset,
                unset_pane_overrides: request.unset_pane_overrides,
                format: request.format,
                format_target: request.format_target,
            },
        )))
        .map_err(ExitFailure::from)?;
    if !(quiet && quiet_option_response(&response)) {
        expect_command_success(response, command.command_name())?;
    }
    Ok(0)
}

/// Runs `set-environment`, rejecting a name containing `=` and a value paired with `-r` or `-u`.
pub(crate) fn run_set_environment(
    args: SetEnvironmentArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    let mode = resolve_set_environment_mode(&args)?;
    if args.name.contains('=') {
        return Err(ExitFailure::new(1, "variable name contains ="));
    }
    let value = match mode {
        Some(SetEnvironmentMode::Clear | SetEnvironmentMode::Unset) => String::new(),
        Some(SetEnvironmentMode::Set) | None => args
            .value
            .clone()
            .ok_or_else(|| ExitFailure::new(1, "no value specified"))?,
    };

    run_command_resolved(socket_path, "set-environment", move |connection| {
        let scope = resolve_environment_scope(connection, args.global, args.target)?;
        connection
            .set_environment(scope, args.name, value, mode, args.hidden, args.format)
            .map_err(ExitFailure::from)
    })
}

/// Runs `show-options` and its variants, printing the server's rendering of the chosen scope.
pub(crate) fn run_show_options(
    command: ShowOptionsCommandKind,
    args: ShowOptionsArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    let command_name = command.command_name();
    let quiet = args.quiet;
    let Some(scope) = unless_quiet(resolve_show_options_scope(command, &args), quiet)? else {
        return Ok(0);
    };

    let mut connection = connect_cli(socket_path)?;
    let Some(scope) = unless_quiet(scope.resolve(&mut connection, command_name), quiet)? else {
        return Ok(0);
    };
    let response = connection
        .show_options_extended(
            scope,
            args.name,
            args.value_only,
            args.include_inherited,
            args.quiet,
            args.include_hooks,
        )
        .map_err(show_options_exit_failure)?;
    match response {
        response if quiet && quiet_option_response(&response) => Ok(0),
        Response::Error(ErrorResponse {
            error: RmuxError::Server(message) | RmuxError::Message(message),
        }) => Err(show_options_message_failure(message)),
        response => {
            let output = expect_command_output(&response, command_name)?;
            write_command_output(output)?;
            Ok(0)
        }
    }
}

/// Unwraps `result`, yielding `None` when `-q` swallows its option-name lookup failure.
fn unless_quiet<T>(result: Result<T, ExitFailure>, quiet: bool) -> Result<Option<T>, ExitFailure> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if quiet && quiet_option_failure(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Reports an unknown, invalid, or ambiguous option lookup as a plain exit-code-`1` message.
fn show_options_exit_failure(error: ClientError) -> ExitFailure {
    match error {
        ClientError::Protocol(RmuxError::Server(message) | RmuxError::Message(message)) => {
            option_lookup_failure(&message).unwrap_or_else(|| {
                ExitFailure::from(ClientError::Protocol(RmuxError::Server(message)))
            })
        }
        error => ExitFailure::from(error),
    }
}

/// Same normalization as for client errors, applied to an error carried inside a response.
fn show_options_message_failure(message: String) -> ExitFailure {
    option_lookup_failure(&message).unwrap_or_else(|| ExitFailure::new(1, message))
}

/// An option-name lookup error with any `server error: ` prefix stripped, as a plain failure.
fn option_lookup_failure(message: &str) -> Option<ExitFailure> {
    let normalized = message.strip_prefix("server error: ").unwrap_or(message);
    option_lookup_error(normalized).then(|| ExitFailure::new(1, normalized.to_owned()))
}

/// Whether `-q` should swallow this failure because it is only an option-name lookup complaint.
fn quiet_option_failure(error: &ExitFailure) -> bool {
    let message = error.message();
    message.starts_with("invalid option: ")
        || message.starts_with("server error: unknown option: ")
        || message.starts_with("server error: invalid option: ")
        || message.starts_with("server error: ambiguous option: ")
}

/// Whether the response is a server error about an option name that `-q` should swallow.
fn quiet_option_response(response: &Response) -> bool {
    matches!(
        response,
        Response::Error(ErrorResponse {
            error: RmuxError::Server(message),
        }) if option_lookup_error(message)
    )
}

/// Whether `message` reports an unknown, invalid, or ambiguous option name.
fn option_lookup_error(message: &str) -> bool {
    message.starts_with("unknown option: ")
        || message.starts_with("invalid option: ")
        || message.starts_with("ambiguous option: ")
}

/// Runs `show-environment`, printing the variables of the global or resolved session scope.
pub(crate) fn run_show_environment(
    args: ShowEnvironmentArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    run_payload_command_resolved(socket_path, "show-environment", move |connection| {
        let scope = resolve_environment_scope(connection, args.global, args.target)?;
        connection
            .show_environment(scope, args.name, args.hidden, args.shell_format)
            .map_err(ExitFailure::from)
    })
}

/// Chooses the global scope, an explicit session target, or the client's current session.
fn resolve_environment_scope(
    connection: &mut rmux_client::Connection,
    global: bool,
    target: Option<TargetSpec>,
) -> Result<ScopeSelector, ExitFailure> {
    if global {
        return Ok(ScopeSelector::Global);
    }
    match target {
        Some(target) => {
            resolve_session_target_spec(connection, &target, false).map(ScopeSelector::Session)
        }
        None => resolve_current_session_target(connection).map(ScopeSelector::Session),
    }
}

/// Maps the `-r` and `-u` flags to a `SetEnvironmentMode`, rejecting a value alongside them.
fn resolve_set_environment_mode(
    args: &SetEnvironmentArgs,
) -> Result<Option<SetEnvironmentMode>, ExitFailure> {
    let mode = match (args.clear, args.unset) {
        (true, false) => Some(SetEnvironmentMode::Clear),
        (false | true, true) => Some(SetEnvironmentMode::Unset),
        (false, false) => Some(SetEnvironmentMode::Set),
    };

    if matches!(
        mode,
        Some(SetEnvironmentMode::Clear | SetEnvironmentMode::Unset)
    ) && args.value.is_some()
    {
        return Err(ExitFailure::new(
            1,
            "set-environment -r and -u do not accept a value",
        ));
    }

    Ok(mode)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::{
        options::{
            ResolvedSetOptionArgs, ResolvedSetOptionCommand, ShowOptionsScope,
            UnresolvedShowOptionsScope, resolve_set_option_args_with_exact_targets,
        },
        resolve_show_options_scope,
    };
    use crate::cli_args::{
        SetEnvironmentArgs, SetOptionArgs, SetOptionCommandKind, ShowOptionsArgs,
        ShowOptionsCommandKind, TargetSpec, parse_target_spec,
    };
    use rmux_proto::{
        OptionScopeSelector, PaneTarget, SessionName, SetEnvironmentMode, WindowTarget,
    };
    use std::path::Path;

    fn target_spec(value: &str) -> TargetSpec {
        parse_target_spec(value).expect("valid target spec")
    }

    fn alpha() -> SessionName {
        SessionName::new("alpha").expect("valid session")
    }

    fn env_args(name: &str) -> SetEnvironmentArgs {
        SetEnvironmentArgs {
            global: true,
            target: None,
            format: false,
            hidden: false,
            clear: false,
            unset: false,
            name: name.to_owned(),
            value: None,
        }
    }

    fn global_set_args(option: &str, value: &str) -> SetOptionArgs {
        SetOptionArgs {
            global: true,
            server: false,
            window: false,
            pane: false,
            quiet: false,
            append: false,
            format: false,
            only_if_unset: false,
            unset: false,
            unset_pane_overrides: false,
            target: None,
            option: option.to_owned(),
            value: Some(value.to_owned()),
        }
    }

    fn targeted_set_args(target: &str, option: &str, value: &str) -> SetOptionArgs {
        SetOptionArgs {
            global: false,
            target: Some(target_spec(target)),
            ..global_set_args(option, value)
        }
    }

    fn show_global_args(name: Option<&str>) -> ShowOptionsArgs {
        ShowOptionsArgs {
            include_inherited: false,
            include_hooks: false,
            global: true,
            server: false,
            window: false,
            pane: false,
            quiet: false,
            value_only: false,
            target: None,
            name: name.map(str::to_owned),
        }
    }

    #[allow(
        clippy::panic_in_result_fn,
        reason = "a test helper has nothing to recover to when the resolution shape is wrong"
    )]
    fn resolve_request(
        command: SetOptionCommandKind,
        args: SetOptionArgs,
    ) -> Result<ResolvedSetOptionArgs, crate::cli::ExitFailure> {
        match resolve_set_option_args_with_exact_targets(command, args)? {
            ResolvedSetOptionCommand::Request(request) => Ok(request),
            ResolvedSetOptionCommand::NoOp => {
                panic!("expected set-option to resolve to a request")
            }
        }
    }

    #[test]
    fn set_environment_rejects_bad_arguments_before_connecting() {
        for (args, socket, message) in [
            (
                SetEnvironmentArgs {
                    global: false,
                    ..env_args("FOO")
                },
                "/tmp/rmux-setenv-value-test.sock",
                "no value specified",
            ),
            // The `=` check runs before value validation, so the missing value is not reported.
            (
                env_args("FOO=bar"),
                "/tmp/rmux-setenv-equals-test.sock",
                "variable name contains =",
            ),
        ] {
            let error = super::run_set_environment(args, Path::new(socket))
                .expect_err("invalid set-environment arguments should fail before connecting");

            assert_eq!(error.exit_code(), 1);
            assert_eq!(error.message(), message);
        }
    }

    #[test]
    fn set_environment_unset_takes_precedence_over_clear() {
        let mode = super::resolve_set_environment_mode(&SetEnvironmentArgs {
            clear: true,
            unset: true,
            ..env_args("AUDIT_VAR")
        })
        .expect("tmux accepts -r and -u together");

        assert_eq!(mode, Some(SetEnvironmentMode::Unset));
    }

    fn assert_set_option_scopes<const N: usize>(
        cases: [(
            &str,
            SetOptionCommandKind,
            SetOptionArgs,
            OptionScopeSelector,
        ); N],
    ) {
        for (case, command, args, expected) in cases {
            let resolved = resolve_request(command, args).expect(case);

            assert_eq!(resolved.scope, expected, "{case}");
        }
    }

    #[test]
    fn set_option_global_scopes_follow_flags_and_option_roots() {
        use OptionScopeSelector::{ServerGlobal, Session, SessionGlobal, WindowGlobal};
        use SetOptionCommandKind::SetOption;

        assert_set_option_scopes([
            (
                "-g server option",
                SetOption,
                global_set_args("message-limit", "77"),
                ServerGlobal,
            ),
            (
                "-g session option",
                SetOption,
                global_set_args("status", "off"),
                SessionGlobal,
            ),
            (
                "-g window option",
                SetOption,
                global_set_args("mode-style", "fg=black,bg=red"),
                WindowGlobal,
            ),
            (
                "-g copy-mode window option",
                SetOption,
                global_set_args("copy-mode-selection-style", "fg=black,bg=cyan"),
                WindowGlobal,
            ),
            (
                "-s is ignored for non-server options like tmux",
                SetOption,
                SetOptionArgs {
                    server: true,
                    ..global_set_args("mode-style", "fg=black,bg=red")
                },
                WindowGlobal,
            ),
            (
                "-s is ignored for a targeted session option",
                SetOption,
                SetOptionArgs {
                    server: true,
                    append: true,
                    ..targeted_set_args("alpha", "status-left", "append")
                },
                Session(alpha()),
            ),
            (
                "explicit -gw still wins",
                SetOption,
                SetOptionArgs {
                    window: true,
                    ..global_set_args("copy-mode-selection-style", "fg=black,bg=cyan")
                },
                WindowGlobal,
            ),
            (
                "-gs uses the option's global root",
                SetOption,
                SetOptionArgs {
                    server: true,
                    ..global_set_args("status", "off")
                },
                SessionGlobal,
            ),
            (
                "-gw uses the option's global root",
                SetOption,
                SetOptionArgs {
                    window: true,
                    ..global_set_args("status", "off")
                },
                SessionGlobal,
            ),
            (
                "-gp uses the option's global root",
                SetOption,
                SetOptionArgs {
                    pane: true,
                    ..global_set_args("status", "off")
                },
                SessionGlobal,
            ),
        ]);
    }

    #[test]
    fn set_option_targeted_scopes_follow_target_and_option_kind() {
        use OptionScopeSelector::{Pane, Session, SessionGlobal, Window};
        use SetOptionCommandKind::{SetOption, SetWindowOption};

        assert_set_option_scopes([
            (
                "explicit -p wins for pane-capable options",
                SetOption,
                SetOptionArgs {
                    pane: true,
                    ..targeted_set_args("alpha:0.1", "window-style", "bg=red")
                },
                Pane(PaneTarget::with_window(alpha(), 0, 1)),
            ),
            (
                "a window-scoped option infers the session target's current window",
                SetOption,
                targeted_set_args("alpha", "remain-on-exit", "on"),
                Window(WindowTarget::new(alpha())),
            ),
            (
                "set-window-option window target",
                SetWindowOption,
                targeted_set_args("alpha:0", "pane-border-style", "fg=colour1"),
                Window(WindowTarget::with_window(alpha(), 0)),
            ),
            (
                "set-window-option keeps a session option's natural scope",
                SetWindowOption,
                targeted_set_args("alpha", "status", "off"),
                Session(alpha()),
            ),
            (
                "set-window-option -g keeps a session option's natural scope",
                SetWindowOption,
                global_set_args("history-limit", "1234"),
                SessionGlobal,
            ),
            (
                "set-window-option session target uses its current window",
                SetWindowOption,
                targeted_set_args("alpha", "pane-border-style", "fg=colour1"),
                Window(WindowTarget::new(alpha())),
            ),
            (
                "set-window-option pane target uses its window",
                SetWindowOption,
                targeted_set_args("alpha:0.1", "pane-border-style", "fg=colour1"),
                Window(WindowTarget::with_window(alpha(), 0)),
            ),
        ]);
    }

    #[test]
    fn set_option_unset_pane_overrides_implies_unset_at_session_scope() {
        // Oracle probe 2026-07-09: plain `set -U` unsets the session copy
        // only; the pane-override sweep applies only when -w selects a
        // window scope.
        let resolved = resolve_request(
            SetOptionCommandKind::SetOption,
            SetOptionArgs {
                unset_pane_overrides: true,
                value: None,
                ..targeted_set_args("alpha:0.1", "@agent.state", "")
            },
        )
        .expect("set-option -U resolves without requiring a value");

        assert_eq!(resolved.scope, OptionScopeSelector::Session(alpha()));
        assert!(resolved.unset);
        assert!(resolved.unset_pane_overrides);
        assert_eq!(resolved.value, None);
    }

    #[test]
    fn show_options_scope_follows_flags_targets_and_option_roots() {
        use OptionScopeSelector::{ServerGlobal, SessionGlobal, WindowGlobal};
        use ShowOptionsCommandKind::{ShowOptions, ShowWindowOptions};
        use ShowOptionsScope::{CurrentPane, CurrentWindow, Resolved};

        let cases = [
            (
                "-g server option",
                ShowOptions,
                show_global_args(Some("message-limit")),
                Resolved(ServerGlobal),
            ),
            (
                "-g session option",
                ShowOptions,
                show_global_args(Some("status")),
                Resolved(SessionGlobal),
            ),
            (
                "-g window option",
                ShowOptions,
                show_global_args(Some("mode-style")),
                Resolved(WindowGlobal),
            ),
            (
                "-g copy-mode window option",
                ShowOptions,
                show_global_args(Some("copy-mode-selection-style")),
                Resolved(WindowGlobal),
            ),
            (
                "-g without a name keeps the session global default",
                ShowOptions,
                show_global_args(None),
                Resolved(SessionGlobal),
            ),
            (
                "-w without a target uses the current window",
                ShowOptions,
                ShowOptionsArgs {
                    global: false,
                    window: true,
                    ..show_global_args(Some("@missing"))
                },
                CurrentWindow,
            ),
            (
                "-p without a target uses the current pane",
                ShowOptions,
                ShowOptionsArgs {
                    global: false,
                    pane: true,
                    ..show_global_args(Some("@missing"))
                },
                CurrentPane,
            ),
            (
                "show-window-options accepts window targets without server scope",
                ShowWindowOptions,
                ShowOptionsArgs {
                    global: false,
                    value_only: true,
                    target: Some(target_spec("alpha:0")),
                    ..show_global_args(Some("pane-border-style"))
                },
                ShowOptionsScope::Unresolved {
                    target: target_spec("alpha:0"),
                    kind: UnresolvedShowOptionsScope::Window,
                },
            ),
            (
                "show-window-options -g uses the window global scope",
                ShowWindowOptions,
                show_global_args(None),
                Resolved(WindowGlobal),
            ),
            (
                "-gsv -t is accepted for target compatibility",
                ShowOptions,
                ShowOptionsArgs {
                    server: true,
                    value_only: true,
                    target: Some(target_spec("missing")),
                    ..show_global_args(Some("message-limit"))
                },
                Resolved(ServerGlobal),
            ),
            (
                "show-window-options -g ignores the target compatibility argument",
                ShowWindowOptions,
                ShowOptionsArgs {
                    value_only: true,
                    target: Some(target_spec("missing")),
                    ..show_global_args(Some("pane-border-style"))
                },
                Resolved(WindowGlobal),
            ),
        ];
        for (case, command, args, expected) in cases {
            let scope = resolve_show_options_scope(command, &args).expect(case);

            assert_eq!(scope, expected, "{case}");
        }
    }

    #[test]
    fn set_option_reports_invalid_option_before_scope_errors() {
        let Err(error) = resolve_request(
            SetOptionCommandKind::SetOption,
            global_set_args("nonexistent", "value"),
        ) else {
            panic!("unknown option should fail")
        };

        assert_eq!(error.message(), "invalid option: nonexistent");
    }
}
