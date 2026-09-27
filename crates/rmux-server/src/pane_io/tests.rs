use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rmux_core::command_parser::CommandParser;
use rmux_core::events::OutputCursorItem;
use rmux_core::{OptionStore, PaneGeometry, TerminalPassthrough};
use rmux_proto::{
    encode_attach_message, AttachFrameDecoder, AttachMessage, AttachShellCommand,
    AttachedKeystroke, BindKeyRequest, DisplayMessageExtRequest, KeyDispatched, KillSessionRequest,
    OptionName, PaneTarget, ScopeSelector, SessionName, SetOptionMode, Target, TerminalSize,
    WaitForMode, WaitForRequest, DEFAULT_MAX_FRAME_LENGTH,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use super::attach_output_batch::{collect_attach_output_batch, AttachOutputBatch};
use super::attach_transport::AttachTransport;
use super::control::{
    apply_pending_attach_controls, coalesce_render_switches, preserves_live_output,
    PendingAttachAction, PendingAttachInputState,
};
use super::exit_log::AttachExitReason;
use super::pending_escape::PendingEscapeFlush;
use super::wire::open_attach_target;
use super::wire::recv_pane_output;
use super::{
    clear_close_pane_output_after_refresh_if_target_changed, consume_predicted_echo,
    finish_pending_attach_exit_with_batch, forward_attach, install_live_attach_input_apply_pause,
    install_live_attach_input_validation_pause, is_predictable_local_echo, pane_output_channel,
    pane_output_channel_with_limits, pending_attach_exit_output_batch,
    predictable_local_echo_prefix_len, process_attach_data_payload, process_socket_messages,
    should_emit_overlay, sync_pending_escape_flush_with_escape_time, AttachControl,
    AttachControlSender, AttachTarget, LiveAttachInputContext, OverlayFrame, PredictedEcho,
};
use crate::daemon::ShutdownHandle;
use crate::handler::RequestHandler;
use crate::outer_terminal::{OuterTerminal, OuterTerminalContext};
use crate::renderer::PaneRenderDeltaFrame;
use crate::test_fixtures::Fixture;
use crate::test_names::session_name;

mod persistent_overlay;

/// A spawned `forward_attach` whose client end the test drives through `peer`.
struct AttachForwarder {
    peer: tokio::net::UnixStream,
    shutdown: watch::Sender<()>,
    closing: Arc<AtomicBool>,
    persistent_overlay_epoch: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl AttachForwarder {
    /// Forwards `target` over a fresh socket pair, reading its attach controls from `control_rx`.
    fn spawn(
        target: AttachTarget,
        control_rx: mpsc::UnboundedReceiver<AttachControl>,
        closing: Arc<AtomicBool>,
        live_input: LiveAttachInputContext,
        render_stream: bool,
    ) -> Self {
        let (stream, peer) = tokio::net::UnixStream::pair().expect("attach stream pair");
        let (shutdown, shutdown_rx) = watch::channel(());
        let persistent_overlay_epoch = Arc::new(AtomicU64::new(0));
        let task = tokio::spawn(forward_attach(
            stream,
            target,
            Vec::new(),
            shutdown_rx,
            control_rx,
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&closing),
            Arc::clone(&persistent_overlay_epoch),
            live_input,
            render_stream,
        ));
        Self {
            peer,
            shutdown,
            closing,
            persistent_overlay_epoch,
            task,
        }
    }

    /// Forwards `target` for a client no handler has registered, and answers with the sender of
    /// its attach controls.
    fn unregistered(target: AttachTarget) -> (Self, mpsc::UnboundedSender<AttachControl>) {
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let live_input = LiveAttachInputContext::unregistered_for_test(
            Arc::new(RequestHandler::new()),
            std::process::id(),
        );
        let forwarder = Self::spawn(target, control_rx, Arc::default(), live_input, false);
        (forwarder, control_tx)
    }

    /// Reads the initial repaint and asserts it renders `base`.
    async fn assert_initial_render(&mut self, base: &str) {
        let initial = read_attach_data_until(&mut self.peer, base.as_bytes()).await;
        assert!(
            String::from_utf8_lossy(&initial).contains(base),
            "initial attach should render the base pane"
        );
    }

    /// Requests shutdown and answers with how the forwarder exited.
    async fn stop(self) -> std::io::Result<()> {
        self.shutdown.send(()).expect("request attach shutdown");
        self.task.await.expect("attach task join")
    }

    /// Requests shutdown and asserts the forwarder exits cleanly.
    async fn assert_stops_healthy(self) {
        let result = self.stop().await;
        assert!(
            result.is_ok(),
            "forward_attach should stay healthy: {result:?}"
        );
    }
}

/// The socket-loop state `process_socket_messages` carries from one read to the next.
struct SocketInput {
    live_input: LiveAttachInputContext,
    stream: AttachTransport,
    decoder: AttachFrameDecoder,
    pending_input: Vec<u8>,
    pending_escape_flush: PendingEscapeFlush,
    active_emit_cache: Option<(u64, rmux_proto::WindowTarget)>,
    locked: bool,
}

impl SocketInput {
    /// Input from `live_input` over a fresh socket pair; answers with the client end too.
    fn new(live_input: LiveAttachInputContext, locked: bool) -> (Self, tokio::net::UnixStream) {
        let (stream, peer) = tokio::net::UnixStream::pair().expect("attach stream pair");
        let input = Self {
            live_input,
            stream: AttachTransport::from(stream),
            decoder: AttachFrameDecoder::new(),
            pending_input: Vec::new(),
            pending_escape_flush: PendingEscapeFlush::default(),
            active_emit_cache: None,
            locked,
        };
        (input, peer)
    }

    /// Queues `message` as if the client had written it.
    fn push(&mut self, message: &AttachMessage, label: &str) {
        self.decoder
            .push_bytes(&encode_attach_message(message).expect(label));
    }

    /// Runs the socket loop over the queued frames.
    async fn process(
        &mut self,
        current_target: Option<&mut super::types::OpenAttachTarget>,
    ) -> std::io::Result<bool> {
        process_socket_messages(
            &mut self.decoder,
            &self.stream,
            &self.live_input,
            current_target,
            PendingAttachInputState::new(&mut self.pending_input, &mut self.pending_escape_flush),
            &mut self.active_emit_cache,
            &mut self.locked,
        )
        .await
    }

    /// Re-arms the retained-input deadline for `escape_time` and answers with it.
    fn sync_escape_time(&mut self, escape_time: Duration) -> Option<Instant> {
        sync_pending_escape_flush_with_escape_time(
            &mut self.pending_escape_flush,
            &self.pending_input,
            escape_time,
        );
        self.pending_escape_flush.deadline()
    }
}

/// The attach-loop state `apply_pending_attach_controls` mutates, over a fresh socket pair.
struct PendingControls {
    stream: AttachTransport,
    peer: tokio::net::UnixStream,
    control_tx: mpsc::UnboundedSender<AttachControl>,
    control_rx: mpsc::UnboundedReceiver<AttachControl>,
    control_backlog: AtomicUsize,
    deferred: VecDeque<AttachControl>,
    target: super::types::OpenAttachTarget,
    render_generation: u64,
    overlay_generation: u64,
    persistent_overlay: Option<Vec<u8>>,
    persistent_overlay_visible: bool,
    persistent_overlay_state_id: Option<u64>,
    locked: bool,
}

impl PendingControls {
    /// Controls for `target`, opened without a render stream.
    fn new(target: AttachTarget) -> Self {
        Self::opened(open_attach_target(target, false).expect("open target"))
    }

    /// Controls for the already opened `target`.
    fn opened(target: super::types::OpenAttachTarget) -> Self {
        let (stream, peer) = tokio::net::UnixStream::pair().expect("attach stream pair");
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        Self {
            stream: AttachTransport::from(stream),
            peer,
            control_tx,
            control_rx,
            control_backlog: AtomicUsize::new(0),
            deferred: VecDeque::new(),
            persistent_overlay_state_id: target.persistent_overlay_state_id,
            target,
            render_generation: 0,
            overlay_generation: 0,
            persistent_overlay: None,
            persistent_overlay_visible: false,
            locked: false,
        }
    }

    /// Applies the queued controls and answers with what the attach loop does next.
    async fn apply(
        &mut self,
        pending_input: Option<PendingAttachInputState<'_>>,
    ) -> std::io::Result<PendingAttachAction> {
        apply_pending_attach_controls(
            &mut self.deferred,
            Some(&mut self.control_rx),
            &self.control_backlog,
            &mut self.target,
            &self.stream,
            &mut self.render_generation,
            &mut self.overlay_generation,
            &mut self.persistent_overlay,
            &mut self.persistent_overlay_visible,
            &mut self.persistent_overlay_state_id,
            &mut self.locked,
            pending_input,
        )
        .await
    }
}

async fn dispatch_live_attach_data_for_test(
    live_input: LiveAttachInputContext,
    bytes: &[u8],
) -> std::io::Result<bool> {
    dispatch_live_attach_message_for_test(live_input, AttachMessage::Data(bytes.to_vec())).await
}

async fn dispatch_live_attach_message_for_test(
    live_input: LiveAttachInputContext,
    message: AttachMessage,
) -> std::io::Result<bool> {
    let (mut input, _peer) = SocketInput::new(live_input, false);
    input.push(&message, "encode attach input");
    input.process(None).await
}

#[tokio::test]
async fn forward_attach_resize_during_command_prompt_keeps_exact_identity_alive() {
    let handler = Arc::new(RequestHandler::new());
    let attach_pid = std::process::id();
    let session_name = handler
        .create_session("resize-command-prompt-identity")
        .await;
    let control_rx = handler.attach_client(attach_pid, &session_name).await;
    let identity = handler.active_attach_identity_for_test(attach_pid).await;

    let prompt = CommandParser::new()
        .parse_one_group("command-prompt -b -p resize")
        .expect("command-prompt parses");
    handler
        .execute_parsed_commands_for_test(attach_pid, prompt)
        .await
        .expect("background prompt starts");

    let mut attach = AttachForwarder::spawn(
        test_attach_target(&session_name, b"BASE", None),
        control_rx,
        Arc::default(),
        LiveAttachInputContext::new(Arc::clone(&handler), identity),
        false,
    );
    let peer = &mut attach.peer;
    let _ = read_attach_data_until(peer, b"BASE").await;

    peer.write_all(
        &encode_attach_message(&AttachMessage::Resize(TerminalSize {
            cols: 100,
            rows: 30,
        }))
        .expect("encode resize"),
    )
    .await
    .expect("send resize");
    peer.write_all(
        &encode_attach_message(&AttachMessage::Keystroke(AttachedKeystroke::new(
            b"x".to_vec(),
        )))
        .expect("encode prompt key"),
    )
    .await
    .expect("send prompt key after resize");

    tokio::time::timeout(Duration::from_secs(2), async {
        let mut decoder = AttachFrameDecoder::new();
        let mut bytes = [0_u8; 4096];
        loop {
            let bytes_read = peer.read(&mut bytes).await.expect("read attach output");
            assert!(
                bytes_read > 0,
                "resize closed the attach before prompt input"
            );
            decoder.push_bytes(&bytes[..bytes_read]);
            while let Some(message) = decoder.next_message().expect("decode attach output") {
                if message == AttachMessage::KeyDispatched(KeyDispatched::new(1)) {
                    return;
                }
            }
        }
    })
    .await
    .expect("prompt key acknowledgement after resize");
    assert_eq!(
        handler.active_attach_identity(attach_pid).await,
        Some(identity),
        "resize must preserve the exact attach identity"
    );

    attach.stop().await.expect("attach exits cleanly");
}

async fn create_attach_input_test_session(handler: &RequestHandler, name: &str) -> PaneTarget {
    let target = PaneTarget::with_window(handler.create_session(name).await, 0, 0);
    handler.start_attached_input_capture_for_test(&target).await;
    target
}

#[tokio::test]
async fn same_pid_replacement_publishes_while_validated_old_input_is_paused() {
    let attach_pid = 910_031;

    // A has passed the socket-level validation but has not yet applied its
    // input. B must still publish promptly: no input or command await may hold
    // registration hostage. Once resumed, A must fail closed at the mutation
    // boundary and must not route through either session.
    let handler = Arc::new(RequestHandler::new());
    let alpha = create_attach_input_test_session(&handler, "identity-order-alpha").await;
    let beta = create_attach_input_test_session(&handler, "identity-order-beta").await;
    let mut alpha_control_rx = handler
        .attach_client(attach_pid, alpha.session_name())
        .await;
    let alpha_input =
        LiveAttachInputContext::current_for_test(Arc::clone(&handler), attach_pid).await;
    let pause = install_live_attach_input_apply_pause(alpha_input.identity);
    let input_task = tokio::spawn(dispatch_live_attach_data_for_test(alpha_input, b"A-WINS"));
    pause.reached.notified().await;

    let replacement_handler = Arc::clone(&handler);
    let beta_name = beta.session_name().clone();
    let (beta_control_tx, _beta_control_rx) = mpsc::unbounded_channel();
    let replacement_task = tokio::spawn(async move {
        replacement_handler
            .register_attach(attach_pid, beta_name, beta_control_tx)
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), replacement_task)
        .await
        .expect("same-PID replacement must not wait for paused old input")
        .expect("replacement task join");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), alpha_control_rx.recv())
            .await
            .expect("old attach must receive Detach promptly"),
        Some(AttachControl::Detach)
    ));

    pause.release.notify_one();
    assert!(
        input_task.await.expect("input task join").is_err(),
        "A input must fail closed after B publishes"
    );
    for pane in [&alpha, &beta] {
        assert_eq!(
            handler.attached_input_capture_for_test(pane).await,
            Some(Vec::new())
        );
    }

    // B already owns the PID before A's next frame reaches the socket loop.
    // The early stale check must reject it too.
    let handler = Arc::new(RequestHandler::new());
    let alpha = create_attach_input_test_session(&handler, "identity-stale-alpha").await;
    let beta = create_attach_input_test_session(&handler, "identity-stale-beta").await;
    let _alpha_control_rx = handler
        .attach_client(attach_pid, alpha.session_name())
        .await;
    let stale_alpha_input =
        LiveAttachInputContext::current_for_test(Arc::clone(&handler), attach_pid).await;
    let _beta_control_rx = handler.attach_client(attach_pid, beta.session_name()).await;

    let stale = dispatch_live_attach_data_for_test(stale_alpha_input, b"MUST-NOT-ROUTE").await;
    assert!(
        stale.is_err(),
        "old same-PID socket input must fail closed once B is published"
    );
    for pane in [&alpha, &beta] {
        assert_eq!(
            handler.attached_input_capture_for_test(pane).await,
            Some(Vec::new())
        );
    }
}

