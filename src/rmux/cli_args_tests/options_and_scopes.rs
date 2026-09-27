use super::super::SetOptionArgs;
use super::*;

/// Parses a `set-option` or `set-window-option` command line, which must resolve to the command
/// it names; both commands carry `SetOptionArgs`.
fn set_option_args(argv: &[&str]) -> SetOptionArgs {
    match parse_args(argv).expect("command line parses").command {
        Some(Command::SetOption(args)) if argv[0] == "set-option" => args,
        Some(Command::SetWindowOption(args)) if argv[0] == "set-window-option" => args,
        other => panic!("expected {} command, got {other:?}", argv[0]),
    }
}

/// Resolves a parsed target option to its statically known exact target.
fn exact_target(target: Option<&TargetSpec>) -> Option<rmux_proto::Target> {
    target.and_then(|target| target.exact().cloned())
}

/// The exact target of pane `alpha:<window>.<pane>`.
fn alpha_pane(window: u32, pane: u32) -> rmux_proto::Target {
    let session = rmux_proto::SessionName::new("alpha").unwrap();
    rmux_proto::Target::Pane(rmux_proto::PaneTarget::with_window(session, window, pane))
}

#[test]
fn option_environment_and_hook_commands_parse_across_scopes_and_expanded_options() {
    for argv in [
        &["show-options"][..],
        &["show-options", "-A", "-t", "alpha", "status"][..],
        &["show-options", "-g"][..],
        &["show-options", "-g", "-t", "alpha", "status"][..],
        &["show-options", "-g", "@status-line"][..],
        &["show-options", "-s"][..],
        &["show-options", "-gs", "-t", "alpha", "message-limit"][..],
        &["show-options", "-gA", "status"][..],
        &["show-options", "-s", "-v", "terminal-features"][..],
        &["show-options", "-gw", "-t", "alpha:2", "pane-border-style"][..],
        &["show-options", "-w", "-t", "alpha:2"][..],
        &["show-options", "-w", "-t", "alpha:2.3"][..],
        &["show-options", "-p", "-t", "alpha:2.3", "-v"][..],
        &["show-options", "-t", "alpha"][..],
        &["show-window-options"][..],
        &["show-window-options", "-t", "alpha:2"][..],
        &["show-window-options", "-g"][..],
        &["show-window-options", "-gv", "synchronize-panes"][..],
        &[
            "show-window-options",
            "-g",
            "-t",
            "alpha",
            "pane-border-style",
        ][..],
        &[
            "show-window-options",
            "-v",
            "-t",
            "alpha:2",
            "synchronize-panes",
        ][..],
        &["show-environment"][..],
        &["show-environment", "-g"][..],
        &["show-environment", "-t", "alpha"][..],
        &["set-environment", "TERM", "screen"][..],
        &["set-environment", "-g", "TERM", "screen"][..],
        &["set-environment", "-t", "alpha", "TERM", "screen"][..],
        &["show-hooks"][..],
        &["set-option", "-g", "base-index", "1"][..],
        &["set-option", "-g", "buffer-limit", "1"][..],
        &["set-option", "-g", "main-pane-width", "1"][..],
        &["set-option", "-g", "status-left", "1"][..],
        &["set-option", "-g", "window-status-current-format", "1"][..],
        &["set-option", "-g", "window-style", "1"][..],
        &[
            "set-option",
            "-w",
            "-t",
            "alpha:2",
            "synchronize-panes",
            "on",
        ][..],
        &[
            "set-window-option",
            "-t",
            "alpha:2",
            "synchronize-panes",
            "on",
        ][..],
        &[
            "set-window-option",
            "-t",
            "alpha",
            "synchronize-panes",
            "on",
        ][..],
    ] {
        parse_args(argv).unwrap_or_else(|error| panic!("{argv:?} must parse: {error}"));
    }
}

#[test]
fn set_option_accepts_default_scope_like_tmux() {
    let args = set_option_args(&["set-option", "status", "off"]);
    assert!(!args.global);
    assert!(!args.server);
    assert!(!args.window);
    assert!(!args.pane);
    assert_eq!(args.target, None);
    assert_eq!(args.option, "status");
    assert_eq!(args.value.as_deref(), Some("off"));
}

