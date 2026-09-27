use std::collections::BTreeSet;
use std::error::Error;
use std::path::PathBuf;
use std::time::{Duration, Instant};

mod common;

use common::{session_name, start_server, ClientConnection, Fixture, TestHarness, PTY_TEST_LOCK};
use rmux_proto::{
    KillWindowRequest, LastWindowRequest, ListPanesRequest, ListWindowsRequest, MoveWindowRequest,
    MoveWindowResponse, MoveWindowTarget, NewWindowRequest, NextWindowRequest, PaneTarget,
    PreviousWindowRequest, RenameWindowRequest, Request, Response, RotateWindowDirection,
    RotateWindowRequest, RotateWindowResponse, SelectWindowRequest, SessionName,
    SplitWindowRequest, SwapWindowRequest, SwapWindowResponse, TerminalSize, WindowTarget,
};

// PTY child processes can take a few seconds to appear consistently in
// `/proc/<pid>/task/*/children` under full-workspace test load.
const PTY_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_SIZE: TerminalSize = TerminalSize {
    cols: 120,
    rows: 40,
};

#[tokio::test(flavor = "multi_thread")]
async fn window_management_requests_round_trip_through_the_socket() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("window-management");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let mut client = ClientConnection::connect(&socket_path).await?;
    let session = session_name("alpha");

    client.create_session((&session, SESSION_SIZE)).await?;

    let new_window = client
        .send(NewWindowRequest {
            name: Some("logs".to_owned()),
            ..Fixture::fixture(&session)
        })
        .await?;
    assert_eq!(
        new_window,
        Response::NewWindow(rmux_proto::NewWindowResponse {
            target: WindowTarget::with_window(session.clone(), 1),
        })
    );

    let selected = client
        .send_request(&Request::SelectWindow(SelectWindowRequest {
            target: WindowTarget::with_window(session.clone(), 1),
        }))
        .await?;
    assert_eq!(
        selected,
        Response::SelectWindow(rmux_proto::SelectWindowResponse {
            target: WindowTarget::with_window(session.clone(), 1),
        })
    );

    let renamed = client
        .send_request(&Request::RenameWindow(RenameWindowRequest {
            target: WindowTarget::with_window(session.clone(), 1),
            name: "renamed".to_owned(),
        }))
        .await?;
    assert_eq!(
        renamed,
        Response::RenameWindow(rmux_proto::RenameWindowResponse {
            target: WindowTarget::with_window(session.clone(), 1),
        })
    );

    let killed = client
        .send_request(&Request::KillWindow(KillWindowRequest {
            target: WindowTarget::with_window(session.clone(), 1),
            kill_all_others: false,
        }))
        .await?;
    assert_eq!(
        killed,
        Response::KillWindow(rmux_proto::KillWindowResponse {
            target: WindowTarget::with_window(session, 0),
        })
    );

    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_window_all_others_cleans_up_removed_window_ptys() -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("kill-window-pty-cleanup");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let mut client = ClientConnection::connect(&socket_path).await?;
    let session = session_name("alpha");

    client.create_session((&session, SESSION_SIZE)).await?;

    client.send_ok(NewWindowRequest::fixture(&session)).await?;
    client
        .send_ok(SelectWindowRequest {
            target: WindowTarget::with_window(session.clone(), 1),
        })
        .await?;
    assert_eq!(
        client.send(SplitWindowRequest::fixture(&session)).await?,
        Response::SplitWindow(rmux_proto::SplitWindowResponse {
            pane: PaneTarget::with_window(session.clone(), 1, 1),
        })
    );
    client.send_ok(NewWindowRequest::fixture(&session)).await?;

    let all_pane_ttys = wait_for_session_pane_ttys(&mut client, &session, 4).await?;
    let target_window_ttys = all_pane_ttys
        .iter()
        .filter(|(window_index, _, _)| *window_index == 1)
        .map(|(_, _, tty)| tty.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(target_window_ttys.len(), 2);

    let killed = client
        .send_request(&Request::KillWindow(KillWindowRequest {
            target: WindowTarget::with_window(session.clone(), 1),
            kill_all_others: true,
        }))
        .await?;
    assert_eq!(
        killed,
        Response::KillWindow(rmux_proto::KillWindowResponse {
            target: WindowTarget::with_window(session, 1),
        })
    );

    let remaining_ttys = wait_for_session_pane_ttys(&mut client, &session_name("alpha"), 2)
        .await?
        .into_iter()
        .map(|(_, _, tty)| tty)
        .collect::<BTreeSet<_>>();
    assert_eq!(remaining_ttys, target_window_ttys);

    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn window_navigation_and_listing_requests_round_trip_through_the_socket(
) -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("window-navigation-management");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let mut client = ClientConnection::connect(&socket_path).await?;
    let session = session_name("alpha");

    client.create_session((&session, SESSION_SIZE)).await?;

    for name in ["logs", "shell"] {
        client
            .send_ok(NewWindowRequest {
                name: Some(name.to_owned()),
                ..Fixture::fixture(&session)
            })
            .await?;
    }
    assert_eq!(
        client
            .send_request(&Request::NextWindow(NextWindowRequest {
                target: session.clone(),
                alerts_only: false,
            }))
            .await?,
        Response::NextWindow(rmux_proto::NextWindowResponse {
            target: WindowTarget::with_window(session.clone(), 1),
        })
    );
    assert_eq!(
        client
            .send_request(&Request::PreviousWindow(PreviousWindowRequest {
                target: session.clone(),
                alerts_only: false,
            }))
            .await?,
        Response::PreviousWindow(rmux_proto::PreviousWindowResponse {
            target: WindowTarget::with_window(session.clone(), 0),
        })
    );
    assert_eq!(
        client
            .send_request(&Request::LastWindow(LastWindowRequest {
                target: session.clone(),
            }))
            .await?,
        Response::LastWindow(rmux_proto::LastWindowResponse {
            target: WindowTarget::with_window(session.clone(), 1),
        })
    );

    let listed = client
        .send(ListWindowsRequest::fixture((
            session,
            "#{window_index}:#{window_id}:#{window_active}",
        )))
        .await?;
    let Response::ListWindows(listed) = listed else {
        panic!("expected list-windows response");
    };
    assert_eq!(
        std::str::from_utf8(listed.output.stdout())?,
        "0:@0:0\n1:@1:1\n2:@2:0\n"
    );

    handle.shutdown().await?;
    Ok(())
}

