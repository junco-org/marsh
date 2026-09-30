use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::RequestHandler;
use crate::pane_io::AttachControl;
use crate::pane_terminals::{seed_scratch_dir, PaneLifecycleProcessState, SeedScratch};
use crate::test_fixtures::{
    unique_temp_path, wait_for_file_contents, Fixture, Grouped, SessionSpec, TestRequest,
};
use crate::test_names::session_name;
use crate::test_shell::{sh_quote_path, stdin_discard_command};
use rmux_proto::{
    BreakPaneRequest, DisplayPanesRequest, HookName, KillPaneRequest, ListPanesRequest,
    ListWindowsRequest, MovePaneRequest, NewSessionExtRequest, OptionName, PaneKillRequest,
    PaneRespawnRequest, PaneSnapshotRequest, PaneTarget, PaneTargetRef, PipePaneRequest,
    ProcessCommand, RenameWindowRequest, Request, ResizePaneAdjustment, ResizePaneRequest,
    RespawnPaneRequest, Response, ScopeSelector, SelectPaneRequest, SessionName, SetOptionMode,
    SplitDirection, SplitWindowExtRequest, SplitWindowRequest, TerminalSize, WindowTarget,
};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

/// The shell that reports a job's own directory, as the probes below spell it.
///
/// Deliberately not `$(pwd)` on its own. A job runs in a *snapshot* of this server's seed, so its
/// absolute directory is `<snapshot root>/<uid>/<seed-relative dir>` and the uid is minted per
/// job: two respawns of one pane report two different absolute paths for the same requested
/// directory, and an expectation that accumulates a line per respawn could not be written against
/// them at all. The final component is the part the request named and the only part that survives
/// a respawn, so that is what a probe prints — and a start directory that was *not* applied still
/// fails loudly, because the job then opens at the snapshot root and prints the uid instead.
///
/// `$PWD` would not do: [`TerminalProfile`](crate::terminal::TerminalProfile) exports it as the
/// *host* directory the caller asked for, so a probe reading it would agree with the request even
/// if the job had opened somewhere else entirely.
const JOB_DIRECTORY_NAME: &str = "dir=$(pwd); name=${dir##*/};";

fn respawn_probe_command(output: &Path) -> String {
    format!(
        "{JOB_DIRECTORY_NAME} printf '%s:%s' \"$name\" \"$RMUX_RESPAWN\" > {}",
        sh_quote_path(output)
    )
}

fn cwd_probe_command(output: &Path) -> String {
    format!(
        "{JOB_DIRECTORY_NAME} printf '%s' \"$name\" > {}",
        sh_quote_path(output)
    )
}

/// What a probe prints for the directory `scratch` names.
///
/// The last component, because that is what [`JOB_DIRECTORY_NAME`] reports.
fn expected_spawn_cwd(scratch: &SeedScratch) -> &str {
    let relative = scratch.relative();
    relative.rsplit('/').next().unwrap_or(relative)
}

fn respawn_replay_script(output: &Path, tag: &str) -> String {
    format!(
        "{JOB_DIRECTORY_NAME} printf '%s:%s:{tag}\\n' \"$name\" \"$RMUX_RESPAWN\" >> {}; sleep 60",
        sh_quote_path(output)
    )
}

fn respawn_argv_probe_command(output: &Path, tag: &str) -> Vec<String> {
    vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        respawn_replay_script(output, tag),
    ]
}

fn respawn_shell_identity_command(output: &Path, tag: &str) -> String {
    format!(
        "printf '%s:%s:{tag}\\n' \"${{0##*/}}\" \"$SHELL\" >> {}; sleep 60",
        sh_quote_path(output)
    )
}

fn respawn_probe_line(cwd: &str, environment: &str, tag: &str) -> String {
    format!("{cwd}:{environment}:{tag}\n")
}

async fn create_three_pane_window(handler: &RequestHandler, session_name: &SessionName) {
    SessionSpec::create(handler, session_name).await;
    for _ in 0..2 {
        TestRequest::send_ok(handler, SplitWindowRequest::fixture(session_name)).await;
    }
}

async fn active_and_last_pane_ids(
    handler: &RequestHandler,
    session_name: &SessionName,
) -> (rmux_core::PaneId, rmux_core::PaneId) {
    let state = handler.state.lock().await;
    let window = state
        .sessions
        .session(session_name)
        .expect("session exists")
        .window_at(0)
        .expect("window exists");
    let active_id = window.active_pane().expect("active pane exists").id();
    let last_id = window
        .last_pane_index()
        .and_then(|pane_index| window.pane(pane_index))
        .expect("last pane exists")
        .id();
    (active_id, last_id)
}

fn list_stdout(response: Response) -> String {
    match response {
        Response::ListPanes(response) => {
            String::from_utf8(response.output.stdout).expect("list-panes stdout is utf8")
        }
        response => panic!("expected list-panes success, got {response:?}"),
    }
}

async fn attach_to_existing_session(
    handler: &RequestHandler,
    requester_pid: u32,
    session_name: &SessionName,
) -> mpsc::UnboundedReceiver<AttachControl> {
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    handler
        .register_attach(requester_pid, session_name.clone(), control_tx)
        .await;
    control_rx
}

async fn wait_for_attached_session_exit(control_rx: &mut mpsc::UnboundedReceiver<AttachControl>) {
    timeout(Duration::from_secs(5), async {
        loop {
            match control_rx.recv().await {
                Some(AttachControl::Exited) => break,
                Some(_) => {}
                None => panic!("attach control channel closed before session exit"),
            }
        }
    })
    .await
    .expect("timed out waiting for source session attach exit");
}

#[tokio::test]
async fn list_panes_activity_sort_follows_selection_counter_like_tmux() {
    // Oracle probes 2026-07-09 (pinned tmux 3.7b): pane "activity" ordering
    // is the active_point selection counter, ascending. Scenario A: fresh
    // window, detached splits, no selections -> index order, identical when
    // reversed. Scenario B: select pane 1 then pane 0 -> "2 1 0", reversed
    // "0 1 2". Output/alert activity does not reorder anything.
    let handler = RequestHandler::new();
    let session = session_name("list-panes-activity-sort");
    SessionSpec::create(&handler, &session).await;
    for _ in 0..2 {
        TestRequest::send_ok(
            &handler,
            SplitWindowRequest {
                direction: SplitDirection::Horizontal,
                ..Fixture::fixture(&session)
            },
        )
        .await;
    }

    let list = |reversed: bool| {
        let handler = handler.clone();
        let session = session.clone();
        async move {
            list_stdout(
                handler
                    .handle(Request::ListPanes(Box::new(ListPanesRequest {
                        target: session,
                        target_window_index: Some(0),
                        format: Some("#{pane_index}".to_owned()),
                        filter: None,
                        sort_order: Some("activity".to_owned()),
                        reversed,
                    })))
                    .await,
            )
        }
    };

    let select = |pane_index: u32| {
        let handler = handler.clone();
        let session = session.clone();
        async move {
            let target = PaneTarget::with_window(session, 0, pane_index);
            TestRequest::send_ok(&handler, SelectPaneRequest::fixture(target)).await;
        }
    };

    // Scenario A: no explicit selection yet -> index order.
    assert_eq!(list(false).await, "0\n1\n2\n");

    // Scenario B: selections define the order.
    select(1).await;
    select(0).await;
    assert_eq!(list(false).await, "2\n1\n0\n");
    assert_eq!(list(true).await, "0\n1\n2\n");
}

#[tokio::test]
async fn list_panes_size_sort_uses_area_in_both_directions() {
    let handler = RequestHandler::new();
    let session = session_name("list-panes-area-sort");
    SessionSpec::create(&handler, &session).await;

    TestRequest::send_ok(
        &handler,
        SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(PaneTarget::with_window(session.clone(), 0, 0))
        },
    )
    .await;
    TestRequest::send_ok(
        &handler,
        ResizePaneRequest {
            target: PaneTarget::with_window(session.clone(), 0, 0),
            adjustment: ResizePaneAdjustment::AbsoluteWidth { columns: 59 },
        },
    )
    .await;
    TestRequest::send_ok(
        &handler,
        SplitWindowRequest::fixture(PaneTarget::with_window(session.clone(), 0, 0)),
    )
    .await;
    TestRequest::send_ok(
        &handler,
        ResizePaneRequest {
            target: PaneTarget::with_window(session.clone(), 0, 0),
            adjustment: ResizePaneAdjustment::AbsoluteHeight { rows: 5 },
        },
    )
    .await;

    let list = |reversed| {
        Request::ListPanes(Box::new(ListPanesRequest {
            target: session.clone(),
            target_window_index: Some(0),
            format: Some("#{pane_index}:#{pane_width}x#{pane_height}".to_owned()),
            filter: None,
            sort_order: Some("size".to_owned()),
            reversed,
        }))
    };

    assert_eq!(
        list_stdout(handler.handle(list(false)).await),
        "0:59x5\n2:20x24\n1:59x18\n"
    );
    assert_eq!(
        list_stdout(handler.handle(list(true)).await),
        "1:59x18\n2:20x24\n0:59x5\n"
    );
}

