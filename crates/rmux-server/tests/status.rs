mod common;

use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{
    create_session, read_attach_message, read_attach_until_contains, send_ok, send_request,
    session_name, shell_quote, start_server, ClientConnection, Fixture, TestHarness, PTY_TEST_LOCK,
};
use rmux_proto::KillServerRequest;
use rmux_proto::{
    AttachMessage, AttachSessionRequest, ListClientsRequest, OptionName, Request, Response,
    ScopeSelector, SetOptionRequest, SuspendClientRequest, TerminalSize,
};
use tokio::time::{timeout, Instant};

const STEP_TIMEOUT: Duration = Duration::from_secs(3);

#[tokio::test(flavor = "multi_thread")]
async fn attach_session_emits_status_row_for_single_pane_session() -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("status-attach");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let alpha = session_name("alpha");

    create_session(&socket_path, (&alpha, TerminalSize { cols: 20, rows: 4 })).await?;

    let (_response, mut attach_stream) = ClientConnection::connect(&socket_path)
        .await?
        .begin_attach(AttachSessionRequest { target: alpha })
        .await?;

    let status_text =
        read_attach_until_contains(&mut attach_stream, "[alpha]", STEP_TIMEOUT).await?;
    assert!(status_text.contains("[alpha]"));
    assert!(status_text.contains("\u{1b}[4;1H"));
    assert!(!status_text.contains('┬'));

    drop(attach_stream);
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_session_status_context_populates_session_attached() -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("status-session-attached");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let alpha = session_name("alpha");

    create_session(&socket_path, (&alpha, TerminalSize { cols: 30, rows: 4 })).await?;

    for (option, value) in [
        (OptionName::StatusLeft, "attached=#{session_attached}"),
        (OptionName::StatusRight, ""),
    ] {
        set_session_option(&socket_path, &alpha, option, value).await?;
    }

    let (_response, mut attach_stream) = ClientConnection::connect(&socket_path)
        .await?
        .begin_attach(AttachSessionRequest { target: alpha })
        .await?;

    let status_text =
        read_attach_until_contains(&mut attach_stream, "attached=1", STEP_TIMEOUT).await?;
    assert!(status_text.contains("attached=1"));

    drop(attach_stream);
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_server_reaps_a_running_status_job_descendant() -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("status-job-stop");
    let socket_path = harness.socket_path().to_path_buf();
    let probe = StatusJobShutdownProbe::new(&socket_path);
    let handle = start_server(&harness).await?;
    let alpha = session_name("alpha");

    create_session(&socket_path, (&alpha, TerminalSize { cols: 40, rows: 4 })).await?;

    for (option, value) in [
        (OptionName::StatusLeft, format!("#({})", probe.command())),
        (OptionName::StatusRight, String::new()),
    ] {
        set_session_option(&socket_path, &alpha, option, value).await?;
    }

    let (_response, attach_stream) = ClientConnection::connect(&socket_path)
        .await?
        .begin_attach(AttachSessionRequest { target: alpha })
        .await?;
    let descendant = probe.wait_for_descendant().await?;

    let killed = send_request(&socket_path, &Request::KillServer(KillServerRequest)).await?;
    assert!(matches!(killed, Response::KillServer(_)));
    handle.wait().await?;

    assert!(
        !rmux_os::process::is_live(descendant),
        "status job descendant {descendant} survived kill-server"
    );
    drop(attach_stream);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn status_interval_refreshes_time_formats_without_pane_output() -> Result<(), Box<dyn Error>>
{
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("status-interval");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let alpha = session_name("alpha");

    create_session(&socket_path, (&alpha, TerminalSize { cols: 40, rows: 4 })).await?;

    for (option, value) in [
        (OptionName::StatusInterval, "1"),
        (OptionName::StatusLeft, "[#{session_name}] "),
        (OptionName::StatusRight, "tick=%S"),
    ] {
        set_session_option(&socket_path, &alpha, option, value).await?;
    }

    let (_response, mut attach_stream) = ClientConnection::connect(&socket_path)
        .await?
        .begin_attach(AttachSessionRequest { target: alpha })
        .await?;

    let first_status =
        read_attach_until_contains(&mut attach_stream, "tick=", STEP_TIMEOUT).await?;
    let first_tick = extract_tick_second(&first_status)
        .ok_or_else(|| io::Error::other(format!("missing first tick in {first_status:?}")))?;
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut output = String::new();

    while Instant::now() < deadline {
        let message = match timeout(
            deadline.saturating_duration_since(Instant::now()),
            read_attach_message(&mut attach_stream),
        )
        .await
        {
            Ok(message) => message?,
            Err(_) => break,
        };
        let Some(message) = message else {
            break;
        };
        if let AttachMessage::Data(bytes) | AttachMessage::Render(bytes) = message {
            output.push_str(&String::from_utf8_lossy(&bytes));
            if extract_tick_second(&output).is_some_and(|tick| tick != first_tick) {
                drop(attach_stream);
                handle.shutdown().await?;
                return Ok(());
            }
        }
    }

    drop(attach_stream);
    handle.shutdown().await?;
    Err(io::Error::other(format!(
        "status interval never refreshed tick from {first_tick}; output was {output:?}"
    ))
    .into())
}

