use super::*;
use rmux_proto::SelectPaneDirection::{Down, Left, Right, Up};

#[test]
fn pipe_pane_accepts_bidirectional_and_once_flags() {
    let args = parse_command!(
        PipePane,
        [
            "pipe-pane",
            "-I",
            "-O",
            "-o",
            "-t",
            "alpha:0.1",
            "cat >/tmp/pipe-pane.out",
        ]
    );
    assert!(args.stdin);
    assert!(args.stdout);
    assert!(args.once);
    assert_eq!(target_text(args.target.as_ref()), "alpha:0.1");
    assert_eq!(args.command, ["cat >/tmp/pipe-pane.out"]);
}

#[test]
fn respawn_pane_accepts_kill_directory_environment_and_command() {
    let args = parse_command!(
        RespawnPane,
        [
            "respawn-pane",
            "-k",
            "-c",
            "/tmp/work",
            "-e",
            "FOO=bar",
            "-e",
            "BAR=baz",
            "-t",
            "alpha:0.1",
            "printf",
            "done",
        ]
    );
    assert!(args.kill);
    assert_eq!(args.start_directory, Some(PathBuf::from("/tmp/work")));
    assert_eq!(args.environment, ["FOO=bar", "BAR=baz"]);
    assert_eq!(target_text(args.target.as_ref()), "alpha:0.1");
    assert_eq!(args.command, ["printf", "done"]);
}

#[test]
fn display_panes_accepts_duration_no_command_and_template_flags() {
    let args = parse_command!(
        DisplayPanes,
        [
            "display-panes",
            "-b",
            "-d",
            "250",
            "-N",
            "-t",
            "alpha",
            "select-pane",
            "-t",
            "%%",
        ]
    );
    assert!(args.non_blocking);
    assert_eq!(args.duration_ms, Some(250));
    assert!(args.no_command);
    assert_eq!(args.target_client.as_deref(), Some("alpha"));
    assert_eq!(
        args.template_command().as_deref(),
        Some("select-pane -t %%")
    );
}

#[test]
fn last_pane_preserves_tmux_style_raw_targets() {
    let args = parse_command!(LastPane, ["last-pane", "-d", "-Z", "-t", "alpha:0.1"]);
    assert!(args.disable_input);
    assert!(!args.enable_input);
    assert!(args.keep_zoom);
    assert_eq!(target_text(args.target.as_ref()), "alpha:0.1");
}

#[test]
fn last_pane_accepts_combined_input_flags_like_tmux() {
    for flags in [&["-de"][..], &["-d", "-e"], &["-e", "-d"]] {
        let argv = [&["last-pane"][..], flags].concat();
        let args = parse_command!(LastPane, argv);
        assert!(args.disable_input, "{argv:?}");
        assert!(args.enable_input, "{argv:?}");
    }
}

#[test]
fn kill_pane_accepts_session_all_except_and_implicit_current_targets_like_tmux() {
    for (argv, all_except, target) in [
        (&["kill-pane", "-t", "alpha"][..], false, Some("alpha")),
        (
            &["kill-pane", "-a", "-t", "alpha:0.0"][..],
            true,
            Some("alpha:0.0"),
        ),
        (&["kill-pane"][..], false, None),
    ] {
        let args = parse_command!(KillPane, argv);
        assert_eq!(args.kill_all_except, all_except, "{argv:?}");
        let parsed_target = args.target.as_ref().map(ToString::to_string);
        assert_eq!(parsed_target.as_deref(), target, "{argv:?}");
    }
}

#[test]
fn select_pane_preserves_session_window_and_runtime_resolved_raw_targets() {
    for targets in [
        &["alpha", "alpha:5.2"][..],
        &[
            "%0", "@0", "alpha:.", "alpha:.+", "alpha:.-", ".", ":", ":.+",
        ],
        &["alpha:", "alpha:x.0", "alpha:0.", "alpha:0.-1", ":0"],
    ] {
        for &target in targets {
            let args = parse_command!(SelectPane, ["select-pane", "-t", target]);
            assert_eq!(target_text(args.target.as_ref()), target);
        }
    }
}