async fn wait_for_lifecycle_exit(
    handler: &RequestHandler,
    pane_id: rmux_core::PaneId,
    expected_status: i32,
) -> (u64, u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let observed = {
            let state = handler.state.lock().await;
            state.pane_lifecycle(pane_id).and_then(|lifecycle| {
                lifecycle
                    .exit_state
                    .map(|exit| (lifecycle.generation, lifecycle.output_sequence, exit))
            })
        };
        if let Some((generation, output_sequence, exit)) = observed {
            assert_eq!(exit.status, Some(expected_status));
            assert_eq!(exit.signal, None);
            return (generation, output_sequence);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for pane {} lifecycle exit state",
            pane_id.as_u32()
        );
        sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn sticky_lifecycle_state_is_id_keyed_and_redacts_spawn_env() {
    let handler = RequestHandler::new();
    let alpha = session_name("sticky");
    // Both directories are *named* in the requests below, and a named directory outside this
    // server's seed is refused rather than silently relocated, so they are allocated inside it.
    let initial_cwd = seed_scratch_dir(&handler, "sticky-initial-cwd");
    let respawn_cwd = seed_scratch_dir(&handler, "sticky-respawn-cwd");
    let initial_command = stdin_discard_command();
    let split_command = stdin_discard_command();
    let respawn_command = stdin_discard_command();
    let initial_secret = "RMUX_PRIVATE_INITIAL=alpha-secret".to_owned();
    let split_secret = "RMUX_PRIVATE_SPLIT=beta-secret".to_owned();
    let respawn_secret = "RMUX_PRIVATE_RESPAWN=gamma-secret".to_owned();

    SessionSpec::create(
        &handler,
        NewSessionExtRequest {
            working_directory: Some(initial_cwd.path().to_string_lossy().into_owned()),
            environment: Some(vec![initial_secret.clone()]),
            command: Some(vec![initial_command.clone()]),
            ..Fixture::fixture(&alpha)
        },
    )
    .await;
    handler
        .wait_for_pane_terminal_for_test(&PaneTarget::new(alpha.clone(), 0))
        .await;

    let (session_id, window_id, initial_pane_id, initial_output_sequence) = {
        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("session exists");
        let window = session.window_at(0).expect("window exists");
        let pane = window.pane(0).expect("pane exists");
        let lifecycle = state
            .pane_lifecycle(pane.id())
            .expect("initial lifecycle exists");
        assert_eq!(lifecycle.session_id, session.id());
        assert_eq!(lifecycle.window_id, window.id());
        assert_eq!(lifecycle.pane_id, pane.id());
        assert_eq!(
            lifecycle.command(),
            Some(std::slice::from_ref(&initial_command))
        );
        assert_eq!(lifecycle.working_directory(), Some(initial_cwd.path()));
        assert_eq!(
            lifecycle.private_environment(),
            std::slice::from_ref(&initial_secret)
        );
        assert!(lifecycle.tags().is_empty());
        assert_eq!(lifecycle.dimensions(), TerminalSize { cols: 80, rows: 24 });
        assert!(matches!(
            lifecycle.process,
            PaneLifecycleProcessState::Running { .. }
        ));
        assert!(lifecycle.generation >= 1);
        assert!(lifecycle.revision >= 1);
        assert!(lifecycle.output_sequence >= 1);
        assert!(lifecycle.exit_state.is_none());
        (
            session.id(),
            window.id(),
            pane.id(),
            lifecycle.output_sequence,
        )
    };

    let split_target = TestRequest::send_ok(
        &handler,
        SplitWindowExtRequest {
            environment: Some(vec![split_secret.clone()]),
            command: Some(vec![split_command.clone()]),
            ..Fixture::fixture(&alpha)
        },
    )
    .await
    .pane;
    let split_pane_id = {
        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("session exists");
        let window = session.window_at(0).expect("window exists");
        let pane = window
            .pane(split_target.pane_index())
            .expect("split pane exists");
        let lifecycle = state
            .pane_lifecycle(pane.id())
            .expect("split lifecycle exists");
        assert_eq!(lifecycle.session_id, session_id);
        assert_eq!(lifecycle.window_id, window_id);
        assert_eq!(
            lifecycle.command(),
            Some(std::slice::from_ref(&split_command))
        );
        assert_eq!(
            lifecycle.private_environment(),
            std::slice::from_ref(&split_secret)
        );
        assert!(lifecycle.dimensions().cols > 0);
        assert!(lifecycle.dimensions().rows > 0);
        assert!(lifecycle.output_sequence >= 1);
        assert!(pane.id().as_u32() > initial_pane_id.as_u32());
        pane.id()
    };

    let list_format = concat!(
        "#{pane_id}\t#{pane_start_command}\t#{pane_start_path}\t",
        "#{pane_lifecycle_generation}\t#{pane_output_sequence}\t",
        "#{RMUX_PRIVATE_INITIAL}\t#{RMUX_PRIVATE_SPLIT}\t#{RMUX_PRIVATE_RESPAWN}"
    )
    .to_owned();
    let listed = handler
        .handle(Request::ListPanes(Box::new(ListPanesRequest {
            target: alpha.clone(),
            target_window_index: None,
            format: Some(list_format.clone()),
            filter: None,
            sort_order: None,
            reversed: false,
        })))
        .await;
    let list_stdout = match listed {
        rmux_proto::Response::ListPanes(response) => {
            String::from_utf8(response.output.stdout).expect("list-panes utf8")
        }
        response => panic!("expected list-panes success, got {response:?}"),
    };
    assert!(list_stdout.contains(&initial_pane_id.to_string()));
    assert!(list_stdout.contains(&split_pane_id.to_string()));
    assert!(!list_stdout.contains(&initial_secret));
    assert!(!list_stdout.contains(&split_secret));

    let windows = handler
        .handle(Request::ListWindows(Box::new(ListWindowsRequest {
            target: alpha.clone(),
            format: Some(list_format),
            filter: None,
            sort_order: None,
            reversed: false,
        })))
        .await;
    let windows_stdout = match windows {
        rmux_proto::Response::ListWindows(response) => {
            assert_eq!(response.windows.len(), 1);
            String::from_utf8(response.output.stdout).expect("list-windows utf8")
        }
        response => panic!("expected list-windows success, got {response:?}"),
    };
    assert!(!windows_stdout.contains(&initial_secret));
    assert!(!windows_stdout.contains(&split_secret));

    TestRequest::send_ok(
        &handler,
        KillPaneRequest {
            target: split_target,
            kill_all_except: false,
        },
    )
    .await;
    {
        let state = handler.state.lock().await;
        assert!(
            state.pane_lifecycle(split_pane_id).is_none(),
            "closed pane lifecycle state must be removed by pane id"
        );
    }

    handler
        .set_option(
            ScopeSelector::Pane(PaneTarget::with_window(alpha.clone(), 0, 0)),
            OptionName::RemainOnExit,
            "on",
        )
        .await;
    TestRequest::send_ok(
        &handler,
        RespawnPaneRequest {
            command: Some(vec!["exit 7".to_owned()]),
            ..Fixture::fixture(PaneTarget::with_window(alpha.clone(), 0, 0))
        },
    )
    .await;
    handler
        .wait_for_pane_exit_for_test(&PaneTarget::new(alpha.clone(), 0))
        .await;
    let (dead_generation, dead_output_sequence) =
        wait_for_lifecycle_exit(&handler, initial_pane_id, 7).await;

    TestRequest::send_ok(
        &handler,
        RespawnPaneRequest {
            start_directory: Some(respawn_cwd.path().to_path_buf()),
            environment: Some(vec![respawn_secret.clone()]),
            command: Some(vec![respawn_command.clone()]),
            ..Fixture::fixture(PaneTarget::with_window(alpha.clone(), 0, 0))
        },
    )
    .await;
    {
        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("session exists");
        let pane = session
            .window_at(0)
            .and_then(|window| window.pane(0))
            .expect("respawned pane exists");
        assert_eq!(pane.id(), initial_pane_id);
        let lifecycle = state
            .pane_lifecycle(initial_pane_id)
            .expect("respawn lifecycle exists");
        assert_eq!(
            lifecycle.command(),
            Some(std::slice::from_ref(&respawn_command))
        );
        assert_eq!(lifecycle.working_directory(), Some(respawn_cwd.path()));
        assert_eq!(
            lifecycle.private_environment(),
            std::slice::from_ref(&respawn_secret)
        );
        assert!(!lifecycle.private_environment().contains(&initial_secret));
        assert!(matches!(
            lifecycle.process,
            PaneLifecycleProcessState::Running { .. }
        ));
        assert!(lifecycle.exit_state.is_none());
        assert!(lifecycle.generation > dead_generation);
        assert!(lifecycle.output_sequence > dead_output_sequence);
        assert!(lifecycle.output_sequence > initial_output_sequence);
    }

    let relisted = handler
        .handle(Request::ListPanes(Box::new(ListPanesRequest {
            target: alpha,
            target_window_index: Some(0),
            format: Some(
                concat!(
                    "#{pane_id}\t#{pane_start_command}\t#{pane_start_path}\t",
                    "#{pane_lifecycle_generation}\t#{pane_output_sequence}\t",
                    "dead=#{pane_dead_status}\t#{RMUX_PRIVATE_INITIAL}\t",
                    "#{RMUX_PRIVATE_SPLIT}\t#{RMUX_PRIVATE_RESPAWN}"
                )
                .to_owned(),
            ),
            filter: None,
            sort_order: None,
            reversed: false,
        })))
        .await;
    let relisted_stdout = match relisted {
        rmux_proto::Response::ListPanes(response) => {
            String::from_utf8(response.output.stdout).expect("list-panes utf8")
        }
        response => panic!("expected list-panes success, got {response:?}"),
    };
    assert!(relisted_stdout.contains(&initial_pane_id.to_string()));
    assert!(!relisted_stdout.contains(&initial_secret));
    assert!(!relisted_stdout.contains(&split_secret));
    assert!(!relisted_stdout.contains(&respawn_secret));
    assert!(!relisted_stdout.contains("dead=7"));
}

#[tokio::test]
async fn split_window_ext_applies_start_directory_to_spawned_process() {
    let handler = RequestHandler::new();
    let alpha = session_name("split-cwd");
    // The start directory is *named*, so it has to be one this server's seed can offer. The probe
    // output stays on the host: a job writing an absolute path writes straight through.
    let cwd = seed_scratch_dir(&handler, "split-cwd");
    let output = unique_temp_path("split-cwd-output");
    SessionSpec::create(&handler, &alpha).await;

    let _split_target = TestRequest::send_ok(
        &handler,
        SplitWindowExtRequest {
            command: Some(vec![cwd_probe_command(&output)]),
            start_directory: Some(cwd.path().to_path_buf()),
            ..Fixture::fixture(&alpha)
        },
    )
    .await
    .pane;

    wait_for_file_contents(&output, expected_spawn_cwd(&cwd)).await;

    let _ = fs::remove_file(output);
}

#[tokio::test]
async fn detached_split_with_zoom_keeps_original_pane_zoomed() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha-detached-split-zoom");
    SessionSpec::create(&handler, &alpha).await;

    let new_pane = TestRequest::send_ok(
        &handler,
        SplitWindowExtRequest {
            detached: true,
            preserve_zoom: true,
            ..Fixture::fixture(PaneTarget::with_window(alpha.clone(), 0, 0))
        },
    )
    .await
    .pane;

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session exists");
    let window = session.window_at(0).expect("window exists");
    assert!(window.is_zoomed());
    assert_eq!(window.active_pane_index(), 0);
    assert_eq!(new_pane.pane_index(), 1);
}

