use super::{Cli, Command, TargetSpec, parse};
use clap::error::ErrorKind;
use clap::{ArgAction, CommandFactory};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Parses `rmux <argv>` and yields the arguments of its primary command, which must be
/// `Command::$variant`.
macro_rules! parse_command {
    ($variant:ident, $argv:expr) => {
        match $crate::cli_args::tests::parse_args(&$argv)
            .expect("command line parses")
            .command
        {
            Some($crate::cli_args::Command::$variant(args)) => args,
            other => panic!("expected {} command, got {other:?}", stringify!($variant)),
        }
    };
}

/// Parses `rmux <args>` through the public entry point.
fn parse_args(args: &[&str]) -> Result<Cli, clap::Error> {
    parse(std::iter::once("rmux").chain(args.iter().copied()))
}

/// Parses `rmux <args>` and returns the error the command line must be rejected with.
fn parse_error(args: &[&str]) -> clap::Error {
    parse_args(args).expect_err("command line must be rejected")
}

/// Renders a parsed target option, which must be present.
fn target_text(target: Option<&TargetSpec>) -> String {
    target.expect("target").to_string()
}

fn rendered_surface_entry(entry: &rmux_core::command_parser::CommandEntry) -> String {
    match entry.alias {
        Some(alias) => format!("{} ({alias})", entry.name),
        None => entry.name.to_owned(),
    }
}

fn help_dispatch_is_supported(name: &str) -> bool {
    let parsed = super::TmuxCommandParser::new()
        .parse(&format!("{name} --help"))
        .unwrap_or_else(|error| panic!("failed to parse help probe for {name}: {error}"))
        .into_commands()
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("missing parsed command for {name}"));

    match super::command_from_parsed(&parsed) {
        Ok(Command::Unsupported(_)) => false,
        Ok(_) => true,
        Err(error) if error.kind() == ErrorKind::DisplayHelp => true,
        Err(error) => panic!("{name} --help failed dispatch classification: {error}"),
    }
}

#[test]
fn direct_cli_rejects_unknown_options_before_command_tails() {
    for argv in [
        &["wait-for", "-Q"][..],
        &["pipe-pane", "-Q", "true"],
        &["respawn-pane", "-Q", "true"],
        &["bind-key", "-Q", "X", "display-message", "ok"],
        &["display-panes", "-Q", "select-pane -t %%"],
    ] {
        let error = parse_error(argv);
        assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{argv:?}");
        assert!(
            error
                .to_string()
                .contains(&format!("command {}: unknown flag -Q", argv[0])),
            "unexpected direct CLI error for {argv:?}: {error}"
        );
    }

    for argv in [
        &["send-keys", "-x", "C-c"][..],
        &["rename-session", "-x"],
        &["rename-window", "-x"],
        &["set-buffer", "-x", "payload"],
        &["display-message", "-p", "-x"],
        &["set-environment", "-x", "poisoned"],
    ] {
        let error = parse_error(argv);
        assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{argv:?}");
        assert!(
            error.to_string().contains("-x"),
            "unexpected direct CLI error for {argv:?}: {error}"
        );
    }
}

#[test]
fn direct_cli_rejects_new_unknown_option_shapes_before_positionals() {
    for argv in [
        &["set-option", "-x", "value"][..],
        &["set-option", "--bogus", "value"],
        &["set-option", "-gx", "value"],
        &["set-window-option", "-x", "value"],
        &["set-window-option", "--bogus", "value"],
        &["set-window-option", "-gx", "value"],
        &["show-options", "-x"],
        &["show-options", "--bogus"],
        &["show-options", "-gx"],
        &["show-window-options", "-x"],
        &["show-window-options", "--bogus"],
        &["show-window-options", "-gx"],
        &["unbind-key", "-x"],
        &["unbind-key", "--bogus"],
        &["unbind-key", "-nx"],
        &["list-keys", "-x"],
        &["list-keys", "--bogus"],
        &["list-keys", "-ax"],
        &["select-layout", "-x"],
        &["select-layout", "--bogus"],
        &["select-layout", "-nx"],
    ] {
        let error = parse_error(argv);
        assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{argv:?}");
        let rendered = error.to_string();
        assert!(
            rendered.contains("unknown flag") || rendered.contains("unexpected argument"),
            "unexpected direct CLI error for {argv:?}: {rendered}"
        );
    }
}

#[test]
fn direct_cli_stops_parsing_options_after_the_first_positional() {
    for argv in [
        &["rename-session", "renamed", "-t", "audit"][..],
        &["set-buffer", "payload", "-b", "named"][..],
        &["set-option", "status", "off", "-g"][..],
        &["set-environment", "FOO", "BAR", "-g"][..],
        &["list-commands", "list-windows", "-F", "format"][..],
    ] {
        let error = parse_error(argv);
        assert_eq!(
            error.kind(),
            ErrorKind::TooManyValues,
            "unexpected error kind for {argv:?}: {error}"
        );
        assert!(
            error.to_string().contains("too many arguments"),
            "unexpected direct CLI error for {argv:?}: {error}"
        );
    }
}