#[test]
fn select_pane_accepts_pane_style_flag() {
    let args = parse_command!(
        SelectPane,
        ["select-pane", "-t", "%0", "-P", "bg=blue,fg=white"]
    );
    assert_eq!(target_text(args.target.as_ref()), "%0");
    assert_eq!(args.style.as_deref(), Some("bg=blue,fg=white"));
}

#[test]
fn select_pane_accepts_bare_current_target_like_tmux() {
    let args = parse_command!(SelectPane, ["select-pane"]);
    assert!(args.target.is_none());
    assert!(args.direction().is_none());
    assert!(!args.mark);
    assert!(!args.clear_marked);
    assert!(args.title.is_none());
}

#[test]
fn select_pane_accepts_title_with_and_without_explicit_target() {
    let args = parse_command!(
        SelectPane,
        ["select-pane", "-t", "alpha:5.2", "-T", "build"]
    );
    assert_eq!(target_text(args.target.as_ref()), "alpha:5.2");
    assert_eq!(args.title.as_deref(), Some("build"));

    let args = parse_command!(SelectPane, ["select-pane", "-T", "current-title"]);
    assert!(args.target.is_none());
    assert_eq!(args.title.as_deref(), Some("current-title"));
}

#[test]
fn select_pane_directional_flags_accept_optional_target_and_follow_tmux_priority() {
    for (argv, direction) in [
        (&["select-pane", "-R", "-t", "alpha:5.2"][..], Right),
        (&["select-pane", "-L", "-R", "-t", "alpha:5.2"][..], Left),
        (&["select-pane", "-R", "-L", "-t", "alpha:5.2"][..], Left),
        (&["select-pane", "-U", "-D", "-t", "alpha:5.2"][..], Up),
        (&["select-pane", "-D", "-U", "-t", "alpha:5.2"][..], Up),
    ] {
        let args = parse_command!(SelectPane, argv);
        assert_eq!(target_text(args.target.as_ref()), "alpha:5.2", "{argv:?}");
        assert_eq!(args.direction(), Some(direction), "{argv:?}");
    }

    let args = parse_command!(SelectPane, ["select-pane", "-D"]);
    assert!(args.target.is_none());
    assert_eq!(args.direction(), Some(Down));
}

#[test]
fn select_pane_accepts_last_keep_zoom_and_input_flags() {
    let args = parse_command!(SelectPane, ["select-pane", "-l", "-Z", "-t", "%1"]);
    assert!(args.last);
    assert!(args.keep_zoom);
    assert_eq!(target_text(args.target.as_ref()), "%1");

    for (argv, disable) in [
        (["select-pane", "-d", "-t", "alpha:0.1"], true),
        (["select-pane", "-e", "-t", "alpha:0.1"], false),
    ] {
        let args = parse_command!(SelectPane, argv);
        assert_eq!(args.disable_input, disable, "{argv:?}");
        assert_eq!(args.enable_input, !disable, "{argv:?}");
        assert_eq!(target_text(args.target.as_ref()), "alpha:0.1", "{argv:?}");
    }
}

#[test]
fn select_pane_accepts_mark_without_explicit_target() {
    let args = parse_command!(SelectPane, ["select-pane", "-m"]);
    assert!(args.mark);
    assert!(!args.clear_marked);
    assert!(args.target.is_none());
}

#[test]
fn clock_mode_accepts_optional_pane_target() {
    let args = parse_command!(ClockMode, ["clock-mode", "-t", "alpha:5.2"]);
    assert_eq!(target_text(args.target.as_ref()), "alpha:5.2");
}

#[test]
fn send_keys_accepts_zero_keys() {
    let args = parse_command!(SendKeys, ["send-keys", "-t", "alpha:0.0"]);
    assert!(args.keys.is_empty());
}

