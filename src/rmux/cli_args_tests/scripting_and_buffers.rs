use super::*;
use rmux_proto::WaitForMode;

#[test]
fn display_message_accepts_print_target_and_hyphen_prefixed_format_text_after_separator() {
    let args = parse_command!(
        DisplayMessage,
        [
            "display-message",
            "-p",
            "-t",
            "alpha:0.1",
            "--",
            "-#{session_name}",
        ]
    );
    assert!(args.print);
    assert_eq!(args.target.as_deref(), Some("alpha:0.1"));
    assert_eq!(args.message, vec!["-#{session_name}"]);
}

#[test]
fn display_message_single_value_flags_follow_tmux_last_wins() {
    let args = parse_command!(
        DisplayMessage,
        [
            "display-message",
            "-p",
            "-t",
            "alpha:0.0",
            "-F",
            "#{pane_id}",
            "-F",
            "#{session_name}",
        ]
    );
    assert!(args.print);
    assert_eq!(args.format.as_deref(), Some("#{session_name}"));
}

#[test]
fn display_message_accepts_target_client_without_treating_it_as_message() {
    let args = parse_command!(
        DisplayMessage,
        ["display-message", "-c", "123", "-p", "hello"]
    );
    assert_eq!(args.target_client.as_deref(), Some("123"));
    assert!(args.print);
    assert_eq!(args.message, vec!["hello"]);
}

#[test]
fn display_message_rejects_multiple_message_arguments() {
    let error = parse_error(&["display-message", "a", "b", "c"]);
    assert_eq!(error.kind(), ErrorKind::TooManyValues);
    assert!(
        error
            .to_string()
            .contains("command display-message: too many arguments (need at most 1)")
    );
}

#[test]
fn display_message_accepts_tmux_delay_and_ignore_input_flags() {
    let args = parse_command!(DisplayMessage, ["display-message", "-d0", "-pN", "hello"]);
    assert_eq!(args.delay.as_deref(), Some("0"));
    assert!(args.print);
    assert!(args.ignore_input);
}

#[test]
fn display_message_rejects_invalid_tmux_delays() {
    for (delay, expected) in [
        ("-1", "delay too small"),
        ("4294967296", "delay too large"),
        ("1 ", "delay invalid"),
        ("1.0", "delay invalid"),
    ] {
        let error = parse_error(&["display-message", "-d", delay, "-p", "hello"]);
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
        assert!(
            error.to_string().contains(expected),
            "unexpected error for {delay:?}: {error}"
        );
    }
}

#[test]
fn script_commands_reject_unknown_flags_before_positionals() {
    for (argv, flag) in [
        (&["display-message", "-Q", "hello"][..], "-Q"),
        (&["if-shell", "-Q", "true", "display-message ok"][..], "-Q"),
        (&["run-shell", "-b", "-printf", "ok"][..], "-p"),
        (&["source-file", "-N", "/tmp/missing.conf"][..], "-N"),
    ] {
        let error = parse_error(argv);
        assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{argv:?}");
        assert!(
            error
                .to_string()
                .contains(&format!("command {}: unknown flag {flag}", argv[0])),
            "unexpected error for {argv:?}: {error}"
        );
    }
}

#[test]
fn display_message_json_rejects_queued_only_modes() {
    for mask in 1_u8..16 {
        let mut compact_flags = String::from("-");
        for (bit, flag) in [(1, 'a'), (2, 'I'), (4, 'l'), (8, 'v')] {
            if mask & bit != 0 {
                compact_flags.push(flag);
            }
        }
        for arguments in [
            vec!["display-message", "--json", compact_flags.as_str()],
            vec!["display-message", compact_flags.as_str(), "--json"],
        ] {
            assert_eq!(
                parse_error(&arguments).kind(),
                ErrorKind::ArgumentConflict,
                "{arguments:?}"
            );
        }
    }
}

#[test]
fn display_message_json_keeps_supported_selectors_and_format() {
    let args = parse_command!(
        DisplayMessage,
        [
            "display-message",
            "--json",
            "-C",
            "-c",
            "client",
            "-t",
            "alpha:0.1",
            "-F",
            "#{pane_id}",
        ]
    );
    assert!(args.json);
    assert!(args.no_freeze);
    assert_eq!(args.target_client.as_deref(), Some("client"));
    assert_eq!(args.target.as_deref(), Some("alpha:0.1"));
    assert_eq!(args.format.as_deref(), Some("#{pane_id}"));
}

#[test]
fn run_shell_accepts_stderr_output_and_positional_arguments() {
    let args = parse_command!(
        RunShell,
        ["run-shell", "-CE", "set-buffer -b out #{1}-#{2}", "a", "b"]
    );
    assert!(args.as_commands);
    assert!(args.show_stderr);
    assert_eq!(args.command, vec!["set-buffer -b out #{1}-#{2}", "a", "b"]);
}

