use super::super::ResizePaneSize;
use super::*;

#[test]
fn new_window_accepts_implicit_target() {
    let args = parse_command!(NewWindow, ["new-window"]);
    assert!(args.target.is_none());
    assert_eq!(args.name, None);
    assert!(!args.detached);
}

#[test]
fn new_window_accepts_name_and_detached_flags() {
    let args = parse_command!(
        NewWindow,
        [
            "new-window",
            "-t",
            "alpha",
            "-n",
            "logs",
            "-d",
            "-c",
            "/tmp/work",
            "--",
            "printf hi",
        ]
    );
    assert_eq!(target_text(args.target.as_ref()), "alpha");
    assert_eq!(args.name.as_deref(), Some("logs"));
    assert!(args.detached);
    assert_eq!(args.start_directory, Some(PathBuf::from("/tmp/work")));
    assert_eq!(args.command, vec!["printf hi".to_owned()]);
}

#[test]
fn new_window_accepts_kill_and_select_existing_flags() {
    let args = parse_command!(
        NewWindow,
        ["new-window", "-t$1:5", "-k", "-S", "-n", "logs"]
    );
    assert_eq!(target_text(args.target.as_ref()), "$1:5");
    assert!(args.kill_existing);
    assert!(args.select_existing);
    assert_eq!(args.name.as_deref(), Some("logs"));
    assert!(args.command.is_empty());
    assert!(args.queue_command.starts_with("new-window "));
    assert!(args.queue_command.contains("-k"));
}

#[test]
fn new_window_k_queue_command_round_trips_literal_metacharacters() {
    let args = parse_command!(
        NewWindow,
        [
            "new-window",
            "-dkP",
            "-F",
            "#{window_index}:#{window_name}",
            "-t",
            "alpha:5",
            "-n",
            "replacement ; literal",
            "-e",
            "RMUX_TEST=value ; literal",
            "--",
            "sh",
            "-c",
            "printf '%s' \"$1\"",
            "sh",
            "argument ; literal",
        ]
    );
    let parsed = rmux_core::command_parser::CommandParser::new()
        .parse(&args.queue_command)
        .expect("canonical new-window command reparses");
    let [command] = parsed.commands() else {
        panic!(
            "literal separators must remain inside one new-window command: {:?}",
            parsed.commands()
        );
    };
    assert_eq!(command.name(), "new-window");
    let arguments = command
        .arguments()
        .iter()
        .filter_map(rmux_core::command_parser::CommandArgument::as_string)
        .collect::<Vec<_>>();
    for literal in [
        "replacement ; literal",
        "RMUX_TEST=value ; literal",
        "printf '%s' \"$1\"",
        "argument ; literal",
    ] {
        assert!(
            arguments.contains(&literal),
            "missing literal {literal:?} in {arguments:?}"
        );
    }
}

#[test]
fn new_window_placement_flags_follow_tmux_priority() {
    for argv in [
        ["new-window", "-a", "-b", "-t", "alpha"],
        ["new-window", "-b", "-a", "-t", "alpha"],
    ] {
        let args = parse_command!(NewWindow, argv);
        assert!(!args.after, "{argv:?}");
        assert!(args.before, "{argv:?}");
    }
}

#[test]
fn respawn_window_accepts_directory_environment_and_command() {
    let args = parse_command!(
        RespawnWindow,
        [
            "respawn-window",
            "-k",
            "-e",
            "FOO=1",
            "-t",
            "alpha:1",
            "-c",
            "/tmp/work",
            "--",
            "sleep",
            "30",
        ]
    );
    assert!(args.kill);
    assert_eq!(args.environment, vec!["FOO=1".to_owned()]);
    assert_eq!(target_text(args.target.as_ref()), "alpha:1");
    assert_eq!(args.start_directory, Some(PathBuf::from("/tmp/work")));
    assert_eq!(args.command, vec!["sleep".to_owned(), "30".to_owned()]);
}

