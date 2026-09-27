use std::borrow::Borrow;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use super::subscriptions::{
    handle_pane_event, refresh_subscriptions, PaneEvent, PaneSubscriptionStart,
};
use super::{
    append_control_input, arm_control_eof_transition, control_commands_require_drain,
    control_control_waits_for_attached_session, drain_control_command_after_eof,
    drain_control_queue_after_eof, ensure_control_newline, extract_complete_control_lines,
    forward_control, install_control_eof_queue_lease_pause, pause_after_control_eof_queue_lease,
    wait_for_control_eof_transition, ActiveControlCommand, ControlCommandOrigin,
    ControlCommandResult, ControlLifecycle, ControlModeUpgrade, ControlOutputQueue,
    ControlQueueEofCancellation, ControlServerEvent, ControlSessionAttachment, ControlUpgradeInput,
    EofDrainContext, CONTROL_EOF_GRACE, CONTROL_SERVER_EVENT_CAPACITY, MAX_CONTROL_LINE_BYTES,
    MAX_QUEUED_CONTROL_LINES,
};
use crate::daemon::ShutdownHandle;
use crate::handler::{
    ControlClientIdentity, ControlQueueDrainLease, ControlRegistration, ControlRegistrationError,
    RequestHandler,
};
use crate::outer_terminal::OuterTerminalContext;
use crate::server_access::{current_owner_uid, AccessMode};
use crate::test_fixtures::{unique_temp_path, wait_until, Fixture, Sizeless};
use crate::test_names::session_name;
use rmux_os::identity::UserIdentity;
use rmux_proto::{
    ControlMode, KillSessionRequest, Request, Response, RmuxError, SessionId, ShowBufferRequest,
    WaitForMode, WaitForRequest,
};

const CONTROL_TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn only_blocking_control_waits_are_cancel_safe_during_shutdown() {
    let handler = RequestHandler::new();
    for line in [
        "wait-for channel",
        "wait-for -- -channel",
        "wait-for -L lock",
    ] {
        let commands = handler
            .parse_control_commands(line)
            .await
            .expect("cancel-safe wait parses");
        assert!(
            !control_commands_require_drain(&commands),
            "{line:?} must not hold the mutation drain barrier"
        );
    }
    for line in [
        "wait-for -S channel",
        "wait-for -U lock",
        "set-buffer changed",
        "RMUX_TEST=value",
    ] {
        let commands = handler
            .parse_control_commands(line)
            .await
            .expect("mutating command parses");
        assert!(
            control_commands_require_drain(&commands),
            "{line:?} must hold the mutation drain barrier"
        );
    }
}

#[tokio::test]
async fn eof_queue_lease_pauses_are_scoped_by_handler_and_cleaned_on_drop() {
    let identity = ControlClientIdentity::new(81_001, 1);
    let first_handler = Arc::new(RequestHandler::new());
    let second_handler = Arc::new(RequestHandler::new());
    let abandoned_pause = install_control_eof_queue_lease_pause(&first_handler, identity);
    drop(abandoned_pause);
    let second_pause = install_control_eof_queue_lease_pause(&second_handler, identity);
    let first_pause = install_control_eof_queue_lease_pause(&first_handler, identity);

    let first_handler_for_task = Arc::clone(&first_handler);
    let first_task = tokio::spawn(async move {
        pause_after_control_eof_queue_lease(&first_handler_for_task, identity).await;
    });
    tokio::time::timeout(CONTROL_TEST_TIMEOUT, first_pause.reached.notified())
        .await
        .expect("first handler reaches its EOF lease pause");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), second_pause.reached.notified())
            .await
            .is_err(),
        "the first handler must not consume the second handler's pause"
    );
    first_pause.release.notify_one();
    first_task.await.expect("first EOF lease pause joins");

    let second_handler_for_task = Arc::clone(&second_handler);
    let second_task = tokio::spawn(async move {
        pause_after_control_eof_queue_lease(&second_handler_for_task, identity).await;
    });
    tokio::time::timeout(CONTROL_TEST_TIMEOUT, second_pause.reached.notified())
        .await
        .expect("second handler reaches its EOF lease pause");

    second_pause.release.notify_one();
    second_task.await.expect("second EOF lease pause joins");
}

#[test]
fn only_control_control_eof_waits_for_an_attached_session() {
    let unattached = ControlSessionAttachment::new(None);
    let attached = ControlSessionAttachment::new(Some(session_name("control-eof-session")));

    assert!(!control_control_waits_for_attached_session(
        ControlMode::Plain,
        &attached,
    ));
    assert!(!control_control_waits_for_attached_session(
        ControlMode::ControlControl,
        &unattached,
    ));
    assert!(control_control_waits_for_attached_session(
        ControlMode::ControlControl,
        &attached,
    ));
}

#[tokio::test]
async fn persistent_eof_deadline_is_global_and_not_rearmed() {
    let mut transition = None;
    arm_control_eof_transition(&mut transition);
    let initial_deadline = transition
        .as_ref()
        .expect("EOF deadline is armed")
        .deadline();

    arm_control_eof_transition(&mut transition);
    assert_eq!(
        transition
            .as_ref()
            .expect("EOF deadline stays armed")
            .deadline(),
        initial_deadline,
        "starting another post-EOF frame must not extend the global budget"
    );

    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            wait_for_control_eof_transition(&mut transition),
        )
        .await
        .is_err(),
        "the deadline must leave a bounded grace for fast command output"
    );
    assert!(
        tokio::time::timeout(
            CONTROL_EOF_GRACE + Duration::from_millis(100),
            wait_for_control_eof_transition(&mut transition),
        )
        .await
        .is_ok(),
        "the persistent EOF deadline must still expire within its global budget"
    );
}

/// A plain control-mode upgrade announcing `initial_command_count` initial commands.
fn plain_upgrade(initial_command_count: u32) -> ControlModeUpgrade {
    ControlModeUpgrade {
        initial_command_count,
        mode: ControlMode::Plain,
        terminal_context: OuterTerminalContext::default(),
    }
}

/// Registers writable control client `requester_pid` with `upgrade`, sending its server events
/// to `event_tx`, and answers with its identity and the flag raised once it starts closing.
async fn register_control(
    handler: &RequestHandler,
    requester_pid: u32,
    upgrade: ControlModeUpgrade,
    event_tx: mpsc::Sender<ControlServerEvent>,
) -> (ControlClientIdentity, Arc<AtomicBool>) {
    let closing = Arc::new(AtomicBool::new(false));
    let control_id = handler
        .register_control_with_closing(requester_pid, upgrade, event_tx, Arc::clone(&closing))
        .await;
    (
        ControlClientIdentity::new(requester_pid, control_id),
        closing,
    )
}

/// The client end of a control connection whose server end a spawned task forwards, with the
/// channel ends that keep the forward loop's inputs open for as long as the test holds them.
struct ControlClient {
    stream: UnixStream,
    task: JoinHandle<std::io::Result<()>>,
    shutdown_tx: watch::Sender<()>,
    shutdown_handle: ShutdownHandle,
    shutdown_request_rx: oneshot::Receiver<()>,
    /// The forward loop's server-event sender, when the test rather than a registration owns it.
    server_event_tx: Option<mpsc::Sender<ControlServerEvent>>,
}

/// The server end of a [`ControlClient`] connection and its forward loop's shutdown inputs.
struct ServerEnd {
    stream: UnixStream,
    shutdown: watch::Receiver<()>,
    shutdown_handle: ShutdownHandle,
}

impl ServerEnd {
    /// Forwards `input` for `identity`, registered with `closing` and the sender of
    /// `server_events`, until the connection ends; with `finish`, then unregisters `identity`.
    async fn forward(
        self,
        handler: Arc<RequestHandler>,
        identity: ControlClientIdentity,
        closing: Arc<AtomicBool>,
        server_events: mpsc::Receiver<ControlServerEvent>,
        input: ControlUpgradeInput,
        finish: bool,
    ) -> std::io::Result<()> {
        let result = forward_control(
            self.stream,
            Arc::clone(&handler),
            identity,
            input,
            self.shutdown,
            server_events,
            ControlLifecycle {
                closing,
                shutdown_handle: self.shutdown_handle,
            },
        )
        .await;
        if finish {
            handler
                .finish_control(identity.requester_pid(), identity.control_id())
                .await;
        }
        result
    }
}

impl ControlClient {
    /// Forwards `input`, holding `initial_command_count` commands, for control client
    /// `requester_pid`, which the task registers without initial commands and unregisters
    /// afterwards; the forward loop reads server events from `server_event_tx` rather than
    /// from that registration.
    fn open(
        handler: &Arc<RequestHandler>,
        requester_pid: u32,
        input: impl Into<Vec<u8>>,
        initial_command_count: usize,
    ) -> Self {
        let handler = Arc::clone(handler);
        let input = ControlUpgradeInput::new(input.into(), initial_command_count);
        let (server_event_tx, server_events) = mpsc::channel(CONTROL_SERVER_EVENT_CAPACITY);
        let mut client = Self::spawn(move |end| async move {
            let (registration_tx, _registration_rx) = mpsc::channel(CONTROL_SERVER_EVENT_CAPACITY);
            let (identity, closing) =
                register_control(&handler, requester_pid, plain_upgrade(0), registration_tx).await;
            end.forward(handler, identity, closing, server_events, input, true)
                .await
        });
        client.server_event_tx = Some(server_event_tx);
        client
    }

    /// Forwards `input` for `identity`, registered with `closing` and the sender of
    /// `server_events`; with `finish`, the task unregisters `identity` afterwards.
    fn forward(
        handler: &Arc<RequestHandler>,
        identity: ControlClientIdentity,
        closing: Arc<AtomicBool>,
        server_events: mpsc::Receiver<ControlServerEvent>,
        input: ControlUpgradeInput,
        finish: bool,
    ) -> Self {
        let handler = Arc::clone(handler);
        Self::spawn(move |end| {
            end.forward(handler, identity, closing, server_events, input, finish)
        })
    }

    /// Connects a fresh socket pair and serves its server end with `serve` on a spawned task.
    fn spawn<F>(serve: impl FnOnce(ServerEnd) -> F) -> Self
    where
        F: Future<Output = std::io::Result<()>> + Send + 'static,
    {
        let (server_stream, stream) = UnixStream::pair().expect("unix stream pair");
        let (shutdown_tx, shutdown) = watch::channel(());
        let (shutdown_handle, shutdown_request_rx) = ShutdownHandle::new();
        let task = tokio::spawn(serve(ServerEnd {
            stream: server_stream,
            shutdown,
            shutdown_handle: shutdown_handle.clone(),
        }));
        Self {
            stream,
            task,
            shutdown_tx,
            shutdown_handle,
            shutdown_request_rx,
            server_event_tx: None,
        }
    }

    /// Closes the client's write half, as a client does once its input ends.
    async fn close_input(&mut self) {
        self.stream
            .shutdown()
            .await
            .expect("client write half closes");
    }

    /// Reads the first chunk of output, which must open a command guard.
    async fn read_begin_prefix(&mut self) -> String {
        let mut begin_prefix = vec![0_u8; 256];
        let bytes_read = self
            .stream
            .read(&mut begin_prefix)
            .await
            .expect("control output begins");
        let begin_prefix = String::from_utf8(begin_prefix[..bytes_read].to_vec())
            .expect("control output is utf-8");
        assert!(
            begin_prefix.contains("%begin "),
            "expected begin guard in initial output: {begin_prefix:?}"
        );
        begin_prefix
    }

    /// Reads output into `output` until it contains `needle`, answering false if the output
    /// ends first; panics with `expectation` if neither happens within [`CONTROL_TEST_TIMEOUT`].
    async fn read_until(&mut self, output: &mut Vec<u8>, needle: &[u8], expectation: &str) -> bool {
        tokio::time::timeout(CONTROL_TEST_TIMEOUT, async {
            let mut buffer = [0_u8; 1024];
            loop {
                let bytes_read = self
                    .stream
                    .read(&mut buffer)
                    .await
                    .expect("control output reads");
                if bytes_read == 0 {
                    return false;
                }
                output.extend_from_slice(&buffer[..bytes_read]);
                if output.windows(needle.len()).any(|window| window == needle) {
                    return true;
                }
            }
        })
        .await
        .expect(expectation)
    }

    /// Reads the rest of the output, which must end within [`CONTROL_TEST_TIMEOUT`].
    async fn read_to_eof(&mut self) -> Vec<u8> {
        self.read_to_eof_within(CONTROL_TEST_TIMEOUT, "control output drains before timeout")
            .await
    }

