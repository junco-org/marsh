//! Combined M9 + M39: the three registration-side places where a *sizing*
//! order and a *session recency* order meet and must not be confused.
//!
//! M9 owns `ActiveAttach::size_sequence` — which attached client is the newest
//! sizing authority for a window. M39 owns `Session`'s `SessionRecency` — which
//! session was most recently used. They are allocated by different counters,
//! stored in different places, and answer different questions. Every test here
//! moves one and pins the other.
//!
//! `SessionRecency`'s allocator is a process-wide `static`, so parallel tests
//! mint tokens too; nothing below compares an absolute value or a global delta.

use super::*;

use super::sizeless_geometry::{attached_client_size_is_inferred, register_sizeless_attach};
use super::switch_frame_geometry::{attach_generation_id, window_content_size};

// This module's own pid block, distinct from every sibling module's.
const SIZELESS_PID: u32 = 94_301;
const STALE_PID: u32 = 94_311;
const RIVAL_PID: u32 = 94_312;
const FIRST_REHOME_PID: u32 = 94_321;
const SECOND_REHOME_PID: u32 = 94_322;

/// One arbitrary but fixed second shared by every session in a fixture, so no
/// public whole second can order a result that only the token may order.
const PINNED_SECOND: i64 = 1_785_500_000;

const ANCHOR_SIZE: TerminalSize = TerminalSize { cols: 80, rows: 24 };
/// The two rehoming clients. Different geometries, so which one lands last in
/// the destination is directly observable.
const LOW_SEQUENCE_SIZE: TerminalSize = TerminalSize {
    cols: 100,
    rows: 40,
};
const HIGH_SEQUENCE_SIZE: TerminalSize = TerminalSize {
    cols: 120,
    rows: 50,
};

async fn attached_size_sequence(handler: &RequestHandler, attach_pid: u32) -> u64 {
    handler
        .active_attach
        .lock()
        .await
        .by_pid
        .get(&attach_pid)
        .expect("the attached client is registered")
        .size_sequence
}

async fn pin_public_times(handler: &RequestHandler, sessions: &[&SessionName]) {
    let mut state = handler.state.lock().await;
    for session in sessions {
        state
            .sessions
            .session_mut(session)
            .expect("session exists")
            .pin_public_times_for_tests(PINNED_SECOND);
    }
}

/// Promoting an inferred anchor to a declared geometry is a *sizing* event and
/// nothing else.
///
/// M9 anchors a client that declared no size to its session's outer terminal
/// geometry and records the provenance. When that client later reports its real
/// geometry — even the numerically identical one — the declaration must take
/// its place in `window-size latest` ordering, so a fresh `size_sequence` is
/// minted. Under M39 the tempting mistake is to treat that as use of the
/// session too. It is not: nobody attached, and nobody typed.
#[tokio::test]
async fn promoting_an_inferred_client_size_does_not_touch_session_recency() {
    let handler = RequestHandler::new();
    let session = session_name("combined-promotion");
    handler.create_session((&session, ANCHOR_SIZE)).await;
    // A status line makes outer terminal geometry and content geometry differ,
    // which is the whole reason the inferred anchor is typed in the first place.
    handler.set_session_status(&session, "2").await;

    // No declared geometry: this is the sizeless client M9 anchors.
    let _control_rx = register_sizeless_attach(&handler, SIZELESS_PID, &session).await;

    assert!(
        attached_client_size_is_inferred(&handler, SIZELESS_PID).await,
        "a client that declared no size must be anchored, not trusted"
    );
    let sequence_before = attached_size_sequence(&handler, SIZELESS_PID).await;
    let recency_before = session_recency(&handler, &session).await;

    // The client now reports the real geometry of its terminal, which happens
    // to equal the anchor it was registered with. That is still a declaration.
    handler
        .handle_attached_resize(SIZELESS_PID, ANCHOR_SIZE)
        .await
        .expect("promoting resize succeeds");

    assert!(
        !attached_client_size_is_inferred(&handler, SIZELESS_PID).await,
        "a real resize leaves no inferred provenance behind"
    );
    assert!(
        attached_size_sequence(&handler, SIZELESS_PID).await > sequence_before,
        "promotion must mint a fresh sizing order even at identical dimensions"
    );
    assert_eq!(
        session_recency(&handler, &session).await,
        recency_before,
        "promotion is a sizing event: it is neither an attach nor an interaction, \
         so the session's recency order must not move"
    );
}

