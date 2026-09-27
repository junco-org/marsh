use super::*;

/// Parses `rmux <argv>` with a runtime snapshot whose command groups are these canonical lines.
fn parse_with_canonical_groups(argv: &[&str], groups: &[&str]) -> Result<Cli, clap::Error> {
    let groups = groups
        .iter()
        .map(|&group| super::super::RuntimeCommandGroup::Canonical(group.to_owned()))
        .collect::<Vec<_>>();
    super::super::parse_with_runtime_command_groups(
        std::iter::once("rmux").chain(argv.iter().copied()),
        &groups,
    )
}

#[test]
fn argv_semicolons_build_an_ordered_command_queue() {
    let cli = parse_args(&["list-sessions;", "display-message", "-p", "ok"]).unwrap();
    let commands = cli.into_command_queue();

    assert_eq!(commands.len(), 2);
    assert!(matches!(&commands[0], Command::ListSessions(_)));
    assert!(matches!(&commands[1], Command::DisplayMessage(_)));
}

#[test]
fn standalone_argv_semicolon_builds_an_ordered_command_queue() {
    let cli = parse_args(&["attach-session", "-t", "alpha", ";", "detach-client"]).unwrap();
    let commands = cli.into_command_queue();

    assert_eq!(commands.len(), 2);
    assert!(matches!(&commands[0], Command::AttachSession(_)));
    assert!(matches!(&commands[1], Command::DetachClient(_)));
}

#[test]
fn argv_command_queue_rejects_refresh_client_pan_fields() {
    for args in [
        &["list-sessions", ";", "refresh-client", "-L", "10"][..],
        &["list-sessions", ";", "refresh-client", "10"][..],
    ] {
        let error = parse_error(args);
        assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{args:?}");
    }
}

#[test]
fn target_client_and_hook_flags_survive_argv_queue_parsing() {
    let cli = parse_args(&[
        "load-buffer",
        "-wt",
        "/dev/pts/10",
        "/tmp/input",
        ";",
        "show-options",
        "-gH",
    ])
    .unwrap();
    let commands = cli.into_command_queue();

    match &commands[0] {
        Command::LoadBuffer(args) => {
            assert!(args.set_clipboard);
            assert_eq!(args.target_client.as_deref(), Some("/dev/pts/10"));
        }
        other => panic!("expected load-buffer, got {other:?}"),
    }
    match &commands[1] {
        Command::ShowOptions(args) => assert!(args.include_hooks),
        other => panic!("expected show-options, got {other:?}"),
    }
}

#[test]
fn trailing_semicolon_after_send_keys_payload_is_a_queue_separator() {
    let error = parse_error(&["send-keys", "-t", "alpha:0.0", "xyz;", "final"]);
    assert!(
        error.to_string().contains("unknown command: final"),
        "{error}"
    );
}

#[test]
fn bare_semicolon_builds_a_noop_command_queue() {
    let commands = parse_args(&[";"]).unwrap().into_command_queue();

    assert_eq!(commands.len(), 1);
    assert!(matches!(&commands[0], Command::Noop));
}

#[test]
fn runtime_canonical_queue_keeps_every_group_on_the_typed_path() {
    let cli = parse_with_canonical_groups(
        &[
            "list-sessions",
            ";",
            "display-message",
            "-p",
            "unalias",
            ";",
            "ls",
        ],
        &[
            "display-message -p canonical",
            "display-message -p unalias ; display-message -p short",
        ],
    )
    .unwrap();
    let commands = cli.into_command_queue();

    assert!(matches!(
        &commands[0],
        Command::DisplayMessage(args) if args.message == ["canonical".to_owned()]
    ));
    assert!(matches!(&commands[1], Command::DisplayMessage(_)));
    assert!(matches!(
        &commands[2],
        Command::DisplayMessage(args) if args.message == ["short".to_owned()]
    ));
}

#[test]
fn runtime_canonical_queue_preserves_reparse_escaped_literals() {
    let literal = "space ; dollar $HOME slash\\ quote' double\"";
    let rendered = rmux_core::command_parser::CommandArgument::String(literal.to_owned())
        .to_tmux_reparse_string();
    let cli = parse_with_canonical_groups(
        &["list-sessions", literal],
        &[&format!("display-message -p -- {rendered}")],
    )
    .unwrap();
    let commands = cli.into_command_queue();

    assert!(matches!(
        &commands[0],
        Command::DisplayMessage(args) if args.message == [literal.to_owned()]
    ));
}

#[test]
fn runtime_canonical_queue_preserves_server_expanded_control_characters() {
    let expected = "line1\nline2\rline3\t$slash\\quote\"`";
    let rendered = rmux_core::command_parser::CommandArgument::String(expected.to_owned())
        .to_tmux_reparse_string();
    let cli = parse_with_canonical_groups(
        &["list-sessions"],
        &[&format!("display-message -p {rendered}")],
    )
    .unwrap();
    let commands = cli.into_command_queue();

    assert!(matches!(
        &commands[0],
        Command::DisplayMessage(args) if args.message == [expected.to_owned()]
    ));
}