    /// Reads the rest of the output, which must end within 500 ms for the reason
    /// `expectation` gives.
    async fn read_promptly(&mut self, expectation: &str) -> Vec<u8> {
        self.read_to_eof_within(Duration::from_millis(500), expectation)
            .await
    }

    async fn read_to_eof_within(&mut self, timeout: Duration, expectation: &str) -> Vec<u8> {
        let mut output = Vec::new();
        tokio::time::timeout(timeout, self.stream.read_to_end(&mut output))
            .await
            .expect(expectation)
            .expect("control output drains");
        output
    }

    /// Joins the forwarding task, which must succeed.
    async fn join(self) {
        self.task
            .await
            .expect("control task joins")
            .expect("control forwarding succeeds");
    }

    /// Reads the output to its end and joins the forwarding task; answers with the transcript.
    async fn transcript(mut self) -> String {
        let output = self.read_to_eof().await;
        self.join().await;
        String::from_utf8(output).expect("control transcript is utf-8")
    }
}

/// `show-buffer -b name`'s response.
async fn show_buffer(handler: &RequestHandler, name: &str) -> Response {
    handler
        .handle(Request::ShowBuffer(ShowBufferRequest {
            name: Some(name.to_owned()),
        }))
        .await
}

/// Asserts that paste buffer `name` holds `expected`; `expectation` says why it exists.
async fn assert_buffer(handler: &RequestHandler, name: &str, expected: &[u8], expectation: &str) {
    let response = show_buffer(handler, name).await;
    assert_eq!(
        response.command_output().expect(expectation).stdout(),
        expected
    );
}

/// Asserts that no paste buffer `name` exists; `context` says why it must not.
async fn assert_buffer_missing(handler: &RequestHandler, name: &str, context: &str) {
    let response = show_buffer(handler, name).await;
    assert!(
        matches!(response, Response::Error(_)),
        "{context}: {response:?}"
    );
}

#[tokio::test]
async fn shutdown_quiesce_finishes_the_active_control_mutation_and_rejects_later_frames() {
    const REQUESTER_PID: u32 = 42_422;

    let handler = Arc::new(RequestHandler::new());
    let (server_event_tx, server_events) = mpsc::channel(CONTROL_SERVER_EVENT_CAPACITY);
    let (identity, closing) =
        register_control(&handler, REQUESTER_PID, plain_upgrade(1), server_event_tx).await;
    let _requester_access_guard =
        handler.begin_test_detached_requester_access(REQUESTER_PID, AccessMode::ReadWrite);
    let marker = unique_temp_path("control-quiesce");
    let input = format!(
        "run-shell 'printf started > {}; sleep 0.4' ; set-buffer -b shutdown-control-active committed\n\
         set-buffer -b shutdown-control-later must-not-run\n",
        marker.display()
    );
    let control = ControlClient::forward(
        &handler,
        identity,
        closing,
        server_events,
        ControlUpgradeInput::new(input.into_bytes(), 1),
        false,
    );

    wait_until(
        CONTROL_TEST_TIMEOUT,
        Duration::from_millis(10),
        async || marker.exists().then_some(()).ok_or(()),
    )
    .await
    .expect("active control frame reaches its foreground shell");
    assert!(!handler.normal_drain_requests_quiesced());

    handler.close_normal_request_admission();
    control.shutdown_tx.send_replace(());
    let rendered = control.transcript().await;
    assert!(handler.normal_drain_requests_quiesced());

    assert_buffer(
        &handler,
        "shutdown-control-active",
        b"committed",
        "the admitted active frame commits",
    )
    .await;
    assert_buffer_missing(
        &handler,
        "shutdown-control-later",
        "a later frame must not be admitted during quiesce",
    )
    .await;
    assert!(rendered.contains("%end "), "{rendered:?}");
    assert!(
        rendered.ends_with("%exit server shutting down\n"),
        "{rendered:?}"
    );

    handler
        .finish_control(REQUESTER_PID, identity.control_id())
        .await;
    let _ = std::fs::remove_file(marker);
}

#[tokio::test]
async fn eof_queue_rechecks_normal_request_admission_before_spawning_each_frame() {
    let handler = Arc::new(RequestHandler::new());
    let requester_pid = 4252;
    let (control_id, mut event_rx) = handler.register_control_for_test(requester_pid, None).await;
    let identity = ControlClientIdentity::new(requester_pid, control_id);
    assert_eq!(
        handler.begin_control_queue_drain(identity).await,
        ControlQueueDrainLease::Acquired
    );

    handler.close_normal_request_admission();
    drain_line_after_eof(
        &handler,
        identity,
        &mut event_rx,
        None,
        "set-buffer -b eof-admission-after-close must-not-run",
    )
    .await
    .expect("closed EOF queue stops without spawning its next frame");

    assert_buffer_missing(
        &handler,
        "eof-admission-after-close",
        "a frame rejected by normal admission must not mutate",
    )
    .await;
    handler.finish_control(requester_pid, control_id).await;
}

#[tokio::test]
async fn live_kill_server_stops_buffered_frames_before_shutdown_watch_propagates() {
    let handler = Arc::new(RequestHandler::new());
    let mut control = ControlClient::open(
        &handler,
        4248,
        b"kill-server\nset-buffer -b live-after-kill must-not-run\n",
        2,
    );

    tokio::time::timeout(CONTROL_TEST_TIMEOUT, &mut control.shutdown_request_rx)
        .await
        .expect("kill-server requests shutdown before timeout")
        .expect("shutdown request channel stays open");
    let rendered = control.read_to_eof().await;
    tokio::time::timeout(CONTROL_TEST_TIMEOUT, control.task)
        .await
        .expect("forward control exits before timeout")
        .expect("forward control task joins")
        .expect("forward control succeeds");

    assert_buffer_missing(
        &handler,
        "live-after-kill",
        "a live frame buffered behind kill-server must never be admitted",
    )
    .await;
    let rendered = String::from_utf8(rendered).expect("control transcript is utf-8");
    assert!(
        rendered.ends_with("%exit server shutting down\n"),
        "{rendered:?}"
    );
}

#[tokio::test]
async fn shutdown_cancels_only_the_explicit_control_wait() {
    const REQUESTER_PID: u32 = 42_421;
    const WAIT_CHANNEL: &str = "control-shutdown-active";

    let handler = Arc::new(RequestHandler::new());
    let (server_event_tx, server_events) = mpsc::channel(CONTROL_SERVER_EVENT_CAPACITY);
    let (identity, closing) =
        register_control(&handler, REQUESTER_PID, plain_upgrade(1), server_event_tx).await;
    let _requester_access_guard =
        handler.begin_test_detached_requester_access(REQUESTER_PID, AccessMode::ReadWrite);
    let input = format!(
        "wait-for {WAIT_CHANNEL}\n\
         set-buffer -b shutdown-active-later must-not-run\n"
    );
    let control = ControlClient::forward(
        &handler,
        identity,
        closing,
        server_events,
        ControlUpgradeInput::new(input.into_bytes(), 1),
        false,
    );

    wait_for_waiter(&handler, WAIT_CHANNEL).await;
    assert!(
        handler.normal_drain_requests_quiesced(),
        "a pure wait-for frame must not hold the mutation barrier"
    );
    assert!(!handler.normal_requests_quiesced());
    handler.close_normal_request_admission();
    control.shutdown_tx.send_replace(());
    wait_until(CONTROL_TEST_TIMEOUT, Duration::from_millis(1), async || {
        handler.normal_requests_quiesced().then_some(()).ok_or(())
    })
    .await
    .expect("the selected wait cancels during shutdown");

    let rendered = control.transcript().await;
    assert!(rendered.contains("%end "), "{rendered:?}");
    assert!(
        rendered.ends_with("%exit server shutting down\n"),
        "{rendered:?}"
    );

    assert_buffer_missing(
        &handler,
        "shutdown-active-later",
        "shutdown must suppress the later frame",
    )
    .await;
    handler
        .finish_control(REQUESTER_PID, identity.control_id())
        .await;
}

#[test]
fn extracts_complete_control_lines_from_buffer() {
    let mut buffer = b"one\ntwo\r\nthree".to_vec();
    let lines = extract_complete_control_lines(&mut buffer);

    assert_eq!(lines, vec!["one".to_owned(), "two".to_owned()]);
    assert_eq!(buffer, b"three");
}

#[test]
fn extracts_empty_line_for_exit_trigger() {
    let mut buffer = b"\n".to_vec();
    let lines = extract_complete_control_lines(&mut buffer);

    assert_eq!(lines, vec!["".to_owned()]);
    assert!(buffer.is_empty());
}

#[test]
fn empty_buffer_produces_no_lines() {
    let mut buffer = Vec::new();
    let lines = extract_complete_control_lines(&mut buffer);

    assert!(lines.is_empty());
    assert!(buffer.is_empty());
}

#[test]
fn multiple_empty_lines_are_preserved() {
    let mut buffer = b"\n\ncommand\n".to_vec();
    let lines = extract_complete_control_lines(&mut buffer);

    assert_eq!(
        lines,
        vec!["".to_owned(), "".to_owned(), "command".to_owned()]
    );
    assert!(buffer.is_empty());
}