#[tokio::test]
async fn detached_split_targeting_inactive_pane_preserves_active_pane() {
    let handler = RequestHandler::new();
    let alpha = session_name("detached-split-preserves-active-pane");
    SessionSpec::create(&handler, &alpha).await;

    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&alpha)).await;
    TestRequest::send_ok(
        &handler,
        SelectPaneRequest::fixture(PaneTarget::with_window(alpha.clone(), 0, 0)),
    )
    .await;
    TestRequest::send_ok(
        &handler,
        SplitWindowExtRequest {
            detached: true,
            ..Fixture::fixture(PaneTarget::with_window(alpha.clone(), 0, 1))
        },
    )
    .await;

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session exists");
    assert_eq!(session.active_pane_index(), 0);
}

#[tokio::test]
async fn split_window_rolls_back_session_when_spawn_fails() {
    let handler = RequestHandler::new();
    let alpha = session_name("split-spawn-fails");
    SessionSpec::create(&handler, &alpha).await;
    let (active_pane_index, pane_id, pane_geometry) = {
        let state = handler.state.lock().await;
        let window = state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .expect("window exists");
        let pane = window.pane(0).expect("initial pane exists");
        (window.active_pane_index(), pane.id(), pane.geometry())
    };
    // A real directory, so the pane profile accepts it, whose `.git` is a malformed gitfile, so
    // shell admission fails synchronously on source-root discovery after the split layout has
    // already been applied. That is the boundary under test: a split whose spawn fails must leave
    // the window exactly as it found it.
    let unadmittable = tempfile::tempdir().expect("create a start directory");
    fs::write(unadmittable.path().join(".git"), "not a gitdir link\n")
        .expect("write a malformed gitfile");

    let response = handler
        .handle(Request::SplitWindowExt(Box::new(SplitWindowExtRequest {
            start_directory: Some(unadmittable.path().to_path_buf()),
            ..Fixture::fixture(&alpha)
        })))
        .await;

    assert!(
        matches!(&response, rmux_proto::Response::Error(error) if error.error.to_string().contains(rmux_proto::SPAWN_FAILED_MESSAGE_PREFIX)),
        "expected spawn failure, got {response:?}"
    );

    {
        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("session exists");
        let window = session.window_at(0).expect("window exists");
        assert_eq!(window.pane_count(), 1);
        assert_eq!(window.active_pane_index(), active_pane_index);
        let pane = window.pane(0).expect("initial pane survives");
        assert_eq!(pane.id(), pane_id);
        assert_eq!(pane.geometry(), pane_geometry);
    }

    let retried = TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&alpha)).await;
    assert_eq!(retried.pane, PaneTarget::with_window(alpha.clone(), 0, 1));
}

#[tokio::test]
async fn pane_output_sequence_advances_when_transcript_changes() {
    let handler = RequestHandler::new();
    let alpha = session_name("sequence");
    SessionSpec::create(
        &handler,
        NewSessionExtRequest {
            command: Some(vec![stdin_discard_command()]),
            ..Fixture::fixture(&alpha)
        },
    )
    .await;

    let pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id())
            .expect("initial pane exists")
    };
    let before = listed_output_sequence(&handler, &alpha).await;
    {
        let mut state = handler.state.lock().await;
        state
            .append_bytes_to_runtime_pane_transcript(&alpha, pane_id, b"transcript output")
            .expect("append to runtime transcript");
    }
    let after = listed_output_sequence(&handler, &alpha).await;

    assert!(
        after > before,
        "pane_output_sequence should advance after pane output, before={before}, after={after}"
    );
}

async fn listed_output_sequence(handler: &RequestHandler, session_name: &SessionName) -> u64 {
    let listed = handler
        .handle(Request::ListPanes(Box::new(ListPanesRequest {
            target: session_name.clone(),
            target_window_index: Some(0),
            format: Some("#{pane_output_sequence}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
        })))
        .await;
    let stdout = match listed {
        rmux_proto::Response::ListPanes(response) => {
            String::from_utf8(response.output.stdout).expect("list-panes utf8")
        }
        response => panic!("expected list-panes success, got {response:?}"),
    };
    stdout
        .trim()
        .parse::<u64>()
        .expect("pane_output_sequence is numeric")
}

#[tokio::test]
async fn move_pane_same_window_keeps_last_when_moving_the_active_pane_like_tmux() {
    // Oracle probe 2026-07-10, pinned tmux 3.7b: moving active %2 next to
    // %0 keeps %2 active and the pre-move %1 as last-pane.
    let handler = RequestHandler::new();
    let alpha = session_name("move-active-keeps-last");
    create_three_pane_window(&handler, &alpha).await;
    let selection_before = active_and_last_pane_ids(&handler, &alpha).await;

    TestRequest::send_ok(
        &handler,
        MovePaneRequest {
            source: PaneTarget::with_window(alpha.clone(), 0, 2),
            target: PaneTarget::with_window(alpha.clone(), 0, 0),
            direction: SplitDirection::Vertical,
            detached: false,
            before: false,
            full_size: false,
            size: None,
        },
    )
    .await;
    assert_eq!(
        active_and_last_pane_ids(&handler, &alpha).await,
        selection_before
    );
}

#[tokio::test]
async fn join_pane_same_window_keeps_last_when_moving_the_active_pane_like_tmux() {
    // Same oracle matrix as move-pane: join-pane keeps active %2 and last %1.
    let handler = RequestHandler::new();
    let alpha = session_name("join-active-keeps-last");
    create_three_pane_window(&handler, &alpha).await;
    let selection_before = active_and_last_pane_ids(&handler, &alpha).await;

    TestRequest::send_ok(
        &handler,
        rmux_proto::JoinPaneRequest {
            source: PaneTarget::with_window(alpha.clone(), 0, 2),
            target: PaneTarget::with_window(alpha.clone(), 0, 0),
            direction: SplitDirection::Vertical,
            detached: false,
            before: false,
            full_size: false,
            size: None,
        },
    )
    .await;
    assert_eq!(
        active_and_last_pane_ids(&handler, &alpha).await,
        selection_before
    );
}

#[tokio::test]
async fn move_pane_routes_through_join_semantics() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;

    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&alpha)).await;
    {
        let mut state = handler.state.lock().await;
        let pane_id = state.sessions.allocate_pane_id();
        state
            .sessions
            .session_mut(&alpha)
            .expect("session exists")
            .insert_window_with_initial_pane_with_id(
                1,
                TerminalSize { cols: 80, rows: 24 },
                pane_id,
            )
            .expect("window insert succeeds");
        state
            .insert_window_terminal(
                &alpha,
                1,
                crate::pane_terminals::WindowSpawnOptions {
                    start_directory: None,
                    command: None,
                    socket_path: Path::new("/tmp/rmux-test.sock"),
                    spawn_environment: None,
                    environment_overrides: None,
                    respawn_shell: None,
                    respawn_environment: None,
                },
            )
            .await
            .expect("window terminal insert succeeds");
    }

    let response = handler
        .handle(Request::MovePane(MovePaneRequest {
            source: PaneTarget::with_window(alpha.clone(), 0, 1),
            target: PaneTarget::with_window(alpha.clone(), 1, 0),
            direction: SplitDirection::Vertical,
            detached: true,
            before: true,
            full_size: false,
            size: Some(rmux_proto::PaneSplitSize::Absolute(12)),
        }))
        .await;

    assert_eq!(
        response,
        rmux_proto::Response::MovePane(rmux_proto::MovePaneResponse {
            target: PaneTarget::with_window(alpha.clone(), 1, 0),
        })
    );

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session exists");
    assert_eq!(
        session
            .window_at(1)
            .expect("destination window exists")
            .panes()
            .iter()
            .map(|pane| pane.index())
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[tokio::test]
async fn join_pane_exits_attached_source_session_when_source_is_removed() {
    let handler = RequestHandler::new();
    let alpha = session_name("join-remove-alpha");
    let beta = session_name("join-remove-beta");
    SessionSpec::create(&handler, &alpha).await;
    SessionSpec::create(&handler, &beta).await;
    let mut control_rx = attach_to_existing_session(&handler, 70_001, &alpha).await;

    TestRequest::send_ok(
        &handler,
        rmux_proto::JoinPaneRequest {
            source: PaneTarget::with_window(alpha.clone(), 0, 0),
            target: PaneTarget::with_window(beta.clone(), 0, 0),
            direction: SplitDirection::Vertical,
            detached: false,
            before: false,
            full_size: false,
            size: None,
        },
    )
    .await;
    wait_for_attached_session_exit(&mut control_rx).await;
}

#[tokio::test]
async fn break_pane_exits_attached_source_session_when_source_is_removed() {
    let handler = RequestHandler::new();
    let alpha = session_name("break-remove-alpha");
    let beta = session_name("break-remove-beta");
    SessionSpec::create(&handler, &alpha).await;
    SessionSpec::create(&handler, &beta).await;
    let mut control_rx = attach_to_existing_session(&handler, 70_002, &alpha).await;

    TestRequest::send_ok(
        &handler,
        BreakPaneRequest {
            source: PaneTarget::with_window(alpha.clone(), 0, 0),
            target: Some(WindowTarget::with_window(beta.clone(), 1)),
            name: None,
            detached: false,
            after: false,
            before: false,
            print_target: false,
            format: None,
        },
    )
    .await;
    wait_for_attached_session_exit(&mut control_rx).await;
}

#[tokio::test]
async fn break_pane_print_target_uses_custom_format() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;

    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&alpha)).await;

    let success = TestRequest::send_ok(
        &handler,
        BreakPaneRequest {
            source: PaneTarget::with_window(alpha.clone(), 0, 1),
            target: Some(WindowTarget::with_window(alpha.clone(), 1)),
            name: None,
            detached: true,
            after: false,
            before: false,
            print_target: true,
            format: Some("#{window_index}.#{pane_index}".to_owned()),
        },
    )
    .await;
    let output = success.command_output().expect("break-pane -P output");
    assert_eq!(output.stdout(), b"1.0\n");
}

