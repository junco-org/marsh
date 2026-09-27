use super::lifecycle::kill_window;
use super::*;
use crate::test_fixtures::Quiet;

#[tokio::test]
async fn list_windows_size_sort_uses_area_and_preserves_equal_area_order() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "list-windows-area-sort").await;
    for window_index in 1..=3 {
        insert_window(&handler, &alpha, window_index).await;
    }

    for (window_index, name, cols, rows) in [
        (0, "z-equal-first", 21, 10),
        (1, "a-equal-second", 10, 21),
        (2, "middle", 59, 5),
        (3, "large", 20, 24),
    ] {
        handler
            .handle_ok(RenameWindowRequest {
                target: WindowTarget::with_window(alpha.clone(), window_index),
                name: name.to_owned(),
            })
            .await;
        handler
            .handle_ok(ResizeWindowRequest {
                target: WindowTarget::with_window(alpha.clone(), window_index),
                width: Some(cols),
                height: Some(rows),
                adjustment: None,
            })
            .await;
    }

    let list = |reversed| ListWindowsRequest {
        target: alpha.clone(),
        format: Some("#{window_index}".to_owned()),
        filter: None,
        sort_order: Some("size".to_owned()),
        reversed,
    };

    let ascending = handler.handle_ok(list(false)).await;
    assert_eq!(ascending.output.stdout(), b"0\n1\n2\n3\n");

    let descending = handler.handle_ok(list(true)).await;
    assert_eq!(descending.output.stdout(), b"3\n2\n0\n1\n");
}

#[tokio::test]
async fn navigation_commands_wrap_and_remain_session_scoped() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    let beta = create_session(&handler, "beta").await;
    insert_window(&handler, &alpha, 3).await;
    insert_window(&handler, &alpha, 7).await;
    insert_window(&handler, &beta, 4).await;

    assert_eq!(
        handler
            .handle(Request::NextWindow(NextWindowRequest {
                target: alpha.clone(),
                alerts_only: false,
            }))
            .await,
        Response::NextWindow(rmux_proto::NextWindowResponse {
            target: WindowTarget::with_window(alpha.clone(), 3),
        })
    );
    assert_eq!(
        handler
            .handle(Request::NextWindow(NextWindowRequest {
                target: alpha.clone(),
                alerts_only: false,
            }))
            .await,
        Response::NextWindow(rmux_proto::NextWindowResponse {
            target: WindowTarget::with_window(alpha.clone(), 7),
        })
    );
    assert_eq!(
        handler
            .handle(Request::PreviousWindow(PreviousWindowRequest {
                target: alpha.clone(),
                alerts_only: false,
            }))
            .await,
        Response::PreviousWindow(rmux_proto::PreviousWindowResponse {
            target: WindowTarget::with_window(alpha.clone(), 3),
        })
    );
    assert_eq!(
        handler
            .handle(Request::LastWindow(LastWindowRequest {
                target: alpha.clone(),
            }))
            .await,
        Response::LastWindow(rmux_proto::LastWindowResponse {
            target: WindowTarget::with_window(alpha.clone(), 7),
        })
    );

    let state = handler.state.lock().await;
    let alpha_session = state
        .sessions
        .session(&alpha)
        .expect("alpha session should exist");
    let beta_session = state
        .sessions
        .session(&beta)
        .expect("beta session should exist");
    assert_eq!(alpha_session.active_window_index(), 7);
    assert_eq!(alpha_session.last_window_index(), Some(3));
    assert_eq!(beta_session.active_window_index(), 0);
    assert_eq!(beta_session.last_window_index(), None);
}

#[tokio::test]
async fn navigation_commands_return_tmux_style_errors_when_history_is_missing() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    insert_window(&handler, &alpha, 2).await;

    assert_eq!(
        handler
            .handle(Request::LastWindow(LastWindowRequest {
                target: alpha.clone(),
            }))
            .await,
        Response::Error(rmux_proto::ErrorResponse {
            error: rmux_proto::RmuxError::Message("no last window".to_owned()),
        })
    );

    assert_eq!(
        kill_window(&handler, &alpha, 2).await,
        WindowTarget::with_window(alpha.clone(), 0)
    );

    assert_eq!(
        handler
            .handle(Request::NextWindow(NextWindowRequest {
                target: alpha,
                alerts_only: false,
            }))
            .await,
        Response::Error(rmux_proto::ErrorResponse {
            error: rmux_proto::RmuxError::Message("no next window".to_owned()),
        })
    );
}