#[test]
fn control_input_rejects_unterminated_oversize_lines() {
    let mut input_buffer = Vec::new();
    let mut queued_lines = std::collections::VecDeque::new();
    let mut queued_bytes = 0;
    let oversized = vec![b'x'; MAX_CONTROL_LINE_BYTES + 1];

    let error = append_control_input(
        &mut input_buffer,
        &mut queued_lines,
        &mut queued_bytes,
        &oversized,
    )
    .expect_err("unterminated oversized input must be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn control_input_rejects_excessive_queued_lines() {
    let mut input_buffer = Vec::new();
    let mut queued_lines = std::collections::VecDeque::new();
    let mut queued_bytes = 0;
    let input = "x\n".repeat(MAX_QUEUED_CONTROL_LINES + 1);

    let error = append_control_input(
        &mut input_buffer,
        &mut queued_lines,
        &mut queued_bytes,
        input.as_bytes(),
    )
    .expect_err("an excessive command backlog must be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn stdout_lines_are_newline_terminated() {
    assert_eq!(ensure_control_newline(b"hello".to_vec()), b"hello\n");
    assert_eq!(ensure_control_newline(b"hello\n".to_vec()), b"hello\n");
}

#[test]
fn output_queue_tracks_buffered_bytes() {
    let mut queue = ControlOutputQueue::default();
    assert_eq!(queue.buffered_bytes, 0);

    queue.enqueue_line(b"hello\n".to_vec(), true);
    assert_eq!(queue.buffered_bytes, 6);

    queue.enqueue_stdout(b"world".to_vec());
    assert_eq!(queue.buffered_bytes, 12); // 6 + "world\n" = 6
}

#[test]
fn enqueue_stdout_skips_empty_bytes() {
    let mut queue = ControlOutputQueue::default();
    queue.enqueue_stdout(Vec::new());
    assert_eq!(queue.blocks.len(), 0);
    assert_eq!(queue.buffered_bytes, 0);
}

#[tokio::test]
async fn pane_output_lag_terminates_control_mode_explicitly() {
    let mut queue = ControlOutputQueue::default();
    let mut paused_panes = std::collections::HashSet::new();
    let lagged = handle_pane_event(
        PaneEvent::Lagged {
            pane_id: 7,
            expected_sequence: 2,
            resume_sequence: 9,
            missed_events: 7,
        },
        &mut queue,
        &mut paused_panes,
        Default::default(),
    )
    .expect("lag handling succeeds");
    assert!(
        lagged,
        "a pane-output gap must be terminal for control mode"
    );

    let (mut writer, mut reader) = tokio::io::duplex(256);
    super::flush_output_queue(
        &mut queue,
        &mut writer,
        Default::default(),
        &mut paused_panes,
    )
    .await
    .expect("terminal lag frame flushes");
    writer.shutdown().await.expect("writer closes");
    let mut rendered = Vec::new();
    reader
        .read_to_end(&mut rendered)
        .await
        .expect("lag transcript reads");
    assert_eq!(rendered, b"%exit too far behind\n");
}

#[tokio::test]
async fn pane_subscriptions_reject_a_recreated_same_name_session() {
    let handler = RequestHandler::new();
    let session_name = handler
        .create_session(Sizeless("control-subscription-identity"))
        .await;
    let replacement_output = handler
        .control_session_panes(&session_name)
        .await
        .expect("replacement session pane output exists")
        .into_iter()
        .next()
        .expect("replacement session has a pane")
        .1;

    let requester_pid = 42_421;
    let (control_id, _event_rx) = handler.register_control_for_test(requester_pid, None).await;
    let control_identity = ControlClientIdentity::new(requester_pid, control_id);
    handler
        .set_control_subscription_identity_for_test(
            control_identity,
            session_name.clone(),
            SessionId::new(u32::MAX),
        )
        .await;

    let (pane_event_tx, mut pane_event_rx) = mpsc::channel(4);
    let mut subscriptions = std::collections::HashMap::new();
    refresh_subscriptions(
        &handler,
        control_identity,
        Some(&session_name),
        &mut subscriptions,
        pane_event_tx,
        PaneSubscriptionStart::Now,
    )
    .await;

    assert!(
        subscriptions.is_empty(),
        "a stale SessionId must not subscribe to a replacement sharing its name"
    );
    replacement_output.send(b"WRONG_SESSION_OUTPUT".to_vec());
    let received = tokio::time::timeout(Duration::from_millis(50), pane_event_rx.recv()).await;
    assert!(
        !matches!(received, Ok(Some(_))),
        "replacement output must not reach the stale control client"
    );
}

#[tokio::test]
async fn notifications_wait_until_after_the_active_command_block() {
    let handler = Arc::new(RequestHandler::new());
    let _requester_access_guard =
        handler.begin_test_detached_requester_access(4242, AccessMode::ReadWrite);
    let mut control = ControlClient::open(&handler, 4242, b"wait-for control-test-block\n\n", 1);

    let begin_prefix = control.read_begin_prefix().await;
    wait_for_waiter(&handler, "control-test-block").await;
    control
        .server_event_tx
        .take()
        .expect("the test owns the forward loop's server events")
        .send(ControlServerEvent::Notification(
            "%message command-notification-finished".to_owned(),
        ))
        .await
        .expect("notification send succeeds");
    handler
        .handle_ok(WaitForRequest::fixture((
            "control-test-block",
            WaitForMode::Signal,
        )))
        .await;

    let rendered = format!("{begin_prefix}{}", control.transcript().await);
    let end_index = rendered.find("%end ").expect("end guard present");
    let notification_index = rendered
        .find("%message command-notification-finished")
        .expect("notification present");

    assert!(
        end_index < notification_index,
        "notifications must flush after the command block closes: {rendered:?}"
    );
}

/// Runs `commands`, one per line, as the initial control batch of client `requester_pid`, and
/// answers with the transcript and its strict parse.
async fn run_initial_control_commands(
    handler: &Arc<RequestHandler>,
    requester_pid: u32,
    commands: &[impl Borrow<str>],
) -> (String, TestControlTranscript) {
    let initial_command_count = commands.len();
    let upgrade =
        plain_upgrade(u32::try_from(initial_command_count).expect("test command count fits u32"));
    let (server_event_tx, server_events) = mpsc::channel(CONTROL_SERVER_EVENT_CAPACITY);
    let (identity, closing) =
        register_control(handler, requester_pid, upgrade, server_event_tx).await;
    let input = format!("{}\n", commands.join("\n")).into_bytes();
    let rendered = ControlClient::forward(
        handler,
        identity,
        closing,
        server_events,
        ControlUpgradeInput::new(input, initial_command_count),
        true,
    )
    .transcript()
    .await;
    let transcript = parse_strict_control_transcript(&rendered);
    (rendered, transcript)
}

fn control_message_test_config(label: &str, contents: &str) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let root = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .expect("test current directory")
                .join("target")
        })
        .join("rmux-control-message-tests");
    std::fs::create_dir_all(&root).expect("control message test directory");
    let path = root.join(format!("{label}-{}-{nonce}.conf", std::process::id()));
    std::fs::write(&path, contents).expect("control message test config");
    path
}

#[tokio::test]
async fn admitted_display_messages_are_owned_by_their_exact_control_guards() {
    let handler = Arc::new(RequestHandler::new());
    let session_name = handler
        .create_session(Sizeless("control-message-guard-pipeline"))
        .await;

    let commands = [
        "display-message -- SYNC-FIRST-A",
        "display-message -p -- PRINT-FIRST",
        "list-sessions -F 'LIST-FIRST:#{session_name}'",
        "display-message -- SYNC-FIRST-B",
        "definitely-not-a-command",
        "display-message -- SYNC-REPEAT-A",
        "display-message -p -- PRINT-REPEAT",
        "list-sessions -F 'LIST-REPEAT:#{session_name}'",
        "display-message -- SYNC-REPEAT-B",
    ];
    let (rendered, transcript) = run_initial_control_commands(&handler, 42_430, &commands).await;

    assert_eq!(transcript.frames.len(), commands.len(), "{rendered:?}");
    assert_message_owned_once(&transcript, 0, "SYNC-FIRST-A");
    assert_message_owned_once(&transcript, 3, "SYNC-FIRST-B");
    assert_message_owned_once(&transcript, 5, "SYNC-REPEAT-A");
    assert_message_owned_once(&transcript, 8, "SYNC-REPEAT-B");
    for (print_frame, list_frame, pass) in [(1, 2, "FIRST"), (6, 7, "REPEAT")] {
        assert_eq!(
            transcript.frames[print_frame].payload,
            [format!("PRINT-{pass}")],
            "{rendered:?}"
        );
        assert!(
            transcript.frames[list_frame]
                .payload
                .iter()
                .any(|line| line == &format!("LIST-{pass}:{session_name}")),
            "{rendered:?}"
        );
    }
    assert_frame_terminal(&rendered, &transcript, 4, TestGuardTerminal::Error);
}

#[tokio::test]
async fn queued_display_messages_stay_once_inside_the_admitted_control_guard() {
    let handler = Arc::new(RequestHandler::new());
    let commands = ["display-message -- QUEUE-SYNC-A ; \
         display-message -p -- QUEUE-PRINT ; \
         display-message -- QUEUE-SYNC-B"];
    let (rendered, transcript) = run_initial_control_commands(&handler, 42_431, &commands).await;

    assert_eq!(transcript.frames.len(), 1, "{rendered:?}");
    assert_message_owned_once(&transcript, 0, "QUEUE-SYNC-A");
    assert_message_owned_once(&transcript, 0, "QUEUE-SYNC-B");
    assert_eq!(
        transcript.frames[0].payload,
        ["QUEUE-PRINT"],
        "{rendered:?}"
    );
}

#[tokio::test]
async fn sourced_and_conditional_display_messages_get_distinct_child_guards() {
    // Fresh tmux 3.7b oracle:
    // source-file: parent end, then one flags-preserving guard per sourced command.
    // if-shell -F: parent end, then one guard for the selected branch.
    let source = control_message_test_config(
        "source-child-ownership",
        "display-message -- SOURCE-CHILD-A\n\
         display-message -- SOURCE-CHILD-B\n",
    );
    let handler = Arc::new(RequestHandler::new());
    let commands = [
        format!("source-file {}", source.display()),
        "if-shell -F 1 'display-message -- IF-TRUE-CHILD' \
         'display-message -- IF-TRUE-UNSELECTED'"
            .to_owned(),
        "if -F 0 'display-message -- IF-FALSE-UNSELECTED' \
         'display-message -- IF-FALSE-CHILD'"
            .to_owned(),
        "display-message -d 0 -- DIRECT-AFTER-CHILDREN".to_owned(),
    ];
    let (rendered, transcript) = run_initial_control_commands(&handler, 42_433, &commands).await;

    assert_eq!(transcript.frames.len(), 8, "{rendered:?}");
    assert_frame_quiet(&rendered, &transcript, 0);
    assert_message_owned_once(&transcript, 1, "SOURCE-CHILD-A");
    assert_message_owned_once(&transcript, 2, "SOURCE-CHILD-B");
    assert_frame_quiet(&rendered, &transcript, 3);
    assert_message_owned_once(&transcript, 4, "IF-TRUE-CHILD");
    assert_frame_quiet(&rendered, &transcript, 5);
    assert_message_owned_once(&transcript, 6, "IF-FALSE-CHILD");
    assert_message_owned_once(&transcript, 7, "DIRECT-AFTER-CHILDREN");
    assert!(
        transcript.frames.iter().all(|frame| frame.guard.flags == 0),
        "initial parent and synchronous children retain flag 0: {rendered:?}"
    );
    assert!(
        !rendered.contains("IF-TRUE-UNSELECTED") && !rendered.contains("IF-FALSE-UNSELECTED"),
        "{rendered:?}"
    );

    std::fs::remove_file(source).expect("remove source child config");
}

#[tokio::test]
async fn sourced_command_alias_keeps_the_sourced_child_owner() {
    let source = control_message_test_config(
        "source-child-command-alias",
        "announce SOURCE-ALIAS-CHILD\n",
    );
    let handler = Arc::new(RequestHandler::new());
    let commands = [
        "set-option -s 'command-alias[100]' 'announce=display-message --'".to_owned(),
        format!("source-file {}", source.display()),
    ];
    let (rendered, transcript) = run_initial_control_commands(&handler, 42_436, &commands).await;

    assert_eq!(transcript.frames.len(), 3, "{rendered:?}");
    assert_frame_quiet(&rendered, &transcript, 1);
    assert_message_owned_once(&transcript, 2, "SOURCE-ALIAS-CHILD");

    std::fs::remove_file(source).expect("remove source command-alias config");
}

#[tokio::test]
async fn command_alias_to_if_shell_keeps_the_selected_child_owner() {
    let handler = Arc::new(RequestHandler::new());
    let commands = [
        "set-option -s 'command-alias[101]' 'choose=if-shell -F 1'".to_owned(),
        "choose 'display-message -- IF-COMMAND-ALIAS-CHILD'".to_owned(),
    ];
    let (rendered, transcript) = run_initial_control_commands(&handler, 42_437, &commands).await;

    assert_eq!(transcript.frames.len(), 3, "{rendered:?}");
    assert_frame_quiet(&rendered, &transcript, 1);
    assert_message_owned_once(&transcript, 2, "IF-COMMAND-ALIAS-CHILD");
}

#[tokio::test]
async fn sourced_runtime_error_stays_in_its_child_guard_after_prior_message() {
    // The invalid target is a runtime command failure, not a parse error: tmux
    // first closes source-file, then ends the display child and errors the
    // following child.
    let source = control_message_test_config(
        "source-child-error",
        "display-message -- SOURCE-BEFORE-ERROR\n\
         kill-pane -t missing-source-session:0.0\n",
    );
    let handler = Arc::new(RequestHandler::new());
    let commands = [format!("source-file {}", source.display())];
    let (rendered, transcript) = run_initial_control_commands(&handler, 42_434, &commands).await;

    assert_eq!(transcript.frames.len(), 3, "{rendered:?}");
    assert_frame_terminal(&rendered, &transcript, 0, TestGuardTerminal::End);
    assert_frame_quiet(&rendered, &transcript, 0);
    assert_message_owned_once(&transcript, 1, "SOURCE-BEFORE-ERROR");
    assert_frame_error(&rendered, &transcript, 2, "missing-source-session");

    std::fs::remove_file(source).expect("remove source error config");
}

#[tokio::test]
async fn conditional_runtime_error_stays_in_its_child_guard_after_prior_message() {
    let handler = Arc::new(RequestHandler::new());
    let commands = ["if-shell -F 1 'display-message -- IF-BEFORE-ERROR ; \
         kill-pane -t missing-if-session:0.0'"];
    let (rendered, transcript) = run_initial_control_commands(&handler, 42_438, &commands).await;

    assert_eq!(transcript.frames.len(), 3, "{rendered:?}");
    assert_frame_terminal(&rendered, &transcript, 0, TestGuardTerminal::End);
    assert_message_owned_once(&transcript, 1, "IF-BEFORE-ERROR");
    assert_frame_error(&rendered, &transcript, 2, "missing-if-session");
}

#[tokio::test]
async fn inserted_child_frames_exceed_channel_capacity_without_fifo_loss() {
    const CHILD_COUNT: usize = CONTROL_SERVER_EVENT_CAPACITY / 2;

    let contents = (0..CHILD_COUNT)
        .map(|index| format!("display-message -- SOURCE-FIFO-{index:03}\n"))
        .collect::<String>();
    let source = control_message_test_config("source-child-fifo", &contents);
    let handler = Arc::new(RequestHandler::new());
    let commands = [format!("source-file {}", source.display())];
    let (rendered, transcript) = run_initial_control_commands(&handler, 42_439, &commands).await;

    assert_eq!(transcript.frames.len(), CHILD_COUNT + 1, "{rendered:?}");
    assert_frame_quiet(&rendered, &transcript, 0);
    for (index, frame) in transcript.frames.iter().skip(1).enumerate() {
        assert_eq!(
            frame.notifications,
            [format!("%message SOURCE-FIFO-{index:03}")],
            "child {index} lost, duplicated, or reordered: {rendered:?}"
        );
    }
    assert_no_asynchronous_messages(&rendered, &transcript);

    std::fs::remove_file(source).expect("remove source FIFO config");
}

