use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rmux_ipc::{LocalStream, PeerIdentity};
use rmux_proto::{
    CreateWebShareRequest, ListWebSharesRequest, Request, Response, SessionName, WebShareRequest,
    WebShareScope,
};
use tokio::sync::watch;

use super::connection_test_support::{read_test_response, spawn_connection, write_test_request};
use crate::handler::{RequestHandler, UndeliveredWebShareGuard};
use crate::test_fixtures::{unique_temp_path, wait_until, Fixture, SessionSpec, TestRequest};

#[tokio::test]
async fn client_disconnect_cancels_web_share_tunnel_start() -> io::Result<()> {
    let _env_lock = crate::test_env::lock_async().await;
    let fixture = TestDirectory::new("web-share-disconnect")?;
    let marker = fixture.path().join("provider.pid");
    let provider = fixture.path().join("provider.sh");
    std::fs::write(
        &provider,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" >'{}'\nexec sleep 30\n",
            marker.display()
        ),
    )?;
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(
        fixture.path().join("slow.toml"),
        format!(
            "name = \"slow\"\nprogram = \"{}\"\nurl_pattern = \"https://example.invalid\"\nurl_source = \"stdout\"\nready_timeout_secs = 10\n",
            provider.display()
        ),
    )?;
    let fixture_path = fixture.path().to_string_lossy();
    let _preset_dir =
        crate::test_env::EnvVarGuard::set("RMUX_TUNNEL_PRESET_DIR", Some(&fixture_path));

    let handler = Arc::new(RequestHandler::new());
    handler.mark_web_listener_available();
    let session_name = SessionSpec::create(&handler, "disconnect-tunnel").await;
    let (mut client, _shutdown_tx, mut connection_task) = spawn_test_connection(&handler)?;
    write_test_request(
        &mut client,
        create_web_share_request(session_name, Some("slow")),
    )
    .await?;

    let provider_pid = wait_for_pid_marker(&marker).await;
    let mut provider_cleanup = ProcessCleanup::new(provider_pid);
    drop(client);
    let connection_result =
        tokio::time::timeout(Duration::from_secs(2), &mut connection_task).await;
    let connection_stopped = matches!(&connection_result, Ok(Ok(Ok(()))));
    let provider_stopped = wait_for_process_exit(provider_pid).await;

    if connection_result.is_err() {
        connection_task.abort();
        let _ = connection_task.await;
    }
    if provider_stopped {
        provider_cleanup.disarm();
    }

    assert!(
        connection_stopped,
        "disconnected web-share create remained blocked on tunnel startup"
    );
    assert!(
        provider_stopped,
        "disconnected web-share create left its tunnel provider running"
    );
    Ok(())
}

#[tokio::test]
async fn undelivered_web_share_response_is_rolled_back() {
    let handler = Arc::new(RequestHandler::new());
    handler.mark_web_listener_available();
    let session_name = SessionSpec::create(&handler, "undelivered-share").await;
    let response = handler
        .handle(create_web_share_request(session_name, None))
        .await;
    let guard = UndeliveredWebShareGuard::for_response(&handler, &response)
        .expect("create response arms rollback");

    drop(guard);

    let listed = handler
        .handle(Request::WebShare(Box::new(WebShareRequest::List(
            ListWebSharesRequest,
        ))))
        .await;
    assert!(matches!(
        listed,
        Response::WebShare(response)
            if matches!(response.as_ref(), rmux_proto::WebShareResponse::List(list) if list.shares.is_empty())
    ));
}

#[tokio::test]
async fn disconnect_after_share_creation_before_hooks_rolls_back() -> io::Result<()> {
    let handler = Arc::new(RequestHandler::new());
    handler.mark_web_listener_available();
    let session_name = SessionSpec::create(&handler, "disconnect-after-create").await;
    let pause = handler.install_web_share_delivery_pause();
    let (mut client, _shutdown_tx, connection_task) = spawn_test_connection(&handler)?;

    write_test_request(&mut client, create_web_share_request(session_name, None)).await?;
    tokio::time::timeout(Duration::from_secs(2), pause.wait_until_reached())
        .await
        .expect("web-share dispatch reached guarded pre-hook interval");
    assert_eq!(list_web_shares(&handler).await.len(), 1);

    drop(client);
    tokio::time::timeout(Duration::from_secs(2), connection_task)
        .await
        .expect("disconnected connection stopped")
        .expect("connection task joined")?;

    assert!(list_web_shares(&handler).await.is_empty());
    Ok(())
}