/// A registration that loses its pid to a newer generation while parked at the
/// credit seam credits nothing, and displaces nothing.
///
/// This exercises M39's `matches_active_lifetime` and M9's `AttachGeneration`
/// against each other on the same registration. The stale attach is still the
/// live entry *by pid*, so a pid-only test would credit it; it is a different
/// generation, so the lifetime predicate must reject it. Symmetrically the
/// replacement's sizing order must survive, because the generation predicate
/// must reject the stale registration as a displacer.
#[tokio::test]
async fn registration_credits_recency_once_across_a_concurrent_same_pid_replacement() {
    let handler = Arc::new(RequestHandler::new());
    let used = session_name("combined-used");
    let rival = session_name("combined-rival");
    handler.create_session((&used, ANCHOR_SIZE)).await;
    handler.create_session((&rival, ANCHOR_SIZE)).await;

    // Park the first registration on M39's own credit seam, after publication
    // and before the credit.
    let pause = handler.install_attach_registration_activity_pause();
    let (stale_tx, _stale_rx) = mpsc::unbounded_channel();
    let registering_handler = Arc::clone(&handler);
    let stale_session = used.clone();
    let registering = tokio::spawn(async move {
        registering_handler
            .register_attach(STALE_PID, stale_session, stale_tx)
            .await
    });
    tokio::time::timeout(ATTACH_LIFECYCLE_TIMEOUT, pause.reached.notified())
        .await
        .expect("attach registration reaches its activity commit");

    // The same pid re-registers as a brand-new generation with a legitimate
    // declared vote. This is the replacement the stale registration must not
    // speak for.
    let (_, _replacement_rx) =
        register_sized_attach(&handler, STALE_PID, &used, HIGH_SEQUENCE_SIZE).await;
    let replacement_generation = attach_generation_id(&handler, STALE_PID).await;
    let replacement_sequence = attached_size_sequence(&handler, STALE_PID).await;

    // Attaching to the rival makes it the most recently used session. It also
    // makes both candidates attached, which matters: targetless resolution
    // prefers an unattached session over a more recent one, so leaving the
    // rival unattached would let it win on that preference alone and the
    // assertion below would hold whatever the credit did.
    let (_, _rival_rx) =
        register_sized_attach(&handler, RIVAL_PID, &rival, LOW_SEQUENCE_SIZE).await;
    // Pin every public second so only the recency token can order the answer.
    pin_public_times(&handler, &[&used, &rival]).await;

    pause.release.notify_one();
    tokio::time::timeout(ATTACH_LIFECYCLE_TIMEOUT, registering)
        .await
        .expect("the stale registration finishes")
        .expect("the registration task does not panic");

    assert_eq!(
        handler
            .preferred_session_name()
            .await
            .expect("preferred session resolves"),
        rival,
        "the stale registration no longer owns this pid, so its credit must not \
         fire and push its session back to the front"
    );
    assert_eq!(
        attach_generation_id(&handler, STALE_PID).await,
        replacement_generation,
        "the replacement must still own the pid"
    );
    assert_eq!(
        attached_size_sequence(&handler, STALE_PID).await,
        replacement_sequence,
        "a stale generation must not displace the replacement's sizing order"
    );
}