#[tokio::test]
async fn rejected_synchronous_insertion_errors_the_parent_without_an_orphan_guard() {
    let inserted =
        "start-server ;".repeat(crate::handler::TEST_CONTROL_QUEUE_INSERTED_COMMAND_LIMIT + 1);
    let handler = Arc::new(RequestHandler::new());
    let commands = [format!("if-shell -F 1 '{inserted}'")];
    let (rendered, transcript) = run_initial_control_commands(&handler, 42_440, &commands).await;

    assert_eq!(transcript.frames.len(), 1, "{rendered:?}");
    assert_frame_error(&rendered, &transcript, 0, "inserted too many commands");
    assert_no_asynchronous_messages(&rendered, &transcript);
}

#[tokio::test]
async fn direct_display_forms_remain_in_their_admitted_guards() {
    let handler = Arc::new(RequestHandler::new());
    let commands = [
        "display -- DIRECT-ALIAS",
        "display-mes -- DIRECT-PREFIX",
        "display-message -d 0 -- DIRECT-EXT-DURATION",
        "display-message -F 'DIRECT-EXT-FORMAT'",
        "display-message -- DIRECT-REPEAT",
        "display-message -- DIRECT-REPEAT",
    ];
    let (rendered, transcript) = run_initial_control_commands(&handler, 42_435, &commands).await;

    assert_eq!(transcript.frames.len(), commands.len(), "{rendered:?}");
    for (index, token) in [
        "DIRECT-ALIAS",
        "DIRECT-PREFIX",
        "DIRECT-EXT-DURATION",
        "DIRECT-EXT-FORMAT",
    ]
    .into_iter()
    .enumerate()
    {
        assert_message_owned_once(&transcript, index, token);
    }
    for index in [4, 5] {
        assert_eq!(
            transcript.frames[index].notifications,
            ["%message DIRECT-REPEAT"],
            "{rendered:?}"
        );
    }
    assert_eq!(
        transcript
            .frames
            .iter()
            .flat_map(|frame| frame.notifications.iter())
            .filter(|line| line.as_str() == "%message DIRECT-REPEAT")
            .count(),
        2,
        "{rendered:?}"
    );
}

#[tokio::test]
async fn immediate_run_shell_commands_get_one_child_guard_per_nesting_level() {
    // Fresh tmux 3.7b oracle: each run-shell -C level closes its current
    // guard before the inserted callback begins in a new guard.
    let handler = Arc::new(RequestHandler::new());
    handler
        .create_session(Sizeless("control-message-run-shell-nesting"))
        .await;

    let commands = [
        "run-shell -C 'display-message -- RUN-C-NEST-1'",
        "run-shell -C \"run-shell -C 'display-message -- RUN-C-NEST-2'\"",
        "run-shell -C \"run-shell -C \\\"run-shell -C \
         'display-message -- RUN-C-NEST-3'\\\"\"",
    ];
    let (rendered, transcript) = run_initial_control_commands(&handler, 42_441, &commands).await;

    assert_eq!(transcript.frames.len(), 9, "{rendered:?}");
    for parent in [0, 2, 3, 5, 6, 7] {
        assert!(
            transcript.frames[parent].notifications.is_empty(),
            "run-shell parent/level {parent} captured its child: {rendered:?}"
        );
    }
    assert_message_owned_once(&transcript, 1, "RUN-C-NEST-1");
    assert_message_owned_once(&transcript, 4, "RUN-C-NEST-2");
    assert_message_owned_once(&transcript, 8, "RUN-C-NEST-3");
    assert!(
        transcript.frames.iter().all(|frame| frame.guard.flags == 0),
        "initial parents and synchronous callbacks retain flag 0: {rendered:?}"
    );
    assert_no_asynchronous_messages(&rendered, &transcript);
}

#[tokio::test]
async fn immediate_run_shell_callback_error_gets_its_own_child_guard() {
    let handler = Arc::new(RequestHandler::new());
    handler
        .create_session(Sizeless("control-message-run-shell-error"))
        .await;

    let commands = ["run-shell -C 'display-message -- RUN-C-BEFORE-ERROR ; \
         kill-pane -t missing-run-session:0.0'"];
    let (rendered, transcript) = run_initial_control_commands(&handler, 42_442, &commands).await;

    assert_eq!(transcript.frames.len(), 3, "{rendered:?}");
    assert_frame_terminal(&rendered, &transcript, 0, TestGuardTerminal::End);
    assert_frame_quiet(&rendered, &transcript, 0);
    assert_message_owned_once(&transcript, 1, "RUN-C-BEFORE-ERROR");
    assert_frame_error(&rendered, &transcript, 2, "missing-run-session");
}

#[tokio::test]
async fn delayed_run_shell_control_message_remains_asynchronous_product_divergence() {
    // Frozen tmux 3.7b gives the delayed `run-shell -C` callback its own
    // control guard. RMUX did not do so before W13-M30, and this fix must not
    // annex that delayed notification to the already-closed parent guard.
    let handler = Arc::new(RequestHandler::new());
    let session_name = handler
        .create_session(Sizeless("control-message-guard-delayed"))
        .await;

    let requester_pid = 42_432;
    let (server_event_tx, server_events) = mpsc::channel(CONTROL_SERVER_EVENT_CAPACITY);
    let (identity, closing) =
        register_control(&handler, requester_pid, plain_upgrade(2), server_event_tx).await;
    let input = format!(
        "attach-session -t {session_name}\n\
         run-shell -d 0.05 -C \"display-message -- DELAYED-RUN-SHELL-MESSAGE\"\n"
    )
    .into_bytes();
    let mut control = ControlClient::forward(
        &handler,
        identity,
        closing,
        server_events,
        ControlUpgradeInput::new(input, 2),
        true,
    );

    let mut rendered = Vec::new();
    let notified = control
        .read_until(
            &mut rendered,
            b"%message DELAYED-RUN-SHELL-MESSAGE",
            "delayed run-shell notification arrives before timeout",
        )
        .await;
    assert!(
        notified,
        "control stream closed before delayed notification"
    );

    control
        .stream
        .write_all(b"\n")
        .await
        .expect("empty command exits control mode");
    rendered.extend(control.read_to_eof().await);
    control.join().await;

    let rendered = String::from_utf8(rendered).expect("control transcript is utf-8");
    let transcript = parse_strict_control_transcript(&rendered);
    assert_eq!(transcript.frames.len(), 2, "{rendered:?}");
    assert_message_asynchronous_once(&transcript, "DELAYED-RUN-SHELL-MESSAGE");
}

#[tokio::test]
async fn eof_on_empty_input_emits_bare_exit() {
    assert_input_emits_initial_frame_then_exit(b"", true).await;
}

#[tokio::test]
async fn eof_after_command_block_appends_exit() {
    let handler = Arc::new(RequestHandler::new());
    let mut control = ControlClient::open(&handler, 4242, b"display-message -p ok\n", 1);
    control.close_input().await;

    let rendered = control.transcript().await;
    let begin = parse_guard_lines(&rendered, "%begin ")
        .pop()
        .expect("expected %begin guard for the command block");
    let end = parse_guard_lines(&rendered, "%end ")
        .pop()
        .expect("expected %end guard for the command block");
    assert_eq!(begin.command_number, end.command_number);
    assert_eq!(begin.flags, end.flags);
    assert_eq!(begin.command_number, 1);
    assert_eq!(begin.flags, 0);
    assert!(
        begin.time_secs > 0,
        "begin timestamp must be populated: {begin:?}"
    );
    assert!(
        end.time_secs >= begin.time_secs,
        "end timestamp must be monotonic: {begin:?} -> {end:?}"
    );
    let last_line = rendered
        .lines()
        .last()
        .expect("control output is non-empty");
    assert_eq!(
        last_line, "%exit",
        "EOF after a command block must terminate with %exit: {rendered:?}"
    );
}

#[tokio::test]
// Product divergence measured against tmux 3.7b: tmux drops queued work once
// control input reaches EOF. RMUX deliberately finishes non-blocking automation
// after closing the transport, while cancelling frames that would wait forever.
async fn eof_closes_transport_while_finite_control_queue_continues_product_divergence() {
    let handler = Arc::new(RequestHandler::new());
    let _requester_access_guard =
        handler.begin_test_detached_requester_access(4242, AccessMode::ReadWrite);
    let marker = unique_temp_path("control-eof-detached");
    let command = format!(
        "run-shell 'sleep 1; printf done > {}'\nset-buffer -b eof-follow-on done\n",
        marker.display()
    );
    let mut control = ControlClient::open(&handler, 4242, command, 1);

    let begin_prefix = control.read_begin_prefix().await;
    control.close_input().await;

    let remaining = control
        .read_promptly("control EOF must not wait for the foreground shell job")
        .await;
    assert!(
        !control.task.is_finished(),
        "the server-side finite queue must remain alive after the transport closes"
    );

    let rendered = format!(
        "{begin_prefix}{}",
        String::from_utf8(remaining).expect("utf-8 control stream")
    );
    assert!(
        rendered.contains("%end "),
        "EOF must close the pending command guard: {rendered:?}"
    );
    assert!(
        !rendered.contains("%error "),
        "finite pending command must not be converted to %error after EOF: {rendered:?}"
    );
    assert!(
        rendered.ends_with("%exit\n"),
        "EOF must terminate control mode immediately: {rendered:?}"
    );

    wait_until(
        Duration::from_secs(3),
        Duration::from_millis(20),
        async || match std::fs::read_to_string(&marker) {
            Ok(contents) if contents == "done" => Ok(()),
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                panic!("read detached shell marker: {error}")
            }
            other => Err(other),
        },
    )
    .await
    .expect("detached foreground shell job still completes server-side");
    assert_eq!(
        std::fs::read_to_string(&marker).expect("read detached shell marker"),
        "done"
    );
    tokio::time::timeout(CONTROL_TEST_TIMEOUT, control.task)
        .await
        .expect("finite control queue completes before timeout")
        .expect("forward control task joins")
        .expect("forward control succeeds");
    assert_buffer(
        &handler,
        "eof-follow-on",
        b"done",
        "follow-on set-buffer succeeds",
    )
    .await;
    let _ = std::fs::remove_file(marker);
}

#[tokio::test]
async fn eof_preserves_active_if_shell_when_wait_is_only_in_unselected_branch_product_divergence() {
    let handler = Arc::new(RequestHandler::new());
    let _requester_access_guard =
        handler.begin_test_detached_requester_access(4250, AccessMode::ReadWrite);
    let input = b"if-shell -F 1 { run-shell 'sleep 1' ; set-buffer -b eof-active-finite-branch done } { wait-for eof-active-unselected-wait }\n";
    let mut control = ControlClient::open(&handler, 4250, input, 1);

    let mut begin_prefix = vec![0_u8; 256];
    let bytes_read = control
        .stream
        .read(&mut begin_prefix)
        .await
        .expect("control output begins");
    assert!(
        String::from_utf8_lossy(&begin_prefix[..bytes_read]).contains("%begin "),
        "active frame emits its begin guard before EOF"
    );
    control.close_input().await;

    control
        .read_promptly("unselected wait does not retain the transport")
        .await;
    assert!(
        !control.task.is_finished(),
        "the selected finite branch keeps draining after transport EOF"
    );
    tokio::time::timeout(CONTROL_TEST_TIMEOUT, control.task)
        .await
        .expect("selected finite branch finishes before timeout")
        .expect("control task joins")
        .expect("control queue drains successfully");

    assert_eq!(
        handler.wait_for_counts("eof-active-unselected-wait"),
        (0, 0, false),
        "the unselected wait branch must never register"
    );
    assert_buffer(
        &handler,
        "eof-active-finite-branch",
        b"done",
        "selected finite branch executes after EOF",
    )
    .await;
}