#[tokio::test]
async fn same_pid_replacement_publishes_while_old_binding_waits() {
    let handler = Arc::new(RequestHandler::new());
    let attach_pid = std::process::id();
    let channel = "attach-identity-blocked-binding";
    let alpha = create_attach_input_test_session(&handler, "identity-wait-alpha").await;
    let beta = create_attach_input_test_session(&handler, "identity-wait-beta").await;
    handler
        .handle_ok(BindKeyRequest {
            note: Some("identity-wait".to_owned()),
            ..Fixture::fixture((
                "identity-wait",
                "x",
                [format!("wait-for {channel} ; detach-client")],
            ))
        })
        .await;

    let mut alpha_control_rx = handler
        .attach_client(attach_pid, alpha.session_name())
        .await;
    handler
        .set_attached_key_table_for_test(attach_pid, Some("identity-wait".to_owned()))
        .await
        .expect("activate blocking test key table");
    while alpha_control_rx.try_recv().is_ok() {}
    let alpha_input =
        LiveAttachInputContext::current_for_test(Arc::clone(&handler), attach_pid).await;
    let input_task = tokio::spawn(dispatch_live_attach_message_for_test(
        alpha_input,
        AttachMessage::Keystroke(AttachedKeystroke::new(b"x".to_vec())),
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while handler.wait_for_counts(channel).0 != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("binding reaches wait-for before replacement");

    let mut beta_control_rx = tokio::time::timeout(
        Duration::from_secs(2),
        handler.attach_client(attach_pid, beta.session_name()),
    )
    .await
    .expect("replacement must not wait for the blocked binding");
    let beta_identity = handler.active_attach_identity_for_test(attach_pid).await;
    while beta_control_rx.try_recv().is_ok() {}
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match alpha_control_rx.recv().await {
                Some(AttachControl::Detach) => break,
                Some(_) => continue,
                None => panic!("old attach control channel closed before Detach"),
            }
        }
    })
    .await
    .expect("old attach receives Detach");

    handler
        .handle_ok(WaitForRequest::fixture((channel, WaitForMode::Signal)))
        .await;
    let _old_input_result = tokio::time::timeout(Duration::from_secs(2), input_task)
        .await
        .expect("old binding unwinds after signal")
        .expect("input task join");
    assert!(
        handler.current_live_attach_input(beta_identity).await,
        "the old queued binding must not detach the same-PID replacement"
    );
    while let Ok(control) = beta_control_rx.try_recv() {
        assert!(
            !matches!(
                control,
                AttachControl::Detach
                    | AttachControl::DetachKill
                    | AttachControl::DetachExecShellCommand(_)
            ),
            "the old queued binding sent a detach control to its replacement"
        );
    }
    for pane in [&alpha, &beta] {
        assert_eq!(
            handler.attached_input_capture_for_test(pane).await,
            Some(Vec::new())
        );
    }
}

#[tokio::test]
async fn unlock_flushes_resume_output_before_following_blocking_keystroke() {
    let handler = Arc::new(RequestHandler::new());
    let attach_pid = std::process::id();
    let channel = "attach-unlock-output-barrier";
    let session_name = handler.create_session("unlock-output-barrier").await;
    handler
        .handle_ok(BindKeyRequest {
            note: Some("unlock output barrier".to_owned()),
            ..Fixture::fixture((
                "unlock-output-barrier",
                "x",
                [format!("wait-for {channel}")],
            ))
        })
        .await;

    let _control_rx = handler.attach_client(attach_pid, &session_name).await;
    handler
        .set_attached_key_table_for_test(attach_pid, Some("unlock-output-barrier".to_owned()))
        .await
        .expect("activate blocking test key table");

    let live_input =
        LiveAttachInputContext::current_for_test(Arc::clone(&handler), attach_pid).await;
    let (mut input, mut peer) = SocketInput::new(live_input, true);
    input.push(&AttachMessage::Unlock, "encode unlock");
    input.push(
        &AttachMessage::Keystroke(AttachedKeystroke::new(b"x".to_vec())),
        "encode blocking keystroke",
    );
    let mut current_target =
        open_attach_target(test_attach_target(&session_name, b"RESUMED", None), false)
            .expect("open attach target");

    let input_task = tokio::spawn(async move {
        input.process(Some(&mut current_target)).await?;
        input.process(Some(&mut current_target)).await
    });

    tokio::time::timeout(Duration::from_secs(2), async {
        while handler.wait_for_counts(channel).0 != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("following binding reaches wait-for");
    assert!(
        !input_task.is_finished(),
        "the following keystroke must still be waiting"
    );
    let resume = read_attach_data_until(&mut peer, b"RESUMED").await;
    assert!(
        contains_bytes(&resume, b"RESUMED"),
        "unlock must restore the terminal before the following binding completes"
    );

    handler
        .handle_ok(WaitForRequest::fixture((channel, WaitForMode::Signal)))
        .await;
    input_task
        .await
        .expect("input task join")
        .expect("coalesced input processing succeeds");
}

#[test]
fn pending_escape_wrapper_covers_apc_csi_paste_and_excludes_utf8() {
    let mut flush = PendingEscapeFlush::default();
    let escape_time = Duration::from_millis(5);
    let before_apc = Instant::now();

    sync_pending_escape_flush_with_escape_time(&mut flush, b"\x1b_Gi=7;payload", escape_time);
    assert!(
        flush
            .deadline()
            .is_some_and(|deadline| deadline > before_apc + Duration::from_secs(1)),
        "the production wrapper must give Kitty APC a stream idle budget, not escape-time"
    );

    flush.clear();
    sync_pending_escape_flush_with_escape_time(&mut flush, b"\x1b[12", escape_time);
    assert!(
        flush.deadline().is_some(),
        "numeric CSI retention must stay timed"
    );

    flush.clear();
    let before_paste = Instant::now();
    sync_pending_escape_flush_with_escape_time(&mut flush, b"\x1b[200~body", escape_time);
    assert!(
        flush
            .deadline()
            .is_some_and(|deadline| deadline > before_paste + Duration::from_secs(1)),
        "streaming bracketed paste must use the long stream idle budget"
    );

    sync_pending_escape_flush_with_escape_time(&mut flush, b"\xe6\x97", escape_time);
    assert!(
        flush.deadline().is_none(),
        "partial UTF-8 must never inherit the escape deadline"
    );
}

#[test]
fn pending_escape_wrapper_resets_stream_deadline_for_new_keyboard_suffix() {
    let mut flush = PendingEscapeFlush::default();
    let escape_time = Duration::from_millis(5);

    sync_pending_escape_flush_with_escape_time(&mut flush, b"\x1b_Gpayload", escape_time);
    let stream_deadline = flush.deadline().expect("APC stream arms");
    let before_escape = Instant::now();
    sync_pending_escape_flush_with_escape_time(&mut flush, b"\x1b", escape_time);
    let escape_deadline = flush.deadline().expect("post-stream Escape arms");

    assert!(escape_deadline >= before_escape + escape_time);
    assert!(
        escape_deadline < stream_deadline,
        "a consumed stream followed by Escape must not inherit its long idle deadline"
    );
    sync_pending_escape_flush_with_escape_time(&mut flush, b"\x1b[12", Duration::from_secs(1));
    assert_eq!(
        flush.deadline(),
        Some(escape_deadline),
        "numeric CSI growth keeps the first keyboard ambiguity deadline"
    );
}

#[test]
fn pending_escape_wrapper_times_only_unterminated_overlong_mouse_input() {
    let mut flush = PendingEscapeFlush::default();
    let escape_time = Duration::from_millis(5);

    sync_pending_escape_flush_with_escape_time(&mut flush, b"\x1b[<700000", escape_time);
    assert!(
        flush.deadline().is_some(),
        "an unterminated overflowing decimal remains bounded by escape-time"
    );

    sync_pending_escape_flush_with_escape_time(&mut flush, b"\x1b[<700000;1;1M", escape_time);
    assert!(
        flush.deadline().is_none(),
        "a lexically complete invalid mouse frame must leave the retained-input grammar"
    );
}

async fn pending_escape_socket_fixture(
    session: &str,
) -> (
    SocketInput,
    tokio::net::UnixStream,
    mpsc::UnboundedReceiver<AttachControl>,
) {
    let handler = Arc::new(RequestHandler::new());
    let attach_pid = std::process::id();
    let session_name = handler.create_session(session).await;
    let target = PaneTarget::with_window(session_name.clone(), 0, 0);
    let control_rx = handler.attach_client(attach_pid, session_name).await;
    handler.start_attached_input_capture_for_test(&target).await;
    let live_input = LiveAttachInputContext::current_for_test(handler, attach_pid).await;
    let (input, peer) = SocketInput::new(live_input, false);
    (input, peer, control_rx)
}

struct PendingEscapeSchedulerFixture {
    handler: Arc<RequestHandler>,
    target: PaneTarget,
    attach: AttachForwarder,
}

async fn current_attach_target(
    handler: &RequestHandler,
    attach_pid: u32,
    session_name: &SessionName,
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
) -> AttachTarget {
    handler
        .refresh_attached_client(attach_pid, session_name)
        .await;
    let target = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match control_rx.recv().await {
                Some(AttachControl::Switch(target)) => break *target.into_target(),
                Some(_) => continue,
                None => panic!("attach control channel closed before initial target"),
            }
        }
    })
    .await
    .expect("timed out building the initial attach target");
    handler
        .clear_attached_render_refresh_pending(attach_pid)
        .await;
    target
}

impl PendingEscapeSchedulerFixture {
    async fn start(session: &str) -> Self {
        let handler = Arc::new(RequestHandler::new());
        let attach_pid = std::process::id();
        let session_name = handler.create_started_session(session).await;
        let target = PaneTarget::with_window(session_name.clone(), 0, 0);
        handler
            .set_option(ScopeSelector::Global, OptionName::EscapeTime, "500")
            .await;

        let mut control_rx = handler.attach_client(attach_pid, &session_name).await;
        handler.start_attached_input_capture_for_test(&target).await;
        let initial_target =
            current_attach_target(&handler, attach_pid, &session_name, &mut control_rx).await;
        let live_input =
            LiveAttachInputContext::current_for_test(Arc::clone(&handler), attach_pid).await;
        let attach = AttachForwarder::spawn(
            initial_target,
            control_rx,
            Arc::default(),
            live_input,
            false,
        );

        Self {
            handler,
            target,
            attach,
        }
    }

    async fn send(&mut self, message: AttachMessage) {
        self.send_batch(&[message]).await;
    }

    async fn send_batch(&mut self, messages: &[AttachMessage]) {
        let mut encoded = Vec::new();
        for message in messages {
            encoded
                .extend_from_slice(&encode_attach_message(message).expect("encode attach input"));
        }
        self.attach
            .peer
            .write_all(&encoded)
            .await
            .expect("write attach input");
    }

    async fn activate_prompt(&self) {
        let prompt = CommandParser::new()
            .parse_one_group("command-prompt -b -p retained-control")
            .expect("command prompt parses");
        self.handler
            .execute_parsed_commands_for_test(std::process::id(), prompt)
            .await
            .expect("background prompt starts");
    }