#[test]
fn kill_window_accepts_window_targets_and_kill_others() {
    let args = parse_command!(KillWindow, ["kill-window", "-a", "-t", "alpha:5"]);
    assert_eq!(target_text(args.target.as_ref()), "alpha:5");
    assert!(args.kill_others);
}

#[test]
fn select_window_preserves_raw_targets_and_navigation_flags() {
    // Session-only and negative relative targets stay raw for runtime resolution.
    for (flags, target, expected) in [
        (&[][..], "alpha", [false; 4]),
        (&[][..], "-1", [false; 4]),
        (&["-l"][..], "alpha:1", [true, false, false, false]),
        (&["-n"][..], "alpha:1", [false, true, false, false]),
        (&["-p"][..], "alpha:1", [false, false, true, false]),
        (&["-T"][..], "alpha:1", [false, false, false, true]),
    ] {
        let mut argv = vec!["select-window"];
        argv.extend(flags.iter().chain(&["-t", target]));
        let args = parse_command!(SelectWindow, argv);
        assert_eq!(target_text(args.target.as_ref()), target, "{argv:?}");
        let navigation = [args.last, args.next, args.previous, args.toggle_last];
        assert_eq!(navigation, expected, "{argv:?}");
    }
}

#[test]
fn select_window_accepts_exact_match_targets() {
    let args = parse_command!(SelectWindow, ["select-window", "-t", "=alpha:1"]);
    let target = args.target.as_ref().expect("target");
    assert_eq!(target.to_string(), "=alpha:1");
    assert!(matches!(
        target.exact(),
        Some(rmux_proto::Target::Window(window))
            if window.session_name().as_str() == "alpha" && window.window_index() == 1
    ));
}

#[test]
fn select_window_navigation_flags_follow_tmux_priority() {
    for argv in [
        ["select-window", "-n", "-p", "-t", "alpha:1"],
        ["select-window", "-p", "-n", "-t", "alpha:1"],
    ] {
        let args = parse_command!(SelectWindow, argv);
        assert!(args.next, "{argv:?}");
        assert!(!args.previous, "{argv:?}");
        assert!(!args.last, "{argv:?}");
    }
}

#[test]
fn invalid_window_commands_and_ambiguous_prefixes_fail_like_tmux() {
    for (argv, kind, message) in [
        (
            &["select-window", "-Z"][..],
            ErrorKind::UnknownArgument,
            "command select-window: unknown flag -Z",
        ),
        (
            &["rename-window", "-t", "alpha:0", "logs", "extra"][..],
            ErrorKind::TooManyValues,
            "command rename-window: too many arguments (need at most 1)",
        ),
        (
            &["rename-window"][..],
            ErrorKind::TooFewValues,
            "command rename-window: too few arguments (need at least 1)",
        ),
        (
            &["swap-window", "-a", "-s", "alpha:0", "-t", "alpha:1"][..],
            ErrorKind::UnknownArgument,
            "command swap-window: unknown flag -a",
        ),
        (
            &["list"][..],
            ErrorKind::InvalidSubcommand,
            "ambiguous command: list, could be:",
        ),
    ] {
        let error = parse_error(argv);
        assert_eq!(error.kind(), kind, "{argv:?}");
        assert!(error.to_string().contains(message), "{argv:?}: {error}");
    }
}

#[test]
fn rename_window_accepts_hyphen_prefixed_names() {
    let args = parse_command!(
        RenameWindow,
        ["rename-window", "-t", "alpha:2", "--", "-scratch"]
    );
    assert_eq!(target_text(args.target.as_ref()), "alpha:2");
    assert_eq!(args.new_name, "-scratch");
}

#[test]
fn next_window_accepts_session_targets() {
    let args = parse_command!(NextWindow, ["next-window", "-t", "alpha"]);
    assert_eq!(target_text(args.target.as_ref()), "alpha");
}

#[test]
fn next_window_allows_implicit_current_session_target() {
    let args = parse_command!(NextWindow, ["next-window"]);
    assert!(args.target.is_none());
}

