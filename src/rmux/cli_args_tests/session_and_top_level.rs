use super::super::{TopLevelCommandScan, scan_top_level_command};
use super::*;
use clap::Parser as _;
use std::ffi::{OsStr, OsString};
use std::path::Path;

fn os_args(arguments: &[&str]) -> Vec<OsString> {
    arguments.iter().map(OsString::from).collect()
}

#[test]
fn top_level_scanner_matches_raw_and_public_config_value_boundaries() {
    let raw = TopLevelCommandScan::try_parse_from(["rmux", "-f", "-Ldemo", "claude"])
        .expect("raw clap consumes the hyphenated token as -f's value");
    assert_eq!(raw.config_files, vec![PathBuf::from("-Ldemo")]);
    assert_eq!(raw.command, os_args(&["claude"]));

    let raw = TopLevelCommandScan::try_parse_from(["rmux", "-Lfixed", "-f", "-Ldemo", "claude"])
        .expect("raw clap preserves the first compact token as the command tail");
    assert!(raw.config_files.is_empty());
    assert!(raw.socket_name.is_none());
    assert_eq!(raw.command, os_args(&["-Lfixed", "-f", "-Ldemo", "claude"]));

    let raw_help_kind = TopLevelCommandScan::try_parse_from(["rmux", "-f", "--help", "claude"])
        .expect_err("raw clap rejects --help as a missing -f value")
        .kind();
    assert_eq!(raw_help_kind, ErrorKind::InvalidValue);

    for arguments in [
        &["-f", "-Ldemo", "claude"][..],
        &["-Lfixed", "-f", "-Ldemo", "claude"][..],
        &["-f", "--help", "claude"][..],
    ] {
        let public_kind = parse_error(arguments).kind();
        let scan_kind = scan_top_level_command(&os_args(arguments))
            .expect_err("the extension scanner must reject the same boundary")
            .kind();

        assert_eq!(
            public_kind,
            ErrorKind::InvalidValue,
            "public parse: {arguments:?}"
        );
        assert_eq!(scan_kind, public_kind, "extension scan: {arguments:?}");
    }
}

#[test]
fn new_session_accepts_omitted_session_name() {
    let args = parse_command!(NewSession, ["new-session"]);
    assert_eq!(args.session_name, None);
    assert!(!args.detached);
}

#[test]
fn new_session_accepts_start_directory_flag() {
    let args = parse_command!(
        NewSession,
        ["new-session", "-d", "-s", "alpha", "-c", "/tmp"]
    );
    assert_eq!(
        args.session_name.as_ref().map(ToString::to_string),
        Some("alpha".to_owned())
    );
    assert!(args.detached);
    assert_eq!(args.working_directory.as_deref(), Some("/tmp"));
}

#[test]
fn new_session_accepts_skip_environment_update_flag() {
    let args = parse_command!(NewSession, ["new-session", "-E", "-d", "-s", "alpha"]);
    assert!(args.skip_environment_update);
    assert!(args.detached);
    assert_eq!(
        args.session_name.as_ref().map(ToString::to_string),
        Some("alpha".to_owned())
    );
}

#[test]
fn command_free_invocation_leaves_command_for_default_client_command() {
    let cli = parse_args(&[]).unwrap();

    assert!(cli.command.is_none());
    assert!(cli.control_command_lines().is_empty());
}

#[test]
fn command_free_control_mode_queues_default_new_session_without_explicit_marker() {
    for control_flag in ["-C", "-CC"] {
        let cli = parse_args(&[control_flag]).unwrap();

        assert_eq!(cli.control_command_lines(), &["new-session".to_owned()]);
        assert!(
            cli.command.is_none(),
            "the implicit control command must not look like explicit argv"
        );
    }
}

#[test]
fn top_level_flags_parse_before_the_command() {
    let cli = parse_args(&[
        "-2",
        "-CC",
        "-f",
        "first.conf",
        "-f",
        "second.conf",
        "-l",
        "-L",
        "named",
        "-N",
        "-S",
        "/tmp/rmux.sock",
        "-T",
        "RGB",
        "-u",
        "-vv",
        "list-sessions",
    ])
    .unwrap();

    assert!(cli.assume_256_colors);
    assert_eq!(cli.control_mode, 2);
    assert!(cli.login_shell);
    assert_eq!(cli.socket_name(), Some(OsStr::new("named")));
    assert!(cli.no_start_server);
    assert_eq!(cli.socket_path(), Some(Path::new("/tmp/rmux.sock")));
    assert_eq!(cli.terminal_features(), &["RGB".to_owned()]);
    assert!(cli.utf8);
    assert_eq!(cli.verbose, 2);
    let files = [PathBuf::from("first.conf"), PathBuf::from("second.conf")];
    assert_eq!(
        cli.config_file_selection(),
        super::super::ConfigFileSelection::Custom(&files)
    );
    assert_eq!(cli.control_command_lines(), &["list-sessions".to_owned()]);
    assert!(matches!(cli.command, Some(Command::Noop)));
}