#[tokio::test]
async fn eof_queued_if_shell_cancels_only_a_selected_wait_frame_product_divergence() {
    let handler = Arc::new(RequestHandler::new());
    let _requester_access_guard =
        handler.begin_test_detached_requester_access(4251, AccessMode::ReadWrite);
    let input = b"run-shell 'sleep 1'\nif-shell -F 1 { set-buffer -b eof-queued-finite-branch done } { wait-for eof-queued-unselected-wait }\nif-shell -F 1 { wait-for eof-queued-selected-wait ; set-buffer -b eof-queued-after-wait must-not-run } { set-buffer -b eof-queued-fallback must-not-run }\nset-buffer -b eof-queued-later-frame done\n";
    let mut control = ControlClient::open(&handler, 4251, input, 1);

    control.close_input().await;
    control
        .read_promptly("queued wait branches do not retain the transport")
        .await;
    tokio::time::timeout(CONTROL_TEST_TIMEOUT, control.task)
        .await
        .expect("EOF queue drains before timeout")
        .expect("control task joins")
        .expect("queued frames drain independently");

    for name in ["eof-queued-finite-branch", "eof-queued-later-frame"] {
        let expectation = format!("buffer {name} must exist");
        assert_buffer(&handler, name, b"done", &expectation).await;
    }
    for name in ["eof-queued-after-wait", "eof-queued-fallback"] {
        let context = format!("selected wait must stop its frame before buffer {name}");
        assert_buffer_missing(&handler, name, &context).await;
    }
    assert_eq!(
        handler.wait_for_counts("eof-queued-unselected-wait"),
        (0, 0, false)
    );
    assert_eq!(
        handler.wait_for_counts("eof-queued-selected-wait"),
        (0, 0, false)
    );
}

#[tokio::test]
async fn eof_queued_ready_wait_consumes_signal_and_finishes_its_frame() {
    let handler = Arc::new(RequestHandler::new());
    let channel = "eof-queued-ready-wait";
    handler
        .handle_ok(WaitForRequest::fixture((channel, WaitForMode::Signal)))
        .await;
    assert_eq!(handler.wait_for_counts(channel), (0, 0, true));

    drain_queued_frame_after_eof(
        &handler,
        4253,
        format!("wait-for {channel} ; set-buffer -b eof-after-ready-wait done"),
    )
    .await;

    assert_eq!(
        handler.wait_for_counts(channel),
        (0, 0, false),
        "the Ready wait must consume its pre-existing signal before EOF cancellation"
    );
    assert_buffer(
        &handler,
        "eof-after-ready-wait",
        b"done",
        "Ready wait continues its queued frame",
    )
    .await;
}

#[tokio::test]
async fn eof_queued_free_lock_acquires_and_finishes_its_frame() {
    let handler = Arc::new(RequestHandler::new());
    let channel = "eof-queued-ready-lock";
    assert_eq!(handler.wait_for_counts(channel), (0, 0, false));

    drain_queued_frame_after_eof(
        &handler,
        4254,
        format!("wait-for -L {channel} ; set-buffer -b eof-after-ready-lock done"),
    )
    .await;

    assert_eq!(
        handler.wait_for_counts(channel),
        (0, 0, true),
        "a free lock is Ready and must be acquired before EOF cancellation"
    );
    assert_buffer(
        &handler,
        "eof-after-ready-lock",
        b"done",
        "Ready lock continues its queued frame",
    )
    .await;

    handler
        .handle_ok(WaitForRequest::fixture((channel, WaitForMode::Unlock)))
        .await;
    assert_eq!(handler.wait_for_counts(channel), (0, 0, false));
}

#[tokio::test]
async fn eof_queue_skips_parse_errors_and_blocking_frames_before_later_finite_frame_product_divergence(
) {
    let handler = Arc::new(RequestHandler::new());
    let input = b"run-shell 'sleep 1'\ndisplay-message -p 'unterminated\nwait-for never-signalled\nset-buffer -b eof-after-skipped-frames done\n";
    let mut control = ControlClient::open(&handler, 4245, input, 1);

    control.close_input().await;
    control
        .read_promptly("parse and wait-for frames must not retain the transport")
        .await;
    tokio::time::timeout(CONTROL_TEST_TIMEOUT, control.task)
        .await
        .expect("blocking wait-for frame is skipped after EOF")
        .expect("control task joins")
        .expect("queued parse errors stay local to their frame");

    assert_buffer(
        &handler,
        "eof-after-skipped-frames",
        b"done",
        "later finite frame still executes",
    )
    .await;
}

#[tokio::test]
async fn eof_queue_exit_event_stops_before_later_mutation_frame() {
    let handler = Arc::new(RequestHandler::new());
    let requester_pid = 4246;
    let (event_tx, mut event_rx) = mpsc::channel(CONTROL_SERVER_EVENT_CAPACITY);
    let (identity, closing) =
        register_control(&handler, requester_pid, plain_upgrade(0), event_tx.clone()).await;
    closing.store(true, Ordering::SeqCst);
    assert_eq!(
        handler.begin_control_queue_drain(identity).await,
        ControlQueueDrainLease::Acquired,
        "exact control registration begins draining"
    );

    // Model a completed first frame that synchronously emitted Exit. Waiting
    // until both the event and JoinHandle are ready pins the select race: the
    // post-join event drain must still suppress frame two.
    let active_task = tokio::spawn(async move {
        event_tx
            .send(ControlServerEvent::Exit(None))
            .await
            .expect("control event receiver remains open");
        successful_command_result()
    });
    wait_until(CONTROL_TEST_TIMEOUT, Duration::from_millis(1), async || {
        active_task.is_finished().then_some(()).ok_or(())
    })
    .await
    .expect("first frame finishes");

    drain_line_after_eof(
        &handler,
        identity,
        &mut event_rx,
        Some(active_task),
        "set-buffer -b eof-after-exit must-not-run",
    )
    .await
    .expect("EOF queue drains without transport");

    assert_buffer_missing(
        &handler,
        "eof-after-exit",
        "an Exit from frame one must suppress frame two",
    )
    .await;
    handler
        .finish_control(requester_pid, identity.control_id())
        .await;
}

/// The result of a control command that succeeded without output.
fn successful_command_result() -> ControlCommandResult {
    ControlCommandResult {
        stdout: Vec::new(),
        error: None,
        source_file_error: None,
        execution_error: None,
        exit_status: Some(0),
        server_shutdown_started: false,
    }
}

#[tokio::test]
async fn eof_queue_rechecks_registration_after_active_exit_delivery_fails() {
    let handler = Arc::new(RequestHandler::new());
    let requester_pid = 4249;
    let (event_tx, mut event_rx) = mpsc::channel(1);
    let (identity, closing) =
        register_control(&handler, requester_pid, plain_upgrade(0), event_tx.clone()).await;
    assert_eq!(
        handler.begin_control_queue_drain(identity).await,
        ControlQueueDrainLease::Acquired
    );
    event_tx
        .try_send(ControlServerEvent::Notification(
            "%message saturated-before-exit".to_owned(),
        ))
        .expect("fill the control event channel");

    let handler_for_task = Arc::clone(&handler);
    let active_task = tokio::spawn(async move {
        let response = handler_for_task
            .handle(Request::DetachClientExt(
                rmux_proto::DetachClientExtRequest {
                    target_client: Some(requester_pid.to_string()),
                    all_other_clients: false,
                    target_session: None,
                    kill_on_detach: false,
                    exec_command: None,
                },
            ))
            .await;
        assert!(
            matches!(response, Response::DetachClient(_)),
            "{response:?}"
        );
        successful_command_result()
    });
    wait_until(CONTROL_TEST_TIMEOUT, Duration::from_millis(1), async || {
        active_task.is_finished().then_some(()).ok_or(())
    })
    .await
    .expect("active detach finishes while the event channel stays saturated");
    assert!(closing.load(Ordering::SeqCst));
    assert_eq!(
        handler.begin_control_queue_drain(identity).await,
        ControlQueueDrainLease::Acquired,
        "failed Exit delivery keeps the exact closing registration"
    );

    let (_shutdown_tx, mut shutdown_rx) = watch::channel(());
    let (shutdown_handle, _shutdown_request_rx) = ShutdownHandle::new();
    let mut drain_context = EofDrainContext {
        server_events: &mut event_rx,
        events_open: true,
        handler: &handler,
        control_identity: identity,
        shutdown: &mut shutdown_rx,
        shutdown_handle: &shutdown_handle,
    };
    assert!(
        drain_control_command_after_eof(active_task, &mut drain_context)
            .await
            .expect("active EOF frame drains"),
        "a closing registration is terminal even when Exit was never delivered"
    );
    handler
        .finish_control(requester_pid, identity.control_id())
        .await;
    assert_eq!(
        handler.begin_control_queue_drain(identity).await,
        ControlQueueDrainLease::Unavailable,
        "transport finish removes the exact registration"
    );
}

#[tokio::test]
async fn eof_after_deferred_exit_with_removed_registration_finishes_only_active_frame_product_divergence(
) {
    const EVENT_CAPACITY: usize = 8;

    let handler = Arc::new(RequestHandler::new());
    let requester_pid = 4248;
    let session = session_name("eof-deferred-exit-session");
    let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
    let (identity, closing) =
        register_control(&handler, requester_pid, plain_upgrade(0), event_tx.clone()).await;
    let _requester_access_guard =
        handler.begin_test_detached_requester_access(requester_pid, AccessMode::ReadWrite);
    let marker = unique_temp_path("control-eof-deferred-exit");
    let command = format!(
        "new-session -s {session}\nrun-shell 'printf started > {}; sleep 2; printf done >> {}'\nset-buffer -b eof-after-deferred-exit must-not-run\n",
        marker.display(),
        marker.display()
    );
    let mut control = ControlClient::forward(
        &handler,
        identity,
        closing,
        event_rx,
        ControlUpgradeInput::new(command.into_bytes(), 1),
        false,
    );

    wait_until(
        CONTROL_TEST_TIMEOUT,
        Duration::from_millis(10),
        async || match std::fs::read_to_string(&marker) {
            Ok(contents) if contents == "started" => Ok(()),
            other => Err(other),
        },
    )
    .await
    .expect("finite active command starts before the detach");
    wait_until(CONTROL_TEST_TIMEOUT, Duration::from_millis(1), async || {
        let capacity = event_tx.capacity();
        (capacity == EVENT_CAPACITY).then_some(()).ok_or(capacity)
    })
    .await
    .expect("startup control events drain before detach");

    let detached = handler
        .handle(Request::DetachClientExt(
            rmux_proto::DetachClientExtRequest {
                target_client: None,
                all_other_clients: false,
                target_session: Some(session),
                kill_on_detach: false,
                exec_command: None,
            },
        ))
        .await;
    assert!(
        matches!(detached, Response::DetachClient(_)),
        "{detached:?}"
    );
    assert_eq!(
        handler.begin_control_queue_drain(identity).await,
        ControlQueueDrainLease::Unavailable,
        "target-session detach removes the exact control registration"
    );

    // Fill through one event beyond channel capacity. Whether Exit was still
    // queued or had just been consumed, the last barrier cannot be accepted
    // until the forward loop has completed at least one later event turn.
    // Therefore Exit is in DeferredServerEvents, rather than merely waiting
    // in the receiver, before EOF is delivered.
    tokio::time::timeout(CONTROL_TEST_TIMEOUT, async {
        for index in 0..=EVENT_CAPACITY {
            event_tx
                .send(ControlServerEvent::Notification(format!(
                    "%message deferred-exit-barrier-{index}"
                )))
                .await
                .expect("forward control still owns the event receiver");
        }
    })
    .await
    .expect("forward loop consumes Exit and a later barrier while the command is active");

    control.close_input().await;
    let rendered = control
        .read_promptly("deferred Exit closes the transport before the active command finishes")
        .await;
    assert!(
        !control.task.is_finished(),
        "the already-started finite command must finish after transport close"
    );
    let rendered = String::from_utf8(rendered).expect("utf-8 control transcript");
    assert!(
        rendered.contains("%end "),
        "EOF closes the active frame guard: {rendered:?}"
    );
    assert!(
        rendered.ends_with("%exit\n"),
        "the deferred Exit remains terminal: {rendered:?}"
    );

    tokio::time::timeout(CONTROL_TEST_TIMEOUT, control.task)
        .await
        .expect("active command finishes before timeout")
        .expect("forward control task joins")
        .expect("missing queue lease is not a product error");
    assert_eq!(
        std::fs::read_to_string(&marker).expect("read completed command marker"),
        "starteddone",
        "the finite command that was active at EOF must finish"
    );
    assert_buffer_missing(
        &handler,
        "eof-after-deferred-exit",
        "a queued frame after deferred Exit must never run",
    )
    .await;
    let _ = std::fs::remove_file(marker);
}

