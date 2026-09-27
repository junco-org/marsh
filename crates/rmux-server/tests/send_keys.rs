use std::error::Error;
use std::fs;
use std::io;
use std::time::{Duration, Instant};

mod common;

use common::{
    capture_pane_text, kill_session, poll_file_contents, read_attach_until_contains, session_name,
    shell_quote, start_server, wait_for_capture, ClientConnection, Fixture, TestHarness,
    PTY_TEST_LOCK,
};
use rmux_proto::{
    AttachSessionRequest, DisplayMessageRequest, PaneTarget, Response, SelectPaneRequest,
    SendKeysRequest, SendKeysResponse, SplitWindowRequest, Target, TerminalSize,
};

const STEP_TIMEOUT: Duration = Duration::from_secs(15);

async fn wait_for_pane_current_command(
    client: &mut ClientConnection,
    target: PaneTarget,
    expected: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + STEP_TIMEOUT;
    let mut last = String::new();

    while Instant::now() < deadline {
        let response = client
            .send(DisplayMessageRequest {
                target: Some(Target::Pane(target.clone())),
                ..Fixture::fixture("#{pane_current_command}")
            })
            .await?;
        let Response::DisplayMessage(response) = response else {
            return Err(io::Error::other("display-message returned the wrong response").into());
        };
        let output = response
            .command_output()
            .expect("display-message -p returns command output");
        last = String::from_utf8_lossy(output.stdout()).trim().to_owned();
        if last == expected {
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    Err(io::Error::other(format!(
        "timed out waiting for foreground command {expected:?}; last={last:?}"
    ))
    .into())
}

#[tokio::test(flavor = "multi_thread")]
async fn send_keys_writes_to_the_correct_pane_through_the_socket() -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("send-keys");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let mut client = ClientConnection::connect(&socket_path).await?;

    client.create_session("alpha").await?;

    let (_, mut attach_stream) = ClientConnection::connect(&socket_path)
        .await?
        .begin_attach(AttachSessionRequest {
            target: session_name("alpha"),
        })
        .await?;

    let response = client
        .send(SendKeysRequest::fixture((
            PaneTarget::new(session_name("alpha"), 0),
            ["printf send-keys-ok", "Enter"],
        )))
        .await?;
    assert_eq!(
        response,
        Response::SendKeys(SendKeysResponse { key_count: 2 })
    );
    let output =
        read_attach_until_contains(&mut attach_stream, "send-keys-ok", STEP_TIMEOUT).await?;
    assert!(output.contains("send-keys-ok"));

    let empty_response = client
        .send(SendKeysRequest {
            target: PaneTarget::new(session_name("alpha"), 0),
            keys: vec![],
        })
        .await?;
    assert_eq!(
        empty_response,
        Response::SendKeys(SendKeysResponse { key_count: 0 })
    );

    let missing = client
        .send(SendKeysRequest::fixture((
            PaneTarget::new(session_name("missing"), 0),
            ["x"],
        )))
        .await?;
    assert!(matches!(missing, Response::Error(_)));

    let missing_pane = client
        .send(SendKeysRequest::fixture((
            PaneTarget::new(session_name("alpha"), 99),
            ["x"],
        )))
        .await?;
    assert!(matches!(missing_pane, Response::Error(_)));

    let empty_missing = client
        .send(SendKeysRequest {
            target: PaneTarget::new(session_name("nonexistent"), 0),
            keys: vec![],
        })
        .await?;
    assert!(matches!(empty_missing, Response::Error(_)));

    drop(attach_stream);
    kill_session(&socket_path, "alpha").await?;
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn send_keys_targets_the_correct_pane_in_a_multi_pane_session() -> Result<(), Box<dyn Error>>
{
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("send-keys-multi-pane");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let mut client = ClientConnection::connect(&socket_path).await?;

    client
        .create_session((
            "beta",
            TerminalSize {
                cols: 120,
                rows: 40,
            },
        ))
        .await?;
    client
        .send_ok(SplitWindowRequest::fixture(session_name("beta")))
        .await?;

    let pane0_response = client
        .send(SendKeysRequest::fixture((
            PaneTarget::new(session_name("beta"), 0),
            ["printf pane-zero", "Enter"],
        )))
        .await?;
    assert_eq!(
        pane0_response,
        Response::SendKeys(SendKeysResponse { key_count: 2 })
    );
    let pane0_output = wait_for_capture(
        &socket_path,
        &PaneTarget::new(session_name("beta"), 0),
        "pane-zero",
        STEP_TIMEOUT,
    )
    .await?;
    assert!(pane0_output.contains("pane-zero"));

    client
        .send_ok(SelectPaneRequest::fixture(PaneTarget::new(
            session_name("beta"),
            1,
        )))
        .await?;

    let pane1_response = client
        .send(SendKeysRequest::fixture((
            PaneTarget::new(session_name("beta"), 1),
            ["printf pane-one", "Enter"],
        )))
        .await?;
    assert_eq!(
        pane1_response,
        Response::SendKeys(SendKeysResponse { key_count: 2 })
    );
    let pane1_output = wait_for_capture(
        &socket_path,
        &PaneTarget::new(session_name("beta"), 1),
        "pane-one",
        STEP_TIMEOUT,
    )
    .await?;
    assert!(pane1_output.contains("pane-one"));
    let pane0_after_pane1 =
        capture_pane_text(&socket_path, &PaneTarget::new(session_name("beta"), 0)).await?;
    assert!(
        !pane0_after_pane1.contains("pane-one"),
        "pane-one output should remain isolated to pane 1, got pane 0 capture {pane0_after_pane1:?}"
    );

    kill_session(&socket_path, "beta").await?;
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn send_keys_ctrl_c_interrupts_a_real_pane_process() -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("send-keys-ctrl-c");
    let socket_path = harness.socket_path().to_path_buf();
    let root = socket_path
        .parent()
        .expect("socket path must have a parent");
    let recovery_path = root.join("ctrl-c-recovered.txt");
    let handle = start_server(&harness).await?;
    let mut client = ClientConnection::connect(&socket_path).await?;

    client.create_session("gamma").await?;

    let (_, mut attach_stream) = ClientConnection::connect(&socket_path)
        .await?
        .begin_attach(AttachSessionRequest {
            target: session_name("gamma"),
        })
        .await?;

    let start_sleep = client
        .send(SendKeysRequest::fixture((
            PaneTarget::new(session_name("gamma"), 0),
            ["sleep 5", "Enter"],
        )))
        .await?;
    assert_eq!(
        start_sleep,
        Response::SendKeys(SendKeysResponse { key_count: 2 })
    );
    let sleep_output =
        read_attach_until_contains(&mut attach_stream, "sleep 5", STEP_TIMEOUT).await?;
    assert!(sleep_output.contains("sleep 5"));
    wait_for_pane_current_command(
        &mut client,
        PaneTarget::new(session_name("gamma"), 0),
        "sleep",
    )
    .await?;

    let interrupt = client
        .send(SendKeysRequest::fixture((
            PaneTarget::new(session_name("gamma"), 0),
            ["C-c"],
        )))
        .await?;
    assert_eq!(
        interrupt,
        Response::SendKeys(SendKeysResponse { key_count: 1 })
    );
    let interrupt_output =
        read_attach_until_contains(&mut attach_stream, "^C", STEP_TIMEOUT).await?;
    assert!(
        interrupt_output.contains("^C"),
        "attach output should include the interrupt echo before recovery, got {interrupt_output:?}"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;

    let recovery_command = format!("printf ctrl-c-recovered > {}", shell_quote(&recovery_path));
    let recovery_deadline = Instant::now() + STEP_TIMEOUT;
    while Instant::now() < recovery_deadline {
        client
            .send_ok(SendKeysRequest::fixture((
                PaneTarget::new(session_name("gamma"), 0),
                [recovery_command.as_str(), "Enter"],
            )))
            .await?;
        if poll_file_contents(
            &recovery_path,
            "ctrl-c-recovered",
            Duration::from_millis(500),
        )
        .await?
        {
            break;
        }
    }
    assert_eq!(
        fs::read_to_string(&recovery_path).ok().as_deref(),
        Some("ctrl-c-recovered"),
        "shell should accept input again after ctrl-c"
    );

    drop(attach_stream);
    kill_session(&socket_path, "gamma").await?;
    handle.shutdown().await?;
    Ok(())
}