#[test]
fn runtime_canonical_extension_preserves_trailing_semicolon_literals() {
    let cli = parse_with_canonical_groups(
        &["list-sessions", ";", "display-message", "-p", "semi\\;"],
        &[
            "display-message -p alias",
            "find-sessions --name \"semi\\;\"",
        ],
    )
    .unwrap();
    let commands = cli.into_command_queue();

    assert!(matches!(
        &commands[1],
        Command::FindSessions(args) if args.name.as_deref() == Some("semi;")
    ));
}

#[test]
fn runtime_alias_assignments_are_applied_before_the_validated_command() {
    let cli = parse_with_canonical_groups(&["zz"], &["FOO=bar ; display-message -p bar"])
        .expect("runtime alias assignment parses");
    assert!(matches!(cli.command, Some(Command::DisplayMessage(_))));

    let commands = cli.into_command_queue();
    assert!(matches!(
        &commands[0],
        Command::ApplyParseTimeAssignments(assignments) if assignments == "FOO=bar"
    ));
    assert!(matches!(&commands[1], Command::DisplayMessage(_)));
}

#[test]
fn invalid_runtime_alias_tail_is_rejected_before_assignments_can_dispatch() {
    let error = parse_with_canonical_groups(&["zz"], &["FOO=bar ; new-window -Q"])
        .expect_err("invalid canonical tail must fail typed validation");

    let message = error.to_string();
    assert!(message.contains("new-window"), "{message}");
    assert!(message.contains("-Q"), "{message}");
}

#[test]
fn runtime_command_groups_use_the_preparsed_snapshot() {
    let cli = parse_with_canonical_groups(
        &[
            "set-option",
            "-s",
            "command-alias[0]",
            "list-sessions=display-message -p new",
            ";",
            "list-sessions",
        ],
        &[
            "set-option -s command-alias[0] 'list-sessions=display-message -p new' ; \
             display-message -p old",
        ],
    )
    .unwrap();
    let commands = cli.into_command_queue();

    assert!(matches!(&commands[0], Command::SetOption(_)));
    assert!(matches!(
        &commands[1],
        Command::DisplayMessage(args) if args.message == ["old".to_owned()]
    ));
}

#[test]
fn runtime_canonical_reparse_does_not_apply_builtin_aliases_twice() {
    let error = parse_with_canonical_groups(
        &["split-pane", "-d", "-t", "alpha:0.0"],
        &["split-pane -d -t alpha:0.0"],
    )
    .unwrap_err();
    assert!(error.to_string().contains("unknown command: split-pane"));

    let cli = parse(["rmux", "split-pane", "-d", "-t", "alpha:0.0"])
        .expect("an absent server keeps built-in defaults");
    assert!(matches!(cli.command, Some(Command::SplitWindow(_))));
}

#[test]
fn runtime_canonical_terminal_commands_stay_on_typed_dispatch_paths() {
    let attach =
        parse_with_canonical_groups(&["list-sessions"], &["attach-session -t alpha"]).unwrap();
    assert!(matches!(attach.command, Some(Command::AttachSession(_))));

    let kill = parse_with_canonical_groups(&["list-sessions"], &["kill-server"]).unwrap();
    assert!(matches!(kill.command, Some(Command::KillServer)));
}

#[test]
fn runtime_canonical_reparse_rejects_an_unexpanded_alias_name() {
    let error = parse_with_canonical_groups(&["foo"], &["bar"]).unwrap_err();
    assert!(error.to_string().contains("unknown command: bar"));
}

#[test]
fn list_keys_accepts_tmux_sort_format_and_reverse_flags() {
    let args = parse_command!(ListKeys, ["list-keys", "-r", "-F", "#{key_table}", "-Okey"]);
    assert!(args.reversed);
    assert_eq!(args.format.as_deref(), Some("#{key_table}"));
    assert_eq!(args.sort_order.as_deref(), Some("key"));
}

#[test]
fn command_arguments_reject_invalid_utf8_without_lossy_replacement() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let error = parse(vec![
        OsString::from("rmux"),
        OsString::from("display-message"),
        OsString::from_vec(vec![0xff]),
    ])
    .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::InvalidUtf8);
    assert!(error.to_string().contains("invalid UTF-8"));
}

#[test]
fn move_window_accepts_reindex_with_a_session_target() {
    let args = parse_command!(MoveWindow, ["move-window", "-r", "-t", "alpha"]);
    assert!(args.reindex);
    assert_eq!(args.source, None);
    assert_eq!(target_text(args.target.as_ref()), "alpha");
}

#[test]
fn move_window_accepts_reindex_without_a_target() {
    let args = parse_command!(MoveWindow, ["move-window", "-r"]);
    assert!(args.reindex);
    assert_eq!(args.source, None);
    assert_eq!(args.target, None);
}

