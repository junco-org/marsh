use super::*;

#[test]
fn parser_detects_diagnose_after_socket_flags() {
    let invocation = parse_invocation(&[
        OsString::from("-Ldiag"),
        OsString::from("-Tclipboard,RGB"),
        OsString::from("diagnose"),
        OsString::from("--json"),
    ])
    .expect("parse diagnose")
    .expect("diagnose invocation");

    assert_eq!(invocation.format, DiagnoseFormat::Json);
    assert_eq!(invocation.socket_name, Some(OsString::from("diag")));
    assert_eq!(
        invocation.terminal_features,
        vec!["clipboard".to_owned(), "RGB".to_owned()]
    );
}

#[test]
fn parser_ignores_non_diagnose_commands() {
    assert_eq!(
        parse_invocation(&[OsString::from("new-session")]).expect("parse"),
        None
    );
}

#[test]
fn json_renderer_escapes_strings() {
    assert_eq!(json_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
}

#[test]
fn path_redaction_replaces_home_prefix() {
    let home = PathBuf::from("/tmp/rmux-diagnose-home");

    assert_eq!(
        redact_path_against(&home.join("rmux.conf"), &[home]),
        "~/rmux.conf"
    );
}

#[test]
fn config_message_filter_uses_message_prefix_not_substrings() {
    assert_eq!(
        config_message_from_show_messages_line("123: user text config ignored: not diagnostic"),
        None
    );
    assert_eq!(
        config_message_from_show_messages_line("123: config ignored: old option")
            .expect("config message"),
        "123: config ignored: old option"
    );
    assert_eq!(
        config_message_from_show_messages_line("123: /tmp/not-config:23: user text"),
        None
    );
}

#[test]
fn config_message_filter_includes_source_file_location_diagnostics() {
    assert_eq!(
        config_message_from_show_messages_line("123: /tmp/tmp.abc.in:23: unmatched }")
            .expect("source-file config diagnostic"),
        "123: /tmp/tmp.abc.in:23: unmatched }"
    );
    assert_eq!(
        config_message_from_show_messages_line(
            "123: C:\\Users\\RMUX\\.tmux.conf:16: unknown command: nope"
        )
        .expect("windows source-file config diagnostic"),
        "123: C:\\Users\\RMUX\\.tmux.conf:16: unknown command: nope"
    );
}

#[test]
fn config_message_filter_redacts_home_paths() {
    let home = PathBuf::from("/tmp/rmux-diagnose-home");
    let raw = format!(
        "123: config error: {}:2: unknown command: nope",
        home.join(".tmux.conf").display()
    );

    let redacted =
        config_message_from_show_messages_line_against(&raw, std::slice::from_ref(&home))
            .expect("config message");

    assert!(redacted.contains("~/.tmux.conf"), "{redacted}");
    assert!(
        !redacted.contains(&home.display().to_string()),
        "{redacted}"
    );
}

#[cfg(windows)]
#[test]
fn detected_shell_uses_windows_resolver_module() {
    assert_ne!(detected_shell(), "");
}