    async fn wait_for_capture(&self, matches: impl Fn(&[u8]) -> bool, label: &str) -> Vec<u8> {
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let captured = self
                    .handler
                    .attached_input_capture_for_test(&self.target)
                    .await
                    .expect("input capture remains installed");
                if matches(&captured) {
                    break captured;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        match result {
            Ok(captured) => captured,
            Err(_) => {
                let captured = self
                    .handler
                    .attached_input_capture_for_test(&self.target)
                    .await;
                panic!(
                    "timed out waiting for {label}; capture={captured:?}, attach_finished={}",
                    self.attach.task.is_finished()
                );
            }
        }
    }

    async fn finish(self) {
        assert!(self.attach.stop().await.is_ok());
    }

    async fn detach(mut self) {
        self.send(AttachMessage::Data(b"\x02d".to_vec())).await;
        let result = tokio::time::timeout(Duration::from_secs(2), &mut self.attach.task)
            .await
            .expect("prefix-d must detach after retained input expires")
            .expect("attach task join");
        assert!(result.is_ok(), "attach exits cleanly after prefix-d");
    }
}

async fn arm_ignored_display_message(fixture: &PendingEscapeSchedulerFixture, duration_ms: u32) {
    fixture
        .handler
        .handle_ok(DisplayMessageExtRequest {
            target: Some(Target::Pane(fixture.target.clone())),
            target_client: Some(std::process::id().to_string()),
            duration_ms: Some(rmux_proto::DisplayMessageDurationMillis::new(duration_ms)),
            ignore_input: true,
            ..Fixture::fixture("ignore input")
        })
        .await;
}

#[tokio::test]
async fn ignored_display_message_expiry_flushes_a_lone_retained_csi() {
    let mut fixture = PendingEscapeSchedulerFixture::start("display-ignore-expiry-lone-csi").await;
    arm_ignored_display_message(&fixture, 40).await;
    fixture.send(AttachMessage::Data(b"\x1b[".to_vec())).await;

    let captured = fixture
        .wait_for_capture(
            |captured| captured == b"\x1b[",
            "ignored CSI after message and escape expiry",
        )
        .await;
    assert_eq!(captured, b"\x1b[");
    fixture.finish().await;
}

#[tokio::test]
async fn ignored_display_message_keeps_csi_contiguous_across_expiry() {
    let mut fixture =
        PendingEscapeSchedulerFixture::start("display-ignore-expiry-completed-csi").await;
    arm_ignored_display_message(&fixture, 40).await;
    fixture.send(AttachMessage::Data(b"\x1b[".to_vec())).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    fixture.send(AttachMessage::Data(b"A".to_vec())).await;

    let captured = fixture
        .wait_for_capture(
            |captured| matches!(captured, b"\x1b[A" | b"\x1bOA"),
            "ignored CSI completed after message expiry",
        )
        .await;
    assert!(
        matches!(captured.as_slice(), b"\x1b[A" | b"\x1bOA"),
        "the split Up key must remain one contiguous key in either cursor-key mode"
    );
    fixture.finish().await;
}

#[tokio::test]
async fn modal_dcs_leader_expires_and_restores_prefix_detach() {
    let mut fixture = PendingEscapeSchedulerFixture::start("modal-dcs-deadline").await;
    fixture.activate_prompt().await;
    fixture.send(AttachMessage::Data(b"\x1bP".to_vec())).await;

    // ESC P is both Meta-P and the 7-bit DCS leader. It must use the
    // configured keyboard escape-time rather than stay opaque forever.
    fixture
        .wait_for_capture(
            |captured| captured == b"P",
            "DCS remainder after prompt cancellation",
        )
        .await;
    fixture.detach().await;
}

#[tokio::test]
async fn modal_c1_dcs_leader_expires_and_releases_later_input() {
    let mut fixture = PendingEscapeSchedulerFixture::start("modal-c1-dcs-deadline").await;
    fixture.activate_prompt().await;
    fixture
        .send(AttachMessage::Keystroke(AttachedKeystroke::new(vec![0x90])))
        .await;

    // A lone C1 DCS leader is not a Meta key, but it is still incomplete and
    // must be bounded. Once flushed, a later Escape can reach and close the
    // prompt instead of being absorbed into the abandoned DCS body.
    fixture
        .wait_for_capture(|captured| captured == [0x90], "timed-out C1 DCS leader")
        .await;
    fixture.send(AttachMessage::Data(b"\x1b".to_vec())).await;
    tokio::time::sleep(Duration::from_millis(750)).await;
    fixture.detach().await;
}

async fn assert_fragmented_meta_control_promotes_to_streaming_deadline(
    session: &str,
    prefix: AttachMessage,
    recognized_opener: AttachMessage,
    completion: AttachMessage,
    expected: &[u8],
) {
    let mut fixture = PendingEscapeSchedulerFixture::start(session).await;
    fixture.send(prefix).await;
    fixture
        .wait_for_capture(|captured| captured == b"A", "prefix dispatch")
        .await;
    fixture.send(recognized_opener).await;

    // The configured escape-time is 500 ms. Once the second fragment selects
    // a recognized OSC/APC family, it must survive beyond that keyboard budget.
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_eq!(
        fixture
            .handler
            .attached_input_capture_for_test(&fixture.target)
            .await,
        Some(b"A".to_vec()),
        "a transport split after ESC must not flush a recognized control body"
    );

    fixture.send(completion).await;
    fixture
        .wait_for_capture(
            |captured| captured == expected,
            "fragmented Meta control completion",
        )
        .await;
    fixture.finish().await;
}

#[tokio::test]
async fn unix_data_osc_split_after_escape_promotes_to_streaming_deadline() {
    assert_fragmented_meta_control_promotes_to_streaming_deadline(
        "unix-data-split-osc-streaming-deadline",
        AttachMessage::Data(b"A\x1b".to_vec()),
        AttachMessage::Data(b"]52;c;UNIX_OSC".to_vec()),
        AttachMessage::Data(b"\x07Z".to_vec()),
        b"AZ",
    )
    .await;
}

#[tokio::test]
async fn windows_keystroke_apc_split_after_escape_promotes_to_streaming_deadline() {
    assert_fragmented_meta_control_promotes_to_streaming_deadline(
        "windows-keystroke-split-apc-streaming-deadline",
        AttachMessage::Keystroke(AttachedKeystroke::new(b"A\x1b".to_vec())),
        AttachMessage::Keystroke(AttachedKeystroke::new(b"_Gi=7;WINDOWS_APC".to_vec())),
        AttachMessage::Keystroke(AttachedKeystroke::new(b"_BODY\x1b\\Z".to_vec())),
        b"A\x1b_Gi=7;WINDOWS_APC_BODY\x1b\\Z",
    )
    .await;
}

async fn assert_invalid_meta_byte_flushes_on_keyboard_deadline(
    session: &str,
    input: AttachMessage,
) {
    let mut fixture = PendingEscapeSchedulerFixture::start(session).await;
    let started = Instant::now();
    fixture.send(input).await;
    let captured = fixture
        .wait_for_capture(
            |captured| captured == b"A\x1b\xff",
            "invalid Meta byte keyboard-deadline flush",
        )
        .await;
    assert_eq!(captured, b"A\x1b\xff");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "invalid Meta input must use escape-time, not the 8-second stream budget"
    );

    fixture.send(AttachMessage::Data(b"Z".to_vec())).await;
    fixture
        .wait_for_capture(
            |captured| captured == b"A\x1b\xffZ",
            "ordinary input after invalid Meta flush",
        )
        .await;
    fixture.finish().await;
}

#[tokio::test]
async fn invalid_meta_byte_deadline_covers_data_and_windows_keystroke_frames() {
    assert_invalid_meta_byte_flushes_on_keyboard_deadline(
        "invalid-meta-data-deadline",
        AttachMessage::Data(b"A\x1b\xff".to_vec()),
    )
    .await;
    assert_invalid_meta_byte_flushes_on_keyboard_deadline(
        "invalid-meta-keystroke-deadline",
        AttachMessage::Keystroke(AttachedKeystroke::new(b"A\x1b\xff".to_vec())),
    )
    .await;
}

async fn assert_invalid_csi_body_is_forwarded_without_retention(
    session: &str,
    input: AttachMessage,
) {
    let mut fixture = PendingEscapeSchedulerFixture::start(session).await;
    fixture.send(input).await;
    let captured = fixture
        .wait_for_capture(
            |captured| captured == b"A\x1b[1\r",
            "invalid CSI body forwarding",
        )
        .await;
    assert_eq!(captured, b"A\x1b[1\r");
    fixture.finish().await;
}

#[tokio::test]
async fn invalid_csi_body_cannot_be_retained_without_a_deadline() {
    assert_invalid_csi_body_is_forwarded_without_retention(
        "invalid-csi-body-data",
        AttachMessage::Data(b"A\x1b[1\r".to_vec()),
    )
    .await;
    assert_invalid_csi_body_is_forwarded_without_retention(
        "invalid-csi-body-keystroke",
        AttachMessage::Keystroke(AttachedKeystroke::new(b"A\x1b[1\r".to_vec())),
    )
    .await;
}

#[tokio::test]
async fn sustained_ready_socket_serves_the_initial_ambiguous_csi_deadline() {
    let mut fixture = PendingEscapeSchedulerFixture::start("sustained-ready-csi-deadline").await;
    fixture
        .send(AttachMessage::Data(b"A\x1b[12;".to_vec()))
        .await;
    fixture
        .wait_for_capture(|captured| captured == b"A", "ambiguous CSI retention")
        .await;

    // Keep producing immediately readable frames for longer than escape-time.
    // The attach loop must still service the original ambiguity deadline.
    let empty = AttachMessage::Data(Vec::new());
    let started = Instant::now();
    let captured = tokio::time::timeout(Duration::from_millis(900), async {
        loop {
            fixture
                .send_batch(&[
                    empty.clone(),
                    empty.clone(),
                    empty.clone(),
                    empty.clone(),
                    empty.clone(),
                    empty.clone(),
                    empty.clone(),
                    empty.clone(),
                ])
                .await;
            tokio::task::yield_now().await;
            let captured = fixture
                .handler
                .attached_input_capture_for_test(&fixture.target)
                .await
                .expect("input capture remains installed");
            if captured == b"A\x1b[12;" {
                break captured;
            }
        }
    })
    .await
    .expect("continuously ready socket must not starve the CSI deadline");
    assert_eq!(captured, b"A\x1b[12;");
    assert!(started.elapsed() < Duration::from_millis(900));
    fixture.finish().await;
}

async fn assert_streaming_control_survives_keyboard_escape_time(
    session: &str,
    prefix: &[u8],
    completion: &[u8],
) -> Vec<u8> {
    let mut fixture = PendingEscapeSchedulerFixture::start(session).await;
    fixture.send(AttachMessage::Data(prefix.to_vec())).await;
    fixture
        .wait_for_capture(|captured| captured == b"A", "stream prefix dispatch")
        .await;

    tokio::time::sleep(Duration::from_millis(750)).await;
    let before_completion = fixture
        .handler
        .attached_input_capture_for_test(&fixture.target)
        .await;
    assert_eq!(
        before_completion,
        Some(b"A".to_vec()),
        "an unambiguous streaming control must outlive keyboard escape-time"
    );

    fixture.send(AttachMessage::Data(completion.to_vec())).await;
    let captured = fixture
        .wait_for_capture(|captured| captured.ends_with(b"Z"), "stream completion")
        .await;
    fixture.finish().await;
    captured
}

#[tokio::test]
async fn fragmented_osc_control_keeps_the_streaming_idle_deadline() {
    let captured = assert_streaming_control_survives_keyboard_escape_time(
        "fragmented-osc-stream-deadline",
        b"A\x1b]52;c;AA",
        b"AA\x07Z",
    )
    .await;
    assert_eq!(captured, b"AZ");
}

#[tokio::test]
async fn fragmented_apc_control_keeps_the_streaming_idle_deadline() {
    let captured = assert_streaming_control_survives_keyboard_escape_time(
        "fragmented-apc-stream-deadline",
        b"A\x1b_Gi=7;PAY",
        b"LOAD\x1b\\Z",
    )
    .await;
    assert_eq!(captured, b"A\x1b_Gi=7;PAYLOAD\x1b\\Z");
}

#[tokio::test]
async fn socket_dispatch_rearms_replaced_same_kind_ambiguous_suffix() {
    let (mut input, _peer, _control_rx) =
        pending_escape_socket_fixture("escape-epoch-ambiguous").await;

    input.push(
        &AttachMessage::Data(b"\x1b".to_vec()),
        "encode initial Escape",
    );
    input.process(None).await.expect("retain initial Escape");
    assert_eq!(input.pending_input, b"\x1b");
    let first_deadline = input
        .sync_escape_time(Duration::from_secs(1))
        .expect("initial Escape arms a deadline");

    input.push(
        &AttachMessage::Data(b"x\x1b".to_vec()),
        "encode replacement Escape",
    );
    input
        .process(None)
        .await
        .expect("consume Meta-x and retain replacement Escape");
    assert_eq!(input.pending_input, b"\x1b");
    let replacement_deadline = input
        .sync_escape_time(Duration::from_secs(3))
        .expect("replacement Escape arms a fresh deadline");

    assert!(
        replacement_deadline > first_deadline + Duration::from_secs(1),
        "a same-kind suffix must not inherit the consumed prefix's deadline"
    );
}

#[tokio::test]
async fn socket_dispatch_promotes_coalesced_split_osc_to_streaming() {
    let (mut input, _peer, _control_rx) =
        pending_escape_socket_fixture("escape-meta-osc-provenance").await;
    input.push(
        &AttachMessage::Data(b"A\x1b".to_vec()),
        "encode initial Meta escape",
    );
    input.push(
        &AttachMessage::Data(b"]52;c;COALESCED".to_vec()),
        "encode OSC-like continuation",
    );

    input
        .process(None)
        .await
        .expect("retain coalesced OSC-like Meta input");
    assert_eq!(input.pending_input, b"\x1b]52;c;COALESCED");

    let before = Instant::now();
    let deadline = input
        .sync_escape_time(Duration::from_millis(500))
        .expect("coalesced split OSC input must arm");
    assert!(
        deadline >= before + Duration::from_secs(8),
        "a recognized OSC opener must promote beyond the initial Meta ambiguity"
    );
}

#[tokio::test]
async fn socket_dispatch_preserves_deadline_for_true_csi_continuation() {
    let (mut input, _peer, _control_rx) =
        pending_escape_socket_fixture("escape-epoch-continuation").await;

    input.push(
        &AttachMessage::Data(b"\x1b[".to_vec()),
        "encode initial CSI opener",
    );
    input
        .process(None)
        .await
        .expect("retain initial CSI opener");
    assert_eq!(input.pending_input, b"\x1b[");
    let original_deadline = input
        .sync_escape_time(Duration::from_secs(1))
        .expect("initial CSI opener arms a deadline");

    input.push(
        &AttachMessage::Data(b"12".to_vec()),
        "encode continued CSI parameters",
    );
    input
        .process(None)
        .await
        .expect("retain continued CSI parameters");
    assert_eq!(input.pending_input, b"\x1b[12");

    assert_eq!(
        input.sync_escape_time(Duration::from_secs(30)),
        Some(original_deadline),
        "a true continuation must not turn keyboard escape-time into a sliding deadline"
    );
}

#[tokio::test]
async fn socket_dispatch_rearms_replaced_same_length_streaming_suffix() {
    let (mut input, _peer, _control_rx) =
        pending_escape_socket_fixture("escape-epoch-streaming").await;
    let incomplete_paste = b"\x1b[200~body";

    input.push(
        &AttachMessage::Data(incomplete_paste.to_vec()),
        "encode initial incomplete paste",
    );
    input
        .process(None)
        .await
        .expect("retain initial incomplete paste");
    assert_eq!(input.pending_input, incomplete_paste);
    let first_deadline = input
        .sync_escape_time(Duration::from_secs(8))
        .expect("initial paste stream arms a deadline");

    let mut replacement = b"\x1b[201~".to_vec();
    replacement.extend_from_slice(incomplete_paste);
    input.push(
        &AttachMessage::Data(replacement),
        "encode completed and replacement paste streams",
    );
    input
        .process(None)
        .await
        .expect("complete first paste and retain replacement stream");
    assert_eq!(
        input.pending_input, incomplete_paste,
        "the replacement intentionally matches the old kind, length, and contents"
    );
    let replacement_deadline = input
        .sync_escape_time(Duration::from_secs(30))
        .expect("replacement paste stream arms a fresh deadline");

    assert!(
        replacement_deadline > first_deadline + Duration::from_secs(20),
        "a same-length streaming suffix must not inherit the completed stream's deadline"
    );
}