#[tokio::test(flavor = "multi_thread")]
async fn status_interval_does_not_refresh_suspended_attach_client() -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("status-interval-suspend");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let alpha = session_name("alpha");

    create_session(&socket_path, (&alpha, TerminalSize { cols: 40, rows: 4 })).await?;

    for (option, value) in [
        (OptionName::StatusInterval, "1"),
        (OptionName::StatusLeft, "[#{session_name}] "),
        (OptionName::StatusRight, "tick=%S"),
    ] {
        set_session_option(&socket_path, &alpha, option, value).await?;
    }

    let (_response, mut attach_stream) = ClientConnection::connect(&socket_path)
        .await?
        .begin_attach(AttachSessionRequest {
            target: alpha.clone(),
        })
        .await?;

    let _initial_status =
        read_attach_until_contains(&mut attach_stream, "tick=", STEP_TIMEOUT).await?;
    let attach_pid = attached_client_pid(&socket_path, &alpha).await?;
    send_ok(
        &socket_path,
        SuspendClientRequest {
            target_client: Some(attach_pid),
        },
    )
    .await?;
    read_attach_until_suspend(&mut attach_stream).await?;

    let deadline = Instant::now() + Duration::from_millis(2200);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, read_attach_message(&mut attach_stream)).await {
            Ok(Ok(Some(AttachMessage::Data(bytes) | AttachMessage::Render(bytes)))) => {
                drop(attach_stream);
                handle.shutdown().await?;
                return Err(io::Error::other(format!(
                    "suspended attach client received status refresh bytes: {:?}",
                    String::from_utf8_lossy(&bytes)
                ))
                .into());
            }
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) | Err(_) => break,
            Ok(Err(error)) => return Err(error),
        }
    }

    drop(attach_stream);
    handle.shutdown().await?;
    Ok(())
}

async fn set_session_option(
    socket_path: &Path,
    session: &rmux_proto::SessionName,
    option: OptionName,
    value: impl Into<String>,
) -> Result<(), Box<dyn Error>> {
    let scope = ScopeSelector::Session(session.clone());
    send_ok(
        socket_path,
        SetOptionRequest::fixture((scope, option, value)),
    )
    .await?;
    Ok(())
}

async fn attached_client_pid(
    socket_path: &std::path::Path,
    session_name: &rmux_proto::SessionName,
) -> Result<String, Box<dyn Error>> {
    match send_request(
        socket_path,
        &Request::ListClients(Box::new(ListClientsRequest {
            format: Some("#{client_pid}".to_owned()),
            filter: None,
            sort_order: None,
            reversed: false,
            target_session: Some(session_name.clone()),
        })),
    )
    .await?
    {
        Response::ListClients(response) => {
            assert_eq!(response.match_count, 1);
            Ok(String::from_utf8_lossy(response.output.stdout())
                .trim()
                .to_owned())
        }
        other => {
            Err(io::Error::other(format!("unexpected list-clients response: {other:?}")).into())
        }
    }
}

async fn read_attach_until_suspend(
    stream: &mut tokio::net::UnixStream,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + STEP_TIMEOUT;

    while Instant::now() < deadline {
        let message = match timeout(
            deadline.saturating_duration_since(Instant::now()),
            read_attach_message(stream),
        )
        .await
        {
            Ok(message) => message?,
            Err(_) => break,
        };
        match message {
            Some(AttachMessage::Suspend) => return Ok(()),
            Some(AttachMessage::Data(bytes)) if bytes == [5] => return Ok(()),
            Some(_) => {}
            None => break,
        }
    }

    Err(io::Error::other("attach stream never received suspend control").into())
}

/// A status job whose one descendant ignores `SIGTERM`, and the file that names it.
///
/// The command passes through the status line's strftime expansion before any shell reads it, as
/// tmux's does, so the printf conversion is written `%%s`: a bare `%s` would reach `printf` as the
/// epoch, a format with no conversions and an argument it can never consume. The job runs in the
/// embedded interpreter, where `$$` is the daemon's own pid, so only the external descendant's pid
/// is recorded.
struct StatusJobShutdownProbe {
    descendant: PathBuf,
}

impl StatusJobShutdownProbe {
    fn new(socket_path: &Path) -> Self {
        let root = socket_path.parent().expect("test socket has a parent");
        Self {
            descendant: root.join("status-descendant.pid"),
        }
    }

    fn command(&self) -> String {
        format!(
            "sh -c 'trap \"\" TERM; printf \"%%s\\n\" \"$$\" > \"$1\"; \
             while :; do sleep 30; done' sh {} & wait",
            shell_quote(&self.descendant),
        )
    }

    async fn wait_for_descendant(&self) -> Result<u32, Box<dyn Error>> {
        let deadline = Instant::now() + STEP_TIMEOUT;
        loop {
            if let Some(pid) = read_pid(&self.descendant) {
                return Ok(pid);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other("status job descendant never started").into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for StatusJobShutdownProbe {
    fn drop(&mut self) {
        use rustix::process::{kill_process, Pid, Signal};

        if let Some(descendant) = read_pid(&self.descendant)
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(Pid::from_raw)
        {
            let _ = kill_process(descendant, Signal::KILL);
        }
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn extract_tick_second(output: &str) -> Option<String> {
    let start = output.rfind("tick=")? + "tick=".len();
    let tick = output.get(start..start + 2)?;
    tick.bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| tick.to_owned())
}