#[test]
fn set_option_unset_pane_overrides_accepts_missing_value() {
    let args = set_option_args(&["set-option", "-U", "-t", "alpha:0.1", "@agent.state"]);
    assert!(args.unset_pane_overrides);
    assert!(!args.unset);
    assert_eq!(exact_target(args.target.as_ref()), Some(alpha_pane(0, 1)));
    assert_eq!(args.option, "@agent.state");
    assert_eq!(args.value, None);
}

#[test]
fn set_option_accepts_global_and_target_for_server_scope_compatibility() {
    let args = set_option_args(&["set-option", "-gs", "-t", "alpha", "buffer-limit", "10"]);
    assert!(args.global);
    assert!(args.server);
    let alpha = rmux_proto::SessionName::new("alpha").unwrap();
    assert_eq!(
        exact_target(args.target.as_ref()),
        Some(rmux_proto::Target::Session(alpha))
    );
}

#[test]
fn set_option_accepts_trailing_colon_session_targets_like_tmux() {
    let args = set_option_args(&["set-option", "-t", "alpha:", "status-left", "LEFT"]);
    assert_eq!(target_text(args.target.as_ref()), "alpha:");
}

#[test]
fn set_option_accepts_combined_and_separate_append_and_server_flags() {
    for argv in [
        &[
            "set-option",
            "-as",
            "terminal-features",
            "xterm-256color:RGB",
        ][..],
        &[
            "set-option",
            "-a",
            "-s",
            "terminal-features",
            "xterm-256color:RGB",
        ][..],
    ] {
        let args = set_option_args(argv);
        assert!(args.append, "{argv:?}");
        assert!(args.server, "{argv:?}");
        assert_eq!(args.target, None, "{argv:?}");
    }
}

#[test]
fn set_option_accepts_window_scope_and_optional_value() {
    let args = set_option_args(&["set-option", "-w", "-t", "alpha:2.3", "synchronize-panes"]);
    assert!(args.window);
    assert_eq!(exact_target(args.target.as_ref()), Some(alpha_pane(2, 3)));
    assert_eq!(args.value, None);
}

#[test]
fn set_option_combined_scope_flags_use_tmux_precedence() {
    for (flags, server, window, pane) in [
        ("-sw", true, false, false),
        ("-ws", true, false, false),
        ("-sp", true, false, false),
        ("-ps", true, false, false),
        ("-pw", false, false, true),
        ("-wp", false, false, true),
    ] {
        let args = set_option_args(&["set-option", flags, "status", "off"]);
        assert_eq!(args.server, server, "{flags}");
        assert_eq!(args.window, window, "{flags}");
        assert_eq!(args.pane, pane, "{flags}");
    }
}

#[test]
fn set_option_scope_scanner_stops_at_mid_cluster_target_value() {
    let args = set_option_args(&["set-option", "-wtvps", "@y", "2"]);
    assert!(!args.server);
    assert!(args.window);
    assert!(!args.pane);
    assert_eq!(target_text(args.target.as_ref()), "vps");
    assert_eq!(args.option, "@y");
    assert_eq!(args.value.as_deref(), Some("2"));
}

#[test]
fn set_option_rejects_dash_dash_before_value() {
    let error = parse_error(&["set-option", "-g", "status-left", "--", "-abc"]);
    assert!(
        error
            .to_string()
            .contains("command set-option: too many arguments (need at most 2)"),
        "{error}"
    );
}