#[test]
fn run_shell_accepts_hyphen_prefixed_shell_text_after_separator() {
    let args = parse_command!(RunShell, ["run-shell", "-b", "--", "-printf", "ok"]);
    assert!(args.background);
    assert_eq!(args.command, vec!["-printf", "ok"]);
}

#[test]
fn run_shell_preserves_hyphenated_subcommand_arguments_after_first_token() {
    let args = parse_command!(
        RunShell,
        [
            "run-shell",
            "env",
            "-C",
            "/tmp/example path",
            "touch",
            "name with spaces",
        ]
    );
    assert!(!args.as_commands);
    assert_eq!(
        args.command,
        vec![
            "env",
            "-C",
            "/tmp/example path",
            "touch",
            "name with spaces",
        ]
    );
}

#[test]
fn source_file_accepts_flags_target_and_hyphen_path() {
    let args = parse_command!(
        SourceFile,
        ["source", "-F", "-n", "-q", "-v", "-t", "alpha:0.1", "-"]
    );
    assert!(args.expand_paths);
    assert!(args.parse_only);
    assert!(args.quiet);
    assert!(args.verbose);
    assert_eq!(target_text(args.target.as_ref()), "alpha:0.1");
    assert_eq!(args.paths, vec!["-"]);
}

#[test]
fn source_file_accepts_hyphen_prefixed_path_after_separator() {
    let args = parse_command!(SourceFile, ["source-file", "--", "-N"]);
    assert_eq!(args.paths, vec!["-N"]);
}

#[test]
fn set_buffer_and_show_aliases_accept_tmux_short_forms() {
    let args = parse_command!(SetBuffer, ["setb", "-b", "named", "payload"]);
    assert_eq!(args.name.as_deref(), Some("named"));
    assert_eq!(args.content.as_deref(), Some("payload"));

    let args = parse_command!(ShowBuffer, ["showb", "-b", "named"]);
    assert_eq!(args.name.as_deref(), Some("named"));

    let args = parse_command!(ShowEnvironment, ["showenv", "-t", "alpha"]);
    assert_eq!(target_text(args.target.as_ref()), "alpha");
    assert!(!args.global);

    let args = parse_command!(ShowOptions, ["show", "-gqv"]);
    assert!(args.global);
    assert!(args.quiet);
    assert!(args.value_only);
    assert_eq!(args.target, None);

    assert!(parse_args(&["show-window-options", "-gqv", "pane-border-style"]).is_err());
}

#[test]
fn set_buffer_accepts_target_and_rename_with_trailing_content() {
    let args = parse_command!(SetBuffer, ["set-buffer", "-t", "/dev/pts/8", "payload"]);
    assert_eq!(args.target_client.as_deref(), Some("/dev/pts/8"));
    assert_eq!(args.content.as_deref(), Some("payload"));

    let args = parse_command!(
        SetBuffer,
        ["set-buffer", "-b", "src", "-n", "dst", "ignored"]
    );
    assert_eq!(args.name.as_deref(), Some("src"));
    assert_eq!(args.new_name.as_deref(), Some("dst"));
    assert_eq!(args.content.as_deref(), Some("ignored"));
}

#[test]
fn load_buffer_accepts_target_client() {
    let args = parse_command!(
        LoadBuffer,
        ["load-buffer", "-w", "-t", "/dev/pts/9", "/tmp/input"]
    );
    assert!(args.set_clipboard);
    assert_eq!(args.target_client.as_deref(), Some("/dev/pts/9"));
    assert_eq!(args.path, "/tmp/input");
}

#[test]
fn buffer_commands_accept_compact_hidden_tmux_flags() {
    let args = parse_command!(LoadBuffer, ["load-buffer", "-wbclip", "/tmp/input"]);
    assert!(args.set_clipboard);
    assert_eq!(args.name.as_deref(), Some("clip"));
    assert_eq!(args.path, "/tmp/input");

    let args = parse_command!(ListBuffers, ["list-buffers", "-rF#{buffer_name}"]);
    assert!(args.reversed);
    assert_eq!(args.format.as_deref(), Some("#{buffer_name}"));
}

#[test]
fn set_buffer_requires_double_dash_for_hyphen_prefixed_content() {
    assert!(parse_args(&["set-buffer", "-b", "named", "-world"]).is_err());

    let args = parse_command!(SetBuffer, ["set-buffer", "-b", "named", "--", "-world"]);
    assert_eq!(args.name.as_deref(), Some("named"));
    assert_eq!(args.content.as_deref(), Some("-world"));
}

#[test]
fn if_shell_accepts_format_mode_target_and_optional_else_command() {
    let args = parse_command!(
        IfShell,
        [
            "if-shell",
            "-F",
            "-t",
            "alpha:0.1",
            "#{pane_active}",
            "set-buffer yes",
            "set-buffer no",
        ]
    );
    assert!(args.format_mode);
    assert_eq!(target_text(args.target.as_ref()), "alpha:0.1");
    assert_eq!(args.condition, "#{pane_active}");
    assert_eq!(args.then_command, "set-buffer yes");
    assert_eq!(args.else_command.as_deref(), Some("set-buffer no"));
    assert_eq!(
        args.queue_command,
        "if-shell -F -t alpha:0.1 \"#{pane_active}\" \"set-buffer yes\" \"set-buffer no\""
    );
}