#[tokio::test]
async fn delivered_share_disarms_pre_hook_rollback() -> io::Result<()> {
    let handler = Arc::new(RequestHandler::new());
    handler.mark_web_listener_available();
    let session_name = SessionSpec::create(&handler, "delivered-after-hooks").await;
    let pause = handler.install_web_share_delivery_pause();
    let (mut client, _shutdown_tx, connection_task) = spawn_test_connection(&handler)?;

    write_test_request(&mut client, create_web_share_request(session_name, None)).await?;
    tokio::time::timeout(Duration::from_secs(2), pause.wait_until_reached())
        .await
        .expect("web-share dispatch reached guarded pre-hook interval");
    pause.release();
    let response = tokio::time::timeout(Duration::from_secs(2), read_test_response(&mut client))
        .await
        .expect("web-share response was delivered")?;
    assert!(matches!(
        response,
        Response::WebShare(response)
            if matches!(response.as_ref(), rmux_proto::WebShareResponse::Created(_))
    ));
    assert_eq!(list_web_shares(&handler).await.len(), 1);

    drop(client);
    tokio::time::timeout(Duration::from_secs(2), connection_task)
        .await
        .expect("delivered connection stopped")
        .expect("connection task joined")?;
    assert_eq!(list_web_shares(&handler).await.len(), 1);
    Ok(())
}

async fn list_web_shares(handler: &RequestHandler) -> Vec<rmux_proto::WebShareSummary> {
    let response = TestRequest::send_ok(handler, WebShareRequest::List(ListWebSharesRequest)).await;
    let rmux_proto::WebShareResponse::List(list) = *response else {
        panic!("expected web-share list payload");
    };
    list.shares
}

fn create_web_share_request(session_name: SessionName, tunnel_provider: Option<&str>) -> Request {
    Request::WebShare(Box::new(WebShareRequest::Create(CreateWebShareRequest {
        tunnel_provider: tunnel_provider.map(str::to_owned),
        max_spectators: Some(1),
        ..Fixture::fixture(WebShareScope::Session(session_name))
    })))
}

fn spawn_test_connection(
    handler: &Arc<RequestHandler>,
) -> io::Result<(
    LocalStream,
    watch::Sender<()>,
    tokio::task::JoinHandle<io::Result<()>>,
)> {
    let (server, client) = LocalStream::pair()?;
    let peer = PeerIdentity {
        pid: std::process::id(),
        uid: rmux_os::identity::real_user_id(),
        user: rmux_os::identity::UserIdentity::Uid(rmux_os::identity::real_user_id()),
    };
    let (shutdown_tx, task) = spawn_connection(handler, peer, server);
    Ok((client, shutdown_tx, task))
}

async fn wait_for_pid_marker(path: &Path) -> i32 {
    wait_until(
        Duration::from_secs(2),
        Duration::from_millis(10),
        async || {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|value| value.parse::<i32>().ok())
                .ok_or(())
        },
    )
    .await
    .unwrap_or_else(|()| panic!("tunnel provider did not write its PID marker"))
}

async fn wait_for_process_exit(pid: i32) -> bool {
    let Some(pid) = rustix::process::Pid::from_raw(pid) else {
        return true;
    };
    wait_until(
        Duration::from_secs(1),
        Duration::from_millis(10),
        async || match rustix::process::test_kill_process(pid) {
            Err(_) => Ok(()),
            Ok(()) => Err(()),
        },
    )
    .await
    .is_ok()
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(prefix: &str) -> io::Result<Self> {
        let path = unique_temp_path(prefix);
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct ProcessCleanup(Option<rustix::process::Pid>);

impl ProcessCleanup {
    fn new(pid: i32) -> Self {
        Self(rustix::process::Pid::from_raw(pid))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessCleanup {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }
}
