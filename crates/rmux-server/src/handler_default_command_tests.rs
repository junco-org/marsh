use std::fs;
use std::path::{Path, PathBuf};

use rmux_core::command_parser::CommandParser;
use rmux_proto::{
    BindKeyRequest, NewWindowRequest, OptionName, PaneTarget, ProcessCommand, RespawnPaneRequest,
    RespawnWindowRequest, ScopeSelector, SessionName, SourceFileRequest, SplitDirection,
    SplitWindowRequest, WindowTarget,
};

use super::RequestHandler;

use crate::test_fixtures::{unique_temp_path, Fixture, Grouped};
use crate::test_names::session_name;

fn tagged_stdin_discard_command(tag: &str) -> String {
    format!("cat >/dev/null # {tag}")
}

fn expected_spawn_cwd(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

async fn pane_process_command(
    handler: &RequestHandler,
    target: &PaneTarget,
) -> Option<ProcessCommand> {
    let state = handler.state.lock().await;
    let pane_id = state
        .sessions
        .session(target.session_name())
        .and_then(|session| session.pane_id_in_window(target.window_index(), target.pane_index()))
        .expect("target pane exists");
    state
        .pane_lifecycle(pane_id)
        .expect("target pane lifecycle exists")
        .process_command()
        .cloned()
}

async fn assert_default_command_on_new_and_split(
    handler: &RequestHandler,
    session: &SessionName,
    expected: &str,
) {
    let new_window = PaneTarget::with_window(session.clone(), 1, 0);
    let split = PaneTarget::with_window(session.clone(), 0, 1);
    assert_eq!(
        pane_process_command(handler, &new_window).await,
        Some(ProcessCommand::Shell(expected.to_owned()))
    );
    assert_eq!(
        pane_process_command(handler, &split).await,
        Some(ProcessCommand::Shell(expected.to_owned()))
    );
}

#[tokio::test]
async fn sdk_new_and_split_resolve_default_command_for_the_addressed_session() {
    let handler = RequestHandler::new();
    let owner = session_name("default-command-owner");
    let alias = session_name("default-command-alias");
    let fallback = session_name("default-command-fallback");
    handler.create_session(&owner).await;
    handler.create_session(Grouped(&alias, &owner)).await;
    handler.create_session(&fallback).await;

    let global_command = tagged_stdin_discard_command("global");
    let owner_command = tagged_stdin_discard_command("owner");
    let alias_command = tagged_stdin_discard_command("alias");
    handler
        .set_option(
            ScopeSelector::Global,
            OptionName::DefaultCommand,
            &global_command,
        )
        .await;
    handler
        .set_option(
            ScopeSelector::Session(owner.clone()),
            OptionName::DefaultCommand,
            &owner_command,
        )
        .await;
    handler
        .set_option(
            ScopeSelector::Session(alias.clone()),
            OptionName::DefaultCommand,
            &alias_command,
        )
        .await;

    let window = handler.create_window(&owner).await;
    let owner_window = PaneTarget::with_window(owner.clone(), window.window_index(), 0);
    assert_eq!(
        pane_process_command(&handler, &owner_window).await,
        Some(ProcessCommand::Shell(owner_command))
    );

    let window = handler.create_window(&alias).await;
    let alias_window = PaneTarget::with_window(alias.clone(), window.window_index(), 0);
    assert_eq!(
        pane_process_command(&handler, &alias_window).await,
        Some(ProcessCommand::Shell(alias_command))
    );

    let split = handler
        .handle_ok(SplitWindowRequest::fixture(owner_window))
        .await
        .pane;
    assert_eq!(
        pane_process_command(&handler, &split).await,
        Some(ProcessCommand::Shell(tagged_stdin_discard_command("owner")))
    );

    let window = handler.create_window(&fallback).await;
    let fallback_window = PaneTarget::with_window(fallback, window.window_index(), 0);
    assert_eq!(
        pane_process_command(&handler, &fallback_window).await,
        Some(ProcessCommand::Shell(global_command))
    );
}

#[tokio::test]
async fn sdk_explicit_command_wins_and_local_empty_masks_global_default_command() {
    let handler = RequestHandler::new();
    let alpha = session_name("default-command-explicit");
    let masked = session_name("default-command-masked");
    handler.create_session(&alpha).await;
    handler.create_session(&masked).await;
    handler
        .set_option(
            ScopeSelector::Global,
            OptionName::DefaultCommand,
            &tagged_stdin_discard_command("global"),
        )
        .await;
    handler
        .set_option(
            ScopeSelector::Session(masked.clone()),
            OptionName::DefaultCommand,
            "",
        )
        .await;

    let explicit = ProcessCommand::Shell(tagged_stdin_discard_command("explicit"));
    let window = handler
        .create_window(NewWindowRequest {
            process_command: Some(explicit.clone()),
            ..Fixture::fixture(alpha)
        })
        .await;
    let explicit_target =
        PaneTarget::with_window(window.session_name().clone(), window.window_index(), 0);
    assert_eq!(
        pane_process_command(&handler, &explicit_target).await,
        Some(explicit)
    );

    let explicit_session = explicit_target.session_name().clone();
    handler
        .set_option(
            ScopeSelector::Session(explicit_session.clone()),
            OptionName::RemainOnExit,
            "on",
        )
        .await;
    let window = handler
        .create_window(NewWindowRequest {
            command: Some(vec![String::new()]),
            ..Fixture::fixture(explicit_session)
        })
        .await;
    let empty_target =
        PaneTarget::with_window(window.session_name().clone(), window.window_index(), 0);
    assert_eq!(
        pane_process_command(&handler, &empty_target).await,
        Some(ProcessCommand::Shell(String::new()))
    );

    let masked_target = handler
        .handle_ok(SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(masked)
        })
        .await
        .pane;
    assert_eq!(pane_process_command(&handler, &masked_target).await, None);
}