#[test]
fn overlay_generation_rejects_stale_clears_after_switches_or_newer_overlays() {
    let mut current_overlay_generation = 0;

    // (render generation, overlay render generation, overlay generation, emitted, and the
    // current overlay generation afterwards where the sequence pins it)
    for (render, overlay_render, overlay, emitted, pinned) in [
        (0, 0, 1, true, Some(1)),
        (0, 0, 1, true, None),
        (0, 0, 2, true, None),
        (0, 0, 1, false, None),
        (1, 0, 3, false, Some(2)),
        (1, 2, 3, true, Some(3)),
        (2, 1, 4, false, None),
    ] {
        let frame = OverlayFrame::new(Vec::new(), overlay_render, overlay);
        assert_eq!(
            should_emit_overlay(render, &mut current_overlay_generation, &frame),
            emitted
        );
        if let Some(pinned) = pinned {
            assert_eq!(current_overlay_generation, pinned);
        }
    }
}

#[test]
fn target_change_clears_deferred_pane_output_close() {
    let mut close_after_refresh = true;

    clear_close_pane_output_after_refresh_if_target_changed(true, &mut close_after_refresh);

    assert!(
        !close_after_refresh,
        "a deferred close belongs to the old attach target and must not apply after a switch"
    );
}

#[test]
fn same_target_keeps_deferred_pane_output_close() {
    let mut close_after_refresh = true;

    clear_close_pane_output_after_refresh_if_target_changed(false, &mut close_after_refresh);

    assert!(close_after_refresh);
}

#[test]
fn predicted_local_echo_accepts_only_single_printable_bytes() {
    assert!(is_predictable_local_echo(b"a"));
    assert!(is_predictable_local_echo(b"abc123"));
    assert!(is_predictable_local_echo(b" "));
    assert!(is_predictable_local_echo(b"~"));
    assert!(!is_predictable_local_echo(b"\n"));
    assert!(!is_predictable_local_echo(b"\x1b"));
    assert!(!is_predictable_local_echo(b"0123456789abcdefg"));
    assert!(!is_predictable_local_echo("é".as_bytes()));
}

#[test]
fn predicted_local_echo_accepts_printable_prefix_before_enter() {
    assert_eq!(predictable_local_echo_prefix_len(b"PING123\r"), 7);
    assert_eq!(predictable_local_echo_prefix_len(b"PING123\n"), 7);
    assert_eq!(predictable_local_echo_prefix_len(b"PING123\t"), 0);
    assert_eq!(predictable_local_echo_prefix_len(b"\r"), 0);
}

#[test]
fn predicted_local_echo_consumes_exact_pty_echo_once() {
    let alpha = session_name("alpha");
    let mut target =
        open_attach_target(test_attach_target(&alpha, b"", None), false).expect("open target");

    for (predicted, echoed, expected) in [
        (
            b"xyz".as_slice(),
            b"xyz".as_slice(),
            PredictedEcho::Consumed,
        ),
        (b"x".as_slice(), b"y".as_slice(), PredictedEcho::Mismatch),
        (b"x".as_slice(), b"xy".as_slice(), PredictedEcho::Mismatch),
    ] {
        target.predicted_echo.extend(predicted);
        assert_eq!(consume_predicted_echo(&mut target, echoed), expected);
        assert!(target.predicted_echo.is_empty());
    }
}

#[test]
fn stale_predicted_local_echo_expires_without_pty_echo() {
    let alpha = session_name("alpha");
    let mut target =
        open_attach_target(test_attach_target(&alpha, b"", None), false).expect("open target");

    target.predicted_echo.extend(b"secret");
    target.predicted_echo_started_at =
        Some(Instant::now() - super::PREDICTED_LOCAL_ECHO_TIMEOUT * 2);

    assert_eq!(
        consume_predicted_echo(&mut target, b"visible"),
        PredictedEcho::NoPrediction
    );
    assert!(target.predicted_echo.is_empty());
    assert!(target.predicted_echo_started_at.is_none());
}

/// Emits `payload` as a live render frame to a render-capable client over a duplex of
/// `capacity` bytes, and answers with both ends.
async fn emit_live_render_payload(
    payload: Vec<u8>,
    replaceable: bool,
    capacity: usize,
    label: &str,
) -> (AttachTransport, tokio::io::DuplexStream) {
    let target = test_attach_target(&session_name("alpha"), b"", None);
    let mut target = open_attach_target(target, true).expect("open attach target");
    let frame = PaneRenderDeltaFrame::new(payload, None);
    let (stream, peer) = tokio::io::duplex(capacity);
    let stream = AttachTransport::from_io(stream);

    super::emit_live_render_frame(&stream, &mut target, &frame, replaceable)
        .await
        .expect(label);
    (stream, peer)
}

/// The first message a render-capable client reads once `payload` is emitted as a live render
/// frame.
async fn first_live_render_message(payload: &[u8], replaceable: bool) -> Option<AttachMessage> {
    let (_stream, mut peer) = emit_live_render_payload(
        payload.to_vec(),
        replaceable,
        1024,
        "emit live render frame",
    )
    .await;
    let mut bytes = [0_u8; 128];
    let count = peer.read(&mut bytes).await.expect("read emitted frame");
    let mut decoder = AttachFrameDecoder::new();
    decoder.push_bytes(&bytes[..count]);
    decoder.next_message().expect("decode emitted frame")
}

#[tokio::test]
async fn live_render_frame_uses_render_message_for_capable_clients() {
    assert!(matches!(
        first_live_render_message(b"live", true).await,
        Some(AttachMessage::Render(bytes)) if bytes.ends_with(b"live")
    ));
}

#[tokio::test]
async fn live_render_delta_uses_data_message_for_stateful_frames() {
    assert!(matches!(
        first_live_render_message(b"delta", false).await,
        Some(AttachMessage::Data(bytes)) if bytes.ends_with(b"delta")
    ));
}

#[tokio::test]
async fn live_replaceable_repaint_above_payload_ceiling_uses_ordered_data_fragments() {
    let (_stream, mut peer) = emit_live_render_payload(
        vec![b'x'; DEFAULT_MAX_FRAME_LENGTH + 1],
        true,
        DEFAULT_MAX_FRAME_LENGTH + 64,
        "emit oversized live repaint",
    )
    .await;

    let mut decoder = AttachFrameDecoder::new();
    let mut messages = Vec::new();
    let mut bytes = [0_u8; 8192];
    while messages.len() < 2 {
        let count = peer.read(&mut bytes).await.expect("read emitted frame");
        assert!(count > 0, "attach stream closed before both fragments");
        decoder.push_bytes(&bytes[..count]);
        while let Some(message) = decoder.next_message().expect("decode emitted frame") {
            messages.push(message);
        }
    }

    assert_eq!(
        messages,
        vec![
            AttachMessage::Data(vec![b'x'; DEFAULT_MAX_FRAME_LENGTH]),
            AttachMessage::Data(vec![b'x']),
        ]
    );
}

#[tokio::test]
async fn pane_output_receiver_reports_lag_and_resumes_from_oldest_retained_event() {
    let sender = pane_output_channel_with_limits(1, 32);
    let mut receiver = sender.subscribe();

    sender.send(b"first".to_vec());
    sender.send(b"second".to_vec());

    let OutputCursorItem::Gap(gap) = recv_pane_output(&mut receiver)
        .await
        .expect("receive explicit output gap")
    else {
        panic!("slow receiver should observe a cursor gap");
    };
    assert_eq!(gap.expected_sequence(), 0);
    assert_eq!(gap.resume_sequence(), 1);
    assert_eq!(gap.missed_events(), 1);
    assert_eq!(gap.missed_range(), 0..1);
    assert_eq!(gap.recent_snapshot().bytes(), b"firstsecond");
    assert_eq!(gap.recent_snapshot().oldest_sequence(), Some(0));
    assert_eq!(gap.recent_snapshot().newest_sequence(), Some(1));

    let OutputCursorItem::Event(event) = recv_pane_output(&mut receiver)
        .await
        .expect("receive oldest retained output event")
    else {
        panic!("receiver should resume with the oldest retained event");
    };
    assert_eq!(event.sequence(), 1);
    assert_eq!(event.bytes(), b"second");
}

/// Sends `keystroke` through the socket loop of a fresh attach to session alpha and answers
/// with the first message the client reads back into `ack_bytes` within a second.
async fn first_keystroke_reply(
    keystroke: &[u8],
    locked: bool,
    [encode_label, process_label]: [&str; 2],
    ack_bytes: &mut [u8],
) -> Option<AttachMessage> {
    let handler = Arc::new(RequestHandler::new());
    let attach_pid = std::process::id();
    let session_name = handler.create_session("alpha").await;
    let _control_rx = handler.attach_client(attach_pid, session_name).await;

    let live_input = LiveAttachInputContext::current_for_test(handler, attach_pid).await;
    let (mut input, mut peer) = SocketInput::new(live_input, locked);
    input.push(
        &AttachMessage::Keystroke(AttachedKeystroke::new(keystroke.to_vec())),
        encode_label,
    );
    input.process(None).await.expect(process_label);

    let bytes_read = tokio::time::timeout(Duration::from_secs(1), peer.read(ack_bytes))
        .await
        .expect("ack read should not time out")
        .expect("read ack");
    let mut ack_decoder = AttachFrameDecoder::new();
    ack_decoder.push_bytes(&ack_bytes[..bytes_read]);
    ack_decoder.next_message().expect("decode ack")
}

#[tokio::test]
async fn typed_keystroke_wire_reaches_stub_and_acknowledges() {
    let proof_root =
        std::env::temp_dir().join(format!("rmux-protocol-boundary-{}", std::process::id()));
    std::fs::create_dir_all(&proof_root).expect("create /tmp check root");

    assert_eq!(
        first_keystroke_reply(
            b"\x1b[A",
            true,
            ["encode typed keystroke", "process typed keystroke"],
            &mut [0_u8; 64],
        )
        .await,
        Some(AttachMessage::KeyDispatched(KeyDispatched::new(3)))
    );

    std::fs::remove_dir_all(proof_root).expect("remove /tmp check root");
}

#[tokio::test]
async fn mouse_keystroke_wire_does_not_error_or_drop_the_attach() {
    assert_eq!(
        first_keystroke_reply(
            b"\x1b[<0;10;10M",
            false,
            ["encode mouse keystroke", "process mouse keystroke"],
            &mut [0_u8; 128],
        )
        .await,
        Some(AttachMessage::KeyDispatched(KeyDispatched::new(11)))
    );
}

#[tokio::test]
async fn data_payload_does_not_trust_an_unversioned_cached_pane_shell() {
    let handler = Arc::new(RequestHandler::new());
    let attach_pid = std::process::id();
    let session_name = handler.create_session("cached-master").await;
    let target = PaneTarget::with_window(session_name.clone(), 0, 0);
    let _control_rx = handler.attach_client(attach_pid, &session_name).await;
    handler.start_attached_input_capture_for_test(&target).await;

    // This master has the same logical target spelling but is deliberately
    // unrelated to the current pane lifetime, as happens while a respawn
    // switch control is still queued.
    let mut cached_target = open_attach_target(test_attach_target(&session_name, b"", None), false)
        .expect("open stale cached target");
    let live_input =
        LiveAttachInputContext::current_for_test(Arc::clone(&handler), attach_pid).await;
    let (mut input, _peer) = SocketInput::new(live_input, false);

    let forwarded = process_attach_data_payload(
        &input.live_input,
        &input.stream,
        Some(&mut cached_target),
        &mut input.pending_input,
        &mut input.active_emit_cache,
        &mut input.locked,
        b"SAFE",
    )
    .await
    .expect("data payload routes through the current handler state");

    assert!(forwarded);
    assert!(input.pending_input.is_empty());
    assert_eq!(
        handler.attached_input_capture_for_test(&target).await,
        Some(b"SAFE".to_vec())
    );
}

#[tokio::test]
async fn forward_attach_emits_stop_sequence_when_processing_errors() {
    let handler = Arc::new(RequestHandler::new());
    let (stream, mut peer) = tokio::net::UnixStream::pair().expect("attach stream pair");
    let target = test_attach_target(&session_name("alpha"), b"", None);
    let expected_stop = target.outer_terminal.attach_stop_sequence();
    let invalid_initial_socket_bytes =
        encode_attach_message(&AttachMessage::Lock("unexpected".to_owned()))
            .expect("encode unexpected lock frame");
    let (_shutdown_tx, shutdown_rx) = watch::channel(());
    let (_control_tx, control_rx) = mpsc::unbounded_channel();
    let closing = Arc::new(AtomicBool::new(false));
    let live_input = LiveAttachInputContext::unregistered_for_test(handler, std::process::id());

    let result = forward_attach(
        stream,
        target,
        invalid_initial_socket_bytes,
        shutdown_rx,
        control_rx,
        Arc::new(AtomicUsize::new(0)),
        closing,
        Arc::new(AtomicU64::new(0)),
        live_input,
        false,
    )
    .await;
    assert!(result.is_err(), "invalid attach input should fail");

    let mut collected = Vec::new();
    let mut frame_bytes = [0_u8; 4096];
    loop {
        let bytes_read = tokio::time::timeout(Duration::from_secs(1), peer.read(&mut frame_bytes))
            .await
            .expect("peer read should not time out")
            .expect("read peer bytes");
        if bytes_read == 0 {
            break;
        }
        let mut decoder = AttachFrameDecoder::new();
        decoder.push_bytes(&frame_bytes[..bytes_read]);
        while let Some(message) = decoder.next_message().expect("decode attach frame") {
            if let AttachMessage::Data(bytes) | AttachMessage::Render(bytes) = message {
                collected.extend_from_slice(&bytes);
            }
        }
    }

    assert!(
        contains_bytes(&collected, &expected_stop),
        "attach stop sequence should be emitted on attach failure"
    );
}

