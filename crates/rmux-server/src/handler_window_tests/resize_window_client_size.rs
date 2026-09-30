//! `resize-window -A` / `-a` selects a client size the way tmux 3.7b does.
//!
//! `cmd_resize_window_exec` hands both flags to `default_window_size(..,
//! WINDOW_SIZE_LARGEST | WINDOW_SIZE_SMALLEST)`, so the command shares the
//! automatic `window-size` selector: every client `ignore_client_size()`
//! rejects owns nothing, each remaining client's status rows come off its outer
//! terminal rows before it competes, and each dimension keeps its own extreme.
//!
//! Measured against the pinned tmux 3.7b (binary sha256 `eb05f981…`, the
//! `frozen_reference.yaml` build) on macOS 26.5.2 arm64 with real PTY clients of
//! the stated winsize. Every run first pinned the window with an explicit
//! `resize-window -x 70 -y 18`, so the reported geometry is what the flag itself
//! chose:
//!
//! ```text
//! status=2    client 80x24                      -A -> 80x22    -a -> 80x22
//! status=2    clients 80x24 + 100x40            -A -> 100x38   -a -> 80x22
//! status=off  ignore-size 80x24 + 100x40        -A -> 100x40   -a -> 100x40
//! status=off  read-only   80x24 + 100x40        -A -> 100x40   -a -> 100x40
//! status=off  clients 100x20 + 80x40            -A -> 100x40   -a -> 80x20
//! status=off  only an ignore-size 80x24 client  -A -> 80x24    -a -> 80x24
//! status=off  client 100x40, unlinked session   -A -> 80x24    -a -> 80x24
//! status=2    client 100x40 on a linked session -A -> 100x38   -a -> 100x38
//! ```
//!
//! The last two land on tmux's `manual:` fallback, which reads the `default-size`
//! option (`80x24` by default) without touching the status rows. rmux uses the
//! target session's own outer terminal size there; the fallback tests below keep
//! an 80x24 session so the two agree exactly.
//!
//! `status_line_size(loop)` reads the options of the session **that client** is
//! attached to, so a window linked into two sessions with different `status`
//! values converts each client with its own. Measured on the same oracle with
//! target `alpha:0` linked into `beta` and one 100x40 client on `beta`:
//!
//! ```text
//! alpha=2    beta=off   -A -> 100x40   -a -> 100x40
//! alpha=off  beta=2     -A -> 100x38   -a -> 100x38
//! alpha=on   beta=3     -A -> 100x37
//! alpha=3    beta=on                   -a -> 100x39
//! ```
//!
//! and with a 100x40 client on `alpha` competing against a 90x39 client on
//! `beta`, which pins the per-dimension extrema against per-client content rows:
//!
//! ```text
//! alpha=2    beta=off   -A -> 100x39   -a -> 90x38
//! alpha=off  beta=2     -A -> 100x40   -a -> 90x37
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::*;
use crate::client_flags::ClientFlags;
use crate::outer_terminal::OuterTerminalContext;
use crate::test_fixtures::{SessionSpec, TestRequest};

const STATUS_TWO: &str = "2";
const STATUS_OFF: &str = "off";

/// The outer terminal of every session here, which is also where the fallback lands.
const SESSION_SIZE: TerminalSize = TerminalSize::new(80, 24);
/// The explicit geometry every run pins the window at before the flag under test.
const PINNED_SIZE: TerminalSize = TerminalSize::new(70, 18);

#[tokio::test]
async fn resize_window_largest_takes_the_status_rows_off_the_outer_client_terminal() {
    status_two_single_client_case(ResizeWindowAdjustment::LargestLinkedSession).await;
}

#[tokio::test]
async fn resize_window_smallest_takes_the_status_rows_off_the_outer_client_terminal() {
    status_two_single_client_case(ResizeWindowAdjustment::SmallestLinkedSession).await;
}

