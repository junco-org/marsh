//! Combined #180 + M39: what the key tables now swallow is still session use.
//!
//! M39 credits an attached client's interaction at the *admission* boundary in
//! `record_attached_input_activity`, before dispatch decides what the key means.
//! #180 changed what dispatch then does with it: the two hardcoded shims are
//! gone, so a key the table leaves unbound is swallowed by copy mode instead of
//! leaking to the pane, and a lone ESC is named `Escape` and routed through the
//! live key path.
//!
//! Both changes move the *consumer*, not the admission. The combined contract
//! is therefore that crediting is unchanged: an admitted key counts exactly
//! once whatever the table does with it afterwards — which is also what tmux
//! 3.7b does, calling `session_update_activity` while admitting a key and
//! before deciding its meaning.
//!
//! Unlike `SessionRecency`, the client activity order is allocated by a
//! per-handler counter, so these tests can count credits exactly rather than
//! only observing that one happened.

use super::*;

const PROBE: &str = "@probe-combined-input";
const CREDIT_PID: u32 = 94_401;

/// The per-client activity order, allocated by the handler's own counter.
async fn client_activity_sequence(handler: &RequestHandler, attach_pid: u32) -> u64 {
    handler
        .active_attach
        .lock()
        .await
        .by_pid
        .get(&attach_pid)
        .expect("the attached client is registered")
        .last_activity_sequence
}

async fn session_activity_at(handler: &RequestHandler, session: &rmux_proto::SessionName) -> i64 {
    handler
        .state
        .lock()
        .await
        .sessions
        .session(session)
        .expect("session exists")
        .activity_at()
}

/// A key the table leaves unbound is swallowed rather than forwarded, and still
/// counts as use of the session.
///
/// Before #180 the copy-mode shim answered these keys itself, so they always
/// "did something". Now an unbound copy-mode key is consumed and nothing runs.
/// The risk this pins is that "nothing ran" quietly becomes "nothing counted":
/// M39 credits at admission, so the client's activity order must still advance
/// even though the key produced no action at all.
#[tokio::test]
async fn an_unbound_copy_mode_key_still_counts_as_session_use() {
    let handler = RequestHandler::new();
    let alpha = session_name("combined-unbound-credit");
    let target = PaneTarget::new(alpha.clone(), 0);

    create_quiet_input_session(&handler, &alpha).await;
    let _control_rx = handler.attach_client(CREDIT_PID, &alpha).await;
    set_mode_keys(&handler, &alpha, "emacs").await;
    // Leave the key with no binding at all: there is no hardcoded fallback
    // behind the table any more, so this key can only be swallowed.
    handler
        .handle_ok(UnbindKeyRequest {
            table_name: "copy-mode".to_owned(),
            key: Some("Enter".to_owned()),
            all: false,
            quiet: true,
        })
        .await;
    handler.handle_ok(CopyModeRequest::fixture(&target)).await;

    let sequence_before = client_activity_sequence(&handler, CREDIT_PID).await;
    let activity_before = session_activity_at(&handler, &alpha).await;

    handler
        .handle_attached_live_input_for_test(CREDIT_PID, b"\r")
        .await
        .expect("attached live input succeeds");

    assert!(
        !pane_mode(&handler, &target).await.is_empty(),
        "an unbound copy-mode key must be swallowed, leaving the pane in copy mode"
    );
    assert_eq!(
        probe_value(&handler, PROBE).await,
        "",
        "an unbound key must run no command"
    );
    assert!(
        client_activity_sequence(&handler, CREDIT_PID).await > sequence_before,
        "M39 credits at admission, not at dispatch: a key the table swallows is \
         still an accepted interaction"
    );
    assert!(
        session_activity_at(&handler, &alpha).await >= activity_before,
        "the public activity second must not go backwards"
    );
}

/// #180's synthesized copy-mode Escape credits exactly once.
///
/// A lone ESC never decodes into a key, so #180 routes it through the escape
/// timeout flush, which names `Escape` and calls `handle_attached_live_key`.
/// That is a second entry into dispatch for one physical keypress. It must not
/// be a second entry into *admission*: `handle_attached_live_key` re-enters
/// dispatch below the live-input admission boundary, so exactly one credit is
/// owed. The client activity counter is per handler, so "exactly one" is
/// measured here rather than argued.
#[tokio::test]
async fn a_synthesized_copy_mode_escape_credits_activity_exactly_once() {
    for (mode_keys, table) in [("emacs", "copy-mode"), ("vi", "copy-mode-vi")] {
        let handler = RequestHandler::new();
        let alpha = session_name("combined-escape-credit");
        let target = PaneTarget::new(alpha.clone(), 0);

        create_quiet_input_session(&handler, &alpha).await;
        let _control_rx = handler.attach_client(CREDIT_PID, &alpha).await;
        set_mode_keys(&handler, &alpha, mode_keys).await;
        bind(
            &handler,
            table,
            "Escape",
            &["set-option", "-g", PROBE, "HIT-Escape"],
        )
        .await;
        handler.handle_ok(CopyModeRequest::fixture(&target)).await;

        let sequence_before = client_activity_sequence(&handler, CREDIT_PID).await;

        // The ESC byte is retained until escape-time expires, then flushed as a
        // named key. One physical keypress, two production entry points.
        let mut pending_input = Vec::new();
        handler
            .handle_attached_live_input(CREDIT_PID, &mut pending_input, b"\x1b")
            .await
            .expect("Escape prefix is retained until escape-time expires");
        assert_eq!(pending_input, b"\x1b", "{table}: the ESC byte is retained");
        let forwarded = handler
            .flush_attached_pending_escape_input(CREDIT_PID, &mut pending_input)
            .await
            .expect("Escape flush reaches copy mode");

        assert!(!forwarded, "{table}: Escape must not leak to the pane");
        assert_eq!(
            probe_value(&handler, PROBE).await,
            "HIT-Escape",
            "{table}: the user binding on Escape must run"
        );
        assert_eq!(
            client_activity_sequence(&handler, CREDIT_PID).await,
            sequence_before + 1,
            "{table}: one physical Escape is one admitted interaction, however \
             many dispatch entry points it passes through"
        );
    }
}