#[tokio::test]
async fn break_pane_implicit_destination_starts_at_base_index() {
    let handler = RequestHandler::new();
    let alpha = session_name("break-base-index");

    handler
        .set_option(ScopeSelector::Global, OptionName::BaseIndex, "1")
        .await;
    SessionSpec::create(&handler, &alpha).await;
    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&alpha)).await;

    let success = TestRequest::send_ok(
        &handler,
        BreakPaneRequest {
            source: PaneTarget::with_window(alpha.clone(), 1, 1),
            target: None,
            name: None,
            detached: true,
            after: false,
            before: false,
            print_target: true,
            format: Some("#{window_index}.#{pane_index}".to_owned()),
        },
    )
    .await;
    assert_eq!(
        success
            .command_output()
            .expect("break-pane -P output")
            .stdout(),
        b"2.0\n"
    );
    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session survives");
    assert_eq!(
        session.windows().keys().copied().collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[tokio::test]
async fn break_pane_print_target_refreshes_automatic_window_name() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha-break-name");
    SessionSpec::create(&handler, &alpha).await;

    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&alpha)).await;

    let success = TestRequest::send_ok(
        &handler,
        BreakPaneRequest {
            source: PaneTarget::with_window(alpha.clone(), 0, 1),
            target: Some(WindowTarget::with_window(alpha.clone(), 1)),
            name: None,
            detached: true,
            after: false,
            before: false,
            print_target: true,
            format: Some("#{window_name}:#{pane_current_command}".to_owned()),
        },
    )
    .await;
    let output = success.command_output().expect("break-pane -P output");
    let output = String::from_utf8(output.stdout().to_vec()).expect("utf-8 output");
    let (window_name, current_command) = output
        .trim_end()
        .split_once(':')
        .expect("window name and current command");
    assert!(
        !window_name.is_empty(),
        "break-pane -P should expose a tmux-style automatic window name, got {output:?}"
    );
    assert!(
        !current_command.is_empty(),
        "pane_current_command sanity check, got {output:?}"
    );
}

#[tokio::test]
async fn attached_input_to_dead_kept_pane_does_not_fail_liveness_probe() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;
    handler
        .set_option(
            ScopeSelector::Pane(PaneTarget::with_window(alpha.clone(), 0, 0)),
            OptionName::RemainOnExit,
            "on",
        )
        .await;

    TestRequest::send_ok(
        &handler,
        RespawnPaneRequest {
            command: Some(vec!["exit 0".to_owned()]),
            ..Fixture::fixture(PaneTarget::with_window(alpha.clone(), 0, 0))
        },
    )
    .await;
    handler
        .wait_for_pane_exit_for_test(&PaneTarget::new(alpha.clone(), 0))
        .await;

    let requester_pid = 44_200_u32;
    let (control_tx, _control_rx) = mpsc::unbounded_channel();
    handler
        .register_attach(requester_pid, alpha.clone(), control_tx)
        .await;

    let result = handler
        .handle_attached_live_input_for_test(requester_pid, b"x")
        .await;
    assert!(
        !matches!(&result, Err(error) if error.to_string().contains("target pane has exited")),
        "attached input must not use child-process liveness as a pane liveness gate: {result:?}"
    );
}

#[tokio::test]
async fn respawn_pane_without_command_restarts_dead_remain_on_exit_workload() {
    let handler = RequestHandler::new();
    let alpha = session_name("respawn-dead-provenance");
    SessionSpec::create(&handler, &alpha).await;
    let target = PaneTarget::with_window(alpha.clone(), 0, 0);
    handler
        .set_option(
            ScopeSelector::Pane(target.clone()),
            OptionName::RemainOnExit,
            "on",
        )
        .await;

    TestRequest::send_ok(
        &handler,
        RespawnPaneRequest {
            process_command: Some(ProcessCommand::Shell("exit 7".to_owned())),
            ..Fixture::fixture(&target)
        },
    )
    .await;
    handler
        .wait_for_pane_exit_for_test(&PaneTarget::new(alpha.clone(), 0))
        .await;
    let pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&alpha)
            .and_then(|session| session.pane_id_in_window(0, 0))
            .expect("dead pane exists")
    };
    let (first_generation, _) = wait_for_lifecycle_exit(&handler, pane_id, 7).await;

    TestRequest::send_ok(
        &handler,
        RespawnPaneRequest {
            kill: false,
            ..Fixture::fixture(target)
        },
    )
    .await;
    handler
        .wait_for_pane_exit_for_test(&PaneTarget::new(alpha.clone(), 0))
        .await;
    let (second_generation, _) = wait_for_lifecycle_exit(&handler, pane_id, 7).await;
    assert!(
        second_generation > first_generation,
        "command-less respawn must run and exit the original workload again"
    );
}

#[tokio::test]
async fn pipe_pane_rejects_dead_panes() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;
    handler
        .set_option(
            ScopeSelector::Pane(PaneTarget::with_window(alpha.clone(), 0, 0)),
            OptionName::RemainOnExit,
            "on",
        )
        .await;

    TestRequest::send_ok(
        &handler,
        RespawnPaneRequest {
            command: Some(vec!["exit 0".to_owned()]),
            ..Fixture::fixture(PaneTarget::with_window(alpha.clone(), 0, 0))
        },
    )
    .await;
    handler
        .wait_for_pane_exit_for_test(&PaneTarget::new(alpha.clone(), 0))
        .await;

    let response = handler
        .handle(Request::PipePane(PipePaneRequest {
            target: PaneTarget::with_window(alpha, 0, 0),
            stdin: false,
            stdout: true,
            once: false,
            command: Some(stdin_discard_command()),
        }))
        .await;

    assert!(
        matches!(&response, rmux_proto::Response::Error(error) if error.error.to_string().contains("target pane has exited")),
        "expected dead-pane error, got {response:?}"
    );
}

#[tokio::test]
async fn respawn_pane_rejects_active_pane_without_kill_flag() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;

    let response = handler
        .handle(Request::RespawnPane(Box::new(RespawnPaneRequest {
            kill: false,
            ..Fixture::fixture(PaneTarget::with_window(alpha, 0, 0))
        })))
        .await;

    assert!(
        matches!(&response, rmux_proto::Response::Error(error) if error.error.to_string().contains("still active")),
        "expected still-active error, got {response:?}"
    );
}

#[tokio::test]
async fn respawn_pane_with_kill_flag_applies_directory_environment_and_command() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    // A *named* start directory must be one this server's seed can offer; the probe output stays
    // on the host, which a job reaches by naming an absolute path.
    let cwd = seed_scratch_dir(&handler, "respawn-pane-cwd");
    let output = unique_temp_path("respawn-pane-output");
    SessionSpec::create(&handler, &alpha).await;

    let response = handler
        .handle(Request::RespawnPane(Box::new(RespawnPaneRequest {
            start_directory: Some(cwd.path().to_path_buf()),
            environment: Some(vec!["RMUX_RESPAWN=ready".to_owned()]),
            command: Some(vec![respawn_probe_command(&output)]),
            ..Fixture::fixture(PaneTarget::with_window(alpha.clone(), 0, 0))
        })))
        .await;

    assert_eq!(
        response,
        rmux_proto::Response::RespawnPane(rmux_proto::RespawnPaneResponse {
            target: PaneTarget::with_window(alpha, 0, 0),
        })
    );
    let expected_cwd = expected_spawn_cwd(&cwd);
    wait_for_file_contents(&output, &format!("{expected_cwd}:ready")).await;
    let _ = fs::remove_file(output);
}