/// One 80x24 client and a two-line status: the window must become 80x22
/// content, never the client's outer 80x24.
async fn status_two_single_client_case(adjustment: ResizeWindowAdjustment) {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, ("alpha", SESSION_SIZE)).await;
    let target = WindowTarget::with_window(alpha.clone(), 0);
    handler.set_session_status(&alpha, STATUS_TWO).await;
    let _client = attach_declared_client(&handler, 60_100, &alpha, TerminalSize::new(80, 24)).await;

    pin_window_size(&handler, &target).await;
    apply_adjustment(&handler, &target, adjustment).await;

    assert_eq!(
        window_size(&handler, &target).await,
        TerminalSize::new(80, 22),
        "{adjustment:?} must convert the client's outer terminal rows to \
         content rows exactly once"
    );
}

#[tokio::test]
async fn resize_window_ranks_clients_by_their_content_rows() {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, ("alpha", SESSION_SIZE)).await;
    let target = WindowTarget::with_window(alpha.clone(), 0);
    handler.set_session_status(&alpha, STATUS_TWO).await;
    let _small = attach_declared_client(&handler, 60_110, &alpha, TerminalSize::new(80, 24)).await;
    let _large = attach_declared_client(&handler, 60_111, &alpha, TerminalSize::new(100, 40)).await;

    for (adjustment, expected) in [
        (
            ResizeWindowAdjustment::LargestLinkedSession,
            TerminalSize::new(100, 38),
        ),
        (
            ResizeWindowAdjustment::SmallestLinkedSession,
            TerminalSize::new(80, 22),
        ),
    ] {
        pin_window_size(&handler, &target).await;
        apply_adjustment(&handler, &target, adjustment).await;

        assert_eq!(
            window_size(&handler, &target).await,
            expected,
            "{adjustment:?} must rank the clients after each one lost its own \
             status rows"
        );
    }
}

#[tokio::test]
async fn resize_window_takes_each_dimension_from_its_own_client() {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, ("alpha", SESSION_SIZE)).await;
    let target = WindowTarget::with_window(alpha.clone(), 0);
    handler.set_session_status(&alpha, STATUS_OFF).await;
    let _wide = attach_declared_client(&handler, 60_120, &alpha, TerminalSize::new(100, 20)).await;
    let _tall = attach_declared_client(&handler, 60_121, &alpha, TerminalSize::new(80, 40)).await;

    for (adjustment, expected) in [
        (
            ResizeWindowAdjustment::LargestLinkedSession,
            TerminalSize::new(100, 40),
        ),
        (
            ResizeWindowAdjustment::SmallestLinkedSession,
            TerminalSize::new(80, 20),
        ),
    ] {
        pin_window_size(&handler, &target).await;
        apply_adjustment(&handler, &target, adjustment).await;

        assert_eq!(
            window_size(&handler, &target).await,
            expected,
            "{adjustment:?} must take the extreme of each dimension on its own, \
             not one client's whole size"
        );
    }
}

/// Every way a client can be barred from sizing: `resize-window -a` must keep
/// the eligible 100x40 peer instead of shrinking to the barred 80x24 client.
///
/// Each kind is reported rather than asserted in place, so one run names every
/// kind that regressed instead of stopping at the first.
#[tokio::test]
async fn resize_window_gives_no_sizing_authority_to_an_ineligible_client() {
    let eligible_size = TerminalSize::new(100, 40);
    let mut regressions = Vec::new();
    for kind in [
        IneligibleClient::IgnoreSize,
        IneligibleClient::ReadOnly,
        IneligibleClient::Closing,
        IneligibleClient::Suspended,
        IneligibleClient::StaleSessionIdentity,
    ] {
        let handler = RequestHandler::new();
        let alpha = SessionSpec::create(&handler, ("alpha", SESSION_SIZE)).await;
        let target = WindowTarget::with_window(alpha.clone(), 0);
        handler.set_session_status(&alpha, STATUS_OFF).await;
        let _eligible = attach_declared_client(&handler, 60_130, &alpha, eligible_size).await;
        let barred = attach_flagged_client(
            &handler,
            60_131,
            &alpha,
            TerminalSize::new(80, 24),
            kind.flags(),
        )
        .await;
        // Bar the client only after the window is pinned: a closing client is
        // reaped by the refresh a resize publishes, and the point here is that
        // the selector rejects it while it is still registered.
        pin_window_size(&handler, &target).await;
        kind.bar_client(&handler, 60_131, &barred).await;
        apply_adjustment(
            &handler,
            &target,
            ResizeWindowAdjustment::SmallestLinkedSession,
        )
        .await;

        let selected = window_size(&handler, &target).await;
        if selected != eligible_size {
            regressions.push(format!("{kind:?} won with {selected:?}"));
        }
    }

    assert!(
        regressions.is_empty(),
        "no ineligible client may win `resize-window -a`, but {regressions:?}"
    );
}

