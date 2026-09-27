use std::error::Error;
use std::fs;
use std::path::Path;
use std::time::Duration;

mod common;

use common::{
    create_session, kill_session, read_response_exact, send, send_ok, session_name, shell_quote,
    shell_quote_str, start_server, wait_for_file_contents, ClientConnection, Fixture, TestHarness,
};
use rmux_proto::{
    encode_frame, AttachSessionRequest, ErrorResponse, HookLifecycle, HookName, NewSessionRequest,
    Request, Response, RmuxError, ScopeSelector, SetHookRequest,
};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::time::{sleep, timeout};

const ATTACH_SETTLE_DELAY: Duration = Duration::from_millis(50);
const STEP_TIMEOUT: Duration = Duration::from_millis(150);
const WAIT_TIMEOUT: Duration = Duration::from_secs(2);

#[tokio::test(flavor = "multi_thread")]
async fn persistent_client_attached_hooks_run_on_every_attach() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("persistent-client-attached-hook");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let output_path = hook_output_path(&socket_path);

    create_session(&socket_path, "alpha").await?;
    register_hook(
        &socket_path,
        ScopeSelector::Global,
        "printf ab >> {path} && printf cd >> {path}",
        &output_path,
        HookLifecycle::Persistent,
    )
    .await?;

    attach_once(&socket_path, "alpha").await?;
    attach_once(&socket_path, "alpha").await?;
    wait_for_file_contents(&output_path, "abcdabcd", WAIT_TIMEOUT).await?;

    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn one_shot_client_attached_hooks_are_removed_after_dispatch() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("oneshot-client-attached-hook");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let output_path = hook_output_path(&socket_path);

    create_session(&socket_path, "keepalive").await?;
    create_session(&socket_path, "alpha").await?;
    register_hook(
        &socket_path,
        ScopeSelector::Session(session_name("alpha")),
        "printf once >> {path}",
        &output_path,
        HookLifecycle::OneShot,
    )
    .await?;

    attach_once(&socket_path, "alpha").await?;
    attach_once(&socket_path, "alpha").await?;
    wait_for_file_contents(&output_path, "once", WAIT_TIMEOUT).await?;
    sleep(Duration::from_millis(100)).await;
    assert_eq!(fs::read_to_string(&output_path)?, "once");

    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn session_scoped_hooks_are_cleared_when_sessions_are_killed() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("session-hook-cleanup-on-kill");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let output_path = hook_output_path(&socket_path);

    create_session(&socket_path, "keepalive")
        .await
        .map_err(|error| format!("create keepalive: {error}"))?;
    create_session(&socket_path, "alpha")
        .await
        .map_err(|error| format!("create alpha before hook: {error}"))?;
    register_hook(
        &socket_path,
        ScopeSelector::Session(session_name("alpha")),
        "printf stale >> {path}",
        &output_path,
        HookLifecycle::Persistent,
    )
    .await
    .map_err(|error| format!("register alpha hook: {error}"))?;
    kill_session(&socket_path, "alpha")
        .await
        .map_err(|error| format!("kill alpha: {error}"))?;
    create_session(&socket_path, "alpha")
        .await
        .map_err(|error| format!("recreate alpha: {error}"))?;

    attach_once(&socket_path, "alpha")
        .await
        .map_err(|error| format!("attach recreated alpha: {error}"))?;
    sleep(Duration::from_millis(100)).await;
    assert!(
        !output_path.exists(),
        "recreated sessions must not inherit prior session-scoped hooks"
    );

    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn session_created_hooks_run_only_after_successful_creates() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("session-created-hook");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let output_path = hook_output_path(&socket_path);

    register_hook_for(
        &socket_path,
        ScopeSelector::Global,
        HookName::SessionCreated,
        "printf created > {path}",
        &output_path,
        HookLifecycle::Persistent,
    )
    .await?;

    create_session(&socket_path, "alpha").await?;
    wait_for_file_contents(&output_path, "created", WAIT_TIMEOUT).await?;
    fs::remove_file(&output_path)?;

    let duplicate = send(&socket_path, NewSessionRequest::fixture("alpha")).await?;
    assert!(matches!(duplicate, Response::Error(_)));
    sleep(Duration::from_millis(100)).await;
    assert!(
        !output_path.exists(),
        "failed session creates must not run session-created hooks"
    );

    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_hooks_do_not_block_attach_completion() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("nonblocking-client-attached-hook");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let output_path = hook_output_path(&socket_path);

    create_session(&socket_path, "alpha").await?;
    register_hook(
        &socket_path,
        ScopeSelector::Global,
        "sleep 0.3; printf ready > {path}",
        &output_path,
        HookLifecycle::Persistent,
    )
    .await?;

    let (_, attach_stream) = timeout(STEP_TIMEOUT, async {
        ClientConnection::connect(&socket_path)
            .await?
            .begin_attach(AttachSessionRequest {
                target: session_name("alpha"),
            })
            .await
    })
    .await??;

    assert!(
        !output_path.exists(),
        "slow hook should not complete before attach returns"
    );
    wait_for_file_contents(&output_path, "ready", WAIT_TIMEOUT).await?;

    drop(attach_stream);
    sleep(ATTACH_SETTLE_DELAY).await;
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_hook_event_wire_values_are_rejected() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("invalid-hook-event");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let mut stream = UnixStream::connect(&socket_path).await?;
    let mut frame = encode_frame(&Request::SetHook(SetHookRequest::fixture((
        ScopeSelector::Global,
        HookName::ClientAttached,
        "true",
    ))))?;

    assert_eq!(&frame[12..16], &[0, 0, 0, 0]);
    frame[12..16].copy_from_slice(&70_u32.to_le_bytes());
    stream.write_all(&frame).await?;

    match read_response_exact(&mut stream).await? {
        Response::Error(ErrorResponse {
            error: RmuxError::Decode(message),
        }) => assert!(
            message.contains("variant"),
            "expected enum-variant decode failure, received: {message}"
        ),
        other => panic!("unexpected response for invalid hook event: {other:?}"),
    }

    create_session(&socket_path, "alpha").await?;
    handle.shutdown().await?;
    Ok(())
}