#[tokio::test]
async fn external_shutdown_drains_admitted_finite_eof_mutation_product_divergence() {
    let handler = Arc::new(RequestHandler::new());
    let marker = unique_temp_path("control-eof-shutdown");
    let command = format!(
        "run-shell 'sleep 0.4; printf done > {}'\n",
        marker.display()
    );
    let mut control = ControlClient::open(&handler, 4247, command, 1);
    control.close_input().await;
    control
        .read_promptly("control transport closes before the finite frame completes")
        .await;
    assert!(
        !control.task.is_finished(),
        "finite frame is still draining before external shutdown"
    );
    assert!(!handler.normal_drain_requests_quiesced());

    handler.close_normal_request_admission();
    control.shutdown_tx.send_replace(());
    tokio::time::timeout(CONTROL_TEST_TIMEOUT, control.task)
        .await
        .expect("external shutdown drains the admitted detached mutation")
        .expect("control task joins")
        .expect("shutdown drain is clean");
    assert!(handler.normal_drain_requests_quiesced());
    assert_eq!(
        std::fs::read_to_string(&marker).expect("admitted EOF mutation commits"),
        "done"
    );
    let _ = std::fs::remove_file(marker);
}

#[tokio::test]
async fn eof_drains_finite_queue_through_kill_server_product_divergence() {
    let handler = Arc::new(RequestHandler::new());
    let mut control = ControlClient::open(
        &handler,
        4243,
        b"run-shell 'sleep 1' ; kill-server ; set-buffer -b eof-same-frame must-not-run\nset-buffer -b eof-next-frame must-not-run\n",
        2,
    );
    handler.install_shutdown_handle(control.shutdown_handle.clone());

    control.close_input().await;
    control
        .read_promptly("control transport closes before the shell job finishes")
        .await;
    assert!(
        !control.task.is_finished(),
        "kill-server must remain queued after transport EOF"
    );

    tokio::time::timeout(CONTROL_TEST_TIMEOUT, control.shutdown_request_rx)
        .await
        .expect("queued kill-server requests shutdown before timeout")
        .expect("shutdown request channel stays open");
    tokio::time::timeout(CONTROL_TEST_TIMEOUT, control.task)
        .await
        .expect("finite control queue completes before timeout")
        .expect("forward control task joins")
        .expect("forward control succeeds");

    for buffer_name in ["eof-same-frame", "eof-next-frame"] {
        let context = format!("kill-server must suppress {buffer_name}");
        assert_buffer_missing(&handler, buffer_name, &context).await;
    }
}

