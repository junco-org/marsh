mod common;

use std::error::Error;

use common::{send, send_ok, start_server, Fixture, TestHarness};
use rmux_proto::{
    IfShellRequest, Response, RunShellRequest, SetBufferRequest, WaitForMode, WaitForRequest,
};

#[tokio::test(flavor = "multi_thread")]
async fn run_shell_foreground_returns_status_and_stdout() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("run-shell-foreground");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;

    let response = send(&socket_path, RunShellRequest::fixture("printf server")).await?;

    let Response::RunShell(response) = response else {
        panic!("unexpected response: {response:?}");
    };
    assert_eq!(response.exit_status(), Some(0));
    assert_eq!(
        response
            .command_output()
            .expect("run-shell stdout")
            .stdout(),
        b"server\n"
    );
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn if_shell_rejects_unsupported_nested_command() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("if-shell-unsupported");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;

    let response = send(
        &socket_path,
        IfShellRequest::fixture(("1", "unsupported-command")),
    )
    .await?;

    assert!(matches!(response, Response::Error(_)));
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn if_shell_returns_nested_command_output() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("if-shell-output");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;

    send_ok(
        &socket_path,
        SetBufferRequest {
            name: Some("selected".to_owned()),
            ..Fixture::fixture(b"yes")
        },
    )
    .await?;

    let response = send(
        &socket_path,
        IfShellRequest::fixture(("1", "show-buffer -b selected")),
    )
    .await?;

    assert!(matches!(response, Response::IfShell(_)));
    assert_eq!(
        response.command_output().expect("if-shell output").stdout(),
        b"yes"
    );

    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_for_signal_without_waiters_is_not_latched() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("wait-for-signal-no-waiters");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;

    send_ok(
        &socket_path,
        WaitForRequest {
            channel: "empty".to_owned(),
            mode: WaitForMode::Signal,
        },
    )
    .await?;

    handle.shutdown().await?;
    Ok(())
}