#[tokio::test]
async fn respawn_pane_reuses_structured_command_cwd_and_private_environment() {
    let handler = RequestHandler::new();
    let alpha = session_name("respawn-pane-provenance");
    // Both directories are named in requests below, so both live inside this server's seed.
    let initial_cwd = seed_scratch_dir(&handler, "respawn-pane-provenance-initial");
    let override_cwd = seed_scratch_dir(&handler, "respawn-pane-provenance-override");
    let output = unique_temp_path("respawn-pane-provenance-output");
    let initial_argv = respawn_argv_probe_command(&output, "argv");
    let initial_process_command = ProcessCommand::Argv(initial_argv.clone());
    let initial_environment = "RMUX_RESPAWN=initial".to_owned();

    SessionSpec::create(
        &handler,
        NewSessionExtRequest {
            working_directory: Some(initial_cwd.path().to_string_lossy().into_owned()),
            environment: Some(vec![initial_environment.clone()]),
            process_command: Some(initial_process_command.clone()),
            ..Fixture::fixture(&alpha)
        },
    )
    .await;

    let initial_cwd_text = expected_spawn_cwd(&initial_cwd);
    let initial_line = respawn_probe_line(initial_cwd_text, "initial", "argv");
    wait_for_file_contents(&output, &initial_line).await;
    let target = PaneTarget::with_window(alpha.clone(), 0, 0);
    let pane_id = {
        let state = handler.state.lock().await;
        let pane_id = state
            .sessions
            .session(&alpha)
            .and_then(|session| session.pane_id_in_window(0, 0))
            .expect("initial pane exists");
        let lifecycle = state.pane_lifecycle(pane_id).expect("initial lifecycle");
        assert_eq!(lifecycle.process_command(), Some(&initial_process_command));
        assert_eq!(
            lifecycle.respawn_environment(),
            std::slice::from_ref(&initial_environment)
        );
        pane_id
    };

    TestRequest::send_ok(&handler, RespawnPaneRequest::fixture(&target)).await;
    wait_for_file_contents(&output, &format!("{initial_line}{initial_line}")).await;

    let override_environment = "RMUX_RESPAWN=override".to_owned();
    let explicit_shell = respawn_replay_script(&output, "shell");
    let explicit_process_command = ProcessCommand::Shell(explicit_shell);
    TestRequest::send_ok(
        &handler,
        RespawnPaneRequest {
            start_directory: Some(override_cwd.path().to_path_buf()),
            environment: Some(vec![override_environment.clone()]),
            process_command: Some(explicit_process_command.clone()),
            ..Fixture::fixture(&target)
        },
    )
    .await;
    let override_cwd_text = expected_spawn_cwd(&override_cwd);
    let override_line = respawn_probe_line(override_cwd_text, "override", "shell");
    wait_for_file_contents(
        &output,
        &format!("{initial_line}{initial_line}{override_line}"),
    )
    .await;
    {
        let state = handler.state.lock().await;
        let lifecycle = state.pane_lifecycle(pane_id).expect("override lifecycle");
        assert_eq!(lifecycle.process_command(), Some(&explicit_process_command));
        assert_eq!(
            lifecycle.private_environment(),
            std::slice::from_ref(&override_environment)
        );
        assert_eq!(
            lifecycle.respawn_environment(),
            std::slice::from_ref(&initial_environment)
        );
    }

    TestRequest::send_ok(&handler, RespawnPaneRequest::fixture(target)).await;
    let inherited_override_line = respawn_probe_line(override_cwd_text, "initial", "shell");
    wait_for_file_contents(
        &output,
        &format!("{initial_line}{initial_line}{override_line}{inherited_override_line}"),
    )
    .await;

    drop(handler);
    let _ = fs::remove_file(output);
}

#[tokio::test]
async fn respawn_pane_keeps_the_original_resolved_shell_after_option_changes() {
    let handler = RequestHandler::new();
    let alpha = session_name("respawn-pane-shell-provenance");
    let output = unique_temp_path("respawn-pane-shell-provenance-output");
    handler
        .set_option(ScopeSelector::Global, OptionName::DefaultShell, "/bin/sh")
        .await;
    SessionSpec::create(&handler, &alpha).await;
    handler.wait_for_initial_panes_for_test().await;
    handler
        .set_option(ScopeSelector::Global, OptionName::DefaultShell, "/bin/bash")
        .await;

    let target = PaneTarget::with_window(alpha.clone(), 0, 0);
    let shell_command = respawn_shell_identity_command(&output, "shell");
    TestRequest::send_ok(
        &handler,
        RespawnPaneRequest {
            process_command: Some(ProcessCommand::Shell(shell_command)),
            ..Fixture::fixture(&target)
        },
    )
    .await;
    let expected_line = "sh:/bin/sh:shell\n";
    wait_for_file_contents(&output, expected_line).await;

    TestRequest::send_ok(&handler, RespawnPaneRequest::fixture(target)).await;
    wait_for_file_contents(&output, &format!("{expected_line}{expected_line}")).await;

    let state = handler.state.lock().await;
    let pane_id = state
        .sessions
        .session(&alpha)
        .and_then(|session| session.pane_id_in_window(0, 0))
        .expect("respawned pane exists");
    assert_eq!(
        state
            .pane_lifecycle(pane_id)
            .expect("respawned pane lifecycle")
            .respawn_shell(),
        &crate::terminal::PaneShell::External(PathBuf::from("/bin/sh"))
    );
    drop(state);
    drop(handler);
    let _ = fs::remove_file(output);
}

#[tokio::test]
async fn respawn_pane_with_kill_flag_does_not_emit_pane_exited_like_tmux() {
    let handler = RequestHandler::new();
    let alpha = session_name("respawn-exit");
    SessionSpec::create(&handler, &alpha).await;
    let target = PaneTarget::with_window(alpha.clone(), 0, 0);
    let (pane_id, previous_generation) = {
        let state = handler.state.lock().await;
        let pane = state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.pane(0))
            .expect("initial pane exists");
        let lifecycle = state
            .pane_lifecycle(pane.id())
            .expect("initial lifecycle exists");
        (pane.id(), lifecycle.generation)
    };
    let mut lifecycle_events = handler.subscribe_lifecycle_events();

    TestRequest::send_ok(
        &handler,
        RespawnPaneRequest {
            command: Some(vec![stdin_discard_command()]),
            ..Fixture::fixture(&target)
        },
    )
    .await;

    assert_no_pane_exited_event(&mut lifecycle_events, "forced respawn").await;

    let state = handler.state.lock().await;
    let pane = state
        .sessions
        .session(&alpha)
        .and_then(|session| session.window_at(0))
        .and_then(|window| window.pane(0))
        .expect("respawned pane exists");
    assert_eq!(pane.id(), pane_id);
    let lifecycle = state
        .pane_lifecycle(pane_id)
        .expect("respawned lifecycle exists");
    assert!(lifecycle.generation > previous_generation);
    assert!(matches!(
        lifecycle.process,
        PaneLifecycleProcessState::Running { .. }
    ));
    assert!(lifecycle.exit_state.is_none());
}

#[tokio::test]
async fn pane_id_kill_does_not_emit_pane_exited_like_tmux() {
    let handler = RequestHandler::new();
    let alpha = session_name("pane-id-kill-exit");
    SessionSpec::create(&handler, &alpha).await;
    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&alpha)).await;
    let pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.pane(1))
            .expect("second pane exists")
            .id()
    };
    let mut lifecycle_events = handler.subscribe_lifecycle_events();

    let response = handler
        .handle(Request::PaneKill(PaneKillRequest {
            target: PaneTargetRef::by_id(alpha, pane_id),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(response, Response::KillPane(_)));

    assert_no_pane_exited_event(&mut lifecycle_events, "stable pane-id kill").await;
}

#[tokio::test]
async fn pane_id_respawn_with_kill_flag_does_not_emit_pane_exited_like_tmux() {
    let handler = RequestHandler::new();
    let alpha = session_name("pane-id-respawn-exit");
    SessionSpec::create(&handler, &alpha).await;
    let (pane_id, previous_generation) = {
        let state = handler.state.lock().await;
        let pane = state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.pane(0))
            .expect("initial pane exists");
        let lifecycle = state
            .pane_lifecycle(pane.id())
            .expect("initial lifecycle exists");
        (pane.id(), lifecycle.generation)
    };
    let mut lifecycle_events = handler.subscribe_lifecycle_events();

    let response = handler
        .handle(Request::PaneRespawn(Box::new(PaneRespawnRequest {
            target: PaneTargetRef::by_id(alpha.clone(), pane_id),
            kill: true,
            start_directory: None,
            environment: None,
            command: Some(vec![stdin_discard_command()]),
            process_command: None,
            keep_alive_on_exit: None,
        })))
        .await;
    assert!(matches!(response, Response::RespawnPane(_)));

    assert_no_pane_exited_event(&mut lifecycle_events, "stable pane-id respawn").await;

    let state = handler.state.lock().await;
    let pane = state
        .sessions
        .session(&alpha)
        .and_then(|session| session.window_at(0))
        .and_then(|window| window.pane(0))
        .expect("respawned pane exists");
    assert_eq!(pane.id(), pane_id);
    let lifecycle = state
        .pane_lifecycle(pane_id)
        .expect("respawned lifecycle exists");
    assert!(lifecycle.generation > previous_generation);
    assert!(matches!(
        lifecycle.process,
        PaneLifecycleProcessState::Running { .. }
    ));
    assert!(lifecycle.exit_state.is_none());
}

async fn assert_no_pane_exited_event(
    lifecycle_events: &mut tokio::sync::broadcast::Receiver<super::QueuedLifecycleEvent>,
    context: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(150);
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        match timeout(deadline - now, lifecycle_events.recv()).await {
            Ok(Ok(event)) => assert_ne!(
                event.hook_name,
                HookName::PaneExited,
                "{context} must not synthesize pane-exited: {event:?}"
            ),
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) | Err(_) => break,
        }
    }
}