#[test]
fn send_keys_accepts_hyphen_prefixed_values() {
    let args = parse_command!(SendKeys, ["send-keys", "-t", "alpha:0.0", "-l", "test"]);
    assert!(args.literal);
    assert_eq!(args.keys, ["test"]);
}

#[test]
fn send_keys_parses_target_client_without_treating_it_as_input() {
    let args = parse_command!(
        SendKeys,
        ["send-keys", "-c", "123", "-t", "alpha:0.0", "Enter"]
    );
    assert_eq!(args.client_target.as_deref(), Some("123"));
    assert_eq!(args.keys, ["Enter"]);
}

#[test]
fn send_keys_marks_unsupported_prefix_flag_before_input_values() {
    let args = parse_command!(SendKeys, ["send-keys", "-p", "-t", "alpha:0.0", "abc"]);
    assert!(args.unsupported_prefix);
    assert_eq!(target_text(args.target.as_ref()), "alpha:0.0");
    assert_eq!(args.keys, ["abc"]);
}

#[test]
fn send_keys_repeated_target_follows_tmux_last_wins() {
    let args = parse_command!(
        SendKeys,
        ["send-keys", "-t", "alpha:0.0", "-t", "alpha:0.1", "Enter"]
    );
    assert_eq!(target_text(args.target.as_ref()), "alpha:0.1");
    assert_eq!(args.keys, ["Enter"]);
}

#[test]
fn capture_pane_accepts_public_command_name_and_flags() {
    let args = parse_command!(
        CapturePane,
        [
            "capture-pane",
            "-t",
            "alpha:0.0",
            "-S",
            "-3",
            "-E",
            "-1",
            "-p",
            "-b",
            "cap",
        ]
    );
    assert_eq!(target_text(args.target.as_ref()), "alpha:0.0");
    assert_eq!(args.start.as_deref(), Some("-3"));
    assert_eq!(args.end.as_deref(), Some("-1"));
    assert!(args.print);
    assert_eq!(args.buffer_name.as_deref(), Some("cap"));
}

#[test]
fn capture_pane_repeated_flags_follow_tmux_last_wins() {
    let args = parse_command!(
        CapturePane,
        [
            "capture-pane",
            "-p",
            "-p",
            "-t",
            "alpha:0.0",
            "-S",
            "0",
            "-S",
            "1",
        ]
    );
    assert!(args.print);
    assert_eq!(args.start.as_deref(), Some("1"));
}

#[test]
fn capture_pane_alias_and_mode_screen_flag_accept_print_mode() {
    for (argv, mode_screen) in [
        (&["capturep", "-p", "-t", "alpha:0.0"][..], false),
        (&["capture-pane", "-M", "-p", "-t", "alpha:0.0"][..], true),
    ] {
        let args = parse_command!(CapturePane, argv);
        assert_eq!(args.use_mode_screen, mode_screen, "{argv:?}");
        assert!(args.print, "{argv:?}");
        assert_eq!(target_text(args.target.as_ref()), "alpha:0.0", "{argv:?}");
    }
}

#[test]
fn copy_mode_accepts_tmux_page_down_and_scrollbar_flags() {
    let args = parse_command!(CopyMode, ["copy-mode", "-dS", "-t", "alpha:0.0"]);
    assert!(args.page_down);
    assert!(args.scrollbar_scroll);
    assert_eq!(target_text(args.target.as_ref()), "alpha:0.0");
}

#[test]
fn pane_commands_accept_session_or_window_targets_like_tmux() {
    let capture = parse_command!(CapturePane, ["capture-pane", "-p", "-t", "alpha"]);
    assert_eq!(target_text(capture.target.as_ref()), "alpha");

    let send_prefix = parse_command!(SendPrefix, ["send-prefix", "-t", "alpha:2"]);
    assert_eq!(target_text(send_prefix.target.as_ref()), "alpha:2");

    let copy_mode = parse_command!(CopyMode, ["copy-mode", "-t", "alpha"]);
    assert_eq!(target_text(copy_mode.target.as_ref()), "alpha");
}
