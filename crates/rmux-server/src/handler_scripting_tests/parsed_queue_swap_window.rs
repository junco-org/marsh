use super::*;

#[tokio::test]
async fn parsed_queue_accepts_tmux_swap_window_flag_clusters() {
    for case in [
        SwapClusterCase {
            session: "swap-ds",
            command: "swap-window -ds one -tswap-ds:0",
            add_d_window: false,
            expected_names: &["one", "rmux"],
        },
        SwapClusterCase {
            session: "swap-dt",
            command: "swap-window -dt +1 -sswap-dt:0",
            add_d_window: false,
            expected_names: &["one", "rmux"],
        },
        SwapClusterCase {
            session: "swap-sd",
            command: "swap-window -sd -t swap-sd:1",
            add_d_window: true,
            expected_names: &["rmux", "d", "one"],
        },
    ] {
        assert_parsed_swap_cluster(case).await;
    }
}

#[tokio::test]
async fn parsed_queue_does_not_expand_the_swap_window_flag_surface() {
    let handler = RequestHandler::new();
    let alpha = handler.create_session("alpha").await;
    handler
        .create_window(NewWindowRequest {
            name: Some("one".to_owned()),
            target_window_index: Some(1),
            ..Fixture::fixture(&alpha)
        })
        .await;

    let parsed = CommandParser::new()
        .parse("swap-window -ad -s alpha:0 -t alpha:1")
        .expect("unknown flags still tokenize");
    let error = handler
        .execute_parsed_commands_for_test(std::process::id(), parsed)
        .await
        .expect_err("-a must remain unsupported for swap-window");

    assert_eq!(
        error.to_string(),
        "server error: command swap-window: unknown flag -a"
    );
}

struct SwapClusterCase {
    session: &'static str,
    command: &'static str,
    add_d_window: bool,
    expected_names: &'static [&'static str],
}

async fn assert_parsed_swap_cluster(case: SwapClusterCase) {
    let handler = RequestHandler::new();
    let session_name = handler.create_session(case.session).await;

    handler
        .create_window(NewWindowRequest {
            name: Some("one".to_owned()),
            target_window_index: Some(1),
            ..Fixture::fixture(&session_name)
        })
        .await;
    if case.add_d_window {
        handler
            .create_window(NewWindowRequest {
                name: Some("d".to_owned()),
                target_window_index: Some(2),
                ..Fixture::fixture(&session_name)
            })
            .await;
    }

    let parsed = CommandParser::new()
        .parse(case.command)
        .unwrap_or_else(|error| panic!("{} should tokenize: {error}", case.command));
    handler
        .execute_parsed_commands(std::process::id(), parsed, pane_context(&session_name, 0))
        .await
        .unwrap_or_else(|error| panic!("{} should execute: {error}", case.command));

    let state = handler.state.lock().await;
    let session = state
        .sessions
        .session(&session_name)
        .expect("session exists");
    let names = session
        .windows()
        .values()
        .map(|window| window.name().expect("named fixture window"))
        .collect::<Vec<_>>();
    assert_eq!(names, case.expected_names, "{}", case.command);
}