#[tokio::test]
async fn respawn_pane_preserves_id_and_clears_parser_state_before_new_output() {
    let handler = RequestHandler::new();
    let alpha = session_name("respawn-reset");
    SessionSpec::create(&handler, &alpha).await;
    let target = PaneTarget::with_window(alpha.clone(), 0, 0);
    let pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id())
            .expect("initial pane exists")
    };

    {
        let mut state = handler.state.lock().await;
        state
            .append_bytes_to_runtime_pane_transcript(&alpha, pane_id, b"OLD_MARKER")
            .expect("append old output");
    }
    let before = snapshot_response(&handler, target.clone()).await;
    assert!(all_visible_text(&before).contains("OLD_MARKER"));
    let (previous_generation, previous_revision, previous_output_sequence) = {
        let state = handler.state.lock().await;
        let lifecycle = state
            .pane_lifecycle(pane_id)
            .expect("initial lifecycle exists");
        (
            lifecycle.generation,
            lifecycle.revision,
            lifecycle.output_sequence,
        )
    };

    TestRequest::send_ok(
        &handler,
        RespawnPaneRequest {
            command: Some(vec![stdin_discard_command()]),
            ..Fixture::fixture(&target)
        },
    )
    .await;

    let after = snapshot_response(&handler, target).await;
    assert!(
        !all_visible_text(&after).contains("OLD_MARKER"),
        "respawn must discard the old transcript and parser screen before fresh output"
    );
    let state = handler.state.lock().await;
    let pane = state
        .sessions
        .session(&alpha)
        .and_then(|session| session.window_at(0))
        .and_then(|window| window.pane(0))
        .expect("respawned pane exists");
    assert_eq!(pane.id(), pane_id);
    let lifecycle = state
        .pane_lifecycle(pane_id)
        .expect("respawned lifecycle exists");
    assert!(lifecycle.generation > previous_generation);
    assert!(lifecycle.revision > previous_revision);
    assert!(lifecycle.output_sequence > previous_output_sequence);
}

#[tokio::test]
async fn display_panes_uses_the_default_select_pane_template() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let requester_pid = 42_u32;
    SessionSpec::create(&handler, &alpha).await;

    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&alpha)).await;
    TestRequest::send_ok(
        &handler,
        SelectPaneRequest::fixture(PaneTarget::with_window(alpha.clone(), 0, 0)),
    )
    .await;

    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    handler
        .register_attach(requester_pid, alpha.clone(), control_tx)
        .await;

    let response = handler
        .handle(Request::DisplayPanes(Box::new(DisplayPanesRequest {
            target: alpha.clone(),
            duration_ms: Some(5_000),
            non_blocking: true,
            no_command: false,
            template: None,
            target_client: None,
        })))
        .await;
    assert!(matches!(response, rmux_proto::Response::DisplayPanes(_)));
    let _overlay = control_rx.recv().await.expect("display-panes overlay");

    handler
        .handle_attached_live_input_for_test(requester_pid, b"1")
        .await
        .expect("display-panes select input");
    let _clear = control_rx
        .recv()
        .await
        .expect("display-panes clear overlay");

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session exists");
    assert_eq!(session.active_pane_index(), 1);
}

#[tokio::test]
async fn display_panes_default_template_runs_select_pane_hooks() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let requester_pid = 4242_u32;
    SessionSpec::create(&handler, &alpha).await;

    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&alpha)).await;
    TestRequest::send_ok(
        &handler,
        SelectPaneRequest::fixture(PaneTarget::with_window(alpha.clone(), 0, 0)),
    )
    .await;
    handler
        .set_global_hook(
            HookName::AfterSelectPane,
            "set-buffer -b display-panes-probe fired",
        )
        .await;

    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    handler
        .register_attach(requester_pid, alpha.clone(), control_tx)
        .await;

    let response = handler
        .handle(Request::DisplayPanes(Box::new(DisplayPanesRequest {
            target: alpha.clone(),
            duration_ms: Some(5_000),
            non_blocking: true,
            no_command: false,
            template: None,
            target_client: None,
        })))
        .await;
    assert!(matches!(response, rmux_proto::Response::DisplayPanes(_)));
    let _overlay = control_rx.recv().await.expect("display-panes overlay");

    handler
        .handle_attached_live_input_for_test(requester_pid, b"1")
        .await
        .expect("display-panes select input");
    let _clear = control_rx
        .recv()
        .await
        .expect("display-panes clear overlay");

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session exists");
    assert_eq!(session.active_pane_index(), 1);
    let (_, buffer) = state
        .buffers
        .show(Some("display-panes-probe"))
        .expect("after-select-pane hook should write the probe buffer");
    assert_eq!(String::from_utf8_lossy(buffer), "fired");
}

#[tokio::test]
async fn display_panes_without_a_command_keeps_the_active_pane() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let requester_pid = 43_u32;
    SessionSpec::create(&handler, &alpha).await;

    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&alpha)).await;
    TestRequest::send_ok(
        &handler,
        SelectPaneRequest::fixture(PaneTarget::with_window(alpha.clone(), 0, 0)),
    )
    .await;

    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    handler
        .register_attach(requester_pid, alpha.clone(), control_tx)
        .await;

    let response = handler
        .handle(Request::DisplayPanes(Box::new(DisplayPanesRequest {
            target: alpha.clone(),
            duration_ms: Some(5_000),
            non_blocking: true,
            no_command: true,
            template: None,
            target_client: None,
        })))
        .await;
    assert!(matches!(response, rmux_proto::Response::DisplayPanes(_)));
    let _overlay = control_rx.recv().await.expect("display-panes overlay");

    handler
        .handle_attached_live_input_for_test(requester_pid, b"1")
        .await
        .expect("display-panes close input");
    let _clear = control_rx
        .recv()
        .await
        .expect("display-panes clear overlay");

    let state = handler.state.lock().await;
    let session = state.sessions.session(&alpha).expect("session exists");
    assert_eq!(session.active_pane_index(), 0);
}