#[test]
fn direct_cli_consumes_option_like_required_option_values() {
    let args = parse_command!(ListWindows, ["list-windows", "-F", "-tfoo", "-t", "beta"]);
    assert_eq!(args.format.as_deref(), Some("-tfoo"));
    assert_eq!(target_text(args.target.as_ref()), "beta");

    // A literal separator is consumed as the format value.
    let args = parse_command!(ListPanes, ["list-panes", "-F", "--", "-t", "beta:0"]);
    assert_eq!(args.format.as_deref(), Some("--"));
    assert_eq!(target_text(args.target.as_ref()), "beta:0");

    let args = parse_command!(ListSessions, ["list-sessions", "-F", "-Q", "-r"]);
    assert_eq!(args.format.as_deref(), Some("-Q"));
    assert!(args.reversed);

    let args = parse_command!(ListBuffers, ["list-buffers", "-F", "-tfoo", "-r"]);
    assert_eq!(args.format.as_deref(), Some("-tfoo"));
    assert!(args.reversed);

    let args = parse_command!(
        BreakPane,
        ["break-pane", "-F", "-Q", "-s", "alpha:0.0", "-t", "beta:0"]
    );
    assert_eq!(args.format.as_deref(), Some("-Q"));
    assert_eq!(target_text(args.source.as_ref()), "alpha:0.0");
    assert_eq!(target_text(args.target.as_ref()), "beta:0");

    // An option-like token outside a value position remains invalid.
    assert_eq!(
        parse_error(&["list-windows", "-Q"]).kind(),
        ErrorKind::UnknownArgument
    );
}

#[test]
fn direct_cli_keeps_optional_option_values_separate_from_following_flags() {
    let args = parse_command!(ResizePane, ["resize-pane", "-D", "-t", "beta:0.0"]);
    assert_eq!(args.down, Some(1));
    assert_eq!(target_text(args.target.as_ref()), "beta:0.0");
}

#[test]
fn direct_cli_keeps_option_prefix_and_command_tail_semantics() {
    for argv in [
        &["rename-session", "-t", "audit", "renamed"][..],
        &["set-buffer", "-b", "named", "payload"][..],
        &["set-option", "-g", "status", "off"][..],
        &["set-environment", "-g", "FOO", "BAR"][..],
    ] {
        parse_args(argv)
            .unwrap_or_else(|error| panic!("valid option prefix failed for {argv:?}: {error}"));
    }

    // Child command flags remain positional tail values.
    let args = parse_command!(NewSession, ["new-session", "-d", "sh", "-c", "printf ok"]);
    assert_eq!(args.command, ["sh", "-c", "printf ok"]);

    // Recognized flag text after an option or environment name stays a value.
    let args = parse_command!(SetOption, ["set-option", "@tail-value", "-g"]);
    assert!(!args.global);
    assert_eq!(args.value.as_deref(), Some("-g"));

    let args = parse_command!(SetEnvironment, ["set-environment", "TAIL_VALUE", "-g"]);
    assert!(!args.global);
    assert_eq!(args.value.as_deref(), Some("-g"));

    let args = parse_command!(SetOption, ["set-option", "-g", "@compact", "-tfoo"]);
    assert!(args.global);
    assert!(args.target.is_none());
    assert_eq!(args.option, "@compact");
    assert_eq!(args.value.as_deref(), Some("-tfoo"));

    let args = parse_command!(SourceFile, ["source-file", "missing.conf", "-tfoo"]);
    assert!(args.target.is_none());
    assert_eq!(args.paths, ["missing.conf", "-tfoo"]);
}

#[path = "cli_args_tests/session_and_top_level.rs"]
mod session_and_top_level;

#[path = "cli_args_tests/window_commands.rs"]
mod window_commands;

#[path = "cli_args_tests/overlays_and_prompts.rs"]
mod overlays_and_prompts;

#[path = "cli_args_tests/surface_docs.rs"]
mod surface_docs;

#[path = "cli_args_tests/queue_and_window_ops.rs"]
mod queue_and_window_ops;

#[path = "cli_args_tests/pane_layout.rs"]
mod pane_layout;

#[path = "cli_args_tests/pane_io.rs"]
mod pane_io;

#[path = "cli_args_tests/automation.rs"]
mod automation;

#[path = "cli_args_tests/scripting_and_buffers.rs"]
mod scripting_and_buffers;

#[path = "cli_args_tests/options_and_scopes.rs"]
mod options_and_scopes;

#[path = "cli_args_tests/server_lifecycle.rs"]
mod server_lifecycle;