#[tokio::test]
async fn eof_queue_lease_blocks_same_pid_registration_and_preserves_permissions_product_divergence()
{
    let handler = Arc::new(RequestHandler::new());
    let requester_pid = 4244;
    let (old_event_tx, old_event_rx) = mpsc::channel(CONTROL_SERVER_EVENT_CAPACITY);
    let (old_identity, old_closing) =
        register_control(&handler, requester_pid, plain_upgrade(0), old_event_tx).await;
    let eof_lease_pause = install_control_eof_queue_lease_pause(&handler, old_identity);
    let mut control = ControlClient::forward(
        &handler,
        old_identity,
        old_closing,
        old_event_rx,
        ControlUpgradeInput::new(
            b"run-shell 'sleep 1' ; set-buffer -b eof-old-identity old\n".to_vec(),
            1,
        ),
        true,
    );

    control.close_input().await;
    tokio::time::timeout(CONTROL_TEST_TIMEOUT, eof_lease_pause.reached.notified())
        .await
        .expect("EOF acquires the old queue lease before its next select turn");

    let (new_event_tx, _new_event_rx) = mpsc::channel(CONTROL_SERVER_EVENT_CAPACITY);
    let handler_for_registration = Arc::clone(&handler);
    let registration_task = tokio::spawn(async move {
        handler_for_registration
            .register_control_with_access(
                requester_pid,
                plain_upgrade(0),
                ControlRegistration {
                    event_tx: new_event_tx,
                    closing: Arc::new(AtomicBool::new(false)),
                    uid: current_owner_uid(),
                    user: UserIdentity::Uid(current_owner_uid()),
                    can_write: false,
                },
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !registration_task.is_finished(),
        "same-PID registration must wait as soon as EOF is observed"
    );
    eof_lease_pause.release.notify_one();

    control
        .read_promptly("old control transport closes before its queue finishes")
        .await;
    assert!(
        !control.task.is_finished(),
        "old control queue must still own its registration lease"
    );

    tokio::time::timeout(CONTROL_TEST_TIMEOUT, control.task)
        .await
        .expect("old finite queue completes before timeout")
        .expect("old control task joins")
        .expect("old control queue succeeds");
    let new_control_id = tokio::time::timeout(CONTROL_TEST_TIMEOUT, registration_task)
        .await
        .expect("new same-PID registration resumes after the old lease")
        .expect("new registration task joins")
        .expect("finite drain finishes within the registration deadline");
    assert_ne!(old_identity.control_id(), new_control_id);

    assert_buffer(
        &handler,
        "eof-old-identity",
        b"old",
        "old queue keeps its write permission",
    )
    .await;

    let commands = handler
        .parse_control_commands("set-buffer -b eof-new-identity new")
        .await
        .expect("new control command parses");
    let denied = handler
        .execute_control_commands_identity(requester_pid, new_control_id, commands)
        .await;
    assert!(
        denied
            .error
            .as_ref()
            .is_some_and(|error| error.to_string().contains("read-only")),
        "new registration must use its own read-only permission: {denied:?}"
    );
    assert!(matches!(
        show_buffer(&handler, "eof-new-identity").await,
        Response::Error(_)
    ));
    handler.finish_control(requester_pid, new_control_id).await;
}

#[tokio::test]
async fn same_pid_registration_times_out_behind_a_stuck_eof_drain() {
    let handler = RequestHandler::new();
    let requester_pid = 42_441;
    let (old_control_id, _old_event_rx) =
        handler.register_control_for_test(requester_pid, None).await;
    let old_identity = ControlClientIdentity::new(requester_pid, old_control_id);
    assert_eq!(
        handler.begin_control_queue_drain(old_identity).await,
        ControlQueueDrainLease::Acquired
    );

    let (replacement_event_tx, _replacement_event_rx) =
        mpsc::channel(CONTROL_SERVER_EVENT_CAPACITY);
    let error = handler
        .register_control_with_access_timeout_for_test(
            requester_pid,
            plain_upgrade(0),
            ControlRegistration {
                event_tx: replacement_event_tx,
                closing: Arc::new(AtomicBool::new(false)),
                uid: current_owner_uid(),
                user: UserIdentity::Uid(current_owner_uid()),
                can_write: true,
            },
            Duration::from_millis(25),
        )
        .await
        .expect_err("a stuck old drain must not retain a replacement forever");
    assert_eq!(
        error,
        ControlRegistrationError::QueueDrainTimedOut { requester_pid }
    );
    assert!(matches!(
        error.into_rmux_error(),
        RmuxError::Server(message)
            if message.contains("previous control queue")
                && message.contains(&requester_pid.to_string())
    ));
    assert!(
        handler.control_queue_identity_is_open(old_identity).await,
        "timing out the replacement must not cancel the old finite automation"
    );

    handler.finish_control(requester_pid, old_control_id).await;
}

#[tokio::test]
async fn stdin_command_after_upgrade_uses_flags_one_after_initial_ack() {
    let handler = Arc::new(RequestHandler::new());
    let mut control = ControlClient::open(&handler, 4242, b"", 0);

    control
        .stream
        .write_all(b"display-message -p ok\n")
        .await
        .expect("stdin command writes");
    control.close_input().await;

    let rendered = control.transcript().await;
    let begins = parse_guard_lines(&rendered, "%begin ");
    let ends = parse_guard_lines(&rendered, "%end ");
    assert_eq!(
        begins.len(),
        2,
        "expected ack plus stdin block: {rendered:?}"
    );
    assert_eq!(ends.len(), 2, "expected ack plus stdin block: {rendered:?}");
    assert_eq!(begins[0].command_number, 1);
    assert_eq!(begins[0].flags, 0);
    assert_eq!(begins[1].command_number, 2);
    assert_eq!(begins[1].flags, 1);
    assert_eq!(ends[1].command_number, begins[1].command_number);
    assert_eq!(ends[1].flags, begins[1].flags);
    assert!(
        rendered.contains("ok\n"),
        "stdin command output should be present: {rendered:?}"
    );
    assert!(
        rendered.ends_with("%exit\n"),
        "EOF after stdin command must terminate with %exit: {rendered:?}"
    );
}

#[tokio::test]
async fn completed_unattached_initial_command_exits_with_stdin_open_and_discards_follow_on_frames()
{
    let handler = Arc::new(RequestHandler::new());
    let rendered = ControlClient::open(
        &handler,
        4242,
        b"display-message -p INITIAL\ndisplay-message -p SHOULD-NOT-RUN\n",
        1,
    )
    .transcript()
    .await;
    assert!(rendered.contains("INITIAL\n"), "{rendered:?}");
    assert!(!rendered.contains("SHOULD-NOT-RUN"), "{rendered:?}");
    assert_eq!(parse_guard_lines(&rendered, "%begin ").len(), 1);
    assert_eq!(parse_guard_lines(&rendered, "%end ").len(), 1);
    assert!(rendered.ends_with("%exit\n"), "{rendered:?}");
}

#[tokio::test]
async fn immediate_socket_eof_preserves_fast_attach_query_payloads_and_guards() {
    let handler = Arc::new(RequestHandler::new());
    let session_name = handler
        .create_session(Sizeless("eof-fast-multi-frame"))
        .await;
    let mut control = ControlClient::open(&handler, 4243, b"", 0);

    let frames = format!(
        "attach-session -t {session_name}\nlist-clients -F '#{{client_flags}}'\ndisplay-message -p second\n"
    );
    control
        .stream
        .write_all(frames.as_bytes())
        .await
        .expect("all control frames write in one socket batch");
    control
        .stream
        .shutdown()
        .await
        .expect("client write half closes immediately after the frames");

    let rendered = control.transcript().await;
    let payloads = rendered
        .lines()
        .filter(|line| *line == "attached,focused,control-mode" || *line == "second")
        .collect::<Vec<_>>();
    assert_eq!(
        payloads,
        vec!["attached,focused,control-mode", "second"],
        "every fast frame accepted before EOF keeps its payload: {rendered:?}"
    );

    let begins = parse_guard_lines(&rendered, "%begin ");
    let ends = parse_guard_lines(&rendered, "%end ");
    assert_eq!(begins.len(), 4, "ACK plus three frame guards: {rendered:?}");
    assert_eq!(ends.len(), 4, "ACK plus three frame guards: {rendered:?}");
    for (begin, end) in begins.iter().zip(&ends) {
        assert_eq!(begin.command_number, end.command_number, "{rendered:?}");
        assert_eq!(begin.flags, end.flags, "{rendered:?}");
    }
    assert_eq!(
        rendered
            .lines()
            .filter(|line| line.starts_with("%exit"))
            .count(),
        1,
        "EOF emits exactly one terminal exit line: {rendered:?}"
    );
    assert!(
        rendered.ends_with("%exit\n"),
        "EOF remains the final control record: {rendered:?}"
    );
}

#[tokio::test]
async fn plain_control_eof_keeps_ready_existing_session_attach_before_exit() {
    let handler = Arc::new(RequestHandler::new());
    let requester_pid = 42_431;
    let session_name = handler
        .create_session(Sizeless("plain-control-eof-attach-race"))
        .await;

    let (event_tx, event_rx) = mpsc::channel(CONTROL_SERVER_EVENT_CAPACITY);
    let (identity, closing) =
        register_control(&handler, requester_pid, plain_upgrade(1), event_tx).await;
    let eof_pause = install_control_eof_queue_lease_pause(&handler, identity);
    let _requester_access_guard =
        handler.begin_test_detached_requester_access(requester_pid, AccessMode::ReadWrite);
    let command = format!("attach-session -t {session_name}\n");
    let mut control = ControlClient::forward(
        &handler,
        identity,
        closing,
        event_rx,
        ControlUpgradeInput::with_mode(command.into_bytes(), 1, ControlMode::Plain),
        true,
    );

    control
        .stream
        .shutdown()
        .await
        .expect("client write half closes immediately");
    tokio::time::timeout(CONTROL_TEST_TIMEOUT, eof_pause.reached.notified())
        .await
        .expect("forward loop observes EOF while attach is active");
    wait_until(CONTROL_TEST_TIMEOUT, Duration::from_millis(1), async || {
        let attached = handler.control_session_name(requester_pid).await;
        (attached.as_ref() == Some(&session_name))
            .then_some(())
            .ok_or(attached)
    })
    .await
    .expect("attach commits while the forward loop remains paused");
    eof_pause.release.notify_one();

    let rendered = control.transcript().await;
    let records = rendered.lines().collect::<Vec<_>>();
    assert_eq!(records.len(), 4, "{rendered:?}");
    assert!(records[0].starts_with("%begin "), "{rendered:?}");
    assert!(records[1].starts_with("%end "), "{rendered:?}");
    assert_eq!(
        records[2],
        format!("%session-changed $0 {session_name}"),
        "{rendered:?}"
    );
    assert_eq!(records[3], "%exit", "{rendered:?}");
}

#[tokio::test]
async fn control_control_eof_reconciles_ready_session_change_before_exit() {
    // tmux 3.7b keeps `-CC new-session` attached after terminal EOF and
    // delivers pane output before `%exit`. Hold the RMUX transport in its EOF
    // path until both the attach command result and SessionChangedAt are ready
    // so the biased-select ordering is deterministic.
    let handler = Arc::new(RequestHandler::new());
    let requester_pid = 42_430;
    let session_name = handler
        .create_session(Sizeless("control-control-eof-session-race"))
        .await;
    let pane_output = handler
        .control_session_panes(&session_name)
        .await
        .expect("session pane output is available")
        .into_iter()
        .next()
        .expect("initial pane has an output sender")
        .1;

    let (event_tx, event_rx) = mpsc::channel(CONTROL_SERVER_EVENT_CAPACITY);
    let upgrade = ControlModeUpgrade {
        mode: ControlMode::ControlControl,
        ..plain_upgrade(1)
    };
    let (identity, closing) = register_control(&handler, requester_pid, upgrade, event_tx).await;
    let attach_pause = handler.install_created_session_control_attach_pause(session_name.clone());
    let eof_pause = install_control_eof_queue_lease_pause(&handler, identity);
    let _requester_access_guard =
        handler.begin_test_detached_requester_access(requester_pid, AccessMode::ReadWrite);
    let command =
        format!("new-session -A -s {session_name} ; set-buffer -b control-cc-race-ready done\n");
    let mut control = ControlClient::forward(
        &handler,
        identity,
        closing,
        event_rx,
        ControlUpgradeInput::with_mode(command.into_bytes(), 1, ControlMode::ControlControl),
        true,
    );

    tokio::time::timeout(CONTROL_TEST_TIMEOUT, attach_pause.reached.notified())
        .await
        .expect("attach command reaches the pre-commit pause");
    control.close_input().await;
    tokio::time::timeout(CONTROL_TEST_TIMEOUT, eof_pause.reached.notified())
        .await
        .expect("forward loop observes EOF while attach is active");

    attach_pause.release.notify_one();
    wait_until(CONTROL_TEST_TIMEOUT, Duration::from_millis(1), async || {
        let shown = show_buffer(&handler, "control-cc-race-ready").await;
        shown
            .command_output()
            .is_some_and(|output| output.stdout() == b"done")
            .then_some(())
            .ok_or(shown)
    })
    .await
    .expect("attach command completes while the forward loop remains paused");
    pane_output.send(b"CONTROL_CC_RACE_LIVE".to_vec());
    eof_pause.release.notify_one();

    let mut rendered = Vec::new();
    let saw_live_output = control
        .read_until(
            &mut rendered,
            b"CONTROL_CC_RACE_LIVE",
            "control client produces live output or closes before timeout",
        )
        .await;
    assert!(
        saw_live_output,
        "ready SessionChangedAt must be reconciled before EOF exit: {:?}",
        String::from_utf8_lossy(&rendered)
    );

    handler
        .handle_ok(KillSessionRequest::fixture(session_name))
        .await;
    rendered.extend(control.read_to_eof().await);
    control.join().await;
    assert!(
        String::from_utf8_lossy(&rendered).contains("%exit"),
        "session teardown terminates the control client: {:?}",
        String::from_utf8_lossy(&rendered)
    );
}

#[tokio::test]
async fn fragmented_argv_command_stays_initial_without_synthetic_ack() {
    let handler = Arc::new(RequestHandler::new());
    let mut control = ControlClient::open(&handler, 4242, b"", 1);

    for fragment in [b"display-message -p ".as_slice(), b"initial", b"\n"] {
        control
            .stream
            .write_all(fragment)
            .await
            .expect("fragment writes");
        tokio::task::yield_now().await;
    }
    control.close_input().await;

    let rendered = control.transcript().await;
    let begins = parse_guard_lines(&rendered, "%begin ");
    let ends = parse_guard_lines(&rendered, "%end ");
    assert_eq!(begins.len(), 1, "no empty ACK is allowed: {rendered:?}");
    assert_eq!(ends.len(), 1, "no empty ACK is allowed: {rendered:?}");
    assert_eq!(begins[0].command_number, 1);
    assert_eq!(begins[0].flags, 0);
    assert_eq!(ends[0].command_number, 1);
    assert_eq!(ends[0].flags, 0);
    assert!(rendered.contains("initial\n"), "{rendered:?}");
}

#[tokio::test]
async fn command_with_more_than_one_thousand_arguments_errors() {
    assert_command_exceeds_argument_cap("display-message", "\n\n", "oversized MSG_COMMAND").await;
}

#[tokio::test]
async fn nested_command_with_more_than_one_thousand_arguments_errors() {
    assert_command_exceeds_argument_cap(
        "bind-key x { display-message",
        " }\n\n",
        "oversized nested command",
    )
    .await;
}

/// Sends `prefix`, 1001 arguments and `suffix` as one initial command, and asserts that control
/// mode rejects it for exceeding the argument cap; `subject` names the command in messages.
async fn assert_command_exceeds_argument_cap(prefix: &str, suffix: &str, subject: &str) {
    let handler = Arc::new(RequestHandler::new());
    let mut input = String::from(prefix);
    for index in 0..1001 {
        input.push_str(" arg");
        input.push_str(&index.to_string());
    }
    input.push_str(suffix);

    let rendered = ControlClient::open(&handler, 4242, input, 1)
        .transcript()
        .await;
    assert!(
        rendered.contains("too many arguments: 1001 (maximum 1000)"),
        "{subject} should report the argument cap: {rendered:?}"
    );
    assert!(
        rendered.contains("%error "),
        "{subject} should close the block with %error: {rendered:?}"
    );
    assert!(
        !rendered
            .lines()
            .any(|line| line.starts_with("%end ") && line.ends_with(" 1")),
        "{subject} must not close the user block with %end: {rendered:?}"
    );
    assert!(
        rendered.ends_with("%exit\n"),
        "empty trailing line should still close control mode: {rendered:?}"
    );
}

#[tokio::test]
async fn pending_control_command_waits_for_completion_without_execution_timeout() {
    let handler = Arc::new(RequestHandler::new());
    let _requester_access_guard =
        handler.begin_test_detached_requester_access(4242, AccessMode::ReadWrite);
    let mut control = ControlClient::open(&handler, 4242, b"wait-for control-timeout-block\n\n", 1);

    let begin_prefix = control.read_begin_prefix().await;
    wait_for_waiter(&handler, "control-timeout-block").await;
    tokio::time::sleep(Duration::from_millis(650)).await;
    handler
        .handle_ok(WaitForRequest::fixture((
            "control-timeout-block",
            WaitForMode::Signal,
        )))
        .await;

    let rendered = format!("{begin_prefix}{}", control.transcript().await);
    assert!(
        !rendered.contains("command timed out after"),
        "control-mode must not cap command execution at 500ms: {rendered:?}"
    );
    assert!(
        rendered.contains("%end "),
        "signalled pending control command should close successfully: {rendered:?}"
    );
    assert!(
        !rendered.contains("%error "),
        "signalled pending control command must not emit %error: {rendered:?}"
    );
    assert!(
        rendered.ends_with("%exit\n"),
        "empty trailing line should close control mode after command completion: {rendered:?}"
    );
}

#[tokio::test]
async fn eof_while_control_command_is_pending_closes_guard_and_exits() {
    let handler = Arc::new(RequestHandler::new());
    let _requester_access_guard =
        handler.begin_test_detached_requester_access(4242, AccessMode::ReadWrite);
    let mut control = ControlClient::open(
        &handler,
        4242,
        b"if-shell -F 1 { wait-for control-eof-block ; set-buffer -b eof-active-after-wait must-not-run } { set-buffer -b eof-active-fallback must-not-run }\n",
        1,
    );

    let begin_prefix = control.read_begin_prefix().await;
    wait_for_waiter(&handler, "control-eof-block").await;

    control.close_input().await;

    let rendered = format!("{begin_prefix}{}", control.transcript().await);
    assert!(
        rendered.contains("%end "),
        "EOF while a command is pending must close the guard: {rendered:?}"
    );
    assert!(
        !rendered.contains("%error "),
        "EOF cancellation should be a clean end guard: {rendered:?}"
    );
    assert!(
        rendered.ends_with("%exit\n"),
        "EOF while a command is pending must terminate control mode: {rendered:?}"
    );
    assert_eq!(
        handler.wait_for_counts("control-eof-block"),
        (0, 0, false),
        "EOF cancellation must remove the selected wait registration"
    );
    for name in ["eof-active-after-wait", "eof-active-fallback"] {
        let context = format!("selected wait cancellation must stop its frame before {name}");
        assert_buffer_missing(&handler, name, &context).await;
    }
}

#[tokio::test]
async fn eof_transition_is_not_starved_by_continuous_server_events() {
    let handler = Arc::new(RequestHandler::new());
    let requester_pid = 42_527;
    let (event_tx, event_rx) = mpsc::channel(CONTROL_SERVER_EVENT_CAPACITY);
    let (identity, closing) =
        register_control(&handler, requester_pid, plain_upgrade(0), event_tx.clone()).await;
    let _requester_access_guard =
        handler.begin_test_detached_requester_access(requester_pid, AccessMode::ReadWrite);
    let mut control = ControlClient::forward(
        &handler,
        identity,
        closing,
        event_rx,
        ControlUpgradeInput::new(b"wait-for eof-event-starvation\n".to_vec(), 1),
        true,
    );
    wait_for_waiter(&handler, "eof-event-starvation").await;

    let producer =
        tokio::spawn(
            async move { while event_tx.send(ControlServerEvent::Refresh).await.is_ok() {} },
        );
    control.close_input().await;

    let rendered = control
        .read_promptly("continuous server events cannot retain the EOF transport")
        .await;
    tokio::time::timeout(CONTROL_TEST_TIMEOUT, control.task)
        .await
        .expect("control task exits before timeout")
        .expect("control task joins")
        .expect("control EOF succeeds");
    producer.await.expect("event producer joins");

    assert_eq!(
        handler.wait_for_counts("eof-event-starvation"),
        (0, 0, false),
        "EOF cancellation removes the selected waiter"
    );
    let rendered = String::from_utf8(rendered).expect("control output is utf-8");
    assert!(
        rendered.contains("%end "),
        "active guard closes: {rendered:?}"
    );
    assert!(
        rendered.ends_with("%exit\n"),
        "transport exits: {rendered:?}"
    );

    let (replacement_id, _replacement_rx) = tokio::time::timeout(
        Duration::from_millis(500),
        handler.register_control_for_test(requester_pid, None),
    )
    .await
    .expect("EOF releases the same-PID queue lease");
    assert_ne!(replacement_id, identity.control_id());
    handler.finish_control(requester_pid, replacement_id).await;
}

#[tokio::test]
async fn eof_cancels_selected_lock_waiter_without_releasing_the_lock_owner() {
    let handler = Arc::new(RequestHandler::new());
    let lock_channel = "control-eof-lock-block";
    handler
        .handle_ok(WaitForRequest::fixture((lock_channel, WaitForMode::Lock)))
        .await;

    let input =
        format!("wait-for -L {lock_channel} ; set-buffer -b eof-active-after-lock must-not-run\n");
    let mut control = ControlClient::open(&handler, 4252, input, 1);

    wait_until(
        CONTROL_TEST_TIMEOUT,
        Duration::from_millis(10),
        async || {
            let counts = handler.wait_for_counts(lock_channel);
            (counts.1 == 1).then_some(()).ok_or(counts)
        },
    )
    .await
    .expect("wait-for lock waiter registers before timeout");
    control.close_input().await;
    control.read_to_eof().await;
    tokio::time::timeout(CONTROL_TEST_TIMEOUT, control.task)
        .await
        .expect("selected lock waiter cancels before timeout")
        .expect("control task joins")
        .expect("control queue drains successfully");

    assert_eq!(
        handler.wait_for_counts(lock_channel),
        (0, 0, true),
        "EOF removes only the queued lock waiter and preserves the current owner"
    );
    assert_buffer_missing(
        &handler,
        "eof-active-after-lock",
        "selected lock cancellation must stop the rest of its frame",
    )
    .await;

    handler
        .handle_ok(WaitForRequest::fixture((lock_channel, WaitForMode::Unlock)))
        .await;
    assert_eq!(handler.wait_for_counts(lock_channel), (0, 0, false));
}

#[tokio::test]
async fn dropping_active_control_command_aborts_inflight_task() {
    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let task_started = Arc::clone(&started);
    let task_dropped = Arc::clone(&dropped);
    let task = tokio::spawn(async move {
        let _probe = DropProbe(task_dropped);
        task_started.store(true, Ordering::SeqCst);
        std::future::pending::<ControlCommandResult>().await
    });

    while !started.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }

    drop(ActiveControlCommand {
        timestamp: 0,
        command_number: 1,
        guard_flag: 0,
        origin: ControlCommandOrigin::Initial {
            completes_batch: true,
        },
        eof_cancellation: ControlQueueEofCancellation::new(ControlClientIdentity::new(4242, 1)),
        task: Some(task),
    });

    for _ in 0..50 {
        if dropped.load(Ordering::SeqCst) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("dropping an in-flight control command must abort its task");
}

async fn drain_queued_frame_after_eof(
    handler: &Arc<RequestHandler>,
    requester_pid: u32,
    line: String,
) {
    let (control_id, mut event_rx) = handler.register_control_for_test(requester_pid, None).await;
    let identity = ControlClientIdentity::new(requester_pid, control_id);
    assert_eq!(
        handler.begin_control_queue_drain(identity).await,
        ControlQueueDrainLease::Acquired
    );
    let (queued_lines, queued_bytes) =
        drain_line_after_eof(handler, identity, &mut event_rx, None, &line)
            .await
            .expect("queued EOF frame drains");
    assert!(queued_lines.is_empty());
    assert_eq!(queued_bytes, 0);
    handler.finish_control(requester_pid, control_id).await;
}

/// Drains `line`, queued behind `active_task` if any, for `identity` after its transport's EOF,
/// and answers with the lines and bytes still queued.
async fn drain_line_after_eof(
    handler: &Arc<RequestHandler>,
    identity: ControlClientIdentity,
    server_events: &mut mpsc::Receiver<ControlServerEvent>,
    active_task: Option<JoinHandle<ControlCommandResult>>,
    line: &str,
) -> std::io::Result<(std::collections::VecDeque<String>, usize)> {
    let mut queued_lines = std::collections::VecDeque::from([line.to_owned()]);
    let mut queued_bytes = queued_lines.iter().map(String::len).sum();
    let (_shutdown_tx, mut shutdown_rx) = watch::channel(());
    let (shutdown_handle, _shutdown_request_rx) = ShutdownHandle::new();
    let mut context = EofDrainContext {
        server_events,
        events_open: true,
        handler,
        control_identity: identity,
        shutdown: &mut shutdown_rx,
        shutdown_handle: &shutdown_handle,
    };
    drain_control_queue_after_eof(
        active_task,
        &mut queued_lines,
        &mut queued_bytes,
        false,
        &mut context,
    )
    .await?;
    Ok((queued_lines, queued_bytes))
}

/// Waits until one client blocks in `wait-for channel`.
async fn wait_for_waiter(handler: &RequestHandler, channel: &str) {
    wait_until(
        CONTROL_TEST_TIMEOUT,
        Duration::from_millis(10),
        async || {
            let counts = handler.wait_for_counts(channel);
            (counts.0 == 1).then_some(()).ok_or(counts)
        },
    )
    .await
    .expect("wait-for waiter registers before timeout");
}

#[tokio::test]
async fn empty_line_input_emits_initial_frame_and_bare_exit() {
    // Minimal control-mode scenario: a bare `\n` as the first input byte must
    // route through the in-loop empty-line branch after the initial tmux-style
    // control guard pair, then terminate with a bare `%exit\n`.
    assert_input_emits_initial_frame_then_exit(b"\n", false).await;
}

#[tokio::test]
async fn crlf_empty_line_also_emits_bare_exit() {
    // `extract_complete_control_lines` strips CR+LF as if it were LF,
    // so a bare CRLF must trip the empty-line exit path identically.
    assert_input_emits_initial_frame_then_exit(b"\r\n", false).await;
}

#[tokio::test]
async fn incomplete_trailing_line_is_discarded_on_eof() {
    // control-mode contract: `extract_complete_control_lines` discards any
    // incomplete trailing line on EOF (tmux `evbuffer_readln` semantics).
    // The command-without-newline must not trigger a user-command %begin, and
    // the transcript must still terminate in a bare `%exit\n`.
    assert_input_emits_initial_frame_then_exit(b"display-message -p hello", true).await;
}

/// Forwards `input`, which holds no initial commands, closing the client's input first when
/// `close_input`, and asserts that only the initial guard pair and a bare `%exit` come back.
async fn assert_input_emits_initial_frame_then_exit(input: &[u8], close_input: bool) {
    let handler = Arc::new(RequestHandler::new());
    let mut control = ControlClient::open(&handler, 4242, input, 0);
    if close_input {
        control.close_input().await;
    }
    let mut rendered = Vec::new();
    control
        .stream
        .read_to_end(&mut rendered)
        .await
        .expect("control output drains");
    control.join().await;

    let rendered = String::from_utf8(rendered).expect("utf-8 control stream");
    let begins = parse_guard_lines(&rendered, "%begin ");
    let ends = parse_guard_lines(&rendered, "%end ");
    assert_eq!(
        begins.len(),
        1,
        "empty/discarded input must emit only the initial %begin: {rendered:?}"
    );
    assert_eq!(
        ends.len(),
        1,
        "empty/discarded input must emit only the initial %end: {rendered:?}"
    );
    assert_eq!(begins[0].command_number, 1);
    assert_eq!(begins[0].flags, 0);
    assert_eq!(ends[0].command_number, 1);
    assert_eq!(ends[0].flags, 0);
    assert!(
        !rendered.contains("%error "),
        "empty/discarded input must not emit %error: {rendered:?}"
    );
    assert!(
        rendered.ends_with("%exit\n"),
        "control stream must end with bare %exit: {rendered:?}"
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TestGuardTuple {
    time_secs: i64,
    command_number: u64,
    flags: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestGuardTerminal {
    End,
    Error,
}

#[derive(Debug)]
struct TestControlFrame {
    guard: TestGuardTuple,
    terminal: TestGuardTerminal,
    payload: Vec<String>,
    notifications: Vec<String>,
}

#[derive(Debug)]
struct TestControlTranscript {
    frames: Vec<TestControlFrame>,
    asynchronous_notifications: Vec<String>,
}

fn parse_strict_control_transcript(output: &str) -> TestControlTranscript {
    struct OpenFrame {
        guard: TestGuardTuple,
        payload: Vec<String>,
        notifications: Vec<String>,
    }

    let mut frames = Vec::new();
    let mut asynchronous_notifications = Vec::new();
    let mut current: Option<OpenFrame> = None;
    for line in output.lines() {
        if let Some(guard) = parse_guard_tuple(line, "%begin ") {
            assert!(
                current.is_none(),
                "nested control guard {guard:?} in {output:?}"
            );
            current = Some(OpenFrame {
                guard,
                payload: Vec::new(),
                notifications: Vec::new(),
            });
            continue;
        }
        let terminal = parse_guard_tuple(line, "%end ")
            .map(|guard| (guard, TestGuardTerminal::End))
            .or_else(|| {
                parse_guard_tuple(line, "%error ").map(|guard| (guard, TestGuardTerminal::Error))
            });
        if let Some((guard, terminal)) = terminal {
            let open = current
                .take()
                .unwrap_or_else(|| panic!("orphan terminal guard {guard:?} in {output:?}"));
            assert_eq!(
                open.guard, guard,
                "terminal tuple differs from its begin in {output:?}"
            );
            frames.push(TestControlFrame {
                guard,
                terminal,
                payload: open.payload,
                notifications: open.notifications,
            });
            continue;
        }
        if line.starts_with('%') {
            if let Some(open) = current.as_mut() {
                open.notifications.push(line.to_owned());
            } else {
                asynchronous_notifications.push(line.to_owned());
            }
        } else if let Some(open) = current.as_mut() {
            open.payload.push(line.to_owned());
        } else if !line.is_empty() {
            panic!("control payload escaped its guard: {line:?} in {output:?}");
        }
    }
    assert!(current.is_none(), "unclosed control guard in {output:?}");
    assert!(
        frames
            .windows(2)
            .all(|pair| pair[0].guard.command_number < pair[1].guard.command_number),
        "control command numbers are not strictly monotone in {output:?}"
    );
    TestControlTranscript {
        frames,
        asynchronous_notifications,
    }
}

fn assert_message_owned_once(
    transcript: &TestControlTranscript,
    expected_frame: usize,
    token: &str,
) {
    let expected_line = format!("%message {token}");
    let owned = transcript
        .frames
        .iter()
        .enumerate()
        .flat_map(|(index, frame)| {
            frame
                .notifications
                .iter()
                .filter(|line| *line == &expected_line)
                .map(move |_| index)
        })
        .collect::<Vec<_>>();
    let asynchronous = transcript
        .asynchronous_notifications
        .iter()
        .filter(|line| *line == &expected_line)
        .count();
    assert_eq!(
        owned.len() + asynchronous,
        1,
        "{expected_line:?} must be emitted exactly once: {transcript:?}"
    );
    assert_eq!(
        owned,
        [expected_frame],
        "{expected_line:?} must belong to frame {expected_frame}: {transcript:?}"
    );
}

fn assert_message_asynchronous_once(transcript: &TestControlTranscript, token: &str) {
    let expected_line = format!("%message {token}");
    let owned = transcript
        .frames
        .iter()
        .flat_map(|frame| &frame.notifications)
        .filter(|line| *line == &expected_line)
        .count();
    let asynchronous = transcript
        .asynchronous_notifications
        .iter()
        .filter(|line| *line == &expected_line)
        .count();
    assert_eq!(
        owned, 0,
        "{expected_line:?} must not enter a command guard: {transcript:?}"
    );
    assert_eq!(
        asynchronous, 1,
        "{expected_line:?} must remain one asynchronous notification: {transcript:?}"
    );
}

/// Asserts that frame `index` of `transcript`, parsed from `rendered`, closed with `terminal`.
fn assert_frame_terminal(
    rendered: &str,
    transcript: &TestControlTranscript,
    index: usize,
    terminal: TestGuardTerminal,
) {
    assert_eq!(transcript.frames[index].terminal, terminal, "{rendered:?}");
}

/// Asserts that frame `index` of `transcript`, parsed from `rendered`, holds no notifications.
fn assert_frame_quiet(rendered: &str, transcript: &TestControlTranscript, index: usize) {
    assert!(
        transcript.frames[index].notifications.is_empty(),
        "{rendered:?}"
    );
}

/// Asserts that frame `index` of `transcript`, parsed from `rendered`, closed with an error
/// whose payload mentions `needle`.
fn assert_frame_error(
    rendered: &str,
    transcript: &TestControlTranscript,
    index: usize,
    needle: &str,
) {
    assert_frame_terminal(rendered, transcript, index, TestGuardTerminal::Error);
    assert!(
        transcript.frames[index]
            .payload
            .iter()
            .any(|line| line.contains(needle)),
        "{rendered:?}"
    );
}

/// Asserts that no `%message` of `transcript`, parsed from `rendered`, escaped its guard.
fn assert_no_asynchronous_messages(rendered: &str, transcript: &TestControlTranscript) {
    assert!(
        transcript
            .asynchronous_notifications
            .iter()
            .all(|line| !line.starts_with("%message ")),
        "{rendered:?}"
    );
}

fn parse_guard_lines(output: &str, prefix: &str) -> Vec<TestGuardTuple> {
    output
        .lines()
        .filter_map(|line| parse_guard_tuple(line, prefix))
        .collect()
}

fn parse_guard_tuple(line: &str, prefix: &str) -> Option<TestGuardTuple> {
    if !line.starts_with(prefix) {
        return None;
    }
    let rest = line.strip_prefix(prefix)?;
    let mut parts = rest.split_whitespace();
    let time_secs = parts.next()?.parse::<i64>().ok()?;
    let command_number = parts.next()?.parse::<u64>().ok()?;
    let flags = parts.next()?.parse::<u8>().ok()?;
    Some(TestGuardTuple {
        time_secs,
        command_number,
        flags,
    })
}
