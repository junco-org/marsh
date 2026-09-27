use super::*;

#[test]
fn wait_pane_accepts_one_full_word_condition() {
    let args = parse_command!(
        WaitPane,
        [
            "wait-pane",
            "-t",
            "%1",
            "--visible-text",
            "Ready",
            "--timeout",
            "30s",
        ]
    );
    assert_eq!(target_text(args.target.as_ref()), "%1");
    assert_eq!(args.visible_text.as_deref(), Some("Ready"));
    assert_eq!(args.timeout.expect("timeout").as_secs(), 30);
}

#[test]
fn rmux_extension_commands_are_exact_only_cli_commands() {
    let _ = parse_command!(WaitPane, ["wait-pane", "--pane-exit"]);
    // RMUX extensions have no prefix aliases.
    assert_eq!(
        parse_error(&["wait-p", "--pane-exit"]).kind(),
        ErrorKind::InvalidSubcommand
    );
}

#[test]
fn with_session_accepts_options_before_or_after_session() {
    for (options, ttl_secs) in [
        (["owned", "--kill-on-owner-exit", "--ttl", "45s"], 45),
        (["--ttl", "2m", "--kill-on-owner-exit", "owned"], 120),
    ] {
        let mut argv = vec!["with-session"];
        argv.extend(options);
        argv.extend(["--", "sh", "-c", "true"]);
        let args = parse_command!(WithSession, argv);
        assert_eq!(args.session_name.as_str(), "owned", "{argv:?}");
        assert!(args.kill_on_owner_exit, "{argv:?}");
        assert_eq!(args.ttl.as_secs(), ttl_secs, "{argv:?}");
        assert_eq!(args.command, ["sh", "-c", "true"], "{argv:?}");
    }
}

#[test]
fn with_session_separator_scopes_child_flags() {
    let args = parse_command!(
        WithSession,
        [
            "with-session",
            "owned",
            "--",
            "sh",
            "--kill-on-owner-exit",
            "--ttl",
            "forever",
        ]
    );
    assert!(!args.kill_on_owner_exit);
    assert_eq!(args.ttl.as_secs(), 30);
    assert_eq!(
        args.command,
        ["sh", "--kill-on-owner-exit", "--ttl", "forever"]
    );
}

#[test]
fn automation_commands_reject_invalid_options_conditions_payloads_and_bounds() {
    for argv in [
        &["with-session", "--unknown", "owned", "--", "sh"][..],
        // RMUX-only quiet has no short flag.
        &["wait-pane", "-q"],
    ] {
        let error = parse_error(argv);
        assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{argv:?}");
    }
    for argv in [
        &["with-session", "owned", "--ttl", "forever", "--", "sh"][..],
        // with-session still requires a child command.
        &["with-session", "owned", "--kill-on-owner-exit", "--"],
        // Only one wait condition is allowed.
        &["wait-pane", "--text", "Done", "--quiet"],
        // send-keys wait requires `--` before the payload.
        &[
            "send-keys",
            "-t",
            "%1",
            "--wait",
            "quiet",
            "make test",
            "Enter",
        ],
        // A send-keys timeout requires a wait condition.
        &["send-keys", "-t", "%1", "--timeout", "2s", "--", "Enter"],
        // collect-pane-output requires --until-pane-exit and a positive byte cap.
        &["collect-pane-output", "-t", "%1", "--max-bytes", "1024"],
        &[
            "collect-pane-output",
            "-t",
            "%1",
            "--until-pane-exit",
            "--max-bytes",
            "0",
        ],
    ] {
        let error = parse_error(argv);
        assert_eq!(error.kind(), ErrorKind::ValueValidation, "{argv:?}");
    }
}

#[test]
fn send_keys_wait_keeps_payload_after_separator() {
    let args = parse_command!(
        SendKeys,
        [
            "send-keys",
            "-t",
            "%1",
            "--wait-next-text",
            "__DONE__",
            "--timeout",
            "2m",
            "--",
            "make test",
            "Enter",
        ]
    );
    assert_eq!(target_text(args.target.as_ref()), "%1");
    assert_eq!(args.wait_next_text.as_deref(), Some("__DONE__"));
    assert_eq!(args.timeout.expect("timeout").as_secs(), 120);
    assert_eq!(args.keys, ["make test", "Enter"]);
}

#[test]
fn send_keys_wait_accepts_normal_send_keys_options() {
    let args = parse_command!(
        SendKeys,
        [
            "send-keys",
            "-t",
            "%1",
            "-F",
            "--wait-next-text",
            "DONE",
            "--",
            "#{pane_id}",
            "Enter",
        ]
    );
    assert!(args.expand_formats);
    assert_eq!(args.wait_next_text.as_deref(), Some("DONE"));
    assert_eq!(args.keys, ["#{pane_id}", "Enter"]);
}