/// Destroy/rehome reads two orders in one function and must not read either
/// through the other.
///
/// `destroy_switch_plans` ranks *sessions* by M39's `SessionRecency` to choose
/// where clients go, and ranks *clients* by M9's `(size_sequence, attach_id,
/// attach_pid)` to choose the order they move in. Both are captured under the
/// same lock pair before any rehome commit runs, which matters now that a
/// successful switch commit writes `size_sequence`: read afterwards, the client
/// order would be self-referential.
///
/// The fixture makes the two client orders disagree on purpose. The client that
/// registers *first* — and so holds the lower `attach_id` — is given the
/// *higher* sizing order by switching second. Ordering by `attach_id` would
/// therefore rehome it first and leave the destination on the other client's
/// geometry; ordering by the captured `size_sequence` rehomes it last and
/// leaves the destination on its own.
#[tokio::test]
async fn destroy_rehome_orders_clients_by_captured_size_sequence_and_sessions_by_recency() {
    let handler = RequestHandler::new();
    let doomed = session_name("combined-doomed");
    // Deliberately ordered so that name and creation id both disagree with
    // recency: `older` sorts first and is created first, but `newer` is used
    // last and must win.
    let older = session_name("combined-a-older");
    let newer = session_name("combined-z-newer");
    for session in [&doomed, &older, &newer] {
        handler.create_session((session, ANCHOR_SIZE)).await;
        // No status line, so a client's outer terminal geometry and the
        // window's content geometry are the same number and the assertion below
        // reads as the client that owns the window.
        handler.set_session_status(session, "off").await;
    }
    handler.set_window_size_policy(&newer, 0, "latest").await;
    // `off` is the "do not detach, switch to the most recently used session"
    // policy, which is the one that ranks survivors by recency.
    handler.set_detach_on_destroy_for_test(&doomed, "off").await;

    // `newer` is the most recently used survivor, by token alone.
    {
        let mut state = handler.state.lock().await;
        for session in [&older, &newer] {
            state
                .sessions
                .session_mut(session)
                .expect("survivor exists")
                .touch_attached();
        }
    }
    pin_public_times(&handler, &[&doomed, &older, &newer]).await;

    // Registered first, so this client holds the *lower* attach id.
    let (_, mut first_rx) =
        register_sized_attach(&handler, FIRST_REHOME_PID, &doomed, HIGH_SEQUENCE_SIZE).await;
    let (_, mut second_rx) =
        register_sized_attach(&handler, SECOND_REHOME_PID, &doomed, LOW_SEQUENCE_SIZE).await;

    // A switch to the session the client is already on still renews its sizing
    // order, which is how each client gets a `size_sequence` assigned by a real
    // switch commit. Switching the *second* client first gives it the lower
    // order, inverting the attach-id ranking.
    for pid in [SECOND_REHOME_PID, FIRST_REHOME_PID] {
        let switched = handler
            .dispatch(
                pid,
                Request::SwitchClient(SwitchClientRequest {
                    target: doomed.clone(),
                }),
            )
            .await;
        assert!(
            matches!(switched.response, Response::SwitchClient(_)),
            "the current-session switch must succeed, got {:?}",
            switched.response
        );
    }
    let first_sequence = attached_size_sequence(&handler, FIRST_REHOME_PID).await;
    let second_sequence = attached_size_sequence(&handler, SECOND_REHOME_PID).await;
    assert!(
        second_sequence < first_sequence,
        "the fixture must invert the two orders: the lower attach id has to hold \
         the higher sizing order, got {second_sequence} and {first_sequence}"
    );
    drain_attach_controls(&mut first_rx);
    drain_attach_controls(&mut second_rx);

    handler.handle_ok(KillSessionRequest::fixture(doomed)).await;

    // Sessions ranked by recency: both clients land on the survivor that was
    // used last, not the one that sorts first or was created first.
    for (label, control_rx) in [("first", &mut first_rx), ("second", &mut second_rx)] {
        let target = recv_switch_target(control_rx, "destroy rehome").await;
        assert_eq!(
            target.session_name, newer,
            "the {label} rehomed client must follow the recency order"
        );
    }

    // Clients ranked by their captured sizing order: the client that switched
    // last holds the highest `size_sequence`, so it commits last and owns the
    // destination under `latest`. Reading the plan through `attach_id` instead
    // would leave the destination on the other client's geometry.
    assert_eq!(
        window_content_size(&handler, &newer, 0).await,
        HIGH_SEQUENCE_SIZE,
        "the rehome order must follow the captured sizing order, not the attach ids"
    );
}