/// The eligibility filter must not swallow the neighbours: with an ignored
/// client present, the *smallest eligible* client still wins `-a` and the
/// largest still wins `-A`.
#[tokio::test]
async fn resize_window_still_ranks_the_eligible_neighbours_of_an_ignored_client() {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, ("alpha", SESSION_SIZE)).await;
    let target = WindowTarget::with_window(alpha.clone(), 0);
    handler.set_session_status(&alpha, STATUS_OFF).await;
    let _ignored = attach_flagged_client(
        &handler,
        60_140,
        &alpha,
        TerminalSize::new(200, 60),
        ClientFlags::IGNORESIZE,
    )
    .await;
    let _small = attach_declared_client(&handler, 60_141, &alpha, TerminalSize::new(80, 24)).await;
    let _large = attach_declared_client(&handler, 60_142, &alpha, TerminalSize::new(100, 40)).await;

    for (adjustment, expected) in [
        (
            ResizeWindowAdjustment::LargestLinkedSession,
            TerminalSize::new(100, 40),
        ),
        (
            ResizeWindowAdjustment::SmallestLinkedSession,
            TerminalSize::new(80, 24),
        ),
    ] {
        pin_window_size(&handler, &target).await;
        apply_adjustment(&handler, &target, adjustment).await;

        assert_eq!(
            window_size(&handler, &target).await,
            expected,
            "{adjustment:?} must still rank the eligible clients around an \
             ignore-size client"
        );
    }
}

#[tokio::test]
async fn resize_window_without_an_eligible_client_falls_back_to_the_session_terminal_size() {
    for adjustment in [
        ResizeWindowAdjustment::LargestLinkedSession,
        ResizeWindowAdjustment::SmallestLinkedSession,
    ] {
        let handler = RequestHandler::new();
        let alpha = SessionSpec::create(&handler, ("alpha", SESSION_SIZE)).await;
        let target = WindowTarget::with_window(alpha.clone(), 0);
        handler.set_session_status(&alpha, STATUS_OFF).await;
        let _ignored = attach_flagged_client(
            &handler,
            60_150,
            &alpha,
            TerminalSize::new(132, 50),
            ClientFlags::IGNORESIZE,
        )
        .await;

        pin_window_size(&handler, &target).await;
        apply_adjustment(&handler, &target, adjustment).await;

        assert_eq!(
            window_size(&handler, &target).await,
            SESSION_SIZE,
            "with no eligible client {adjustment:?} must fall back instead of \
             borrowing the ignored client's geometry"
        );
    }
}

#[tokio::test]
async fn resize_window_selects_a_client_attached_to_a_linked_session() {
    let handler = RequestHandler::new();
    let alpha = SessionSpec::create(&handler, ("alpha", SESSION_SIZE)).await;
    let beta = SessionSpec::create(&handler, ("beta", SESSION_SIZE)).await;
    let target = WindowTarget::with_window(alpha.clone(), 0);
    handler.set_session_status(&alpha, STATUS_TWO).await;
    handler.set_session_status(&beta, STATUS_TWO).await;
    TestRequest::send_ok(
        &handler,
        LinkWindowRequest {
            detached: false,
            ..Fixture::fixture((&target, WindowTarget::with_window(beta.clone(), 1)))
        },
    )
    .await;
    let _client = attach_declared_client(&handler, 60_160, &beta, TerminalSize::new(100, 40)).await;

    pin_window_size(&handler, &target).await;
    apply_adjustment(
        &handler,
        &target,
        ResizeWindowAdjustment::LargestLinkedSession,
    )
    .await;

    assert_eq!(
        window_size(&handler, &target).await,
        TerminalSize::new(100, 38),
        "a client attached to a linked session still owns the window size, \
         minus its status rows"
    );
}