#[tokio::test]
async fn display_panes_uses_the_session_option_duration_by_default() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let requester_pid = 44_u32;
    SessionSpec::create(&handler, &alpha).await;

    {
        let mut state = handler.state.lock().await;
        state
            .options
            .set(
                ScopeSelector::Session(alpha.clone()),
                OptionName::DisplayPanesTime,
                "25".to_owned(),
                SetOptionMode::Replace,
            )
            .expect("set display-panes-time");
    }

    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    handler
        .register_attach(requester_pid, alpha.clone(), control_tx)
        .await;

    let response = handler
        .handle(Request::DisplayPanes(Box::new(DisplayPanesRequest {
            target: alpha.clone(),
            duration_ms: None,
            non_blocking: true,
            no_command: true,
            template: None,
            target_client: None,
        })))
        .await;
    assert!(matches!(response, rmux_proto::Response::DisplayPanes(_)));
    let _overlay = control_rx.recv().await.expect("display-panes overlay");

    timeout(Duration::from_millis(250), async {
        loop {
            let cleared = {
                let active_attach = handler.active_attach.lock().await;
                active_attach
                    .by_pid
                    .get(&requester_pid)
                    .and_then(|active| active.display_panes.as_ref())
                    .is_none()
            };
            if cleared {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("display-panes state should clear with option duration");
}

#[tokio::test]
async fn display_panes_timeout_emits_a_clear_overlay_to_the_attached_client() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let requester_pid = 45_u32;
    SessionSpec::create(&handler, &alpha).await;

    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    handler
        .register_attach(requester_pid, alpha.clone(), control_tx)
        .await;

    let response = handler
        .handle(Request::DisplayPanes(Box::new(DisplayPanesRequest {
            target: alpha.clone(),
            duration_ms: Some(25),
            non_blocking: true,
            no_command: true,
            template: None,
            target_client: None,
        })))
        .await;
    assert!(matches!(response, rmux_proto::Response::DisplayPanes(_)));

    let first = timeout(Duration::from_secs(1), control_rx.recv())
        .await
        .expect("overlay should arrive")
        .expect("overlay command");
    assert!(matches!(first, AttachControl::Overlay(_)));

    let mut seen = Vec::new();
    let clear = timeout(Duration::from_secs(1), async {
        loop {
            let next = control_rx.recv().await.expect("follow-up control");
            match next {
                AttachControl::Overlay(clear) => break clear,
                other => seen.push(format!("{other:?}")),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("clear overlay should arrive; saw {seen:?}"));
    assert!(
        !clear.frame.is_empty(),
        "display-panes clear overlay should repaint the client"
    );
}

#[tokio::test]
async fn join_pane_rejects_same_source_and_target() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;

    let response = handler
        .handle(Request::JoinPane(rmux_proto::JoinPaneRequest {
            source: PaneTarget::with_window(alpha.clone(), 0, 0),
            target: PaneTarget::with_window(alpha.clone(), 0, 0),
            direction: SplitDirection::Vertical,
            detached: false,
            before: false,
            full_size: false,
            size: None,
        }))
        .await;

    assert!(
        matches!(&response, rmux_proto::Response::Error(error) if error.error.to_string().contains("must be different")),
        "expected same-pane error, got {response:?}"
    );
}

#[tokio::test]
async fn move_pane_rejects_same_source_and_target() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;

    let response = handler
        .handle(Request::MovePane(MovePaneRequest {
            source: PaneTarget::with_window(alpha.clone(), 0, 0),
            target: PaneTarget::with_window(alpha.clone(), 0, 0),
            direction: SplitDirection::Vertical,
            detached: false,
            before: false,
            full_size: false,
            size: None,
        }))
        .await;

    assert!(
        matches!(&response, rmux_proto::Response::Error(error) if error.error.to_string().contains("must be different")),
        "expected same-pane error, got {response:?}"
    );
}

#[tokio::test]
async fn cross_session_swap_from_group_owner_preserves_owner_and_session_pair() {
    let handler = RequestHandler::new();
    let owner = session_name("swap-owner-success");
    let peer = session_name("swap-peer-success");
    let target = session_name("swap-target-success");
    SessionSpec::create(&handler, &owner).await;
    SessionSpec::create(&handler, Grouped(&peer, &owner)).await;
    SessionSpec::create(&handler, &target).await;
    let (source_pane_id, target_pane_id) = {
        let state = handler.state.lock().await;
        (
            state
                .sessions
                .session(&owner)
                .and_then(|session| session.pane_id_in_window(0, 0))
                .expect("source pane exists"),
            state
                .sessions
                .session(&target)
                .and_then(|session| session.pane_id_in_window(0, 0))
                .expect("target pane exists"),
        )
    };

    TestRequest::send_ok(
        &handler,
        rmux_proto::SwapPaneRequest {
            source: PaneTarget::with_window(owner.clone(), 0, 0),
            target: PaneTarget::with_window(target.clone(), 0, 0),
            direction: None,
            detached: false,
            preserve_zoom: false,
        },
    )
    .await;
    let state = handler.state.lock().await;
    assert_eq!(state.sessions.runtime_owner(&owner), Some(owner.clone()));
    assert_eq!(state.sessions.runtime_owner(&peer), Some(owner.clone()));
    assert_eq!(
        state
            .sessions
            .session(&owner)
            .and_then(|session| session.pane_id_in_window(0, 0)),
        Some(target_pane_id)
    );
    assert_eq!(
        state
            .sessions
            .session(&peer)
            .and_then(|session| session.pane_id_in_window(0, 0)),
        Some(target_pane_id)
    );
    assert_eq!(
        state
            .sessions
            .session(&target)
            .and_then(|session| session.pane_id_in_window(0, 0)),
        Some(source_pane_id)
    );
}

#[tokio::test]
async fn cross_session_swap_rollback_restores_owner_and_session_pair() {
    let handler = RequestHandler::new();
    let owner = session_name("swap-owner-rollback");
    let peer = session_name("swap-peer-rollback");
    let target = session_name("swap-target-rollback");
    SessionSpec::create(&handler, &owner).await;
    SessionSpec::create(&handler, Grouped(&peer, &owner)).await;
    SessionSpec::create(&handler, &target).await;
    handler.wait_for_initial_panes_for_test().await;
    let (owner_before, peer_before, target_before) = {
        let mut state = handler.state.lock().await;
        let snapshots = (
            state
                .sessions
                .session(&owner)
                .expect("owner exists")
                .clone(),
            state.sessions.session(&peer).expect("peer exists").clone(),
            state
                .sessions
                .session(&target)
                .expect("target exists")
                .clone(),
        );
        state.fail_next_resize_for_test();
        snapshots
    };

    let response = handler
        .handle(Request::SwapPane(rmux_proto::SwapPaneRequest {
            source: PaneTarget::with_window(owner.clone(), 0, 0),
            target: PaneTarget::with_window(target.clone(), 0, 0),
            direction: None,
            detached: false,
            preserve_zoom: false,
        }))
        .await;

    assert!(
        matches!(&response, Response::Error(error) if error.error == rmux_proto::RmuxError::Server("injected pane terminal resize failure".to_owned())),
        "expected injected resize failure, got {response:?}"
    );
    let state = handler.state.lock().await;
    assert_eq!(state.sessions.runtime_owner(&owner), Some(owner.clone()));
    assert_eq!(state.sessions.runtime_owner(&peer), Some(owner.clone()));
    assert_eq!(state.sessions.session(&owner), Some(&owner_before));
    assert_eq!(state.sessions.session(&peer), Some(&peer_before));
    assert_eq!(state.sessions.session(&target), Some(&target_before));
}

#[tokio::test]
async fn swap_pane_self_swap_is_a_no_op() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;

    TestRequest::send_ok(&handler, SplitWindowRequest::fixture(&alpha)).await;

    TestRequest::send_ok(
        &handler,
        rmux_proto::SwapPaneRequest {
            source: PaneTarget::with_window(alpha.clone(), 0, 0),
            target: PaneTarget::with_window(alpha.clone(), 0, 0),
            direction: None,
            detached: false,
            preserve_zoom: false,
        },
    )
    .await;
}

#[tokio::test]
async fn respawn_pane_dead_pane_succeeds_without_kill_flag() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let target = PaneTarget::with_window(alpha.clone(), 0, 0);
    SessionSpec::create(&handler, &alpha).await;

    handler
        .set_option(
            ScopeSelector::Pane(target.clone()),
            OptionName::RemainOnExit,
            "on",
        )
        .await;

    TestRequest::send_ok(
        &handler,
        RespawnPaneRequest {
            command: Some(vec!["exit 0".to_owned()]),
            ..Fixture::fixture(&target)
        },
    )
    .await;
    handler
        .wait_for_pane_exit_for_test(&PaneTarget::new(alpha.clone(), 0))
        .await;

    TestRequest::send_ok(
        &handler,
        RespawnPaneRequest {
            kill: false,
            ..Fixture::fixture(target)
        },
    )
    .await;
}

#[tokio::test]
async fn remain_on_exit_keeps_the_existing_window_name() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let target = PaneTarget::with_window(alpha.clone(), 0, 0);
    SessionSpec::create(&handler, &alpha).await;

    TestRequest::send_ok(
        &handler,
        RenameWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 0),
            name: "custom".to_owned(),
        },
    )
    .await;

    handler
        .set_option(
            ScopeSelector::Pane(target.clone()),
            OptionName::RemainOnExit,
            "on",
        )
        .await;

    let expected_window_name = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.name())
            .expect("renamed window keeps its explicit name")
            .to_owned()
    };

    TestRequest::send_ok(
        &handler,
        RespawnPaneRequest {
            command: Some(vec!["exit 0".to_owned()]),
            ..Fixture::fixture(&target)
        },
    )
    .await;
    handler
        .wait_for_pane_exit_for_test(&PaneTarget::new(alpha.clone(), 0))
        .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (ready, observation) = {
            let state = handler.state.lock().await;
            match state
                .sessions
                .session(&alpha)
                .and_then(|session| session.window_at(0))
                .and_then(|window| {
                    window
                        .pane(0)
                        .map(|pane| (window.name().map(str::to_owned), pane.id()))
                }) {
                Some((window_name, pane_id)) => {
                    let dead = state.pane_is_dead(&alpha, pane_id);
                    (
                        window_name.as_deref() == Some(expected_window_name.as_str()) && dead,
                        format!(
                            "last_window_name={window_name:?} last_dead={dead:?} last_pane_id={:?}",
                            pane_id.as_u32()
                        ),
                    )
                }
                None => (
                    false,
                    "last_window_name=None last_dead=None last_pane_id=None".to_owned(),
                ),
            }
        };
        if ready {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for remain-on-exit window name to stay at {expected_window_name:?}; {observation}"
            );
        }
        sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn remain_on_exit_auto_named_window_gets_tmux_dead_suffix_when_unattached() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let target = PaneTarget::with_window(alpha.clone(), 0, 0);
    SessionSpec::create(&handler, &alpha).await;

    handler
        .set_option(
            ScopeSelector::Pane(target.clone()),
            OptionName::RemainOnExit,
            "on",
        )
        .await;

    let expected_window_name = "exit[dead]".to_owned();

    TestRequest::send_ok(
        &handler,
        RespawnPaneRequest {
            command: Some(vec!["exit 0".to_owned()]),
            ..Fixture::fixture(&target)
        },
    )
    .await;
    handler
        .wait_for_pane_exit_for_test(&PaneTarget::new(alpha.clone(), 0))
        .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (ready, observation) = {
            let state = handler.state.lock().await;
            match state
                .sessions
                .session(&alpha)
                .and_then(|session| session.window_at(0))
                .and_then(|window| {
                    window
                        .pane(0)
                        .map(|pane| (window.name().map(str::to_owned), pane.id()))
                }) {
                Some((window_name, pane_id)) => {
                    let dead = state.pane_is_dead(&alpha, pane_id);
                    (
                        window_name.as_deref() == Some(expected_window_name.as_str()) && dead,
                        format!(
                            "last_window_name={window_name:?} last_dead={dead:?} last_pane_id={:?}",
                            pane_id.as_u32()
                        ),
                    )
                }
                None => (
                    false,
                    "last_window_name=None last_dead=None last_pane_id=None".to_owned(),
                ),
            }
        };
        if ready {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for remain-on-exit automatic dead name {expected_window_name:?}; {observation}"
            );
        }
        sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn pipe_pane_close_on_nonexistent_pipe_is_a_no_op() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;

    TestRequest::send_ok(
        &handler,
        PipePaneRequest {
            target: PaneTarget::with_window(alpha, 0, 0),
            stdin: false,
            stdout: true,
            once: false,
            command: None,
        },
    )
    .await;
}

#[tokio::test]
async fn pipe_pane_empty_command_closes_existing_pipe() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, &alpha).await;
    let target = PaneTarget::with_window(alpha.clone(), 0, 0);

    TestRequest::send_ok(
        &handler,
        PipePaneRequest {
            target: target.clone(),
            stdin: false,
            stdout: true,
            once: false,
            command: Some(stdin_discard_command()),
        },
    )
    .await;
    assert_eq!(pane_pipe_state(&handler, &target).await, "1");

    TestRequest::send_ok(
        &handler,
        PipePaneRequest {
            target: target.clone(),
            stdin: false,
            stdout: true,
            once: false,
            command: Some(String::new()),
        },
    )
    .await;
    assert_eq!(pane_pipe_state(&handler, &target).await, "0");

    // Opening a new pipe after an empty-command close should succeed, confirming the previous
    // pipe was cleaned up.
    TestRequest::send_ok(
        &handler,
        PipePaneRequest {
            target: target.clone(),
            stdin: false,
            stdout: true,
            once: true,
            command: Some(stdin_discard_command()),
        },
    )
    .await;
    assert_eq!(pane_pipe_state(&handler, &target).await, "1");
    TestRequest::send_ok(
        &handler,
        PipePaneRequest {
            target: target.clone(),
            stdin: false,
            stdout: true,
            once: false,
            command: None,
        },
    )
    .await;
    assert_eq!(pane_pipe_state(&handler, &target).await, "0");
}