#[test]
fn if_shell_preserves_runtime_resolved_target_syntax() {
    for target in ["alph", "alpha*", "=alpha:", "$1", "@2", "%3", "{mouse}"] {
        let args = parse_command!(
            IfShell,
            [
                "if-shell",
                "-F",
                "-t",
                target,
                "#{pane_active}",
                "set-buffer yes",
            ]
        );
        assert_eq!(args.target.expect("target").raw(), target, "{target:?}");
    }
}

#[test]
fn if_shell_preserves_mouse_target_for_server_queue() {
    let args = parse_command!(
        IfShell,
        [
            "if-shell",
            "-F",
            "-t",
            "{mouse}",
            "1",
            "display-message -p ok",
        ]
    );
    assert_eq!(args.target.expect("target").raw(), "{mouse}");
}

#[test]
fn set_hook_accepts_target_scope_and_indexed_hook() {
    let args = parse_command!(
        SetHook,
        ["set-hook", "-t", "alpha", "client-attached[2]", "true"]
    );
    assert_eq!(target_text(args.target.as_ref()), "alpha");
    assert_eq!(args.hook.hook, rmux_proto::HookName::ClientAttached);
    assert_eq!(args.hook.index, Some(2));
    assert_eq!(args.command.as_deref(), Some("true"));
}

#[test]
fn show_hooks_accepts_global_and_target_scope_flags() {
    let args = parse_command!(ShowHooks, ["show-hooks", "-g", "client-attached"]);
    assert!(args.global);
    assert_eq!(args.hook, Some(rmux_proto::HookName::ClientAttached));
    assert_eq!(args.target, None);

    let args = parse_command!(
        ShowHooks,
        ["show-hooks", "-p", "-t", "alpha:0.1", "client-attached"]
    );
    assert!(args.pane);
    assert_eq!(target_text(args.target.as_ref()), "alpha:0.1");
    assert_eq!(args.hook, Some(rmux_proto::HookName::ClientAttached));
}

#[test]
fn wait_for_and_wait_alias_accept_all_modes() {
    for (argv, mode) in [
        (&["wait-for", "-S", "channel"][..], WaitForMode::Signal),
        (&["wait-for", "-L", "channel"][..], WaitForMode::Lock),
        (&["wait-for", "-U", "channel"][..], WaitForMode::Unlock),
        (&["wait-for", "channel"][..], WaitForMode::Wait),
        (&["wait", "-L", "channel"][..], WaitForMode::Lock),
    ] {
        let args = parse_command!(WaitFor, argv);
        assert_eq!(args.channel, "channel", "{argv:?}");
        assert_eq!(args.mode(), mode, "{argv:?}");
    }
}

#[test]
fn link_window_and_aliases_accept_position_flags_and_optional_source() {
    // Flags are [after, before, detached, kill_target].
    for (argv, flags, source) in [
        (
            &[
                "link-window",
                "-a",
                "-d",
                "-k",
                "-s",
                "alpha:0",
                "-t",
                "beta:1",
            ][..],
            [true, false, true, true],
            Some("alpha:0"),
        ),
        (
            &["link-window", "-b", "-s", "alpha:0", "-t", "beta:1"][..],
            [false, true, false, false],
            Some("alpha:0"),
        ),
        (&["link-window", "-t", "beta:1"][..], [false; 4], None),
        (
            &["link", "-s", "alpha:0", "-t", "beta:1"][..],
            [false; 4],
            Some("alpha:0"),
        ),
        (
            &["linkw", "-s", "alpha:0", "-t", "beta:1"][..],
            [false; 4],
            Some("alpha:0"),
        ),
    ] {
        let args = parse_command!(LinkWindow, argv);
        let actual = [args.after, args.before, args.detached, args.kill_target];
        assert_eq!(actual, flags, "{argv:?}");
        assert_eq!(
            args.source.as_ref().map(TargetSpec::raw),
            source,
            "{argv:?}"
        );
        assert_eq!(target_text(args.target.as_ref()), "beta:1", "{argv:?}");
    }
}

#[test]
fn unlink_window_and_alias_accept_target_and_kill_if_last_flag() {
    for (argv, kill_if_last) in [
        (&["unlink-window", "-k", "-t", "alpha:0"][..], true),
        (&["unlinkw", "-t", "alpha:0"][..], false),
    ] {
        let args = parse_command!(UnlinkWindow, argv);
        assert_eq!(args.kill_if_last, kill_if_last, "{argv:?}");
        assert_eq!(target_text(args.target.as_ref()), "alpha:0", "{argv:?}");
    }
}