#[test]
fn repeated_idempotent_top_level_flags_match_tmux_3_7b() {
    for (short, compact) in [
        ('2', "-22"),
        ('D', "-DD"),
        ('l', "-ll"),
        ('N', "-NN"),
        ('u', "-uu"),
    ] {
        let separated = format!("-{short}");
        for mut invocation in [vec![separated.as_str(), separated.as_str()], vec![compact]] {
            if short != 'D' {
                invocation.push("list-sessions");
            }

            let cli = parse_args(&invocation)
                .unwrap_or_else(|error| panic!("{invocation:?} should parse: {error}"));
            let cli_switch_is_set = match short {
                '2' => cli.assume_256_colors,
                'D' => cli.no_fork,
                'l' => cli.login_shell,
                'N' => cli.no_start_server,
                'u' => cli.utf8,
                _ => unreachable!("unmeasured top-level switch"),
            };
            assert!(cli_switch_is_set, "{invocation:?}");

            let scan = scan_top_level_command(&os_args(&invocation))
                .unwrap_or_else(|error| panic!("scanner rejected {invocation:?}: {error}"));
            let scan_switch_is_set = match short {
                '2' => scan.assume_256_colors,
                'D' => scan.no_fork,
                'l' => scan.login_shell,
                'N' => scan.no_start_server,
                'u' => scan.utf8,
                _ => unreachable!("unmeasured top-level switch"),
            };
            assert!(scan_switch_is_set, "scanner lost {invocation:?}");

            if short == 'D' {
                assert!(cli.command.is_none());
                assert!(scan.command.is_empty());
            } else {
                assert!(matches!(cli.command, Some(Command::ListSessions(_))));
                assert_eq!(scan.command, [OsString::from("list-sessions")]);
            }
        }
    }
}

#[test]
fn repeated_top_level_flags_preserve_mixed_clusters_and_terminator() {
    let invocation = [
        "-u2lN",
        "-Nlu2",
        "-L",
        "named",
        "-vv",
        "--",
        "list-sessions",
    ];
    let cli = parse_args(&invocation).expect("mixed repeated switches should parse");
    assert!(cli.assume_256_colors);
    assert!(cli.login_shell);
    assert!(cli.no_start_server);
    assert!(cli.utf8);
    assert_eq!(cli.socket_name(), Some(OsStr::new("named")));
    assert_eq!(cli.verbose, 2);
    assert!(matches!(cli.command, Some(Command::ListSessions(_))));

    let scan = scan_top_level_command(&os_args(&invocation))
        .expect("scanner should preserve mixed repeated switches");
    assert!(scan.assume_256_colors);
    assert!(scan.login_shell);
    assert!(scan.no_start_server);
    assert!(scan.utf8);
    assert_eq!(scan.socket_name, Some(OsString::from("named")));
    assert_eq!(scan.verbose, 2);
    assert_eq!(scan.command, [OsString::from("list-sessions")]);

    let foreground = ["-DD", "-uu", "-22", "-ll", "-L", "named", "--"];
    let cli = parse_args(&foreground).expect("repeated foreground switches should parse");
    assert!(cli.no_fork);
    assert!(cli.utf8);
    assert!(cli.assume_256_colors);
    assert!(cli.login_shell);
    assert_eq!(cli.socket_name(), Some(OsStr::new("named")));
    assert!(cli.command.is_none());

    let scan = scan_top_level_command(&os_args(&foreground))
        .expect("scanner should preserve repeated foreground switches");
    assert!(scan.no_fork);
    assert!(scan.utf8);
    assert!(scan.assume_256_colors);
    assert!(scan.login_shell);
    assert_eq!(scan.socket_name, Some(OsString::from("named")));
    assert!(scan.command.is_empty());
}

#[test]
fn top_level_accepts_attached_socket_name_before_the_command() {
    let cli = parse_args(&["-Lnamed", "list-sessions"]).unwrap();
    assert_eq!(cli.socket_name(), Some(OsStr::new("named")));
    assert!(matches!(cli.command, Some(Command::ListSessions(_))));
}

#[test]
fn top_level_preserves_separate_hyphen_prefixed_values() {
    let arguments = [
        "-L",
        "-socket",
        "-S",
        "-path",
        "-T",
        "-feature",
        "list-sessions",
    ];
    let cli = parse_args(&arguments).unwrap();
    assert_eq!(cli.socket_name(), Some(OsStr::new("-socket")));
    assert_eq!(cli.socket_path(), Some(Path::new("-path")));
    assert_eq!(cli.terminal_features(), &["-feature".to_owned()]);
    assert!(matches!(cli.command, Some(Command::ListSessions(_))));

    let scan = scan_top_level_command(&os_args(&arguments))
        .expect("extension scan preserves the same value boundaries");
    assert_eq!(scan.socket_name, Some(OsString::from("-socket")));
    assert_eq!(scan.socket_path, Some(OsString::from("-path")));
    assert_eq!(scan.terminal_features, ["-feature"]);
    assert_eq!(scan.command, [OsString::from("list-sessions")]);
}