#[tokio::test]
async fn detach_control_emits_stop_and_banner_in_one_data_frame() {
    let mut controls =
        PendingControls::new(test_attach_target(&session_name("alpha"), b"BASE-A", None));
    let expected_stop = controls.target.outer_terminal.attach_stop_sequence();

    controls
        .control_tx
        .send(AttachControl::Detach)
        .expect("send detach control");

    let action = controls.apply(None).await.expect("apply pending detach");

    assert!(matches!(action, PendingAttachAction::Exit(_)));

    let mut frame_bytes = [0_u8; 4096];
    let bytes_read = controls
        .peer
        .read(&mut frame_bytes)
        .await
        .expect("read detach frame");
    let mut decoder = AttachFrameDecoder::new();
    decoder.push_bytes(&frame_bytes[..bytes_read]);
    let Some(AttachMessage::Data(bytes)) = decoder.next_message().expect("decode detach frame")
    else {
        panic!("detach should emit a data frame");
    };

    assert!(
        contains_bytes(&bytes, &expected_stop),
        "detach data must contain attach-stop before close"
    );
    assert!(
        contains_bytes(&bytes, b"[detached (from session alpha)]\r\n"),
        "detach data must contain detached banner"
    );
}

#[tokio::test]
async fn lock_control_emits_attach_stop_before_transferring_terminal_ownership() {
    let alpha = session_name("alpha-lock-stop");
    let mut controls = PendingControls::new(test_attach_target(&alpha, b"BASE", None));
    let expected_stop = controls.target.outer_terminal.attach_stop_sequence();
    let command = AttachShellCommand::new(
        "lock-command".to_owned(),
        "/bin/sh".to_owned(),
        "/tmp".to_owned(),
    );
    controls
        .control_tx
        .send(AttachControl::LockShellCommand(command.clone()))
        .expect("send lock control");

    let action = controls.apply(None).await.expect("apply pending lock");
    assert!(matches!(action, PendingAttachAction::Continue { .. }));
    assert!(controls.locked);

    let peer = &mut controls.peer;
    let messages = tokio::time::timeout(Duration::from_secs(1), async {
        let mut decoder = AttachFrameDecoder::new();
        let mut messages = Vec::new();
        let mut bytes = [0_u8; 4096];
        while messages.len() < 2 {
            let read = peer.read(&mut bytes).await.expect("read lock frames");
            assert!(read > 0, "attach stream closed before lock frames");
            decoder.push_bytes(&bytes[..read]);
            while let Some(message) = decoder.next_message().expect("decode lock frame") {
                messages.push(message);
            }
        }
        messages
    })
    .await
    .expect("lock frames timed out");

    let AttachMessage::Data(stop) = &messages[0] else {
        panic!(
            "first lock frame must restore the outer terminal: {:?}",
            messages[0]
        );
    };
    assert!(
        contains_bytes(stop, &expected_stop),
        "lock must emit the complete attach-stop sequence first"
    );
    assert_eq!(messages[1], AttachMessage::LockShellCommand(command));
}

fn test_attach_target(
    session_name: &SessionName,
    render_frame: &[u8],
    persistent_overlay_state_id: Option<u64>,
) -> AttachTarget {
    AttachTarget {
        persistent_overlay_state_id,
        ..test_attach_target_with_output(session_name, render_frame, &pane_output_channel(), false)
    }
}

fn test_attach_target_with_output(
    session_name: &SessionName,
    render_frame: &[u8],
    pane_output: &super::types::PaneOutputSender,
    kitty_graphics_passthrough: bool,
) -> AttachTarget {
    test_attach_target_with_protocols(
        session_name,
        render_frame,
        pane_output,
        kitty_graphics_passthrough,
        false,
    )
}

fn test_attach_target_with_protocols(
    session_name: &SessionName,
    render_frame: &[u8],
    pane_output: &super::types::PaneOutputSender,
    kitty_graphics_passthrough: bool,
    sixel_passthrough: bool,
) -> AttachTarget {
    let (pane_output_start_sequence, pane_output) = pane_output.subscribe_live_from_now();
    AttachTarget {
        session_name: session_name.clone(),
        live_pane_handover: true,
        pane_output,
        pane_output_start_sequence,
        render_frame: render_frame.to_vec(),
        outer_terminal: OuterTerminal::resolve(
            &OptionStore::default(),
            OuterTerminalContext::default(),
        ),
        client_title: None,
        cursor_style: 0,
        active_pane_geometry: PaneGeometry::new(0, 0, 80, 24),
        raw_passthrough: kitty_graphics_passthrough || sixel_passthrough,
        kitty_graphics_passthrough,
        sixel_passthrough,
        persistent_overlay_state_id: None,
        live_pane: None,
    }
}

/// `target` as a plain re-render of the pane: it hands the client no live pane.
fn render_only(mut target: AttachTarget) -> AttachTarget {
    target.live_pane_handover = false;
    target
}

fn test_render_only_attach_target(session_name: &SessionName, render_frame: &[u8]) -> AttachTarget {
    render_only(test_attach_target(session_name, render_frame, None))
}

/// A render-only refresh fed by `pane_output` that passes Kitty graphics through.
fn test_render_only_kitty_target(
    session_name: &SessionName,
    render_frame: &[u8],
    pane_output: &super::types::PaneOutputSender,
) -> AttachTarget {
    let target = test_attach_target_with_output(session_name, render_frame, pane_output, true);
    render_only(target)
}

/// A switch to the attach target `test_attach_target` builds from the same arguments.
fn switch_control(
    session_name: &SessionName,
    render_frame: &[u8],
    persistent_overlay_state_id: Option<u64>,
) -> AttachControl {
    AttachControl::switch(test_attach_target(
        session_name,
        render_frame,
        persistent_overlay_state_id,
    ))
}

/// A persistent overlay `frame` for overlay state `state_id`.
fn persistent_overlay_control(
    frame: &[u8],
    render_generation: u64,
    overlay_generation: u64,
    state_id: u64,
) -> AttachControl {
    AttachControl::Overlay(OverlayFrame::persistent_with_state(
        frame.to_vec(),
        render_generation,
        overlay_generation,
        state_id,
    ))
}

#[test]
fn live_output_is_preserved_only_for_coalescible_same_source_refreshes() {
    let session_name = session_name("live-output-source");
    let shared_output = pane_output_channel();
    let initial = test_render_only_kitty_target(&session_name, b"BASE-A", &shared_output);
    let current = open_attach_target(initial, false).expect("open initial target");

    let same_source = test_render_only_kitty_target(&session_name, b"BASE-B", &shared_output);
    assert!(preserves_live_output(&current, &same_source));

    let different_source =
        test_render_only_kitty_target(&session_name, b"BASE-C", &pane_output_channel());
    assert!(different_source.is_coalescible_render_refresh());
    assert!(!preserves_live_output(&current, &different_source));

    let non_coalescible_same_source =
        test_attach_target_with_output(&session_name, b"BASE-D", &shared_output, true);
    assert!(!non_coalescible_same_source.is_coalescible_render_refresh());
    assert!(!preserves_live_output(
        &current,
        &non_coalescible_same_source
    ));

    // A frame carrying OSC 0 is kept out of the queue's replaceable slot, but
    // it is still a re-render of the same pane: its buffered kitty/sixel
    // passthroughs must survive. Coupling the two predicates would drop them on
    // every title change (issue #182).
    let mut title_carrying =
        test_render_only_kitty_target(&session_name, b"BASE-E", &shared_output);
    let terminal = OuterTerminal::resolve(
        &OptionStore::new(),
        OuterTerminalContext::from_pairs(&[("TERM", "tmux-256color")]),
    );
    title_carrying.client_title =
        terminal.rendered_client_title(crate::outer_terminal::ClientTitleUpdate {
            resolved: Some("LIVE-OUTPUT-TITLE"),
            path: crate::outer_terminal::ClientPathUpdate::Unread,
            previous: None,
        });
    assert!(
        !title_carrying.is_coalescible_render_refresh(),
        "a title-carrying frame must not be replaceable in the queue"
    );
    assert!(title_carrying.is_plain_render_refresh());
    assert!(
        preserves_live_output(&current, &title_carrying),
        "a title-carrying refresh of the same pane must keep its live output"
    );
}

#[test]
fn render_only_switches_coalesce_before_reliable_controls() {
    let alpha = session_name("alpha");
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let mut deferred_controls = VecDeque::new();

    let first = test_render_only_attach_target(&alpha, b"first");
    let second = test_render_only_attach_target(&alpha, b"second");
    let third = test_render_only_attach_target(&alpha, b"third");
    control_tx
        .send(AttachControl::switch(second))
        .expect("queue second switch");
    control_tx
        .send(AttachControl::switch(third))
        .expect("queue third switch");
    control_tx
        .send(AttachControl::Detach)
        .expect("queue reliable detach");

    let control_backlog = AtomicUsize::new(0);
    let (coalesced, switch_count) = coalesce_render_switches(
        super::attach_control::QueuedAttachTarget::Direct(Box::new(first)),
        &mut deferred_controls,
        Some(&mut control_rx),
        &control_backlog,
    );

    assert_eq!(coalesced.render_frame, b"third");
    assert_eq!(switch_count, 3);
    assert!(matches!(
        deferred_controls.pop_front(),
        Some(AttachControl::Detach)
    ));
}

