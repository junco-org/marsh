use super::super::{WebShareArgs, WebShareTerminalThemeArg};
use super::*;

/// Parses `rmux web-share <argv>`.
fn web_share(argv: &[&str]) -> WebShareArgs {
    parse_command!(WebShare, [&["web-share"][..], argv].concat())
}

#[test]
fn build_scope_produces_global_or_session_selector() {
    let scope = super::super::build_scope(true, None);
    assert!(matches!(scope, rmux_proto::ScopeSelector::Global));

    let name = rmux_proto::SessionName::new("test").unwrap();
    let scope = super::super::build_scope(false, Some(name.clone()));
    assert!(matches!(scope, rmux_proto::ScopeSelector::Session(n) if n == name));
}

#[test]
fn argumentless_server_commands_parse_and_reject_extra_arguments() {
    let command = parse_args(&["start-server"]).unwrap().command;
    assert!(matches!(command, Some(Command::StartServer(_))));
    let command = parse_args(&["kill-server"]).unwrap().command;
    assert!(matches!(command, Some(Command::KillServer)));
    let command = parse_args(&["lock-server"]).unwrap().command;
    assert!(matches!(command, Some(Command::LockServer)));

    for name in ["start-server", "kill-server", "lock-server"] {
        let error = parse_error(&[name, "extra"]);
        assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{name}");
    }
}

#[test]
fn start_server_accepts_web_listener_flags() {
    let argv = [
        "start-server",
        "--web-port",
        "9778",
        "--frontend-url",
        "https://share.example.com",
    ];
    let args = parse_command!(StartServer, argv);
    assert_eq!(args.web_port, Some(9778));
    assert_eq!(
        args.web_frontend.as_deref(),
        Some("https://share.example.com")
    );
}

#[test]
fn web_share_accepts_subcommand_style_lifecycle_forms_and_off_alias() {
    assert!(web_share(&["list"]).list);
    let args = web_share(&["stop", "abc12345"]);
    assert_eq!(args.stop.as_deref(), Some("abc12345"));
    let args = web_share(&["disconnect", "abc12345"]);
    assert_eq!(args.disconnect.as_deref(), Some("abc12345"));
    assert!(web_share(&["stop", "all"]).stop_all);
    assert!(web_share(&["off"]).stop_all);
}

#[test]
fn web_share_accepts_frontend_and_tunnel_url_flags() {
    let args = web_share(&[
        "--frontend-url",
        "https://ui.example.com/share",
        "--tunnel-url",
        "https://terminal.example.com",
    ]);
    assert_eq!(
        args.frontend_url.as_deref(),
        Some("https://ui.example.com/share")
    );
    assert_eq!(
        args.public_base_url.as_deref(),
        Some("https://terminal.example.com")
    );

    let args = web_share(&["--public-url", "https://terminal.example.com"]);
    assert_eq!(
        args.public_base_url.as_deref(),
        Some("https://terminal.example.com")
    );

    let args = web_share(&["--tunnel-provider", "srv-us"]);
    assert_eq!(args.tunnel_provider.as_deref(), Some("srv-us"));
    let args = web_share(&["--tunnel-provider"]);
    assert_eq!(args.tunnel_provider.as_deref(), Some(""));
}

#[test]
fn web_share_accepts_presentation_and_pairing_opt_out_flags() {
    let args = web_share(&[
        "--no-navbar",
        "--no-disclaimer",
        "--hide-viewers",
        "--theme",
        "user",
        "--no-pin",
    ]);
    assert!(args.no_navbar);
    assert!(args.no_disclaimer);
    assert!(args.hide_viewers);
    assert!(matches!(
        args.terminal_theme,
        Some(WebShareTerminalThemeArg::User)
    ));
    assert!(args.no_pin);

    let args = web_share(&["--terminal-theme", "dark"]);
    assert!(matches!(
        args.terminal_theme,
        Some(WebShareTerminalThemeArg::Dark)
    ));
    assert!(!args.hide_viewers);
    assert!(!args.no_pin);

    for argv in [
        &["--show-viewers", "--pin"][..],
        &["--show-viewer-count", "--pairing-code"][..],
    ] {
        let args = web_share(argv);
        assert!(args.show_viewers, "{argv:?}");
        assert!(args.pin, "{argv:?}");
    }
}

#[test]
fn web_share_accepts_role_pins_restrictions_and_caps() {
    let args = web_share(&["--pin-operator", "123456", "--pin-spectator", "654321"]);
    assert_eq!(args.pin_operator.as_deref(), Some("123456"));
    assert_eq!(args.pin_spectator.as_deref(), Some("654321"));

    let args = web_share(&["--spectator-only", "--max-spectators", "25"]);
    assert!(!args.operator_only);
    assert!(args.spectator_only);
    assert_eq!(args.max_spectators, Some(25));

    let args = web_share(&["--operator-only", "--max-operators", "3"]);
    assert!(args.operator_only);
    assert!(!args.spectator_only);
    assert_eq!(args.max_operators, Some(3));
}

#[test]
fn web_share_accepts_absolute_expiry_and_kill_session_flag() {
    let args = web_share(&[
        "-t",
        "demo",
        "--expires-at",
        "2026-05-26T22:00:00Z",
        "--kill-session-on-expire",
    ]);
    assert_eq!(args.expires_at.as_deref(), Some("2026-05-26T22:00:00Z"));
    assert!(args.kill_session_on_expire);
}