#[test]
fn top_level_empty_socket_path_is_preserved_as_explicit_selection() {
    let cli = parse_args(&["-S", "", "list-sessions"]).unwrap();
    assert_eq!(cli.socket_path(), Some(Path::new("")));
    assert!(matches!(cli.command, Some(Command::ListSessions(_))));
}

#[test]
fn single_dash_help_and_version_use_display_exits() {
    assert_eq!(parse_error(&["-h"]).kind(), ErrorKind::DisplayHelp);
    assert_eq!(parse_error(&["-V"]).kind(), ErrorKind::DisplayVersion);
}

#[test]
fn new_session_accepts_attached_short_value_flags() {
    let args = parse_command!(
        NewSession,
        ["new-session", "-P", "-F#{pane_id}", "-sfoo", "-d"]
    );
    assert!(args.detached);
    assert!(args.print_session_info);
    assert_eq!(args.print_format.as_deref(), Some("#{pane_id}"));
    assert_eq!(args.session_name.expect("session name").to_string(), "foo");
    assert!(args.command.is_empty());
}

#[test]
fn new_session_rejects_unknown_flags_before_shell_command() {
    let error = parse_error(&["new-session", "-d", "-Z", "-s", "alpha"]);
    assert_eq!(error.kind(), ErrorKind::UnknownArgument);
    assert!(
        error
            .to_string()
            .contains("command new-session: unknown flag -Z")
    );
}

#[test]
fn session_commands_reject_empty_session_names_and_non_tmux_flags() {
    for (argv, kind) in [
        (&["new-session", "-s", ""][..], ErrorKind::ValueValidation),
        (
            &["switch-client", "-f", "read-only"][..],
            ErrorKind::UnknownArgument,
        ),
        (
            &["rename-session", "-t", "alpha", "-n", "beta"][..],
            ErrorKind::UnknownArgument,
        ),
    ] {
        assert_eq!(parse_error(argv).kind(), kind, "{argv:?}");
    }
}

#[test]
fn command_targets_accept_tmux_last_wins_repetition() {
    let args = parse_command!(
        NewWindow,
        [
            "new-window",
            "-d",
            "-t$1:",
            "-P",
            "-F#{window_id}",
            "-t",
            "$1:",
        ]
    );
    assert!(args.detached);
    assert!(args.print_target);
    assert_eq!(args.format.as_deref(), Some("#{window_id}"));
    assert_eq!(target_text(args.target.as_ref()), "$1:");
}

#[test]
fn new_session_single_value_flags_follow_tmux_last_wins() {
    let args = parse_command!(
        NewSession,
        [
            "new-session",
            "-d",
            "-s",
            "beta",
            "-s",
            "gamma",
            "sleep",
            "1",
        ]
    );
    assert!(args.detached);
    assert_eq!(
        args.session_name.expect("session name").to_string(),
        "gamma"
    );
    assert_eq!(args.command, ["sleep", "1"]);
}

#[test]
fn new_session_sanitizes_colon_and_dot_in_session_name() {
    for name in ["bad:name", "bad.name"] {
        let args = parse_command!(NewSession, ["new-session", "-s", name]);
        assert_eq!(
            args.session_name.expect("session name").to_string(),
            "bad_name",
            "{name:?}"
        );
    }
}

#[test]
fn has_session_accepts_implicit_target_and_exact_marker_in_attached_short_value() {
    for (argv, target) in [
        (&["has-session"][..], None),
        (&["has-session", "-t=foo"][..], Some("=foo")),
    ] {
        let args = parse_command!(HasSession, argv);
        assert_eq!(
            args.target.as_ref().map(TargetSpec::raw),
            target,
            "{argv:?}"
        );
    }
}

#[test]
fn kill_session_requires_target() {
    let args = parse_command!(KillSession, ["kill-session"]);
    assert!(args.target.is_none());
}

#[test]
fn rename_session_and_rename_alias_accept_a_positional_new_name() {
    for command in ["rename-session", "rename"] {
        let args = parse_command!(RenameSession, [command, "-t", "alpha", "beta"]);
        assert_eq!(target_text(args.target.as_ref()), "alpha", "{command}");
        assert_eq!(args.new_name.to_string(), "beta", "{command}");
    }
}

#[test]
fn kill_session_accepts_all_except_and_clear_alerts_flags() {
    let args = parse_command!(KillSession, ["kill-session", "-a", "-C", "-t", "alpha"]);
    assert!(args.kill_all_except_target);
    assert!(args.clear_alerts);
    assert_eq!(target_text(args.target.as_ref()), "alpha");
}

#[test]
fn list_sessions_preserves_equals_in_attached_format_value() {
    let args = parse_command!(ListSessions, ["list-sessions", "-F=#{session_name}"]);
    assert_eq!(args.format.as_deref(), Some("=#{session_name}"));
}