#[test]
fn sender_side_switch_coalescing_preserves_render_generation_count() {
    let alpha = session_name("alpha");
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let control_backlog = Arc::new(AtomicUsize::new(0));
    let sender = AttachControlSender::new(
        control_tx,
        Arc::clone(&control_backlog),
        8,
        Arc::new(AtomicBool::new(false)),
    );
    for frame in [b"first".as_slice(), b"second", b"third"] {
        sender
            .send(AttachControl::switch(test_render_only_attach_target(
                &alpha, frame,
            )))
            .expect("coalesced switch fits");
    }

    let AttachControl::Switch(target) = control_rx.try_recv().expect("one coalesced switch") else {
        panic!("expected a coalesced switch");
    };
    let (target, switch_count) = coalesce_render_switches(
        target,
        &mut VecDeque::new(),
        Some(&mut control_rx),
        &control_backlog,
    );

    assert_eq!(target.render_frame, b"third");
    assert_eq!(switch_count, 3);
    assert_eq!(control_backlog.load(Ordering::Acquire), 0);
    assert!(matches!(
        control_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn render_only_switch_coalescing_preserves_deferred_control_order() {
    let alpha = session_name("alpha");
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let mut deferred_controls = VecDeque::from([AttachControl::Refresh]);

    let first = test_render_only_attach_target(&alpha, b"first");
    let second = test_render_only_attach_target(&alpha, b"second");
    control_tx
        .send(AttachControl::switch(second))
        .expect("queue render switch");
    control_tx
        .send(AttachControl::Detach)
        .expect("queue reliable detach");

    let control_backlog = AtomicUsize::new(0);
    let (coalesced, switch_count) = coalesce_render_switches(
        super::attach_control::QueuedAttachTarget::Direct(Box::new(first)),
        &mut deferred_controls,
        Some(&mut control_rx),
        &control_backlog,
    );

    assert_eq!(coalesced.render_frame, b"second");
    assert_eq!(switch_count, 2);
    assert!(matches!(
        deferred_controls.pop_front(),
        Some(AttachControl::Refresh)
    ));
    assert!(matches!(
        deferred_controls.pop_front(),
        Some(AttachControl::Detach)
    ));
}

#[tokio::test]
async fn pending_switch_action_reports_target_change_for_status_reschedule() {
    let alpha = session_name("alpha");
    let beta = session_name("beta");
    let mut controls = PendingControls::new(test_attach_target(&alpha, b"BASE-A", None));

    controls
        .control_tx
        .send(switch_control(&beta, b"BASE-B", None))
        .expect("send switch control");

    let action = controls.apply(None).await.expect("apply pending switch");

    assert!(matches!(
        action,
        PendingAttachAction::Continue {
            target_changed: true
        }
    ));
    assert_eq!(controls.target.session_name, beta);
    let refresh = read_attach_data_until(&mut controls.peer, b"BASE-B").await;
    assert!(
        String::from_utf8_lossy(&refresh).contains("BASE-B"),
        "switch should render the target frame"
    );
}

#[tokio::test]
async fn pending_refresh_after_switch_preserves_target_change() {
    let alpha = session_name("alpha");
    let beta = session_name("beta");
    let mut controls = PendingControls::new(test_attach_target(&alpha, b"BASE-A", None));

    controls
        .control_tx
        .send(switch_control(&beta, b"BASE-B", None))
        .expect("send switch control");
    controls
        .control_tx
        .send(AttachControl::Refresh)
        .expect("send refresh control");

    let action = controls
        .apply(None)
        .await
        .expect("apply pending switch and refresh");

    assert!(matches!(
        action,
        PendingAttachAction::Refresh {
            target_changed: true
        }
    ));
    assert_eq!(controls.target.session_name, beta);
    let refresh = read_attach_data_until(&mut controls.peer, b"BASE-B").await;
    assert!(
        String::from_utf8_lossy(&refresh).contains("BASE-B"),
        "switch should render before the refresh is scheduled"
    );
}

#[tokio::test]
async fn pending_same_pane_switch_preserves_partial_input_and_escape_deadline() {
    let alpha = session_name("pending-input-refresh");
    let pane_output = pane_output_channel();
    let initial = test_attach_target_with_output(&alpha, b"BASE-A", &pane_output, false);
    let mut controls = PendingControls::new(initial);
    let mut pending_input = b"\x1b_".to_vec();
    let mut pending_escape_flush = PendingEscapeFlush::default();
    pending_escape_flush.sync(&pending_input, Duration::from_secs(30));
    let original_deadline = pending_escape_flush
        .deadline()
        .expect("Meta-_ should arm the escape deadline");
    let replacement = test_attach_target_with_output(&alpha, b"BASE-B", &pane_output, false);
    controls
        .deferred
        .push_back(AttachControl::switch(replacement));

    let action = controls
        .apply(Some(PendingAttachInputState::new(
            &mut pending_input,
            &mut pending_escape_flush,
        )))
        .await
        .expect("apply queued same-pane refresh");

    assert!(matches!(action, PendingAttachAction::Continue { .. }));
    assert_eq!(pending_input, b"\x1b_");
    assert_eq!(pending_escape_flush.deadline(), Some(original_deadline));
}

#[tokio::test]
async fn pending_different_pane_switch_clears_partial_input_and_escape_deadline() {
    let alpha = session_name("pending-input-pane-change");
    let mut controls = PendingControls::new(test_attach_target(&alpha, b"BASE-A", None));
    let mut pending_input = b"\x1b_".to_vec();
    let mut pending_escape_flush = PendingEscapeFlush::default();
    pending_escape_flush.sync(&pending_input, Duration::from_secs(30));
    controls
        .deferred
        .push_back(switch_control(&alpha, b"BASE-B", None));

    controls
        .apply(Some(PendingAttachInputState::new(
            &mut pending_input,
            &mut pending_escape_flush,
        )))
        .await
        .expect("apply queued pane change");

    assert!(pending_input.is_empty());
    assert!(pending_escape_flush.deadline().is_none());
}

#[tokio::test]
async fn terminal_ownership_controls_clear_partial_input_and_escape_deadline() {
    for (label, control) in [
        (
            "lock",
            AttachControl::LockShellCommand(AttachShellCommand::new(
                "lock-command".to_owned(),
                "/bin/sh".to_owned(),
                "/tmp".to_owned(),
            )),
        ),
        ("suspend", AttachControl::Suspend),
    ] {
        let session_name = session_name(&format!("pending-input-{label}"));
        let mut controls = PendingControls::new(test_attach_target(&session_name, b"BASE", None));
        let mut pending_input = b"\x1b_".to_vec();
        let mut pending_escape_flush = PendingEscapeFlush::default();
        pending_escape_flush.sync(&pending_input, Duration::from_secs(30));
        assert!(pending_escape_flush.deadline().is_some());
        controls.deferred.push_back(control);

        let action = controls
            .apply(Some(PendingAttachInputState::new(
                &mut pending_input,
                &mut pending_escape_flush,
            )))
            .await
            .unwrap_or_else(|error| panic!("apply pending {label}: {error}"));

        assert!(
            matches!(action, PendingAttachAction::Continue { .. }),
            "{label} transfers terminal ownership"
        );
        assert!(controls.locked, "{label} marks the attach as locked");
        assert!(pending_input.is_empty(), "{label} drops partial input");
        assert!(
            pending_escape_flush.deadline().is_none(),
            "{label} cancels the stale escape deadline"
        );
    }
}

#[tokio::test]
async fn stale_persistent_switches_still_advance_render_generation() {
    let alpha = session_name("alpha");
    let beta = session_name("beta");
    let mut controls = PendingControls::new(test_attach_target(&alpha, b"BASE-A", Some(10)));
    controls.render_generation = 41;

    controls
        .control_tx
        .send(switch_control(&beta, b"STALE-B", Some(9)))
        .expect("send stale switch control");

    let action = controls
        .apply(None)
        .await
        .expect("apply stale pending switch");

    assert!(matches!(action, PendingAttachAction::Write));
    assert_eq!(controls.target.session_name, alpha);
    assert_eq!(controls.render_generation, 42);
}

#[tokio::test]
async fn render_only_switch_forwards_pending_live_passthroughs() {
    let alpha = session_name("alpha");
    let pane_output = pane_output_channel();
    let initial = test_render_only_kitty_target(&alpha, b"BASE-A", &pane_output);
    let mut replacement = test_render_only_kitty_target(&alpha, b"BASE-B", &pane_output);
    let mut controls = PendingControls::new(initial);

    pane_output.send_for_generation_with_passthroughs(
        None,
        b"image".to_vec(),
        vec![TerminalPassthrough::kitty_graphics(
            0,
            0,
            b"Gf=100;AAAA".to_vec(),
        )],
    );
    pane_output.send_for_generation_with_passthroughs(
        None,
        b"next-image".to_vec(),
        vec![TerminalPassthrough::kitty_graphics(
            0,
            0,
            b"Gf=100;BBBB".to_vec(),
        )],
    );
    replacement.pane_output_start_sequence = 1;
    controls
        .control_tx
        .send(AttachControl::switch(replacement))
        .expect("send render-only switch");

    let action = controls.apply(None).await.expect("apply pending switch");

    assert!(matches!(action, PendingAttachAction::Write));
    let refresh = read_attach_data_until(&mut controls.peer, b"Gf=100;AAAA").await;
    assert!(
        String::from_utf8_lossy(&refresh).contains("BASE-B"),
        "render-only switch should still write the replacement frame"
    );
    assert!(
        contains_bytes(&refresh, b"\x1b_Gf=100;AAAA\x1b\\"),
        "render-only switch must not drop pending live passthroughs"
    );
    assert!(
        !contains_bytes(&refresh, b"\x1b_Gf=100;BBBB\x1b\\"),
        "render-only switch must not duplicate passthroughs covered by the replacement receiver"
    );
}

/// Where `needle` first occurs in `haystack`.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    find_bytes(haystack, needle).is_some()
}

/// Reads one chunk from `peer` and appends the Data/Render payloads it completes to `collected`;
/// answers with the chunk length, 0 at end of stream.
async fn read_attach_data(
    peer: &mut tokio::net::UnixStream,
    decoder: &mut AttachFrameDecoder,
    collected: &mut Vec<u8>,
) -> usize {
    let mut frame_bytes = [0_u8; 4096];
    let bytes_read = peer.read(&mut frame_bytes).await.expect("read peer bytes");
    decoder.push_bytes(&frame_bytes[..bytes_read]);
    while let Some(message) = decoder.next_message().expect("decode attach frame") {
        if let AttachMessage::Data(bytes) | AttachMessage::Render(bytes) = message {
            collected.extend_from_slice(&bytes);
        }
    }
    bytes_read
}

/// Reads attach data from `peer` until it contains `needle`, failing after a second.
async fn read_attach_data_until(peer: &mut tokio::net::UnixStream, needle: &[u8]) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(1), async {
        let mut decoder = AttachFrameDecoder::new();
        let mut collected = Vec::new();
        loop {
            let bytes_read = read_attach_data(peer, &mut decoder, &mut collected).await;
            assert!(bytes_read > 0, "attach stream closed before expected data");
            if contains_bytes(&collected, needle) {
                break collected;
            }
        }
    })
    .await
    .expect("timed out waiting for attach data")
}

/// Reads whatever attach data `peer` receives within `duration`, or until it closes.
async fn read_attach_data_for(peer: &mut tokio::net::UnixStream, duration: Duration) -> Vec<u8> {
    let mut decoder = AttachFrameDecoder::new();
    let mut collected = Vec::new();
    let deadline = tokio::time::sleep(duration);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => break,
            bytes_read = read_attach_data(peer, &mut decoder, &mut collected) => {
                if bytes_read == 0 {
                    break;
                }
            }
        }
    }

    collected
}

#[tokio::test]
async fn initial_attach_repaint_above_two_mib_uses_bounded_ordered_fragments() {
    let session_name = session_name("large-initial-repaint");
    let repaint = (0..(2 * DEFAULT_MAX_FRAME_LENGTH + 17))
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let (_control_tx, control_rx) = mpsc::unbounded_channel();
    let mut attach = AttachForwarder::spawn(
        test_render_only_attach_target(&session_name, &repaint),
        control_rx,
        Arc::default(),
        LiveAttachInputContext::unregistered_for_test(
            Arc::new(RequestHandler::new()),
            std::process::id(),
        ),
        true,
    );
    let peer = &mut attach.peer;

    let mut decoder = AttachFrameDecoder::new();
    let mut reconstructed = Vec::with_capacity(repaint.len());
    let mut fragment_lengths = Vec::new();
    let mut bytes = [0_u8; 64 * 1024];
    tokio::time::timeout(Duration::from_secs(5), async {
        while reconstructed.len() < repaint.len() {
            let count = peer.read(&mut bytes).await.expect("read initial repaint");
            assert!(count > 0, "attach stream closed during initial repaint");
            decoder.push_bytes(&bytes[..count]);
            while let Some(message) = decoder.next_message().expect("decode initial repaint") {
                let AttachMessage::Data(fragment) = message else {
                    panic!("oversized initial repaint must use strict ordered data");
                };
                if fragment.len() == DEFAULT_MAX_FRAME_LENGTH || !fragment_lengths.is_empty() {
                    fragment_lengths.push(fragment.len());
                    reconstructed.extend_from_slice(&fragment);
                }
            }
        }
    })
    .await
    .expect("initial repaint timed out");

    assert_eq!(
        fragment_lengths,
        vec![DEFAULT_MAX_FRAME_LENGTH, DEFAULT_MAX_FRAME_LENGTH, 17]
    );
    assert_eq!(reconstructed, repaint);

    attach.shutdown.send(()).expect("request attach shutdown");
    assert!(
        attach
            .task
            .await
            .expect("attach task joins after shutdown")
            .is_ok(),
        "large initial repaint must not terminate the attach forwarder"
    );
}

#[tokio::test]
async fn forward_attach_exited_control_wins_over_closing_shutdown() {
    let (mut attach, control_tx) =
        AttachForwarder::unregistered(test_attach_target(&session_name("alpha"), b"BASE-0", None));

    attach.assert_initial_render("BASE-0").await;

    control_tx
        .send(AttachControl::Refresh)
        .expect("queue non-terminal control");
    control_tx
        .send(AttachControl::Exited)
        .expect("send exited control");
    attach.closing.store(true, Ordering::SeqCst);
    attach.shutdown.send(()).expect("request attach shutdown");

    let exited = read_attach_data_until(&mut attach.peer, b"[exited]\r\n").await;
    assert!(
        contains_bytes(&exited, b"[exited]\r\n"),
        "exited control must win over the closing shutdown race"
    );

    let result = attach.task.await.expect("attach task join");
    assert!(
        result.is_ok(),
        "forward_attach should exit cleanly: {result:?}"
    );
}

#[tokio::test]
async fn admitted_attach_input_batch_drains_after_shutdown_admission_closes() {
    let handler = Arc::new(RequestHandler::new());
    let pane_target =
        create_attach_input_test_session(&handler, "attach-admitted-shutdown-drain").await;
    let session_name = pane_target.session_name().clone();
    let attach_pid = 912_051;
    let control_rx = handler.attach_client(attach_pid, &session_name).await;
    let live_input =
        LiveAttachInputContext::current_for_test(Arc::clone(&handler), attach_pid).await;
    let live_input_identity = live_input.identity;
    let mut attach = AttachForwarder::spawn(
        test_attach_target(&session_name, b"BASE-ADMITTED", None),
        control_rx,
        Arc::default(),
        live_input,
        false,
    );
    let peer = &mut attach.peer;
    let _ = read_attach_data_until(peer, b"BASE-ADMITTED").await;
    let pause = install_live_attach_input_validation_pause(live_input_identity);

    peer.write_all(
        &encode_attach_message(&AttachMessage::Data(b"ADMITTED-BEFORE-CLOSE".to_vec()))
            .expect("encode attach input"),
    )
    .await
    .expect("write admitted attach input");
    tokio::time::timeout(Duration::from_secs(2), pause.reached.notified())
        .await
        .expect("attach input reaches the post-admission pause");
    assert!(
        handler
            .attached_input_capture_for_test(&pane_target)
            .await
            .expect("input capture remains installed")
            .is_empty(),
        "the deterministic pause must precede the first attach mutation"
    );

    handler.close_normal_request_admission();
    assert!(
        !handler.normal_drain_requests_quiesced(),
        "the admitted attach mutation must remain counted while paused"
    );
    attach.shutdown.send_replace(());
    pause.release.notify_one();

    tokio::time::timeout(Duration::from_secs(2), attach.task)
        .await
        .expect("admitted attach batch drains before shutdown")
        .expect("attach task join")
        .expect("attach exits cleanly");
    assert_eq!(
        handler
            .attached_input_capture_for_test(&pane_target)
            .await
            .expect("input capture remains installed"),
        b"ADMITTED-BEFORE-CLOSE",
        "a batch admitted before the shutdown barrier must drain to completion"
    );
    assert!(
        handler.normal_drain_requests_quiesced(),
        "the attach Drain admission must release after the batch completes"
    );
}

#[tokio::test]
async fn attach_input_ready_after_shutdown_admission_closes_is_rejected() {
    let handler = Arc::new(RequestHandler::new());
    let pane_target =
        create_attach_input_test_session(&handler, "attach-rejected-after-shutdown").await;
    let session_name = pane_target.session_name().clone();
    let attach_pid = 912_052;
    let control_rx = handler.attach_client(attach_pid, &session_name).await;
    let live_input =
        LiveAttachInputContext::current_for_test(Arc::clone(&handler), attach_pid).await;
    let (stream, _peer) = tokio::net::UnixStream::pair().expect("attach stream pair");
    let (_shutdown_tx, shutdown_rx) = watch::channel(());
    handler.close_normal_request_admission();

    let initial_socket_bytes =
        encode_attach_message(&AttachMessage::Data(b"REJECTED-AFTER-CLOSE".to_vec()))
            .expect("encode buffered attach input");
    tokio::time::timeout(
        Duration::from_secs(2),
        forward_attach(
            stream,
            test_attach_target(&session_name, b"BASE-REJECTED", None),
            initial_socket_bytes,
            shutdown_rx,
            control_rx,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(0)),
            live_input,
            false,
        ),
    )
    .await
    .expect("admission-closed attach exits promptly")
    .expect("attach exits cleanly");

    assert!(
        handler
            .attached_input_capture_for_test(&pane_target)
            .await
            .expect("input capture remains installed")
            .is_empty(),
        "buffered socket input must not begin after shutdown admission closes"
    );
}