/// Both crossed directions, for both flags, with a client that declared its
/// outer 100x40 terminal: the converting status is the one of the session the
/// client is attached to, never the resize target's.
#[tokio::test]
async fn resize_window_converts_a_declared_linked_client_with_its_own_session_status() {
    let mut regressions = Vec::new();
    for (target_status, client_status, expected) in crossed_status_expectations() {
        for adjustment in [
            ResizeWindowAdjustment::LargestLinkedSession,
            ResizeWindowAdjustment::SmallestLinkedSession,
        ] {
            let handler = RequestHandler::new();
            let (alpha, beta) =
                crossed_status_sessions(&handler, target_status, client_status, SESSION_SIZE).await;
            let target = WindowTarget::with_window(alpha, 0);
            let _client =
                attach_declared_client(&handler, 60_170, &beta, TerminalSize::new(100, 40)).await;

            pin_window_size(&handler, &target).await;
            apply_adjustment(&handler, &target, adjustment).await;

            let selected = window_size(&handler, &target).await;
            if selected != expected {
                regressions.push(format!(
                    "target status={target_status} client status={client_status} \
                     {adjustment:?}: expected {expected:?}, got {selected:?}"
                ));
            }
        }
    }

    assert!(
        regressions.is_empty(),
        "each client's outer rows lose the status rows of its own session, but \
         {regressions:?}"
    );
}

/// The same crossed matrix for a client that never declared a size: its anchor
/// is its own session's outer terminal geometry, and that geometry converts with
/// that same session's status.
#[tokio::test]
async fn resize_window_converts_a_sizeless_linked_client_with_its_own_session_status() {
    let client_size = TerminalSize::new(100, 40);
    let mut regressions = Vec::new();
    for (target_status, client_status, expected) in crossed_status_expectations() {
        for adjustment in [
            ResizeWindowAdjustment::LargestLinkedSession,
            ResizeWindowAdjustment::SmallestLinkedSession,
        ] {
            let handler = RequestHandler::new();
            let (alpha, beta) =
                crossed_status_sessions(&handler, target_status, client_status, client_size).await;
            let target = WindowTarget::with_window(alpha, 0);
            let _client =
                attach_sizeless_client(&handler, 60_180, &beta, ClientFlags::default()).await;
            assert_inferred_client_size(&handler, 60_180, client_size).await;

            pin_window_size(&handler, &target).await;
            apply_adjustment(&handler, &target, adjustment).await;

            let selected = window_size(&handler, &target).await;
            if selected != expected {
                regressions.push(format!(
                    "target status={target_status} client status={client_status} \
                     {adjustment:?}: expected {expected:?}, got {selected:?}"
                ));
            }
        }
    }

    assert!(
        regressions.is_empty(),
        "a sizeless client votes with its own session's outer geometry and \
         status, but {regressions:?}"
    );
}