#[test]
fn global_set_option_and_set_window_option_keep_option_names_and_literal_values() {
    // Every row ends in `<option> <value>`: a `--` before the option name ends the flags, while a
    // trailing `--` or a hyphen-prefixed word is a literal value.
    for argv in [
        &["set-option", "-g", "--", "status-left", "plain"][..],
        &["set-option", "-g", "status-left", "--"][..],
        &["set-option", "-g", "base-index", "-1"][..],
        &["set-option", "-gF", "@probe", "#{session_name}"][..],
        &["set-window-option", "-g", "pane-border-style", "fg=colour1"][..],
        &["set-window-option", "-g", "main-pane-width", "-5"][..],
    ] {
        let args = set_option_args(argv);
        let (option, value) = (argv[argv.len() - 2], argv[argv.len() - 1]);
        assert!(args.global, "{argv:?}");
        assert_eq!(args.format, argv[1] == "-gF", "{argv:?}");
        assert_eq!(args.option, option, "{argv:?}");
        assert_eq!(args.value.as_deref(), Some(value), "{argv:?}");
    }
}

#[test]
fn show_window_options_parses_as_a_distinct_public_command() {
    let argv = [
        "show-window-options",
        "-v",
        "-t",
        "alpha:2.3",
        "pane-border-style",
    ];
    let args = parse_command!(ShowWindowOptions, argv);
    assert!(args.value_only);
    assert_eq!(args.name.as_deref(), Some("pane-border-style"));
    assert_eq!(exact_target(args.target.as_ref()), Some(alpha_pane(2, 3)));
}

#[test]
fn show_options_accepts_include_hooks() {
    let args = parse_command!(ShowOptions, ["show-options", "-gH"]);
    assert!(args.global);
    assert!(args.include_hooks);
}

#[test]
fn show_options_rejects_conflicting_scope_flags() {
    let error = parse_error(&["show-options", "-s", "-w"]);
    assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
}

#[test]
fn window_option_commands_reject_server_window_pane_inherited_and_hook_flags() {
    for flag in ["-s", "-w", "-p", "-U"] {
        let argv = [
            "set-window-option",
            flag,
            "-t",
            "alpha",
            "synchronize-panes",
            "on",
        ];
        let error = parse_error(&argv);
        assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{argv:?}");
    }
    for flag in ["-s", "-w", "-p"] {
        let argv = ["show-window-options", flag, "-t", "alpha"];
        let error = parse_error(&argv);
        assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{argv:?}");
    }
    for flag in ["-A", "-H"] {
        let error = parse_error(&["show-window-options", flag]);
        let expected = format!("command show-window-options: unknown flag {flag}");
        assert!(error.to_string().contains(&expected), "{error}");
    }
}

#[test]
fn set_environment_accepts_hyphen_prefixed_values() {
    let args = parse_command!(SetEnvironment, ["set-environment", "TERM", "-screen"]);
    assert_eq!(args.name, "TERM");
    assert_eq!(args.value.as_deref(), Some("-screen"));
}

#[test]
fn set_environment_accepts_compact_clear_and_unset_in_either_order() {
    // Measured against the pinned tmux 3.7b oracle on 2026-07-14: `-u`
    // takes precedence over `-r`, independent of their order.
    for flags in ["-gru", "-gur"] {
        let args = parse_command!(SetEnvironment, ["set-environment", flags, "AUDIT_VAR"]);
        assert!(args.global, "{flags}");
        assert!(args.clear, "{flags}");
        assert!(args.unset, "{flags}");
    }
}

#[test]
fn set_hook_accepts_current_pane_and_window_scopes_without_target() {
    for flag in ["-p", "-w"] {
        let args = parse_command!(
            SetHook,
            ["set-hook", flag, "pane-died", "display-message hi"]
        );
        assert_eq!(args.pane, flag == "-p", "{flag}");
        assert_eq!(args.window, flag == "-w", "{flag}");
        assert!(args.target.is_none(), "{flag}");
    }
}

#[test]
fn set_hook_unknown_hook_uses_tmux_error_text() {
    let error = parse_error(&["set-hook", "-g", "no-such-hook", "display hi"]);
    assert!(
        error.to_string().contains("invalid option: no-such-hook"),
        "{error}"
    );
}

#[test]
fn detach_client_rejects_trailing_arguments() {
    let error = parse_error(&["detach-client", "unexpected"]);
    assert_eq!(error.kind(), ErrorKind::UnknownArgument);
}

#[test]
fn detach_client_accepts_session_id_target() {
    let args = parse_command!(DetachClient, ["detach-client", "-s", "$1"]);
    assert_eq!(target_text(args.target_session.as_ref()), "$1");
    assert!(args.target_client.is_none());
}