#[tokio::test]
async fn closing_shutdown_discards_mutating_controls_but_finishes_terminal_exit() {
    let handler = Arc::new(RequestHandler::new());
    let session_name = session_name("attach-closing-shutdown-barrier");
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let mut attach = AttachForwarder::spawn(
        test_attach_target(&session_name, b"BASE-CLOSING", None),
        control_rx,
        Arc::default(),
        LiveAttachInputContext::unregistered_for_test(Arc::clone(&handler), 912_053),
        false,
    );
    let _ = read_attach_data_until(&mut attach.peer, b"BASE-CLOSING").await;

    attach.closing.store(true, Ordering::SeqCst);
    handler.close_normal_request_admission();
    control_tx
        .send(AttachControl::Suspend)
        .expect("queue mutating attach control");
    control_tx
        .send(AttachControl::Exited)
        .expect("queue terminal attach control");

    tokio::time::timeout(Duration::from_secs(2), attach.task)
        .await
        .expect("closing attach finishes terminal transport state")
        .expect("attach task join")
        .expect("attach exits cleanly");

    let mut wire = Vec::new();
    attach
        .peer
        .read_to_end(&mut wire)
        .await
        .expect("read completed attach transport");
    let mut decoder = AttachFrameDecoder::new();
    decoder.push_bytes(&wire);
    let mut messages = Vec::new();
    while let Some(message) = decoder.next_message().expect("decode attach frame") {
        messages.push(message);
    }
    assert!(
        !messages.contains(&AttachMessage::Suspend),
        "shutdown must discard a queued mutating attach control"
    );
    assert!(
        messages.iter().any(|message| match message {
            AttachMessage::Data(bytes) | AttachMessage::Render(bytes) => {
                contains_bytes(bytes, b"[exited]\r\n")
            }
            _ => false,
        }),
        "the non-mutating terminal control must still finish the exit banner"
    );
}

#[tokio::test]
async fn last_session_exit_waits_for_attach_wire_drain_before_daemon_shutdown() {
    let handler = Arc::new(RequestHandler::new());
    let session_name = handler.create_session("attach-drain").await;
    let (daemon_shutdown, mut daemon_shutdown_rx) = ShutdownHandle::new();
    handler.install_shutdown_handle(daemon_shutdown);
    let forwarder_guard = handler.begin_attach_forwarder();
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let closing = Arc::new(AtomicBool::new(false));
    let attach_pid = std::process::id();
    let attach_id = handler
        .register_attach_with_closing(
            attach_pid,
            session_name.clone(),
            control_tx,
            Arc::clone(&closing),
            OuterTerminalContext::default(),
            crate::client_flags::ClientFlags::default(),
        )
        .await;
    let live_input =
        LiveAttachInputContext::current_for_test(Arc::clone(&handler), attach_pid).await;

    let mut attach = AttachForwarder::spawn(
        test_attach_target(&session_name, b"BASE-0", None),
        control_rx,
        closing,
        live_input,
        false,
    );
    let _ = read_attach_data_until(&mut attach.peer, b"BASE-0").await;

    handler
        .handle_ok(KillSessionRequest::fixture(session_name))
        .await;
    assert!(
        !handler.request_shutdown_if_pending(),
        "exit-empty must wait for the attached exit frame to drain"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut daemon_shutdown_rx)
            .await
            .is_err(),
        "daemon shutdown must stay pending while the attach forwarder owns the wire"
    );

    let exited = read_attach_data_until(&mut attach.peer, b"[exited]\r\n").await;
    assert!(
        contains_bytes(&exited, b"[exited]\r\n"),
        "the terminal exit frame must arrive before daemon shutdown"
    );
    let result = attach.task.await.expect("attach task join");
    assert!(result.is_ok(), "forward_attach should drain: {result:?}");
    handler.finish_attach(attach_pid, attach_id).await;
    drop(forwarder_guard);
    let _ = handler.request_shutdown_if_pending();
    tokio::time::timeout(Duration::from_millis(500), daemon_shutdown_rx)
        .await
        .expect("daemon should shut down after the attach exit frame drains")
        .expect("shutdown receiver should complete cleanly");
}

#[tokio::test]
async fn forward_attach_exited_control_drains_final_output_and_passthrough_before_banner() {
    let session_name = session_name("exit-drain");
    let pane_output = pane_output_channel();
    let target = test_attach_target_with_output(&session_name, b"BASE-0", &pane_output, true);
    let (mut attach, control_tx) = AttachForwarder::unregistered(target);

    let _initial = read_attach_data_until(&mut attach.peer, b"BASE-0").await;
    let _ = pane_output.send_for_generation_with_passthroughs(
        None,
        b"FINAL_TAIL".to_vec(),
        vec![TerminalPassthrough::kitty_graphics(
            0,
            0,
            b"Gf=100;TAIL".to_vec(),
        )],
    );
    let _ = pane_output.send_for_generation(None, Vec::new());
    control_tx
        .send(AttachControl::Exited)
        .expect("send exited control");

    let exited = read_attach_data_until(&mut attach.peer, b"[exited]\r\n").await;
    let tail = find_bytes(&exited, b"FINAL_TAIL").expect("final pane output must be delivered");
    let passthrough = find_bytes(&exited, b"\x1b_Gf=100;TAIL\x1b\\")
        .expect("final passthrough must be delivered");
    let banner = find_bytes(&exited, b"[exited]\r\n").expect("exit banner must be delivered");
    assert!(tail < banner);
    assert!(passthrough < banner);

    assert!(attach.task.await.expect("attach task join").is_ok());
}

#[tokio::test]
async fn session_exit_before_input_validation_still_drains_final_output() {
    let handler = Arc::new(RequestHandler::new());
    let pane_target =
        create_attach_input_test_session(&handler, "exit-during-validated-input").await;
    let session_name = pane_target.session_name().clone();
    let attach_pid = 912_044;
    let closing = Arc::new(AtomicBool::new(false));
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    handler
        .register_attach_with_closing(
            attach_pid,
            session_name.clone(),
            control_tx,
            Arc::clone(&closing),
            OuterTerminalContext::default(),
            crate::client_flags::ClientFlags::default(),
        )
        .await;
    let live_input =
        LiveAttachInputContext::current_for_test(Arc::clone(&handler), attach_pid).await;
    let pause = install_live_attach_input_validation_pause(live_input.identity);
    let pane_output = pane_output_channel();
    let mut attach = AttachForwarder::spawn(
        test_attach_target_with_output(&session_name, b"BASE-0", &pane_output, false),
        control_rx,
        closing,
        live_input,
        false,
    );
    let peer = &mut attach.peer;
    let _initial = read_attach_data_until(peer, b"BASE-0").await;

    peer.write_all(
        &encode_attach_message(&AttachMessage::Data(b"RACING_INPUT".to_vec()))
            .expect("encode attach input"),
    )
    .await
    .expect("write racing input");
    tokio::time::timeout(Duration::from_secs(2), pause.reached.notified())
        .await
        .expect("input reaches the pre-validation pause");

    let _ = pane_output.send_for_generation(None, b"FINAL_AFTER_CLOSE".to_vec());
    let _ = pane_output.send_for_generation(None, Vec::new());
    handler
        .handle_ok(KillSessionRequest::fixture(session_name))
        .await;
    pause.release.notify_one();

    let exited = read_attach_data_until(peer, b"[exited]\r\n").await;
    let tail = find_bytes(&exited, b"FINAL_AFTER_CLOSE")
        .expect("final output must survive the concurrent input close");
    let banner = find_bytes(&exited, b"[exited]\r\n").expect("exit banner must be delivered");
    assert!(tail < banner, "final output must precede the exit banner");
    assert!(
        attach.task.await.expect("attach task join").is_ok(),
        "terminal close must outrank stale input after it is published"
    );
}

#[tokio::test]
async fn finish_attach_exit_forwards_an_already_dequeued_batch_before_banner() {
    let session_name = session_name("dequeued-exit-drain");
    let (stream, mut peer) = tokio::net::UnixStream::pair().expect("attach stream pair");
    let stream = AttachTransport::from(stream);
    let mut current_target = open_attach_target(
        test_attach_target_with_output(&session_name, b"BASE-0", &pane_output_channel(), true),
        false,
    )
    .expect("open attach target");
    let mut deferred_passthroughs = Vec::new();

    finish_pending_attach_exit_with_batch(
        AttachExitReason::AttachControlExited,
        &stream,
        &mut current_target,
        &mut deferred_passthroughs,
        Some(AttachOutputBatch::Events {
            bytes: b"DEQUEUED_FINAL_TAIL".to_vec(),
            passthroughs: vec![TerminalPassthrough::kitty_graphics(
                0,
                0,
                b"Gf=100;DEQUEUED".to_vec(),
            )],
            passthrough_sequences: vec![0],
            close_after_render: true,
            close_sequence: Some(1),
            sustained: false,
        }),
    )
    .await
    .expect("finish attach exit");

    let exited = read_attach_data_until(&mut peer, b"[exited]\r\n").await;
    let tail = find_bytes(&exited, b"DEQUEUED_FINAL_TAIL")
        .expect("the already-dequeued output must be delivered");
    let passthrough = find_bytes(&exited, b"\x1b_Gf=100;DEQUEUED\x1b\\")
        .expect("the already-dequeued passthrough must be delivered");
    let banner = find_bytes(&exited, b"[exited]\r\n").expect("exit banner must be delivered");
    assert!(tail < banner);
    assert!(passthrough < banner);
}

/// Applies the queue of `controls`, which must end the attach, finishes the exit with the batch
/// the old receiver had already dequeued, and answers with the client's attach data.
async fn exit_after_queued_controls(
    mut controls: PendingControls,
    pending_batch: AttachOutputBatch,
    [apply_label, exit_message, finish_label]: [&str; 3],
) -> Vec<u8> {
    let exit = controls.apply(None).await.expect(apply_label);
    let PendingAttachAction::Exit(exit) = exit else {
        panic!("{exit_message}");
    };
    let mut deferred_passthroughs = Vec::new();
    finish_pending_attach_exit_with_batch(
        exit.reason,
        &controls.stream,
        &mut controls.target,
        &mut deferred_passthroughs,
        pending_attach_exit_output_batch(
            exit.drop_pending_output,
            exit.snapshot_covered_output_before_sequence,
            pending_batch,
        ),
    )
    .await
    .expect(finish_label);
    read_attach_data_until(&mut controls.peer, b"[exited]\r\n").await
}

#[tokio::test]
async fn exited_after_same_source_render_switch_does_not_duplicate_dequeued_output() {
    let session_name = session_name("render-switch-exit-drain");
    let pane_output = pane_output_channel();
    let initial = test_render_only_kitty_target(&session_name, b"BASE-0", &pane_output);
    let mut current_target = open_attach_target(initial, true).expect("open initial target");

    let covered_sequence = pane_output
        .send_for_generation_with_passthroughs(
            None,
            b"COVERED_ONCE".to_vec(),
            vec![TerminalPassthrough::kitty_graphics(
                0,
                0,
                b"Gf=100;COVERED".to_vec(),
            )],
        )
        .expect("publish output covered by the refresh snapshot");
    let covered_item = current_target
        .pane_output
        .as_mut()
        .and_then(super::types::PaneOutputReceiver::try_recv)
        .expect("old receiver dequeues covered output before the switch");
    let pending_batch = collect_attach_output_batch(covered_item, None);

    let replacement = test_render_only_kitty_target(&session_name, b"COVERED_ONCE", &pane_output);
    assert_eq!(
        replacement.pane_output_start_sequence,
        covered_sequence + 1,
        "the replacement snapshot boundary follows the covered output"
    );
    let _ = pane_output.send_for_generation_with_passthroughs(
        None,
        b"AFTER_SNAPSHOT_ONCE".to_vec(),
        vec![TerminalPassthrough::kitty_graphics(
            0,
            0,
            b"Gf=100;AFTER".to_vec(),
        )],
    );

    let controls = PendingControls::opened(current_target);
    controls
        .control_tx
        .send(AttachControl::switch(replacement))
        .expect("queue same-source render refresh");
    controls
        .control_tx
        .send(AttachControl::Exited)
        .expect("queue terminal exit");
    let exited = exit_after_queued_controls(
        controls,
        pending_batch,
        [
            "apply switch and terminal exit",
            "switch followed by Exited must terminate the attach",
            "finish render-refresh exit",
        ],
    )
    .await;
    for marker in [
        b"COVERED_ONCE".as_slice(),
        b"AFTER_SNAPSHOT_ONCE".as_slice(),
        b"\x1b_Gf=100;COVERED\x1b\\".as_slice(),
        b"\x1b_Gf=100;AFTER\x1b\\".as_slice(),
    ] {
        assert_eq!(
            exited
                .windows(marker.len())
                .filter(|bytes| *bytes == marker)
                .count(),
            1,
            "snapshot partition must deliver every output and passthrough exactly once: {marker:?}"
        );
    }
}