#[test]
fn choose_window_alias_routes_through_mode_tree_queue_command() {
    let args = parse_command!(ChooseTree, ["choose-window"]);
    assert!(args.sessions_collapsed || args.windows_collapsed);
    assert_eq!(args.queue_command, "choose-tree -w");
}

#[test]
fn choose_buffer_parses_as_queued_mode_tree_command() {
    let args = parse_command!(ChooseBuffer, ["choose-buffer", "-NN", "-O", "size"]);
    assert_eq!(args.preview, 2);
    assert_eq!(args.sort_order.as_deref(), Some("size"));
    assert_eq!(args.queue_command, "choose-buffer -NN -O size");
}

#[test]
fn mode_tree_commands_reject_tmux_invalid_auto_accept_flag() {
    for command in ["choose-tree", "choose-buffer", "choose-client"] {
        let error = parse_error(&[command, "-y", "1"]);
        assert!(
            error
                .to_string()
                .contains(&format!("command {command}: unknown flag -y")),
            "{error}"
        );
    }
}

#[test]
fn previous_window_preserves_tmux_style_raw_targets() {
    let args = parse_command!(PreviousWindow, ["previous-window", "-t", "alpha:1"]);
    assert_eq!(target_text(args.target.as_ref()), "alpha:1");
}

#[test]
fn list_windows_accepts_optional_compatibility_format() {
    let args = parse_command!(
        ListWindows,
        ["list-windows", "-t", "alpha", "-F", "#{window_index}"]
    );
    assert_eq!(target_text(args.target.as_ref()), "alpha");
    assert_eq!(args.format.as_deref(), Some("#{window_index}"));
    assert!(args.filter.is_none());
    assert!(!args.all_sessions);
}

#[test]
fn list_windows_accepts_tmux_filter_flag() {
    let args = parse_command!(
        ListWindows,
        [
            "list-windows",
            "-t",
            "$1",
            "-f",
            "#{m:logs*,#{window_name}}",
            "-F",
            "#{window_id}",
        ]
    );
    assert_eq!(target_text(args.target.as_ref()), "$1");
    assert_eq!(args.filter.as_deref(), Some("#{m:logs*,#{window_name}}"));
    assert_eq!(args.format.as_deref(), Some("#{window_id}"));
}

#[test]
fn list_windows_accepts_all_sessions_without_an_explicit_target() {
    let args = parse_command!(ListWindows, ["list-windows", "-a"]);
    assert!(args.all_sessions);
    assert!(args.target.is_none());
    assert!(args.format.is_none());
    assert!(args.filter.is_none());
}

#[test]
fn list_sessions_aliases_and_unique_prefixes_resolve_before_flag_parsing() {
    for command in ["list-sessions", "ls", "list-s"] {
        let args = parse_command!(ListSessions, [command, "-F", "#{session_name}"]);
        assert_eq!(args.format.as_deref(), Some("#{session_name}"), "{command}");
        assert_eq!(args.filter, None, "{command}");
    }
}

#[test]
fn list_sessions_accepts_sort_order_and_reverse() {
    let args = parse_command!(
        ListSessions,
        [
            "list-sessions",
            "-f",
            "#{==:#{session_name},alpha}",
            "-O",
            "index",
            "-r",
        ]
    );
    assert_eq!(args.filter.as_deref(), Some("#{==:#{session_name},alpha}"));
    assert_eq!(args.sort_order.as_deref(), Some("index"));
    assert!(args.reversed);

    assert_eq!(
        parse_error(&["list-sessions", "-O"]).to_string(),
        "error: command list-sessions: -O expects an argument"
    );

    let args = parse_command!(ListSessions, ["list-sessions", "-r"]);
    assert!(args.reversed);
}

#[test]
fn new_session_accepts_print_and_window_name_flags() {
    let args = parse_command!(
        NewSession,
        [
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{session_name}",
            "-n",
            "logs",
            "-s",
            "alpha",
        ]
    );
    assert!(args.detached);
    assert!(args.print_session_info);
    assert_eq!(args.print_format.as_deref(), Some("#{session_name}"));
    assert_eq!(args.window_name.as_deref(), Some("logs"));
    assert_eq!(
        args.session_name.expect("session name").to_string(),
        "alpha"
    );
}