#[test]
fn unrecognized_subcommand_fails() {
    let error = parse_error(&["bogus-command"]);
    assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
}

#[test]
fn help_produces_display_help_kind() {
    let error = parse_error(&["--help"]);
    assert_eq!(error.kind(), ErrorKind::DisplayHelp);
}

#[test]
fn resize_pane_accepts_target_only_noop_like_tmux() {
    let args = parse_command!(ResizePane, ["resize-pane", "-t", "alpha:0.0"]);
    assert_eq!(target_text(args.target.as_ref()), "alpha:0.0");
    assert!(args.down.is_none());
    assert!(args.up.is_none());
    assert!(args.left.is_none());
    assert!(args.right.is_none());
    assert!(args.columns.is_none());
    assert!(args.rows.is_none());
    assert!(!args.zoom);
}

#[test]
fn resize_pane_valueless_and_explicit_adjustments_follow_tmux_priority_and_composition() {
    // Valueless adjustment flags default to a delta of 1.
    for (delta, relative, absolute, zoom) in [
        (
            1,
            &["resize-pane", "-R", "-L", "-t", "alpha:0.0"][..],
            &["resize-pane", "-R", "-x", "80", "-t", "alpha:0.0"][..],
            &["resize-pane", "-Z", "-R", "-t", "alpha:0.0"][..],
        ),
        (
            5,
            &["resize-pane", "-t", "alpha:0.0", "-R", "-L", "5"][..],
            &["resize-pane", "-t", "alpha:0.0", "-x", "80", "-R", "5"][..],
            &["resize-pane", "-t", "alpha:0.0", "-Z", "-R", "5"][..],
        ),
    ] {
        let args = parse_command!(ResizePane, relative);
        assert_eq!(args.left, Some(delta), "{relative:?}");
        assert_eq!(args.right, None, "{relative:?}");
        let args = parse_command!(ResizePane, absolute);
        assert_eq!(args.right, Some(delta), "{absolute:?}");
        assert!(args.columns.is_some(), "{absolute:?}");
        let args = parse_command!(ResizePane, zoom);
        assert!(args.zoom, "{zoom:?}");
        assert_eq!(args.right, Some(delta), "{zoom:?}");
    }
}

#[test]
fn resize_pane_explicit_relative_deltas_still_reject_too_many_arguments_like_tmux() {
    for argv in [
        ["resize-pane", "-R", "5", "-L", "3", "-t", "alpha:0.0"].as_slice(),
        ["resize-pane", "-R", "5", "-x", "80", "-t", "alpha:0.0"].as_slice(),
    ] {
        let error = parse_error(argv).to_string();
        assert!(
            error.contains("too many arguments")
                || error.contains("unexpected argument")
                || error.contains("accepts only one relative adjustment"),
            "{argv:?}: {error}"
        );
    }
}

#[test]
fn resize_pane_rejects_attached_relative_delta_like_tmux() {
    let error = parse_error(&["resize-pane", "-t", "alpha:0.0", "-R5"]);
    assert_eq!(error.kind(), ErrorKind::UnknownArgument);
    assert!(
        error.to_string().contains("unknown flag -5"),
        "resize-pane -R5 should reject the attached delta like tmux, got {error}"
    );
}

#[test]
fn select_layout_accepts_target_only_noop_like_tmux() {
    let args = parse_command!(SelectLayout, ["select-layout", "-t", "alpha:0"]);
    assert_eq!(target_text(args.target.as_ref()), "alpha:0");
    assert!(args.layout.is_none());
}

#[test]
fn select_layout_preserves_tmux_and_invalid_layout_names_for_runtime_validation() {
    for layout in [
        "even-horizontal",
        "even-vertical",
        "main-horizontal",
        "main-vertical",
        "tiled",
        "invalid-layout",
    ] {
        let args = parse_command!(SelectLayout, ["select-layout", "-t", "alpha:0", layout]);
        assert_eq!(args.layout.as_deref(), Some(layout), "{layout}");
    }
}