#[tokio::test]
async fn list_windows_returns_structured_entries_and_rendered_stdout() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;
    insert_window(&handler, &alpha, 2).await;

    handler
        .handle_ok(RenameWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 2),
            name: "logs".to_owned(),
        })
        .await;
    handler
        .handle_ok(SelectWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 2),
        })
        .await;

    let response = handler
        .handle_ok(ListWindowsRequest {
            target: alpha.clone(),
            format: Some("#{window_index}:#{window_id}:#{window_last_flag}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
        })
        .await;
    assert_eq!(response.windows.len(), 2);
    assert_eq!(
        response.windows[0].target,
        WindowTarget::with_window(alpha.clone(), 0)
    );
    assert_eq!(response.windows[0].window_id, "@0");
    assert_eq!(response.windows[0].rendered, "0:@0:1");
    assert!(response.windows[0].last);
    assert!(!response.windows[0].active);
    assert_eq!(
        response.windows[1].target,
        WindowTarget::with_window(alpha.clone(), 2)
    );
    assert_eq!(response.windows[1].name.as_deref(), Some("logs"));
    assert_eq!(response.windows[1].window_id, "@1");
    assert_eq!(response.windows[1].rendered, "2:@1:0");
    assert!(response.windows[1].active);
    assert_eq!(
        std::str::from_utf8(response.output.stdout()).expect("list-windows output is utf-8"),
        "0:@0:1\n2:@1:0\n"
    );
}

#[tokio::test]
async fn list_windows_format_uses_each_windows_active_pane_context() {
    let handler = RequestHandler::new();
    let alpha = create_session(&handler, "alpha").await;

    handler.handle_ok(SplitWindowRequest::fixture(&alpha)).await;
    insert_window(&handler, &alpha, 2).await;

    let expected_active_panes = {
        let state = handler.state.lock().await;
        let session = state.sessions.session(&alpha).expect("alpha exists");
        session
            .windows()
            .iter()
            .map(|(window_index, window)| {
                format!("{}:{}", window_index, window.active_pane_index())
            })
            .collect::<Vec<_>>()
    };

    let response = handler
        .handle_ok(ListWindowsRequest {
            target: alpha.clone(),
            format: Some("#{window_index}:#{pane_index}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
        })
        .await;
    assert_eq!(
        response
            .windows
            .iter()
            .map(|window| window.rendered.clone())
            .collect::<Vec<_>>(),
        expected_active_panes
    );
}

#[tokio::test]
async fn window_mutations_refresh_attached_sessions() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = create_session(&handler, "alpha").await;

    let mut control_rx = handler.attach_client(requester_pid, &alpha).await;
    drain_attach_controls(&mut control_rx).await;

    handler.create_window(Quiet(&alpha)).await;
    assert_refresh(control_rx.try_recv().expect("new-window refresh"));

    handler
        .handle_ok(SelectWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 1),
        })
        .await;
    assert_refresh(control_rx.try_recv().expect("select-window refresh"));

    assert!(matches!(
        handler
            .handle(Request::NextWindow(NextWindowRequest {
                target: alpha.clone(),
                alerts_only: false,
            }))
            .await,
        Response::NextWindow(_)
    ));
    assert_refresh(control_rx.try_recv().expect("next-window refresh"));

    assert!(matches!(
        handler
            .handle(Request::PreviousWindow(PreviousWindowRequest {
                target: alpha.clone(),
                alerts_only: false,
            }))
            .await,
        Response::PreviousWindow(_)
    ));
    assert_refresh(control_rx.try_recv().expect("previous-window refresh"));

    assert!(matches!(
        handler
            .handle(Request::LastWindow(LastWindowRequest {
                target: alpha.clone(),
            }))
            .await,
        Response::LastWindow(_)
    ));
    assert_refresh(control_rx.try_recv().expect("last-window refresh"));

    handler
        .handle_ok(RenameWindowRequest {
            target: WindowTarget::with_window(alpha.clone(), 1),
            name: "logs".to_owned(),
        })
        .await;
    assert_refresh(control_rx.try_recv().expect("rename-window refresh"));

    kill_window(&handler, &alpha, 1).await;
    assert_refresh(control_rx.try_recv().expect("kill-window refresh"));
    drain_attach_controls(&mut control_rx).await;

    handler
        .handle_ok(ListWindowsRequest {
            target: alpha,
            format: None,
            filter: None,
            sort_order: None,
            reversed: false,
        })
        .await;
    match timeout(Duration::from_millis(100), control_rx.recv()).await {
        Err(_) | Ok(None) => {}
        Ok(Some(control)) => {
            panic!("list-windows should not refresh attached clients, got {control:?}")
        }
    }
}
