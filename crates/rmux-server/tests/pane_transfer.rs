use std::error::Error;
use std::time::Duration;

mod common;

use common::{
    session_name, start_server, wait_for_file_contents, ClientConnection, Fixture, TestHarness,
    PTY_TEST_LOCK,
};
use rmux_proto::{
    BreakPaneRequest, JoinPaneRequest, KillPaneRequest, LastPaneRequest, ListPanesRequest,
    ListSessionsRequest, NewSessionExtRequest, NewWindowRequest, PaneTarget, Request, Response,
    SelectPaneRequest, SendKeysRequest, SplitWindowRequest, SwapPaneRequest, TerminalSize,
    WindowTarget,
};

const FILE_TIMEOUT: Duration = Duration::from_secs(15);
const SESSION_SIZE: TerminalSize = TerminalSize {
    cols: 120,
    rows: 40,
};

#[tokio::test(flavor = "multi_thread")]
async fn break_pane_last_source_window_to_other_session_removes_source_session(
) -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("break-pane-last-source-to-other-session");
    let socket_path = harness.socket_path().to_path_buf();
    let _handle = start_server(&harness).await?;
    let mut client = ClientConnection::connect(&socket_path).await?;
    let source = session_name("src");
    let hidden = session_name("hidden");

    for session in [&source, &hidden] {
        client.create_session((session, SESSION_SIZE)).await?;
    }

    assert_eq!(
        client
            .send(BreakPaneRequest::fixture((
                PaneTarget::new(source.clone(), 0),
                WindowTarget::with_window(hidden.clone(), 1),
            )))
            .await?,
        Response::BreakPane(rmux_proto::BreakPaneResponse {
            target: PaneTarget::with_window(hidden.clone(), 1, 0),
            output: None,
        })
    );

    let sessions = client
        .send(ListSessionsRequest::fixture("#{session_name}"))
        .await?;
    let Response::ListSessions(sessions) = sessions else {
        panic!("expected list-sessions response");
    };
    let sessions = String::from_utf8(sessions.output.stdout)?;
    assert!(!sessions.lines().any(|line| line == source.as_str()));
    assert!(sessions.lines().any(|line| line == hidden.as_str()));

    let panes = client
        .send(ListPanesRequest::fixture((
            &hidden,
            "#{window_index}.#{pane_index}:#{pane_id}",
        )))
        .await?;
    let Response::ListPanes(panes) = panes else {
        panic!("expected list-panes response");
    };
    let panes = String::from_utf8(panes.output.stdout)?;
    assert!(panes.lines().any(|line| line.starts_with("0.0:%")));
    assert!(panes.lines().any(|line| line.starts_with("1.0:%")));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn break_pane_last_grouped_source_removes_entire_source_group() -> Result<(), Box<dyn Error>>
{
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("break-pane-last-grouped-source-removes-group");
    let socket_path = harness.socket_path().to_path_buf();
    let _handle = start_server(&harness).await?;
    let mut client = ClientConnection::connect(&socket_path).await?;
    let source = session_name("src");
    let grouped = session_name("src-peer");
    let hidden = session_name("hidden");

    for session in [&source, &hidden] {
        client.create_session((session, SESSION_SIZE)).await?;
    }
    client
        .create_session(NewSessionExtRequest {
            size: Some(SESSION_SIZE),
            group_target: Some(source.clone()),
            ..Fixture::fixture(&grouped)
        })
        .await?;

    client
        .send_ok(BreakPaneRequest::fixture((
            PaneTarget::new(source.clone(), 0),
            WindowTarget::with_window(hidden.clone(), 1),
        )))
        .await?;

    let sessions = client
        .send_ok(ListSessionsRequest::fixture("#{session_name}"))
        .await?;
    let sessions = String::from_utf8(sessions.output.stdout)?;
    assert!(!sessions.lines().any(|line| line == source.as_str()));
    assert!(!sessions.lines().any(|line| line == grouped.as_str()));
    assert!(sessions.lines().any(|line| line == hidden.as_str()));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn join_pane_last_source_window_to_other_session_removes_source_session(
) -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("join-pane-last-source-to-other-session");
    let socket_path = harness.socket_path().to_path_buf();
    let _handle = start_server(&harness).await?;
    let mut client = ClientConnection::connect(&socket_path).await?;
    let source = session_name("src");
    let hidden = session_name("hidden");

    for session in [&source, &hidden] {
        client.create_session((session, SESSION_SIZE)).await?;
    }

    assert_eq!(
        client
            .send(JoinPaneRequest::fixture((
                PaneTarget::new(source.clone(), 0),
                PaneTarget::new(hidden.clone(), 0),
            )))
            .await?,
        Response::JoinPane(rmux_proto::JoinPaneResponse {
            target: PaneTarget::with_window(hidden.clone(), 0, 1),
        })
    );

    let sessions = client
        .send(ListSessionsRequest::fixture("#{session_name}"))
        .await?;
    let Response::ListSessions(sessions) = sessions else {
        panic!("expected list-sessions response");
    };
    let sessions = String::from_utf8(sessions.output.stdout)?;
    assert!(!sessions.lines().any(|line| line == source.as_str()));
    assert!(sessions.lines().any(|line| line == hidden.as_str()));

    let panes = client
        .send(ListPanesRequest::fixture((
            &hidden,
            "#{window_index}.#{pane_index}:#{pane_id}",
        )))
        .await?;
    let Response::ListPanes(panes) = panes else {
        panic!("expected list-panes response");
    };
    let panes = String::from_utf8(panes.output.stdout)?;
    assert!(panes.lines().any(|line| line.starts_with("0.0:%")));
    assert!(panes.lines().any(|line| line.starts_with("0.1:%")));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn join_pane_last_grouped_source_removes_entire_source_group() -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("join-pane-last-grouped-source-removes-group");
    let socket_path = harness.socket_path().to_path_buf();
    let _handle = start_server(&harness).await?;
    let mut client = ClientConnection::connect(&socket_path).await?;
    let source = session_name("src");
    let grouped = session_name("src-peer");
    let hidden = session_name("hidden");

    for session in [&source, &hidden] {
        client.create_session((session, SESSION_SIZE)).await?;
    }
    client
        .create_session(NewSessionExtRequest {
            size: Some(SESSION_SIZE),
            group_target: Some(source.clone()),
            ..Fixture::fixture(&grouped)
        })
        .await?;

    client
        .send_ok(JoinPaneRequest::fixture((
            PaneTarget::new(source.clone(), 0),
            PaneTarget::new(hidden.clone(), 0),
        )))
        .await?;

    let sessions = client
        .send_ok(ListSessionsRequest::fixture("#{session_name}"))
        .await?;
    let sessions = String::from_utf8(sessions.output.stdout)?;
    assert!(!sessions.lines().any(|line| line == source.as_str()));
    assert!(!sessions.lines().any(|line| line == grouped.as_str()));
    assert!(sessions.lines().any(|line| line == hidden.as_str()));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pane_transfer_commands_move_live_ptys_between_windows() -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("pane-transfer-live-ptys");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let mut client = ClientConnection::connect(&socket_path).await?;
    let session = session_name("alpha");
    let root = socket_path
        .parent()
        .expect("socket path must have a parent");
    let join_path = root.join("join.txt");
    let break_path = root.join("break.txt");
    let swap_source_path = root.join("swap-source.txt");
    let swap_target_path = root.join("swap-target.txt");

    client.create_session((&session, SESSION_SIZE)).await?;

    assert_eq!(
        client.send(SplitWindowRequest::fixture(&session)).await?,
        Response::SplitWindow(rmux_proto::SplitWindowResponse {
            pane: PaneTarget::new(session.clone(), 1),
        })
    );
    assert_eq!(
        client
            .send(SelectPaneRequest::fixture(PaneTarget::new(
                session.clone(),
                1
            )))
            .await?,
        Response::SelectPane(rmux_proto::SelectPaneResponse {
            target: PaneTarget::new(session.clone(), 1),
        })
    );
    assert_eq!(
        client
            .send(SelectPaneRequest::fixture(PaneTarget::new(
                session.clone(),
                0
            )))
            .await?,
        Response::SelectPane(rmux_proto::SelectPaneResponse {
            target: PaneTarget::new(session.clone(), 0),
        })
    );
    assert_eq!(
        client
            .send_request(&Request::LastPane(LastPaneRequest {
                target: WindowTarget::new(session.clone()),
                preserve_zoom: false,
                input_disabled: None,
            }))
            .await?,
        Response::LastPane(rmux_proto::LastPaneResponse {
            target: PaneTarget::new(session.clone(), 1),
        })
    );

    client
        .send_ok(SendKeysRequest::fixture((
            PaneTarget::new(session.clone(), 1),
            ["export RMUX_TRANSFER_MARK=joined", "Enter"],
        )))
        .await?;

    assert_eq!(
        client
            .send(NewWindowRequest {
                name: Some("dest".to_owned()),
                ..Fixture::fixture(&session)
            })
            .await?,
        Response::NewWindow(rmux_proto::NewWindowResponse {
            target: WindowTarget::with_window(session.clone(), 1),
        })
    );
    assert_eq!(
        client
            .send(JoinPaneRequest::fixture((
                PaneTarget::new(session.clone(), 1),
                PaneTarget::with_window(session.clone(), 1, 0),
            )))
            .await?,
        Response::JoinPane(rmux_proto::JoinPaneResponse {
            target: PaneTarget::with_window(session.clone(), 1, 1),
        })
    );
    client
        .send_ok(SendKeysRequest::fixture((
            PaneTarget::with_window(session.clone(), 1, 1),
            [
                format!("printf \"$RMUX_TRANSFER_MARK\" > {}", join_path.display()),
                "Enter".to_owned(),
            ],
        )))
        .await?;
    wait_for_file_contents(&join_path, "joined", FILE_TIMEOUT).await?;

    assert_eq!(
        client
            .send(BreakPaneRequest {
                name: Some("broken".to_owned()),
                ..Fixture::fixture((
                    PaneTarget::with_window(session.clone(), 1, 1),
                    WindowTarget::with_window(session.clone(), 2),
                ))
            })
            .await?,
        Response::BreakPane(rmux_proto::BreakPaneResponse {
            target: PaneTarget::with_window(session.clone(), 2, 0),
            output: None,
        })
    );
    client
        .send_ok(SendKeysRequest::fixture((
            PaneTarget::with_window(session.clone(), 2, 0),
            [
                format!("printf \"$RMUX_TRANSFER_MARK\" > {}", break_path.display()),
                "Enter".to_owned(),
            ],
        )))
        .await?;
    wait_for_file_contents(&break_path, "joined", FILE_TIMEOUT).await?;

    client
        .send_ok(NewWindowRequest {
            name: Some("swap".to_owned()),
            ..Fixture::fixture(&session)
        })
        .await?;
    client
        .send_ok(SplitWindowRequest::fixture(PaneTarget::with_window(
            session.clone(),
            3,
            0,
        )))
        .await?;
    client
        .send_ok(KillPaneRequest {
            target: PaneTarget::with_window(session.clone(), 3, 0),
            kill_all_except: false,
        })
        .await?;
    client
        .send_ok(SendKeysRequest::fixture((
            PaneTarget::with_window(session.clone(), 3, 0),
            ["export RMUX_TRANSFER_MARK=swapped", "Enter"],
        )))
        .await?;

    assert_eq!(
        client
            .send_request(&Request::SwapPane(SwapPaneRequest {
                source: PaneTarget::with_window(session.clone(), 2, 0),
                target: PaneTarget::with_window(session.clone(), 3, 0),
                direction: None,
                detached: true,
                preserve_zoom: false,
            }))
            .await?,
        Response::SwapPane(rmux_proto::SwapPaneResponse {
            source: PaneTarget::with_window(session.clone(), 2, 0),
            target: PaneTarget::with_window(session.clone(), 3, 0),
        })
    );
    client
        .send_ok(SendKeysRequest::fixture((
            PaneTarget::with_window(session.clone(), 2, 0),
            [
                format!(
                    "printf \"$RMUX_TRANSFER_MARK\" > {}",
                    swap_source_path.display()
                ),
                "Enter".to_owned(),
            ],
        )))
        .await?;
    client
        .send_ok(SendKeysRequest::fixture((
            PaneTarget::with_window(session.clone(), 3, 0),
            [
                format!(
                    "printf \"$RMUX_TRANSFER_MARK\" > {}",
                    swap_target_path.display()
                ),
                "Enter".to_owned(),
            ],
        )))
        .await?;
    wait_for_file_contents(&swap_source_path, "swapped", FILE_TIMEOUT).await?;
    wait_for_file_contents(&swap_target_path, "joined", FILE_TIMEOUT).await?;

    handle.shutdown().await?;
    Ok(())
}
