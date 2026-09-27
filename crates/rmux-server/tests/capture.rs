mod common;

use std::error::Error;
use std::time::Duration;

use common::{
    create_session, send, send_ok, send_request, session_name, start_server, wait_for_capture,
    Fixture, TestHarness, PTY_TEST_LOCK,
};
use rmux_proto::{
    CapturePaneRequest, PaneTarget, Request, Response, SendKeysRequest, ShowBufferRequest,
};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);

#[tokio::test(flavor = "multi_thread")]
async fn capture_pane_reads_unattached_transcript() -> Result<(), Box<dyn Error>> {
    let _pty_guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("capture-unattached");
    let server = start_server(&harness).await?;
    let target = PaneTarget::with_window(session_name("alpha"), 0, 0);
    let marker = "server_capture_unattached_marker";

    create_session(harness.socket_path(), "alpha").await?;
    send_ok(
        harness.socket_path(),
        SendKeysRequest::fixture((
            &target,
            [format!("printf '{marker}\\n'"), "Enter".to_owned()],
        )),
    )
    .await?;

    let output = wait_for_capture(harness.socket_path(), &target, marker, CAPTURE_TIMEOUT).await?;
    assert!(output.contains(marker));

    let captured = send(
        harness.socket_path(),
        CapturePaneRequest {
            print: false,
            buffer_name: Some("server-cap".to_owned()),
            ..Fixture::fixture(target)
        },
    )
    .await?;
    match captured {
        Response::CapturePane(response) => {
            assert_eq!(response.buffer_name.as_deref(), Some("server-cap"));
            assert!(response.command_output().is_none());
        }
        other => panic!("expected capture-pane response, got {other:?}"),
    }

    let show = send_request(
        harness.socket_path(),
        &Request::ShowBuffer(ShowBufferRequest {
            name: Some("server-cap".to_owned()),
        }),
    )
    .await?;
    assert!(String::from_utf8_lossy(
        show.command_output()
            .expect("show-buffer returns output")
            .stdout()
    )
    .contains(marker));

    server.shutdown().await?;
    Ok(())
}