#[test]
fn new_session_accepts_trailing_shell_command() {
    let args = parse_command!(NewSession, ["new-session", "-d", "-s", "alpha", "sleep 30"]);
    assert!(args.detached);
    assert_eq!(
        args.session_name.expect("session name").to_string(),
        "alpha"
    );
    assert_eq!(args.command, vec!["sleep 30"]);
}

#[test]
fn new_session_dimensions_report_short_errors() {
    for (flag, value, expected) in [
        ("-x", "70000", "width too large"),
        ("-x", "abc", "width invalid"),
        ("-y", "70000", "height too large"),
        ("-y", "abc", "height invalid"),
    ] {
        let error = parse_error(&["new-session", flag, value, "-d", "-s", "alpha"]);
        assert!(
            error.to_string().contains(expected),
            "expected {expected:?} in {error}"
        );
    }
}

#[test]
fn pane_size_flags_report_short_errors() {
    for (args, expected) in [
        (
            &["resize-pane", "-x"][..],
            "command resize-pane: -x expects an argument",
        ),
        (
            &["split-window", "-l"][..],
            "command split-window: -l expects an argument",
        ),
        (&["resize-pane", "-D", "abc"][..], "adjustment invalid"),
    ] {
        let error = parse_error(args);
        assert!(
            error.to_string().contains(expected),
            "expected {expected:?} in {error}"
        );
    }
}

#[test]
fn new_window_preserves_target_flags_after_trailing_command_starts() {
    let args = parse_command!(
        NewWindow,
        [
            "new-window",
            "-t",
            "alpha:1",
            "bash",
            "-tc",
            "printf new-window-command",
        ]
    );
    assert_eq!(target_text(args.target.as_ref()), "alpha:1");
    assert_eq!(
        args.command,
        ["bash", "-tc", "printf new-window-command"].map(str::to_owned)
    );
}

#[test]
fn list_commands_parse_with_format_and_optional_target() {
    let args = parse_command!(ListCommands, ["list-commands", "-F", "#{command_name}"]);
    assert_eq!(args.format.as_deref(), Some("#{command_name}"));
    assert_eq!(args.command, None);
}

#[test]
fn resize_window_accepts_expand_and_shrink_flags() {
    for (argv, expand) in [
        (["resize-window", "-A", "-t", "$1:0"], true),
        (["resize-window", "-a", "-t", "$1:0"], false),
    ] {
        let args = parse_command!(ResizeWindow, argv);
        assert_eq!((args.expand, args.shrink), (expand, !expand), "{argv:?}");
        assert_eq!(target_text(args.target.as_ref()), "$1:0", "{argv:?}");
    }
}

#[test]
fn top_level_parse_preserves_hyphenated_split_window_flags() {
    let args = parse_command!(SplitWindow, ["split-window", "-l", "10", "-t", "alpha:0.0"]);
    assert_eq!(args.size.as_deref(), Some("10"));
    let target = args.target.as_ref().expect("target");
    assert_eq!(target.raw(), "alpha:0.0");
    assert_eq!(
        target.exact(),
        Some(&rmux_proto::Target::Pane(
            rmux_proto::PaneTarget::with_window(
                rmux_proto::SessionName::new("alpha").expect("valid session"),
                0,
                0,
            )
        ))
    );
}

#[test]
fn top_level_parse_preserves_hyphenated_resize_pane_flags() {
    let args = parse_command!(ResizePane, ["resize-pane", "-D", "-t", "alpha:0.0"]);
    assert_eq!(args.down, Some(1));
    assert_eq!(target_text(args.target.as_ref()), "alpha:0.0");
}

#[test]
fn resize_pane_accepts_mouse_and_trim_flags() {
    let args = parse_command!(ResizePane, ["resize-pane", "-M", "-T", "-t", "alpha:0.0"]);
    assert!(args.mouse);
    assert!(args.trim_below);
    assert_eq!(target_text(args.target.as_ref()), "alpha:0.0");
}