async fn register_hook(
    socket_path: &Path,
    scope: ScopeSelector,
    command_template: &str,
    output_path: &Path,
    lifecycle: HookLifecycle,
) -> Result<(), Box<dyn Error>> {
    register_hook_for(
        socket_path,
        scope,
        HookName::ClientAttached,
        command_template,
        output_path,
        lifecycle,
    )
    .await
}

async fn register_hook_for(
    socket_path: &Path,
    scope: ScopeSelector,
    hook: HookName,
    command_template: &str,
    output_path: &Path,
    lifecycle: HookLifecycle,
) -> Result<(), Box<dyn Error>> {
    let shell_command = command_template.replace("{path}", &shell_quote(output_path));
    let command = format!("run-shell {}", shell_quote_str(&shell_command));
    let response = send_ok(
        socket_path,
        SetHookRequest {
            lifecycle,
            ..Fixture::fixture((scope.clone(), hook, command))
        },
    )
    .await?;

    assert_eq!(
        response,
        rmux_proto::SetHookResponse {
            scope,
            hook,
            lifecycle,
        }
    );
    Ok(())
}

async fn attach_once(socket_path: &Path, name: &str) -> Result<(), Box<dyn Error>> {
    let (_, attach_stream) = ClientConnection::connect(socket_path)
        .await?
        .begin_attach(AttachSessionRequest {
            target: session_name(name),
        })
        .await?;

    drop(attach_stream);
    sleep(ATTACH_SETTLE_DELAY).await;
    Ok(())
}

fn hook_output_path(socket_path: &Path) -> std::path::PathBuf {
    socket_path
        .parent()
        .expect("test harness socket path has a parent directory")
        .join("hook-output.txt")
}