#[tokio::test]
async fn default_command_preserves_requested_cwd_and_respawn_provenance() {
    let handler = RequestHandler::new();
    let alpha = session_name("default-command-cwd-respawn");
    // A start directory the caller NAMES has to be inside the seed this daemon leased: a pane
    // runs in a snapshot of that seed, so a path outside it is one the daemon genuinely cannot
    // open a job over and refuses loudly. The provenance assertion below still compares the host
    // path, which is what a pane's lifecycle records.
    let cwd = crate::pane_terminals::seed_scratch_dir(&handler, "default-command-cwd")
        .path()
        .to_path_buf();
    handler.create_session(&alpha).await;

    let original = tagged_stdin_discard_command("original");
    handler
        .set_option(ScopeSelector::Global, OptionName::DefaultCommand, &original)
        .await;
    let window = handler
        .create_window(NewWindowRequest {
            start_directory: Some(cwd.clone()),
            ..Fixture::fixture(&alpha)
        })
        .await;
    let target = PaneTarget::with_window(alpha.clone(), window.window_index(), 0);
    {
        let state = handler.state.lock().await;
        let pane_id = state
            .sessions
            .session(&alpha)
            .and_then(|session| {
                session.pane_id_in_window(target.window_index(), target.pane_index())
            })
            .expect("cwd pane exists");
        assert_eq!(
            state
                .pane_lifecycle(pane_id)
                .expect("cwd pane lifecycle exists")
                .working_directory(),
            Some(expected_spawn_cwd(&cwd).as_path())
        );
    }

    handler
        .set_option(
            ScopeSelector::Global,
            OptionName::DefaultCommand,
            &tagged_stdin_discard_command("changed"),
        )
        .await;
    handler
        .handle_ok(RespawnWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), target.window_index()),
            kill: true,
            start_directory: None,
            environment: None,
            command: None,
        })
        .await;
    assert_eq!(
        pane_process_command(&handler, &target).await,
        Some(ProcessCommand::Shell(original.clone()))
    );

    handler
        .set_option(
            ScopeSelector::Global,
            OptionName::DefaultCommand,
            &tagged_stdin_discard_command("changed-again"),
        )
        .await;
    handler
        .handle_ok(RespawnPaneRequest::fixture(&target))
        .await;
    assert_eq!(
        pane_process_command(&handler, &target).await,
        Some(ProcessCommand::Shell(original))
    );

    drop(handler);
    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn queued_source_and_binding_paths_apply_default_command() {
    let handler = RequestHandler::new();
    let queued = session_name("default-command-queued");
    let sourced = session_name("default-command-sourced");
    let bound = session_name("default-command-bound");
    for session in [&queued, &sourced, &bound] {
        handler.create_session(session).await;
    }
    let default_command = tagged_stdin_discard_command("entry-paths");
    handler
        .set_option(
            ScopeSelector::Global,
            OptionName::DefaultCommand,
            &default_command,
        )
        .await;

    let parsed = CommandParser::new()
        .parse(&format!(
            "new-window -d -t {queued} ; split-window -d -t {queued}:0.0"
        ))
        .expect("queued window commands parse");
    handler
        .execute_parsed_commands_for_test(std::process::id(), parsed)
        .await
        .expect("queued window commands execute");
    assert_default_command_on_new_and_split(&handler, &queued, &default_command).await;

    let root = unique_temp_path("default-command-source-entry-path");
    fs::create_dir_all(&root).expect("create source-file root");
    let config = root.join("windows.conf");
    fs::write(
        &config,
        format!("new-window -d -t {sourced}\nsplit-window -d -t {sourced}:0.0\n"),
    )
    .expect("write source-file commands");
    handler
        .handle_ok(SourceFileRequest::fixture([config.to_string_lossy()]))
        .await;
    assert_default_command_on_new_and_split(&handler, &sourced, &default_command).await;

    let requester_pid = u32::MAX - 91;
    let _control_rx = handler.attach_client(requester_pid, &bound).await;
    for (key, command) in [
        (
            "N",
            vec![
                "new-window".to_owned(),
                "-d".to_owned(),
                "-t".to_owned(),
                bound.to_string(),
            ],
        ),
        (
            "S",
            vec![
                "split-window".to_owned(),
                "-d".to_owned(),
                "-t".to_owned(),
                format!("{bound}:0.0"),
            ],
        ),
    ] {
        handler
            .handle_ok(BindKeyRequest {
                note: Some("default-command regression".to_owned()),
                ..Fixture::fixture(("prefix", key, command))
            })
            .await;
    }
    handler
        .handle_attached_live_input_for_test(requester_pid, b"\x02N\x02S")
        .await
        .expect("attached bindings execute");
    assert_default_command_on_new_and_split(&handler, &bound, &default_command).await;

    drop(handler);
    let _ = fs::remove_dir_all(root);
}