/// A client on the target and a client on the linked session compete while their
/// sessions disagree about `status`: each dimension keeps its own extreme, taken
/// after each client lost *its own* status rows.
#[tokio::test]
async fn resize_window_ranks_crossed_status_clients_by_their_own_content_rows() {
    let mut regressions = Vec::new();
    for (target_status, client_status, largest, smallest) in [
        // alpha's 100x40 -> 38 content rows, beta's 90x39 -> 39.
        (
            STATUS_TWO,
            STATUS_OFF,
            TerminalSize::new(100, 39),
            TerminalSize::new(90, 38),
        ),
        // alpha's 100x40 -> 40 content rows, beta's 90x39 -> 37.
        (
            STATUS_OFF,
            STATUS_TWO,
            TerminalSize::new(100, 40),
            TerminalSize::new(90, 37),
        ),
    ] {
        for (adjustment, expected) in [
            (ResizeWindowAdjustment::LargestLinkedSession, largest),
            (ResizeWindowAdjustment::SmallestLinkedSession, smallest),
        ] {
            let handler = RequestHandler::new();
            let (alpha, beta) =
                crossed_status_sessions(&handler, target_status, client_status, SESSION_SIZE).await;
            let target = WindowTarget::with_window(alpha.clone(), 0);
            let _on_target =
                attach_declared_client(&handler, 60_190, &alpha, TerminalSize::new(100, 40)).await;
            let _on_linked =
                attach_declared_client(&handler, 60_191, &beta, TerminalSize::new(90, 39)).await;

            pin_window_size(&handler, &target).await;
            apply_adjustment(&handler, &target, adjustment).await;

            let selected = window_size(&handler, &target).await;
            if selected != expected {
                regressions.push(format!(
                    "target status={target_status} linked status={client_status} \
                     {adjustment:?}: expected {expected:?}, got {selected:?}"
                ));
            }
        }
    }

    assert!(
        regressions.is_empty(),
        "each dimension keeps its own extreme of the per-client content rows, \
         but {regressions:?}"
    );
}

/// A session reached through the window's *session group* is a linked alias, and
/// it resolves its own `status` exactly like an explicitly linked session.
#[tokio::test]
async fn resize_window_converts_a_session_group_alias_client_with_its_own_status() {
    let mut regressions = Vec::new();
    for (target_status, client_status, expected) in crossed_status_expectations() {
        for adjustment in [
            ResizeWindowAdjustment::LargestLinkedSession,
            ResizeWindowAdjustment::SmallestLinkedSession,
        ] {
            let handler = RequestHandler::new();
            let alpha = SessionSpec::create(&handler, ("alpha", SESSION_SIZE)).await;
            let beta = session_name("beta");
            let target = WindowTarget::with_window(alpha.clone(), 0);
            handler
                .state
                .lock()
                .await
                .sessions
                .create_grouped_session_with_base_index(
                    beta.clone(),
                    SESSION_SIZE,
                    0,
                    alpha.clone(),
                )
                .expect("grouped session creation succeeds");
            handler.set_session_status(&alpha, target_status).await;
            handler.set_session_status(&beta, client_status).await;
            let _client =
                attach_declared_client(&handler, 60_200, &beta, TerminalSize::new(100, 40)).await;

            pin_window_size(&handler, &target).await;
            apply_adjustment(&handler, &target, adjustment).await;

            let selected = window_size(&handler, &target).await;
            if selected != expected {
                regressions.push(format!(
                    "target status={target_status} alias status={client_status} \
                     {adjustment:?}: expected {expected:?}, got {selected:?}"
                ));
            }
        }
    }

    assert!(
        regressions.is_empty(),
        "a session-group alias converts its own clients with its own status, \
         but {regressions:?}"
    );
}

/// `-A`/`-a` and the automatic `window-size` policies share one selector, so the
/// sibling callers must convert per client too. Measured on the same oracle with
/// a 100x40 client on the linked session and no `resize-window` at all:
///
/// ```text
/// window-size=largest   alpha=2    beta=off  -> 100x40
/// window-size=largest   alpha=off  beta=2    -> 100x38
/// window-size=smallest  alpha=2    beta=off  -> 100x40
/// window-size=smallest  alpha=off  beta=2    -> 100x38
/// window-size=latest    alpha=2    beta=off  -> 100x40
/// window-size=latest    alpha=off  beta=2    -> 100x38
/// ```
#[tokio::test]
async fn automatic_window_size_policies_convert_a_linked_client_with_its_own_status() {
    let mut regressions = Vec::new();
    for (target_status, client_status, expected) in crossed_status_expectations() {
        for policy in ["largest", "smallest", "latest"] {
            let handler = RequestHandler::new();
            let (alpha, beta) =
                crossed_status_sessions(&handler, target_status, client_status, SESSION_SIZE).await;
            let _client =
                attach_declared_client(&handler, 60_220, &beta, TerminalSize::new(100, 40)).await;

            handler.set_window_size_policy(&alpha, 0, policy).await;

            let selected = window_size(&handler, &WindowTarget::with_window(alpha, 0)).await;
            if selected != expected {
                regressions.push(format!(
                    "window-size={policy} target status={target_status} client \
                     status={client_status}: expected {expected:?}, got {selected:?}"
                ));
            }
        }
    }

    assert!(
        regressions.is_empty(),
        "the automatic policies share the command's per-client conversion, but \
         {regressions:?}"
    );
}