#[test]
fn move_window_accepts_reindex_with_source_for_tmux_compatibility() {
    let args = parse_command!(MoveWindow, ["move-window", "-r", "-s", "$1:2", "-t", "$1"]);
    assert!(args.reindex);
    assert_eq!(target_text(args.source.as_ref()), "$1:2");
    assert_eq!(target_text(args.target.as_ref()), "$1");
}

#[test]
fn move_window_accepts_source_destination_and_kill_flags() {
    let args = parse_command!(
        MoveWindow,
        ["move-window", "-k", "-d", "-s", "alpha:2", "-t", "beta:5"]
    );
    assert!(!args.reindex);
    assert!(args.kill_target);
    assert!(args.detached);
    assert_eq!(target_text(args.source.as_ref()), "alpha:2");
    assert_eq!(target_text(args.target.as_ref()), "beta:5");
}

#[test]
fn move_window_placement_flags_follow_tmux_priority() {
    for argv in [
        ["move-window", "-a", "-b", "-s", "alpha:2", "-t", "beta:5"],
        ["move-window", "-b", "-a", "-s", "alpha:2", "-t", "beta:5"],
    ] {
        let args = parse_command!(MoveWindow, argv);
        assert!(!args.after, "{argv:?}");
        assert!(args.before, "{argv:?}");
    }
}

#[test]
fn move_window_accepts_implicit_source_and_relative_destination() {
    let args = parse_command!(MoveWindow, ["move-window", "-t", "-1"]);
    assert!(!args.reindex);
    assert!(args.source.is_none());
    assert_eq!(target_text(args.target.as_ref()), "-1");
}

#[test]
fn move_window_accepts_position_flags_and_id_targets() {
    for (argv, after, source) in [
        (["move-window", "-a", "-s", "@1", "-t", "$2:0"], true, "@1"),
        (
            ["move-window", "-b", "-s", "alpha:1", "-t", "$2:0"],
            false,
            "alpha:1",
        ),
    ] {
        let args = parse_command!(MoveWindow, argv);
        assert_eq!((args.after, args.before), (after, !after), "{argv:?}");
        assert_eq!(target_text(args.source.as_ref()), source, "{argv:?}");
        assert_eq!(target_text(args.target.as_ref()), "$2:0", "{argv:?}");
    }
}

#[test]
fn swap_window_preserves_session_targets_for_runtime_resolution() {
    let args = parse_command!(SwapWindow, ["swap-window", "-s", "alpha", "-t", "beta:1"]);
    assert_eq!(target_text(args.source.as_ref()), "alpha");
    assert_eq!(target_text(args.target.as_ref()), "beta:1");
}

#[test]
fn swap_window_accepts_implicit_source() {
    let args = parse_command!(SwapWindow, ["swap-window", "-t", "beta:1"]);
    assert!(args.source.is_none());
    assert_eq!(target_text(args.target.as_ref()), "beta:1");
}

#[test]
fn swap_window_compact_flags_stop_at_the_first_value_flag_like_tmux() {
    let args = parse_command!(SwapWindow, ["swap-window", "-ds", "alpha:0", "-tbeta:1"]);
    assert!(args.detached);
    assert_eq!(target_text(args.source.as_ref()), "alpha:0");
    assert_eq!(target_text(args.target.as_ref()), "beta:1");

    let args = parse_command!(SwapWindow, ["swap-window", "-sd", "-t", "alpha:1"]);
    assert!(!args.detached, "-sd is -s d, not -s plus -d");
    assert_eq!(target_text(args.source.as_ref()), "d");
    assert_eq!(target_text(args.target.as_ref()), "alpha:1");
}

#[test]
fn rotate_window_defaults_to_up_direction() {
    let args = parse_command!(RotateWindow, ["rotate-window", "-t", "alpha:2"]);
    assert_eq!(target_text(args.target.as_ref()), "alpha:2");
    assert_eq!(args.direction(), rmux_proto::RotateWindowDirection::Up);
}

#[test]
fn rotate_window_accepts_zoom_restore_flag() {
    let args = parse_command!(RotateWindow, ["rotate-window", "-Z", "-t", "alpha:2"]);
    assert_eq!(target_text(args.target.as_ref()), "alpha:2");
    assert!(args.restore_zoom);
}

#[test]
fn rotate_window_rejects_both_directions() {
    let error = parse_error(&["rotate-window", "-D", "-U", "-t", "alpha:2"]);
    assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
}

#[test]
fn next_window_accepts_alert_navigation_flag() {
    let args = parse_command!(NextWindow, ["next-window", "-a", "-t", "alpha"]);
    assert!(args.alerts_only);
    assert_eq!(target_text(args.target.as_ref()), "alpha");
}

#[test]
fn show_messages_accepts_tmux_flags() {
    let args = parse_command!(ShowMessages, ["show-messages", "-J", "-T", "-t", "="]);
    assert!(args.jobs);
    assert!(args.terminals);
    assert_eq!(args.target_client.as_deref(), Some("="));
}