#[test]
fn web_share_rejects_conflicting_tunnel_presentation_pin_and_role_flags() {
    for argv in [
        &[
            "--tunnel-url",
            "https://terminal.example.com",
            "--tunnel-provider",
            "srv-us",
        ][..],
        &["--show-viewers", "--hide-viewers"][..],
        &["--pin", "--no-pin"][..],
        &["--no-pin", "--pin-operator", "123456"][..],
        &["--no-pin", "--pin-spectator", "654321"][..],
        &["--spectator-only", "--pin-operator", "123456"][..],
        &["--operator-only", "--pin-spectator", "654321"][..],
        &["--operator-only", "--spectator-only"][..],
    ] {
        let argv = [&["web-share"][..], argv].concat();
        assert!(parse_args(&argv).is_err(), "{argv:?}");
    }
}

#[test]
fn server_access_parses_mode_flags_and_optional_user() {
    // `-l` accepts a user and the otherwise conflicting flags; a missing user is a runtime error
    // rather than a parse error.
    for (argv, flags, user) in [
        (&["-l"][..], "l", None),
        (&["-l", "alice"][..], "l", Some("alice")),
        (&["-l", "-a", "alice"][..], "al", Some("alice")),
        (&["-l", "-d"][..], "dl", None),
        (
            &["-l", "-a", "-d", "-r", "-w", "alice"][..],
            "adlrw",
            Some("alice"),
        ),
        (&["-a", "alice"][..], "a", Some("alice")),
        (&["-d", "alice"][..], "d", Some("alice")),
        (&["-r", "alice"][..], "r", Some("alice")),
        (&["-r"][..], "r", None),
        (&["-w", "alice"][..], "w", Some("alice")),
        (&["alice"][..], "", Some("alice")),
    ] {
        let args = parse_command!(ServerAccess, [&["server-access"][..], argv].concat());
        let set_flags: String = "adlrw"
            .chars()
            .zip([args.add, args.deny, args.list, args.read_only, args.write])
            .filter_map(|(flag, set)| set.then_some(flag))
            .collect();
        assert_eq!(set_flags, flags, "{argv:?}");
        assert_eq!(args.user.as_deref(), user, "{argv:?}");
    }
}

#[test]
fn server_access_rejects_conflicting_unknown_and_target_flags_like_tmux() {
    for (first, second) in [("-a", "-d"), ("-r", "-w")] {
        let error = parse_error(&["server-access", first, second, "alice"]);
        assert_eq!(
            error.kind(),
            ErrorKind::ArgumentConflict,
            "{first} {second}"
        );
        let expected = format!("{first} and {second} cannot be used together");
        assert!(error.to_string().contains(&expected), "{error}");
    }

    for (argv, message) in [
        (&["-t", "%0", "-l"][..], "unknown flag -t"),
        (&["-t%0", "root"][..], "unknown flag -t"),
        (&["-xt", "root"][..], "unknown flag -x"),
        (&["--target", "%0", "root"][..], "invalid flag --"),
        (&["-"][..], "invalid flag -"),
    ] {
        let error = parse_error(&[&["server-access"][..], argv].concat());
        assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{argv:?}");
        let expected = format!("command server-access: {message}");
        assert!(error.to_string().contains(&expected), "{argv:?}: {error}");
    }
}

#[test]
fn server_access_help_and_completion_omit_rejected_target_flag() {
    let help = parse_error(&["server-access", "--help"]);
    assert_eq!(help.kind(), ErrorKind::DisplayHelp);
    let help = help.to_string();
    assert!(help.lines().any(|line| line.trim_start().starts_with("-a")));
    assert!(help.lines().any(|line| line.trim_start().starts_with("-w")));
    assert!(
        !help.lines().any(|line| line.trim_start().starts_with("-t")),
        "server-access help advertised rejected -t: {help}"
    );

    let completion = super::super::completion_command();
    let server_access = completion
        .get_subcommands()
        .find(|command| command.get_name() == "server-access")
        .expect("server-access completion subcommand");
    let visible_flags = server_access
        .get_arguments()
        .filter(|argument| !argument.is_hide_set())
        .filter_map(|argument| argument.get_short())
        .collect::<Vec<_>>();
    for flag in ['a', 'd', 'l', 'r', 'w'] {
        assert!(
            visible_flags.contains(&flag),
            "missing completion flag -{flag}"
        );
    }
    assert!(
        !visible_flags.contains(&'t'),
        "server-access completion advertised rejected -t"
    );
}

#[test]
fn lock_session_and_lock_client_parse_optional_targets() {
    let args = parse_command!(LockSession, ["lock-session", "-t", "alpha"]);
    assert_eq!(target_text(args.target.as_ref()), "alpha");
    assert!(
        parse_command!(LockSession, ["lock-session"])
            .target
            .is_none()
    );

    let args = parse_command!(LockClient, ["lock-client", "-t", "="]);
    assert_eq!(args.target.as_deref(), Some("="));
    assert_eq!(parse_command!(LockClient, ["lock-client"]).target, None);
}