/// Resolving a client's status through its session identity must not become a
/// way in: a client whose session id no longer matches the linked session owns
/// nothing, and the command falls back instead of converting it with any status.
#[tokio::test]
async fn resize_window_rejects_a_stale_identity_client_on_a_crossed_status_session() {
    for adjustment in [
        ResizeWindowAdjustment::LargestLinkedSession,
        ResizeWindowAdjustment::SmallestLinkedSession,
    ] {
        let handler = RequestHandler::new();
        let (alpha, beta) =
            crossed_status_sessions(&handler, STATUS_TWO, STATUS_OFF, SESSION_SIZE).await;
        let target = WindowTarget::with_window(alpha, 0);
        let _client =
            attach_declared_client(&handler, 60_210, &beta, TerminalSize::new(100, 40)).await;

        pin_window_size(&handler, &target).await;
        IneligibleClient::StaleSessionIdentity
            .bar_client(&handler, 60_210, &_client)
            .await;
        apply_adjustment(&handler, &target, adjustment).await;

        assert_eq!(
            window_size(&handler, &target).await,
            SESSION_SIZE,
            "{adjustment:?} must fall back rather than let a stale linked \
             identity vote"
        );
    }
}

/// The measured crossed matrix, as `(target status, client session status,
/// expected content geometry)` for a 100x40 client on the linked session.
const fn crossed_status_expectations() -> [(&'static str, &'static str, TerminalSize); 2] {
    [
        (STATUS_TWO, STATUS_OFF, TerminalSize::new(100, 40)),
        (STATUS_OFF, STATUS_TWO, TerminalSize::new(100, 38)),
    ]
}

/// `alpha` is the resize target and `beta` is a second session the target window
/// is linked into. Both statuses are set before any client attaches, so a
/// sizeless client anchors to the geometry it will really vote with.
async fn crossed_status_sessions(
    handler: &RequestHandler,
    target_status: &str,
    linked_status: &str,
    linked_size: TerminalSize,
) -> (SessionName, SessionName) {
    let alpha = SessionSpec::create(handler, ("alpha", SESSION_SIZE)).await;
    let beta = SessionSpec::create(handler, ("beta", linked_size)).await;
    handler.set_session_status(&alpha, target_status).await;
    handler.set_session_status(&beta, linked_status).await;
    TestRequest::send_ok(
        handler,
        LinkWindowRequest {
            detached: false,
            ..Fixture::fixture((
                WindowTarget::with_window(alpha.clone(), 0),
                WindowTarget::with_window(beta.clone(), 1),
            ))
        },
    )
    .await;
    (alpha, beta)
}

#[derive(Debug, Clone, Copy)]
enum IneligibleClient {
    IgnoreSize,
    ReadOnly,
    Closing,
    Suspended,
    StaleSessionIdentity,
}

impl IneligibleClient {
    fn flags(self) -> ClientFlags {
        match self {
            Self::IgnoreSize => ClientFlags::IGNORESIZE,
            Self::ReadOnly => ClientFlags::default().with_read_only(),
            Self::Closing | Self::Suspended | Self::StaleSessionIdentity => ClientFlags::default(),
        }
    }