async fn wait_for_session_pane_ttys(
    client: &mut ClientConnection,
    session: &SessionName,
    expected_count: usize,
) -> Result<Vec<(u32, u32, PathBuf)>, Box<dyn Error>> {
    let deadline = Instant::now() + PTY_TIMEOUT;

    while Instant::now() < deadline {
        let listed = client
            .send(ListPanesRequest::fixture((
                session,
                "#{window_index}:#{pane_index}:#{pane_tty}",
            )))
            .await?;
        let output = listed
            .command_output()
            .ok_or("list-panes response did not include command output")?;
        let mut pane_ttys = std::str::from_utf8(output.stdout())?
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, ':');
                Some((parts.next()?, parts.next()?, parts.next()?))
            })
            .map(|(window_index, pane_index, tty)| {
                let window_index = window_index
                    .parse::<u32>()
                    .map_err(|error| format!("invalid window index '{window_index}': {error}"))?;
                let pane_index = pane_index
                    .parse::<u32>()
                    .map_err(|error| format!("invalid pane index '{pane_index}': {error}"))?;
                Ok((window_index, pane_index, PathBuf::from(tty)))
            })
            .collect::<Result<Vec<_>, String>>()?;
        pane_ttys.sort_by_key(|(window_index, pane_index, _)| (*window_index, *pane_index));
        if pane_ttys.len() == expected_count
            && pane_ttys
                .iter()
                .all(|(_, _, tty)| !tty.as_os_str().is_empty())
        {
            return Ok(pane_ttys);
        }

        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    Err(
        format!("timed out waiting for {expected_count} pane tty paths in session {session}")
            .into(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn window_move_swap_and_rotate_requests_round_trip_through_the_socket(
) -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("window-movement-management");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let mut client = ClientConnection::connect(&socket_path).await?;
    let alpha = session_name("alpha");
    let beta = session_name("beta");

    for session in [&alpha, &beta] {
        client.create_session((session, SESSION_SIZE)).await?;
    }

    for name in ["logs", "scratch"] {
        client
            .send_ok(NewWindowRequest {
                name: Some(name.to_owned()),
                ..Fixture::fixture(&alpha)
            })
            .await?;
    }

    let moved = client
        .send_request(&Request::MoveWindow(MoveWindowRequest {
            source: Some(WindowTarget::with_window(alpha.clone(), 1)),
            target: MoveWindowTarget::Window(WindowTarget::with_window(beta.clone(), 4)),
            renumber: false,
            kill_destination: false,
            detached: true,
            after: false,
            before: false,
        }))
        .await?;
    assert_eq!(
        moved,
        Response::MoveWindow(MoveWindowResponse {
            session_name: beta.clone(),
            target: Some(WindowTarget::with_window(beta.clone(), 4)),
        })
    );

    let swapped = client
        .send_request(&Request::SwapWindow(SwapWindowRequest {
            source: WindowTarget::with_window(alpha.clone(), 2),
            target: WindowTarget::with_window(beta.clone(), 4),
            detached: true,
        }))
        .await?;
    assert_eq!(
        swapped,
        Response::SwapWindow(SwapWindowResponse {
            source: WindowTarget::with_window(alpha.clone(), 2),
            target: WindowTarget::with_window(beta.clone(), 4),
        })
    );

    assert_eq!(
        client
            .send(SplitWindowRequest::fixture(PaneTarget::with_window(
                alpha.clone(),
                2,
                0,
            )))
            .await?,
        Response::SplitWindow(rmux_proto::SplitWindowResponse {
            pane: PaneTarget::with_window(alpha.clone(), 2, 1),
        })
    );

    let rotated = client
        .send_request(&Request::RotateWindow(RotateWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 2),
            direction: RotateWindowDirection::Up,
            restore_zoom: false,
        }))
        .await?;
    assert_eq!(
        rotated,
        Response::RotateWindow(RotateWindowResponse {
            target: WindowTarget::with_window(alpha.clone(), 2),
        })
    );

    let listed = client
        .send(ListWindowsRequest::fixture((
            alpha.clone(),
            "#{window_index}:#{window_panes}",
        )))
        .await?;
    let Response::ListWindows(listed) = listed else {
        panic!("expected list-windows response");
    };
    assert_eq!(std::str::from_utf8(listed.output.stdout())?, "0:1\n2:2\n");

    handle.shutdown().await?;
    Ok(())
}
