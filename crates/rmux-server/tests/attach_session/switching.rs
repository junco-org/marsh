use std::error::Error;

use rmux_proto::{
    AttachSessionRequest, OptionName, PaneTarget, Request, Response, ScopeSelector,
    SelectPaneRequest, SendKeysRequest, SetOptionRequest, SplitDirection, SplitWindowRequest,
    SwitchClientRequest, WindowTarget,
};
use tokio::time::timeout;

use crate::common::{
    create_session, kill_session, read_attach_until_contains, send_ok, send_request, session_name,
    start_server, ClientConnection, Fixture, TestHarness, PTY_TEST_LOCK,
};
use crate::support::{send_attach_command, STEP_TIMEOUT};

#[tokio::test(flavor = "multi_thread")]
async fn switch_client_reroutes_attach_input_and_output() -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("switch-client");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;

    create_session(&socket_path, "alpha").await?;
    create_session(&socket_path, "beta").await?;

    let (_, mut attach_stream) = ClientConnection::connect(&socket_path)
        .await?
        .begin_attach(AttachSessionRequest {
            target: session_name("alpha"),
        })
        .await?;

    send_ok(
        &socket_path,
        SendKeysRequest::fixture((
            PaneTarget::new(session_name("alpha"), 0),
            ["printf alpha-output", "Enter"],
        )),
    )
    .await?;
    let alpha_output =
        read_attach_until_contains(&mut attach_stream, "alpha-output", STEP_TIMEOUT).await?;
    assert!(alpha_output.contains("alpha-output"));

    let switched = send_request(
        &socket_path,
        &Request::SwitchClient(SwitchClientRequest {
            target: session_name("beta"),
        }),
    )
    .await?;
    assert_eq!(
        switched,
        Response::SwitchClient(rmux_proto::SwitchClientResponse {
            session_name: session_name("beta"),
        })
    );

    send_attach_command(&mut attach_stream, "printf beta-input").await?;
    let beta_input =
        read_attach_until_contains(&mut attach_stream, "beta-input", STEP_TIMEOUT).await?;
    assert!(beta_input.contains("beta-input"));

    send_ok(
        &socket_path,
        SendKeysRequest::fixture((
            PaneTarget::new(session_name("beta"), 0),
            ["printf beta-output", "Enter"],
        )),
    )
    .await?;
    let beta_output =
        read_attach_until_contains(&mut attach_stream, "beta-output", STEP_TIMEOUT).await?;
    assert!(beta_output.contains("beta-output"));

    drop(attach_stream);
    for target in ["alpha", "beta"] {
        kill_session(&socket_path, target).await?;
    }
    timeout(STEP_TIMEOUT, handle.shutdown()).await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn switch_client_to_multi_pane_session_emits_border_frame_before_forwarding_io(
) -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("switch-client-borders");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let alpha = session_name("alpha");
    let beta = session_name("beta");

    create_session(&socket_path, &alpha).await?;
    create_session(&socket_path, &beta).await?;
    send_ok(
        &socket_path,
        SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(&beta)
        },
    )
    .await?;
    send_ok(
        &socket_path,
        SplitWindowRequest {
            direction: SplitDirection::Horizontal,
            ..Fixture::fixture(PaneTarget::new(beta.clone(), 1))
        },
    )
    .await?;
    send_ok(
        &socket_path,
        SelectPaneRequest::fixture(PaneTarget::new(beta.clone(), 2)),
    )
    .await?;

    for (scope, option, value) in [
        (
            ScopeSelector::Window(WindowTarget::new(beta.clone())),
            OptionName::PaneBorderStyle,
            "blue",
        ),
        (
            ScopeSelector::Window(WindowTarget::new(beta.clone())),
            OptionName::PaneActiveBorderStyle,
            "colour196",
        ),
        (
            ScopeSelector::Session(beta.clone()),
            OptionName::Status,
            "off",
        ),
    ] {
        send_ok(
            &socket_path,
            SetOptionRequest::fixture((scope, option, value)),
        )
        .await?;
    }

    let (_, mut attach_stream) = ClientConnection::connect(&socket_path)
        .await?
        .begin_attach(AttachSessionRequest { target: alpha })
        .await?;
    read_attach_until_contains(&mut attach_stream, "[alpha]", STEP_TIMEOUT).await?;

    let switched = send_request(
        &socket_path,
        &Request::SwitchClient(SwitchClientRequest {
            target: beta.clone(),
        }),
    )
    .await?;
    assert_eq!(
        switched,
        Response::SwitchClient(rmux_proto::SwitchClientResponse {
            session_name: beta.clone(),
        })
    );

    let border_text =
        read_attach_until_contains(&mut attach_stream, "\u{1b}[34m", STEP_TIMEOUT).await?;
    assert!(border_text.contains("\u{1b}[34m"));
    assert!(border_text.contains("\u{1b}[38;5;196m"));
    assert!(border_text.contains('│'));

    send_attach_command(&mut attach_stream, "printf beta-input").await?;
    let beta_input =
        read_attach_until_contains(&mut attach_stream, "beta-input", STEP_TIMEOUT).await?;
    assert!(beta_input.contains("beta-input"));

    send_ok(
        &socket_path,
        SendKeysRequest::fixture((
            PaneTarget::new(beta.clone(), 2),
            ["printf beta-output", "Enter"],
        )),
    )
    .await?;
    let beta_output =
        read_attach_until_contains(&mut attach_stream, "beta-output", STEP_TIMEOUT).await?;
    assert!(beta_output.contains("beta-output"));

    drop(attach_stream);
    for target in ["alpha", "beta"] {
        kill_session(&socket_path, target).await?;
    }
    timeout(STEP_TIMEOUT, handle.shutdown()).await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn switch_client_to_missing_session_keeps_the_current_attach_stream(
) -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("switch-missing");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;

    create_session(&socket_path, "alpha").await?;

    let (_, mut attach_stream) = ClientConnection::connect(&socket_path)
        .await?
        .begin_attach(AttachSessionRequest {
            target: session_name("alpha"),
        })
        .await?;

    let switched = send_request(
        &socket_path,
        &Request::SwitchClient(SwitchClientRequest {
            target: session_name("missing"),
        }),
    )
    .await?;
    assert_eq!(
        switched,
        Response::Error(rmux_proto::ErrorResponse {
            error: rmux_proto::RmuxError::SessionNotFound("missing".to_owned()),
        })
    );

    send_attach_command(&mut attach_stream, "printf still-alpha").await?;
    let still_alpha =
        read_attach_until_contains(&mut attach_stream, "still-alpha", STEP_TIMEOUT).await?;
    assert!(still_alpha.contains("still-alpha"));

    send_ok(
        &socket_path,
        SendKeysRequest::fixture((
            PaneTarget::new(session_name("alpha"), 0),
            ["printf still-output", "Enter"],
        )),
    )
    .await?;
    let still_output =
        read_attach_until_contains(&mut attach_stream, "still-output", STEP_TIMEOUT).await?;
    assert!(still_output.contains("still-output"));

    drop(attach_stream);
    kill_session(&socket_path, "alpha").await?;
    timeout(STEP_TIMEOUT, handle.shutdown()).await??;
    Ok(())
}