    async fn bar_client(self, handler: &RequestHandler, pid: u32, client: &AttachedTestClient) {
        match self {
            Self::IgnoreSize | Self::ReadOnly => {}
            Self::Closing => client.closing.store(true, Ordering::SeqCst),
            Self::Suspended => {
                let mut active_attach = handler.active_attach.lock().await;
                active_attach
                    .by_pid
                    .get_mut(&pid)
                    .expect("registered attach exists")
                    .suspended = true;
            }
            Self::StaleSessionIdentity => {
                let mut active_attach = handler.active_attach.lock().await;
                let active = active_attach
                    .by_pid
                    .get_mut(&pid)
                    .expect("registered attach exists");
                active.session_id = rmux_proto::SessionId::new(active.session_id.as_u32() + 1_000);
            }
        }
    }
}

/// Keeps the attach's control channel and closing flag alive for the test.
struct AttachedTestClient {
    _control_rx: mpsc::UnboundedReceiver<AttachControl>,
    closing: Arc<AtomicBool>,
}

/// Registers a default-flag client that declared its outer terminal `size`.
async fn attach_declared_client(
    handler: &RequestHandler,
    pid: u32,
    session: &SessionName,
    size: TerminalSize,
) -> AttachedTestClient {
    attach_flagged_client(handler, pid, session, size, ClientFlags::default()).await
}

/// Registers a client with `flags` that declared its outer terminal `size`.
async fn attach_flagged_client(
    handler: &RequestHandler,
    pid: u32,
    session: &SessionName,
    size: TerminalSize,
    flags: ClientFlags,
) -> AttachedTestClient {
    let client = attach_sizeless_client(handler, pid, session, flags).await;
    handler
        .active_attach
        .lock()
        .await
        .by_pid
        .get_mut(&pid)
        .expect("registered attach exists")
        .set_declared_client_size(size);
    client
}

/// Registers a client that never declares a size, so registration anchors it to
/// its own session's outer terminal geometry.
async fn attach_sizeless_client(
    handler: &RequestHandler,
    pid: u32,
    session: &SessionName,
    flags: ClientFlags,
) -> AttachedTestClient {
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let closing = Arc::new(AtomicBool::new(false));
    handler
        .register_attach_with_closing(
            pid,
            session.clone(),
            control_tx,
            Arc::clone(&closing),
            OuterTerminalContext::default(),
            flags,
        )
        .await;
    AttachedTestClient {
        _control_rx: control_rx,
        closing,
    }
}

/// Proves the client really is the inferred kind, so the crossed-status result
/// below is the sizeless path's and not a declared client's.
async fn assert_inferred_client_size(handler: &RequestHandler, pid: u32, expected: TerminalSize) {
    let active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .get(&pid)
        .expect("registered attach exists");
    assert!(
        active.client_size_is_inferred(),
        "the client must still carry its inferred anchor"
    );
    assert_eq!(
        active.client_size, expected,
        "a sizeless client anchors to its own session's outer terminal geometry"
    );
}

/// Pins the window at [`PINNED_SIZE`], which also parks `window-size` on `manual`
/// so only the command under test can move it again.
async fn pin_window_size(handler: &RequestHandler, target: &WindowTarget) {
    TestRequest::send_ok(
        handler,
        ResizeWindowRequest {
            target: target.clone(),
            width: Some(PINNED_SIZE.cols),
            height: Some(PINNED_SIZE.rows),
            adjustment: None,
        },
    )
    .await;
    assert_eq!(window_size(handler, target).await, PINNED_SIZE);
}

async fn apply_adjustment(
    handler: &RequestHandler,
    target: &WindowTarget,
    adjustment: ResizeWindowAdjustment,
) {
    TestRequest::send_ok(
        handler,
        ResizeWindowRequest {
            target: target.clone(),
            width: None,
            height: None,
            adjustment: Some(adjustment),
        },
    )
    .await;
}

async fn window_size(handler: &RequestHandler, target: &WindowTarget) -> TerminalSize {
    let state = handler.state.lock().await;
    state
        .sessions
        .session(target.session_name())
        .and_then(|session| session.window_at(target.window_index()))
        .expect("window exists")
        .size()
}