#[tokio::test]
async fn exited_after_non_coalesced_same_source_switches_forwards_passthroughs_once() {
    let session_name = session_name("multi-render-switch-exit-drain");
    let pane_output = pane_output_channel();
    let mut clipboard_options = OptionStore::new();
    clipboard_options
        .set(
            ScopeSelector::Global,
            OptionName::SetClipboard,
            "on".to_owned(),
            SetOptionMode::Replace,
        )
        .expect("enable application clipboard passthrough");
    let outer_terminal = OuterTerminal::resolve(
        &clipboard_options,
        OuterTerminalContext::from_pairs(&[("TERM", "xterm-kitty")]),
    );
    let passthroughs = |suffix: &str, clipboard_payload: &[u8]| {
        vec![
            TerminalPassthrough::raw(0, 0, format!("RAW-{suffix}").into_bytes()),
            TerminalPassthrough::clipboard(clipboard_payload.to_vec()),
            TerminalPassthrough::kitty_graphics(
                0,
                0,
                format!("Gf=100;KITTY-{suffix}").into_bytes(),
            ),
            TerminalPassthrough::sixel(0, 0, format!("qSIXEL-{suffix}").into_bytes()),
        ]
    };
    let refresh_target = |render_frame: &[u8]| AttachTarget {
        outer_terminal: outer_terminal.clone(),
        ..render_only(test_attach_target_with_protocols(
            &session_name,
            render_frame,
            &pane_output,
            true,
            true,
        ))
    };

    let mut current_target =
        open_attach_target(refresh_target(b"BASE-0"), true).expect("open initial target");

    let sequence_0 = pane_output
        .send_for_generation_with_passthroughs(
            None,
            b"OUTPUT-0".to_vec(),
            passthroughs("0", b"\x1b]52;c;UDA=\x07"),
        )
        .expect("publish first output interval");
    let replacement_1 = refresh_target(b"SNAPSHOT-1");
    assert_eq!(
        replacement_1.pane_output_start_sequence,
        sequence_0 + 1,
        "first replacement must start after the first interval"
    );

    let sequence_1 = pane_output
        .send_for_generation_with_passthroughs(
            None,
            b"OUTPUT-1".to_vec(),
            passthroughs("1", b"\x1b]52;c;UDE=\x07"),
        )
        .expect("publish middle output interval");
    let replacement_2 = refresh_target(b"SNAPSHOT-2");
    assert_eq!(
        replacement_2.pane_output_start_sequence,
        sequence_1 + 1,
        "second replacement must start after the middle interval"
    );

    let _sequence_2 = pane_output
        .send_for_generation_with_passthroughs(
            None,
            b"OUTPUT-2".to_vec(),
            passthroughs("2", b"\x1b]52;c;UDI=\x07"),
        )
        .expect("publish final output interval");

    let first_item = current_target
        .pane_output
        .as_mut()
        .and_then(super::types::PaneOutputReceiver::try_recv)
        .expect("old receiver dequeues the first interval");
    let pending_batch =
        collect_attach_output_batch(first_item, current_target.pane_output.as_mut());

    let controls = PendingControls::opened(current_target);
    controls
        .control_tx
        .send(AttachControl::switch(replacement_1))
        .expect("queue first same-source render refresh");
    controls
        .control_tx
        .send(AttachControl::Write(b"INTERLEAVED-CONTROL".to_vec()))
        .expect("separate the render refreshes so they cannot coalesce");
    controls
        .control_tx
        .send(AttachControl::switch(replacement_2))
        .expect("queue second same-source render refresh");
    controls
        .control_tx
        .send(AttachControl::Exited)
        .expect("queue terminal exit");

    let exited = exit_after_queued_controls(
        controls,
        pending_batch,
        [
            "apply two refreshes and terminal exit",
            "refreshes followed by Exited must terminate the attach",
            "finish multi-refresh exit",
        ],
    )
    .await;
    for marker in [
        b"RAW-0".as_slice(),
        b"RAW-1".as_slice(),
        b"RAW-2".as_slice(),
        b"\x1b]52;c;UDA=\x07".as_slice(),
        b"\x1b]52;c;UDE=\x07".as_slice(),
        b"\x1b]52;c;UDI=\x07".as_slice(),
        b"\x1b_Gf=100;KITTY-0\x1b\\".as_slice(),
        b"\x1b_Gf=100;KITTY-1\x1b\\".as_slice(),
        b"\x1b_Gf=100;KITTY-2\x1b\\".as_slice(),
        b"\x1bPqSIXEL-0\x1b\\".as_slice(),
        b"\x1bPqSIXEL-1\x1b\\".as_slice(),
        b"\x1bPqSIXEL-2\x1b\\".as_slice(),
    ] {
        assert_eq!(
            exited
                .windows(marker.len())
                .filter(|bytes| *bytes == marker)
                .count(),
            1,
            "every passthrough must be delivered exactly once: {marker:?}"
        );
    }
}

#[tokio::test]
async fn forward_attach_plain_refresh_does_not_clear_the_screen() {
    let session_name = session_name("alpha");
    let (mut attach, control_tx) =
        AttachForwarder::unregistered(test_attach_target(&session_name, b"BASE-0", None));

    attach.assert_initial_render("BASE-0").await;

    control_tx
        .send(switch_control(&session_name, b"BASE-1", None))
        .expect("send refreshed attach target");

    let refresh = read_attach_data_until(&mut attach.peer, b"BASE-1").await;
    let refresh_text = String::from_utf8_lossy(&refresh);
    assert!(
        !refresh_text.contains("\x1b[2J"),
        "plain pane-output refresh must not clear the whole terminal: {refresh_text:?}"
    );

    attach.assert_stops_healthy().await;
}

#[tokio::test]
async fn forward_attach_select_switch_preserves_fragmented_same_pane_input() {
    let handler = Arc::new(RequestHandler::new());
    let attach_pid = std::process::id();
    let session_name = handler.create_session("refresh-pending-input").await;
    let target = PaneTarget::with_window(session_name.clone(), 0, 0);

    let (control_tx, control_rx) = mpsc::unbounded_channel();
    handler
        .register_attach(attach_pid, session_name.clone(), control_tx.clone())
        .await;
    handler.start_attached_input_capture_for_test(&target).await;

    let pane_output = pane_output_channel();
    let initial = test_attach_target_with_output(&session_name, b"BASE-0", &pane_output, false);
    let live_input =
        LiveAttachInputContext::current_for_test(Arc::clone(&handler), attach_pid).await;
    let mut attach = AttachForwarder::spawn(initial, control_rx, Arc::default(), live_input, false);
    let peer = &mut attach.peer;

    let _initial = read_attach_data_until(peer, b"BASE-0").await;
    let prefix = b"A\x1b_Gi=7";
    peer.write_all(
        &encode_attach_message(&AttachMessage::Data(prefix.to_vec()))
            .expect("encode fragmented Kitty prefix"),
    )
    .await
    .expect("write fragmented Kitty prefix");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if handler.attached_input_capture_for_test(&target).await == Some(b"A".to_vec()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("prefix should reach the attach loop before the switch");

    control_tx
        .send(AttachControl::switch(test_attach_target_with_output(
            &session_name,
            b"BASE-1",
            &pane_output,
            false,
        )))
        .expect("send same-pane refresh through the select branch");
    let _refresh = read_attach_data_until(peer, b"BASE-1").await;

    let suffix = b";OK\x1b\\";
    peer.write_all(
        &encode_attach_message(&AttachMessage::Data(suffix.to_vec()))
            .expect("encode fragmented Kitty suffix"),
    )
    .await
    .expect("write fragmented Kitty suffix");
    let expected = b"A\x1b_Gi=7;OK\x1b\\";
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if handler.attached_input_capture_for_test(&target).await == Some(expected.to_vec()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("same-pane refresh must preserve the fragmented Kitty APC");

    assert!(attach.stop().await.is_ok());
}

#[tokio::test]
async fn forward_attach_lock_boundary_discards_fragmented_input_before_unlock() {
    let handler = Arc::new(RequestHandler::new());
    let attach_pid = std::process::id();
    let session_name = handler.create_session("lock-pending-input").await;
    let target = PaneTarget::with_window(session_name.clone(), 0, 0);
    handler
        .set_option(ScopeSelector::Global, OptionName::EscapeTime, "30000")
        .await;

    let (control_tx, control_rx) = mpsc::unbounded_channel();
    handler
        .register_attach(attach_pid, session_name.clone(), control_tx.clone())
        .await;
    handler.start_attached_input_capture_for_test(&target).await;

    let initial = test_attach_target(&session_name, b"BASE-0", None);
    let expected_stop = initial.outer_terminal.attach_stop_sequence();
    let live_input =
        LiveAttachInputContext::current_for_test(Arc::clone(&handler), attach_pid).await;
    let mut attach = AttachForwarder::spawn(initial, control_rx, Arc::default(), live_input, false);
    let peer = &mut attach.peer;

    let _initial = read_attach_data_until(peer, b"BASE-0").await;
    let prefix = b"A\x1b_Gi=7";
    peer.write_all(
        &encode_attach_message(&AttachMessage::Data(prefix.to_vec()))
            .expect("encode fragmented Kitty prefix"),
    )
    .await
    .expect("write fragmented Kitty prefix");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if handler.attached_input_capture_for_test(&target).await == Some(b"A".to_vec()) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fragment should reach the attach loop before lock");

    control_tx
        .send(AttachControl::LockShellCommand(AttachShellCommand::new(
            "lock-command".to_owned(),
            "/bin/sh".to_owned(),
            "/tmp".to_owned(),
        )))
        .expect("send lock control");
    let _stop = read_attach_data_until(peer, &expected_stop).await;

    peer.write_all(&encode_attach_message(&AttachMessage::Unlock).expect("encode unlock"))
        .await
        .expect("write unlock");
    let suffix = b";OWNERSHIP\x1b\\";
    peer.write_all(
        &encode_attach_message(&AttachMessage::Data(suffix.to_vec()))
            .expect("encode post-unlock suffix"),
    )
    .await
    .expect("write post-unlock suffix");

    let captured = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let captured = handler
                .attached_input_capture_for_test(&target)
                .await
                .expect("input capture remains installed");
            if contains_bytes(&captured, b"OWNERSHIP") {
                break captured;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("post-unlock input should reach the pane");
    assert!(
        !contains_bytes(&captured, b"Gi=7"),
        "pre-lock fragmented input must not cross the terminal ownership boundary: {captured:?}"
    );

    assert!(attach.stop().await.is_ok());
}

#[tokio::test]
async fn forward_attach_preserves_persistent_overlay_across_stateful_switch_refreshes() {
    let session_name = session_name("alpha");
    let (mut attach, control_tx) =
        AttachForwarder::unregistered(test_attach_target(&session_name, b"BASE-0", None));

    attach.assert_initial_render("BASE-0").await;

    control_tx
        .send(persistent_overlay_control(b"MENU-OLD", 0, 1, 7))
        .expect("send initial persistent overlay");
    let overlay = read_attach_data_until(&mut attach.peer, b"MENU-OLD").await;
    assert!(
        String::from_utf8_lossy(&overlay).contains("MENU-OLD"),
        "persistent overlay should be visible before the refresh"
    );

    control_tx
        .send(AttachControl::AdvancePersistentOverlayState(8))
        .expect("send overlay state advance");
    control_tx
        .send(switch_control(&session_name, b"BASE-1", Some(8)))
        .expect("send refreshed attach target");

    let refresh = read_attach_data_until(&mut attach.peer, b"MENU-OLD").await;
    let refresh_text = String::from_utf8_lossy(&refresh);
    assert!(
            refresh_text.contains("BASE-1") && refresh_text.contains("MENU-OLD"),
            "stateful choose-tree refresh should compose the refreshed base and cached overlay in one render frame: {refresh_text:?}"
        );
    assert!(
            !refresh_text.contains("\x1b[2J"),
            "stateful choose-tree refresh must not clear to the base pane before the replacement overlay: {refresh_text:?}"
        );

    attach.assert_stops_healthy().await;
}

#[tokio::test]
async fn forward_attach_counts_coalesced_switches_before_persistent_overlay() {
    let session_name = session_name("alpha");
    let (mut attach, control_tx) =
        AttachForwarder::unregistered(test_render_only_attach_target(&session_name, b"BASE-0"));

    attach.assert_initial_render("BASE-0").await;

    control_tx
        .send(AttachControl::switch(test_render_only_attach_target(
            &session_name,
            b"BASE-1",
        )))
        .expect("send prompt close refresh");
    control_tx
        .send(AttachControl::switch(test_render_only_attach_target(
            &session_name,
            b"BASE-2",
        )))
        .expect("send session mutation refresh");
    control_tx
        .send(AttachControl::switch(render_only(test_attach_target(
            &session_name,
            b"BASE-3",
            Some(8),
        ))))
        .expect("send mode-tree switch");
    control_tx
        .send(persistent_overlay_control(b"MENU-NEW", 3, 1, 8))
        .expect("send mode-tree overlay");

    let refresh = read_attach_data_until(&mut attach.peer, b"MENU-NEW").await;
    let refresh_text = String::from_utf8_lossy(&refresh);
    assert!(
        refresh_text.contains("BASE-3") && refresh_text.contains("MENU-NEW"),
        "coalesced switch generation must still match the pending overlay: {refresh_text:?}"
    );

    attach.assert_stops_healthy().await;
}

#[tokio::test]
async fn forward_attach_emits_overlay_control_frames() {
    let handler = Arc::new(RequestHandler::new());
    let attach_pid = std::process::id();
    let session_name = handler.create_session("alpha").await;
    handler
        .handle_ok(rmux_proto::SplitWindowRequest::fixture(&session_name))
        .await;
    handler
        .set_option(
            ScopeSelector::Session(session_name.clone()),
            OptionName::DisplayPanesTime,
            "5000",
        )
        .await;

    let (control_tx, control_rx) = mpsc::unbounded_channel();
    handler
        .register_attach(attach_pid, session_name.clone(), control_tx.clone())
        .await;

    let live_input = LiveAttachInputContext::current_for_test(handler, attach_pid).await;
    let mut attach = AttachForwarder::spawn(
        test_attach_target(&session_name, b"", None),
        control_rx,
        Arc::default(),
        live_input,
        false,
    );
    let peer = &mut attach.peer;

    let mut frame_bytes = [0_u8; 4096];
    let mut decoder = AttachFrameDecoder::new();
    while let Ok(Ok(bytes_read)) =
        tokio::time::timeout(Duration::from_millis(25), peer.read(&mut frame_bytes)).await
    {
        if bytes_read == 0 {
            break;
        }
        decoder.push_bytes(&frame_bytes[..bytes_read]);
        while decoder
            .next_message()
            .expect("decode initial attach frame")
            .is_some()
        {}
    }

    let overlay_marker = b"\x1b[s\x1b[?25l";
    let overlay_frame =
        OverlayFrame::new(b"\x1b[s\x1b[?25lDISPLAY-PANES\x1b[0m\x1b[u".to_vec(), 0, 1);
    control_tx
        .send(AttachControl::Overlay(overlay_frame))
        .expect("send overlay control");
    let mut collected = Vec::new();
    let overlay_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = overlay_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let read_timeout = remaining.min(Duration::from_millis(250));
        let bytes_read = match tokio::time::timeout(read_timeout, peer.read(&mut frame_bytes)).await
        {
            Ok(Ok(0)) => break,
            Ok(Ok(bytes_read)) => bytes_read,
            Ok(Err(error)) => panic!("read attach frame: {error}"),
            Err(_) => continue,
        };
        decoder.push_bytes(&frame_bytes[..bytes_read]);
        while let Some(message) = decoder.next_message().expect("decode attach frame") {
            match message {
                AttachMessage::Data(bytes) | AttachMessage::Render(bytes) => {
                    collected.extend_from_slice(&bytes)
                }
                _ => {}
            }
        }
        if contains_bytes(&collected, overlay_marker) {
            break;
        }
    }

    assert!(
        contains_bytes(&collected, overlay_marker),
        "overlay control should emit a frame, got: {:?}",
        String::from_utf8_lossy(&collected)
    );

    peer.shutdown().await.expect("close client peer");
    let result = attach.task.await.expect("attach task join");
    assert!(
        result.is_ok(),
        "forward_attach should stay healthy: {result:?}"
    );
}