/// The pane's public `#{pane_pipe}` flag, trimmed of display-message's line ending.
async fn pane_pipe_state(handler: &RequestHandler, target: &PaneTarget) -> String {
    String::from_utf8_lossy(&handler.display_print(target.clone(), "#{pane_pipe}").await)
        .trim_end()
        .to_owned()
}

async fn snapshot_response(
    handler: &RequestHandler,
    target: PaneTarget,
) -> rmux_proto::PaneSnapshotResponse {
    match handler
        .handle(Request::PaneSnapshot(PaneSnapshotRequest { target }))
        .await
    {
        rmux_proto::Response::PaneSnapshot(response) => response,
        other => panic!("expected pane-snapshot response, got {other:?}"),
    }
}

fn collect_visible_text(response: &rmux_proto::PaneSnapshotResponse, row: usize) -> String {
    let cols = usize::from(response.cols);
    let start = row.saturating_mul(cols);
    let end = start.saturating_add(cols).min(response.cells.len());
    response.cells[start..end]
        .iter()
        .filter(|cell| !cell.padding)
        .map(|cell| cell.text.as_str())
        .collect::<String>()
        .trim_end_matches(' ')
        .to_owned()
}

fn all_visible_text(response: &rmux_proto::PaneSnapshotResponse) -> String {
    (0..usize::from(response.rows))
        .map(|row| collect_visible_text(response, row))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn pane_snapshot_returns_live_screen_built_via_terminal_parser() {
    // The hardening contract for the snapshot endpoint: cells must come from
    // the live `Screen` fed by rmux-core's crate-private terminal parser, and
    // not from a `String::from_utf8_lossy(capture-pane -p)` reconstruction.
    // Feeding raw PTY-style bytes through the transcript parser and then
    // observing the structured cells exercises that exact path end-to-end.
    let handler = RequestHandler::new();
    let alpha = session_name("snapshot-live");
    SessionSpec::create(
        &handler,
        NewSessionExtRequest {
            size: Some(TerminalSize { cols: 12, rows: 4 }),
            command: Some(vec![stdin_discard_command()]),
            ..Fixture::fixture(&alpha)
        },
    )
    .await;

    let pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id())
            .expect("initial pane exists")
    };

    let target = PaneTarget::with_window(alpha.clone(), 0, 0);

    let baseline = snapshot_response(&handler, target.clone()).await;
    assert_eq!(baseline.cols, 12);
    assert_eq!(baseline.rows, 4);
    assert_eq!(baseline.cells.len(), 48);
    assert_ne!(
        baseline.revision, 0,
        "live panes must carry a non-zero revision"
    );

    // Feed bytes that include a wide glyph and an SGR escape into the
    // transcript. Both must reach the structured cells, since the parser is
    // the only producer of the screen state behind the snapshot endpoint.
    {
        let mut state = handler.state.lock().await;
        state
            .append_bytes_to_runtime_pane_transcript(
                &alpha,
                pane_id,
                "hi界\x1b[31mZ\x1b[0m".as_bytes(),
            )
            .expect("append bytes through parser");
    }

    let after = snapshot_response(&handler, target.clone()).await;
    assert_eq!(after.cols, 12);
    assert_eq!(after.rows, 4);
    assert_eq!(after.cells.len(), 48);
    assert_ne!(
        after.revision, baseline.revision,
        "fed bytes must change the snapshot revision",
    );

    // The first row must contain the parsed glyphs in column order, with a
    // padding cell for the second column of the wide glyph.
    let row0 = &after.cells[0..12];
    assert_eq!(row0[0].text, "h");
    assert_eq!(row0[0].width, 1);
    assert!(!row0[0].padding);
    assert_eq!(row0[1].text, "i");
    assert_eq!(row0[2].text, "界");
    assert_eq!(row0[2].width, 2);
    assert!(!row0[2].padding);
    assert!(
        row0[3].padding,
        "the column following a wide glyph must be padding"
    );
    assert_eq!(row0[3].width, 0);
    assert_eq!(row0[4].text, "Z");
    // The SGR sequence must paint the foreground colour onto the Z cell, not
    // pollute the cell text with literal escape bytes.
    assert!(
        !row0[4].text.contains('\x1b'),
        "raw escape bytes must never leak into cell text"
    );
    assert_ne!(
        row0[4].fg, baseline.cells[4].fg,
        "the parsed SGR must change the foreground colour for the Z cell"
    );
    assert_eq!(
        collect_visible_text(&after, 0),
        "hi界Z",
        "padding-skipped row text must reflect the parsed glyphs"
    );

    // A subsequent capture without further bytes must yield the same cells
    // and revision, confirming determinism for unchanged screen state.
    let again = snapshot_response(&handler, target).await;
    assert_eq!(again.revision, after.revision);
    assert_eq!(again.cells, after.cells);
}

#[tokio::test]
async fn pane_snapshot_invalid_target_returns_error_response() {
    let handler = RequestHandler::new();
    let alpha = session_name("snapshot-missing");
    let response = handler
        .handle(Request::PaneSnapshot(PaneSnapshotRequest {
            target: PaneTarget::with_window(alpha, 0, 0),
        }))
        .await;
    match response {
        rmux_proto::Response::Error(_) => {}
        other => panic!("expected error response for missing session, got {other:?}"),
    }
}

#[tokio::test]
async fn pane_snapshot_folds_invalid_utf8_through_parser_not_raw_bytes() {
    // Invalid UTF-8 bytes must be folded into U+FFFD by the rmux-core
    // terminal parser *before* they reach the structured snapshot cells.
    // The endpoint must not leak raw invalid bytes into `cell.text`, since
    // there is no `String::from_utf8_lossy(capture-pane -p)` salvage step
    // to clean them up later.
    let handler = RequestHandler::new();
    let alpha = session_name("snapshot-bad-utf8");
    SessionSpec::create(
        &handler,
        NewSessionExtRequest {
            size: Some(TerminalSize { cols: 8, rows: 2 }),
            command: Some(vec![stdin_discard_command()]),
            ..Fixture::fixture(&alpha)
        },
    )
    .await;

    let pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id())
            .expect("initial pane exists")
    };

    {
        let mut state = handler.state.lock().await;
        // 0xFF is invalid as a UTF-8 leading byte, and 0xC3 0x28 is an
        // invalid 2-byte sequence (continuation byte 0x28 is not 0x80..=0xBF).
        // The parser must absorb both and emit replacement cells instead of
        // leaking the raw bytes.
        state
            .append_bytes_to_runtime_pane_transcript(&alpha, pane_id, b"a\xFFb\xC3\x28c")
            .expect("append invalid utf-8 through parser");
    }

    let target = PaneTarget::with_window(alpha, 0, 0);
    let response = snapshot_response(&handler, target).await;
    let row0 = &response.cells[0..usize::from(response.cols)];
    for (col, cell) in row0.iter().enumerate() {
        // Every cell text must be valid UTF-8 (a Vec<u8> from `text` is
        // already constrained, but assert no cell carries raw bytes that
        // happen to look like an escape or NUL).
        assert!(
            !cell.text.contains('\u{0000}'),
            "col {col} text {:?} leaks NUL",
            cell.text,
        );
        assert!(
            cell.text.chars().all(|ch| ch != '\u{001B}'),
            "col {col} text {:?} leaks escape byte",
            cell.text,
        );
    }
    let visible: String = row0
        .iter()
        .filter(|cell| !cell.padding)
        .map(|cell| cell.text.as_str())
        .collect::<String>()
        .trim_end_matches(' ')
        .to_owned();
    assert!(
        visible.contains('a') && visible.contains('b') && visible.contains('c'),
        "valid bytes around the invalid sequences must survive: {visible:?}",
    );
    assert!(
        visible.contains('\u{FFFD}'),
        "invalid utf-8 must be folded by the parser into U+FFFD, got {visible:?}",
    );
}

#[tokio::test]
async fn pane_snapshot_revision_changes_after_clear_history() {
    // Clearing scrollback is observable: history_size and history_bytes drop
    // to zero. The revision must change so SDK consumers don't treat the
    // post-clear screen as identical to the pre-clear one.
    let handler = RequestHandler::new();
    let alpha = session_name("snapshot-clear");
    SessionSpec::create(
        &handler,
        NewSessionExtRequest {
            size: Some(TerminalSize { cols: 4, rows: 2 }),
            command: Some(vec![stdin_discard_command()]),
            ..Fixture::fixture(&alpha)
        },
    )
    .await;

    let pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&alpha)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.pane(0))
            .map(|pane| pane.id())
            .expect("initial pane exists")
    };

    {
        let mut state = handler.state.lock().await;
        // Pump enough lines to push older content into scrollback history.
        state
            .append_bytes_to_runtime_pane_transcript(&alpha, pane_id, b"L1\r\nL2\r\nL3\r\nL4\r\n")
            .expect("append lines through parser");
    }

    let target = PaneTarget::with_window(alpha.clone(), 0, 0);
    let before = snapshot_response(&handler, target.clone()).await;

    let cleared = handler
        .handle(Request::ClearHistory(rmux_proto::ClearHistoryRequest {
            target: target.clone(),
            reset_hyperlinks: false,
        }))
        .await;
    assert!(matches!(cleared, rmux_proto::Response::ClearHistory(_)));

    let after = snapshot_response(&handler, target).await;
    assert_ne!(
        before.revision, after.revision,
        "clearing scrollback must change the snapshot revision",
    );
}