#[test]
fn resize_pane_trim_flag_accepts_size_flags_like_tmux() {
    let args = parse_command!(
        ResizePane,
        ["resize-pane", "-T", "-y", "5", "-t", "alpha:0.0"]
    );
    assert!(args.trim_below);
    assert!(args.rows.is_some());
}

#[test]
fn resize_pane_rejects_invalid_sizes_and_adjustments_with_short_messages() {
    for (flag, value, message) in [
        ("-x", "-5", "width too small"),
        ("-x", "abc", "width invalid"),
        ("-x", "2147483648", "width too large"),
        ("-y", "-5", "height too small"),
        ("-y", "abc", "height invalid"),
        ("-y", "2147483648", "height too large"),
        ("-D", "0", "adjustment too small"),
        ("-D", "2147483648", "adjustment too large"),
    ] {
        let error = parse_error(&["resize-pane", flag, value]);
        assert_eq!(error.kind(), ErrorKind::ValueValidation, "{flag} {value}");
        assert!(
            error.to_string().contains(message),
            "{flag} {value} should report {message}, got {error}"
        );
    }
}

#[test]
fn resize_pane_clamps_large_absolute_and_relative_sizes() {
    let args = parse_command!(ResizePane, ["resize-pane", "-x", "0", "-t", "alpha:0.0"]);
    assert_eq!(args.columns, Some(ResizePaneSize::Cells(0)));

    let args = parse_command!(
        ResizePane,
        ["resize-pane", "-x", "70000", "-t", "alpha:0.0"]
    );
    assert_eq!(args.columns, Some(ResizePaneSize::Cells(u16::MAX)));

    let args = parse_command!(
        ResizePane,
        ["resize-pane", "-t", "alpha:0.0", "-D", "70000"]
    );
    assert_eq!(args.down, Some(u16::MAX));
}

#[test]
fn resize_pane_accepts_percentage_dimensions() {
    let args = parse_command!(
        ResizePane,
        ["resize-pane", "-x15%", "-y", "20%", "-t", "alpha:0.0"]
    );
    assert_eq!(args.columns, Some(ResizePaneSize::Percent(15)));
    assert_eq!(args.rows, Some(ResizePaneSize::Percent(20)));
    assert_eq!(target_text(args.target.as_ref()), "alpha:0.0");
}

#[test]
fn resize_pane_accepts_tmux_style_space_separated_direction_delta() {
    for argv in [
        ["resize-pane", "-t", "alpha:0.1", "-R", "5"],
        ["resize-pane", "-R", "-t", "alpha:0.1", "5"],
    ] {
        let args = parse_command!(ResizePane, argv);
        assert_eq!(args.right, Some(5), "{argv:?}");
        assert_eq!(target_text(args.target.as_ref()), "alpha:0.1", "{argv:?}");
    }
}

#[test]
fn resize_pane_valueless_relative_before_absolute_composes_like_tmux() {
    let args = parse_command!(ResizePane, ["resize-pane", "-U", "-x", "10"]);
    assert_eq!(args.up, Some(1));
    assert_eq!(args.columns, Some(ResizePaneSize::Cells(10)));
}

#[test]
fn resize_pane_rejects_direction_delta_before_later_flags_like_tmux() {
    let error = parse_error(&["resize-pane", "-R", "5", "-t", "alpha:0.1"]);
    assert_eq!(error.kind(), ErrorKind::UnknownArgument);

    let error = parse_error(&["resize-pane", "-R=5", "-t", "alpha:0.1"]);
    assert_eq!(error.kind(), ErrorKind::UnknownArgument);
}

#[test]
fn queued_and_gated_commands_use_clap_help() {
    for command in [
        "command-prompt",
        "choose-tree",
        "clear-prompt-history",
        "display-menu",
        "display-popup",
        "link-window",
        "show-prompt-history",
        "unlink-window",
        "set-window-option",
        "show-window-options",
    ] {
        let error = parse_error(&[command, "--help"]);
        assert_eq!(error.kind(), ErrorKind::DisplayHelp, "{command}");
    }
}
