//! Final-sink proof for attached bracketed paste on the active pane.
//!
//! Unlike the sibling bracketed-paste suites, nothing here installs
//! `RawPaneInputProbe`. That probe returns `PaneInputSink::CapturedForTest`
//! before the starting-pane queue and before the pane's real shell sink is
//! resolved, so it cannot observe whether the real destination received the
//! delimiters. Each test below runs a real child process in the pane and
//! compares the bytes that child actually read from its standard input.
//!
//! The slot/child machinery lives in [`crate::test_shell::final_sink`] because
//! the deferred-start proof in `handler_session` needs the same child and
//! cannot reach into this private test tree.

use std::time::Instant;

use super::*;
use crate::test_shell::final_sink::{
    create_final_sink_session, describe_missing_bracketed_mode, observe_pane_output,
    split_final_sink_pane, FinalSinkSlot,
};

const OPEN: &[u8] = b"\x1b[200~";
const CLOSE: &[u8] = b"\x1b[201~";

/// CR/LF, `β`, `😀` and control-looking bytes a naive VT filter would eat.
const RICH_BODY: &str = "alpha\r\nβ 😀 \u{2} \u{1b}[9;2u \u{1b}[<64;2;2M omega";

fn wrapped(body: &[u8]) -> Vec<u8> {
    let mut wrapped = Vec::with_capacity(OPEN.len() + body.len() + CLOSE.len());
    wrapped.extend_from_slice(OPEN);
    wrapped.extend_from_slice(body);
    wrapped.extend_from_slice(CLOSE);
    wrapped
}

/// How long a real child is given to publish its capability through pane output.
const ANNOUNCEMENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Waits until the server transcript has observed the child's real DECSET 2004
/// announcement, so routing is decided from production state.
///
/// An expired wait reports what that transcript had actually observed. The
/// deadline alone said only that the mode never changed, which reads the same
/// for a pane that received no child output, one that received output carrying
/// no announcement, and one whose terminal handed it the announcement as
/// screen text.
pub(super) async fn wait_for_bracketed_mode(
    handler: &RequestHandler,
    target: &PaneTarget,
    expected: bool,
) {
    let deadline = Instant::now() + ANNOUNCEMENT_TIMEOUT;
    loop {
        {
            let state = handler.state.lock().await;
            let pane_mode = state
                .sessions
                .session(target.session_name())
                .and_then(|session| session.window_at(target.window_index()))
                .and_then(|window| window.pane(target.pane_index()))
                .map(rmux_core::Pane::id)
                .and_then(|pane_id| state.pane_screen_state(target.session_name(), pane_id))
                .map(|screen_state| screen_state.mode)
                .unwrap_or_default();
            if (pane_mode & mode::MODE_BRACKETPASTE != 0) == expected {
                return;
            }
            // Read under the same lock as the mode above, so the report and the
            // decision it explains describe one state of the transcript.
            if Instant::now() >= deadline {
                panic!(
                    "{}",
                    describe_missing_bracketed_mode(
                        &target.to_string(),
                        expected,
                        ANNOUNCEMENT_TIMEOUT,
                        &observe_pane_output(&state, target),
                    )
                );
            }
        }
        sleep(Duration::from_millis(25)).await;
    }
}

pub(super) async fn attach_to(handler: &RequestHandler, session: &rmux_proto::SessionName) -> u32 {
    let requester_pid = std::process::id();
    let _control_rx = handler.attach_client(requester_pid, session).await;
    requester_pid
}

async fn set_synchronize_panes(handler: &RequestHandler, session: &rmux_proto::SessionName) {
    let scope = ScopeSelector::Window(WindowTarget::with_window(session.clone(), 0));
    handler
        .set_option(scope, OptionName::SynchronizePanes, "on")
        .await;
}

/// Creates the session, waits for the pane child and returns its target.
async fn started_final_sink_pane(
    handler: &RequestHandler,
    session: &rmux_proto::SessionName,
    slot: &FinalSinkSlot,
    bracket_aware: bool,
) -> PaneTarget {
    create_final_sink_session(handler, session, slot).await;
    let target = PaneTarget::with_window(session.clone(), 0, 0);
    handler
        .wait_for_pane_startup_to_finish_for_test(&target)
        .await;
    slot.wait_until_ready();
    wait_for_bracketed_mode(handler, &target, bracket_aware).await;
    target
}

// ---------------------------------------------------------------------------
// Active destination
// ---------------------------------------------------------------------------

#[tokio::test]
async fn active_aware_pane_child_receives_the_bracketed_paste_delimiters() {
    let handler = RequestHandler::new();
    let session = session_name("final-sink-aware");
    let expected = wrapped(RICH_BODY.as_bytes());
    let slot = FinalSinkSlot::new("aware", &expected, true);

    started_final_sink_pane(&handler, &session, &slot, true).await;
    let requester_pid = attach_to(&handler, &session).await;
    handler
        .handle_attached_live_input_for_test(requester_pid, &expected)
        .await
        .expect("attached bracketed paste");

    slot.assert_application_bytes(
        "a bracketed-paste-aware child must receive ESC[200~ + body + ESC[201~ verbatim",
    );
}

#[tokio::test]
async fn active_unaware_pane_child_receives_only_the_paste_body() {
    let handler = RequestHandler::new();
    let session = session_name("final-sink-unaware");
    let body = RICH_BODY.as_bytes();
    let slot = FinalSinkSlot::new("unaware", body, false);

    started_final_sink_pane(&handler, &session, &slot, false).await;
    let requester_pid = attach_to(&handler, &session).await;
    handler
        .handle_attached_live_input_for_test(requester_pid, &wrapped(body))
        .await
        .expect("attached bracketed paste");

    slot.assert_application_bytes("an unaware child must receive the body with no delimiters");
}

/// A body of several kibibytes, with a supplementary character and CR/LF
/// inside, so a truncating, splitting or re-ordering writer becomes visible in
/// the child's bytes.
fn multi_kibibyte_body() -> String {
    let mut body = String::from("β😀\r\n");
    while body.len() < 2_100 {
        body.push_str("0123456789");
    }
    body.push_str("\r\nEND😀");
    body
}

#[tokio::test]
async fn bracketed_paste_child_receives_a_multi_kibibyte_body_whole() {
    let handler = RequestHandler::new();
    let session = session_name("final-sink-batch");
    let expected = wrapped(multi_kibibyte_body().as_bytes());
    let slot = FinalSinkSlot::new("batch", &expected, true);

    started_final_sink_pane(&handler, &session, &slot, true).await;
    let requester_pid = attach_to(&handler, &session).await;
    handler
        .handle_attached_live_input_for_test(requester_pid, &expected)
        .await
        .expect("attached bracketed paste");

    slot.assert_application_bytes("a payload of several kibibytes must arrive whole");
}

#[tokio::test]
async fn split_bracketed_paste_delimiters_reach_the_child_unchanged() {
    let handler = RequestHandler::new();
    let session = session_name("final-sink-split");
    let expected = wrapped(b"split-markers");
    let slot = FinalSinkSlot::new("split", &expected, true);

    started_final_sink_pane(&handler, &session, &slot, true).await;
    let requester_pid = attach_to(&handler, &session).await;

    // Representative interior splits of both delimiters, which is what a real
    // client produces when a paste straddles read boundaries.
    let mut pending_input = Vec::new();
    for chunk in [
        &expected[..2],
        &expected[2..4],
        &expected[4..OPEN.len() + 3],
        &expected[OPEN.len() + 3..expected.len() - 4],
        &expected[expected.len() - 4..expected.len() - 1],
        &expected[expected.len() - 1..],
    ] {
        handler
            .handle_attached_live_input(requester_pid, &mut pending_input, chunk)
            .await
            .expect("split bracketed paste chunk");
    }
    assert!(pending_input.is_empty());

    slot.assert_application_bytes(
        "delimiters split across reads must be reassembled for the child",
    );
}

// ---------------------------------------------------------------------------
// Mixed synchronize-panes
// ---------------------------------------------------------------------------

async fn assert_mixed_synchronized_final_sink(label: &str, active_pane: u32) {
    let handler = RequestHandler::new();
    let session = session_name(label);
    let body = b"mixed-destinations";
    let aware_expected = wrapped(body);

    let aware = FinalSinkSlot::new("mixed-aware", &aware_expected, true);
    let unaware = FinalSinkSlot::new("mixed-unaware", body, false);

    let aware_target = started_final_sink_pane(&handler, &session, &aware, true).await;
    let unaware_target = split_final_sink_pane(&handler, &session, &unaware).await;
    handler
        .wait_for_pane_startup_to_finish_for_test(&unaware_target)
        .await;
    unaware.wait_until_ready();
    wait_for_bracketed_mode(&handler, &unaware_target, false).await;
    assert_eq!(aware_target, PaneTarget::with_window(session.clone(), 0, 0));

    let pane = PaneTarget::with_window(session.clone(), 0, active_pane);
    handler.handle_ok(SelectPaneRequest::fixture(pane)).await;
    set_synchronize_panes(&handler, &session).await;

    let requester_pid = attach_to(&handler, &session).await;
    // The client always sends the wrapped form; the server must recompute the
    // mode for each destination instead of reusing the source pane's.
    handler
        .handle_attached_live_input_for_test(requester_pid, &aware_expected)
        .await
        .expect("synchronized bracketed paste");

    aware.assert_application_bytes(&format!(
        "the aware destination must keep its delimiters (active pane {active_pane})"
    ));
    unaware.assert_application_bytes(&format!(
        "the unaware destination must receive the body only (active pane {active_pane})"
    ));
}

#[tokio::test]
async fn synchronized_mixed_panes_recompute_mode_per_child_when_the_active_pane_is_aware() {
    assert_mixed_synchronized_final_sink("final-sink-mixed-aware", 0).await;
}

#[tokio::test]
async fn synchronized_mixed_panes_recompute_mode_per_child_when_the_active_pane_is_unaware() {
    assert_mixed_synchronized_final_sink("final-sink-mixed-unaware", 1).await;
}

// ---------------------------------------------------------------------------
// Child-observed ordering
// ---------------------------------------------------------------------------

/// Ordering is what the child read, not the order the server awaited.
///
/// A keystroke and a pasted body both take the pane's pseudoterminal, but they
/// are separate writes, so a correct await order alone does not settle what
/// the child reads first. The child must observe `key -> paste -> key`.
async fn assert_paste_arrives_between_the_keystrokes(label: &str, bracket_aware: bool) {
    let handler = RequestHandler::new();
    let session = session_name(label);
    let body = b"yy";
    let wrapped_body = wrapped(body);
    let mut expected = b"x".to_vec();
    expected.extend_from_slice(if bracket_aware { &wrapped_body } else { body });
    expected.extend_from_slice(b"z");
    let slot = FinalSinkSlot::new(label, &expected, bracket_aware);

    started_final_sink_pane(&handler, &session, &slot, bracket_aware).await;
    let requester_pid = attach_to(&handler, &session).await;
    for chunk in [b"x".as_slice(), &wrapped_body, b"z".as_slice()] {
        handler
            .handle_attached_live_input_for_test(requester_pid, chunk)
            .await
            .expect("attached input");
    }

    slot.assert_application_bytes(
        "the child must read the paste between the keystrokes that bracket it",
    );
}

#[tokio::test]
async fn a_paste_reaches_an_aware_child_between_the_keystrokes_that_bracket_it() {
    assert_paste_arrives_between_the_keystrokes("final-sink-order-aware", true).await;
}

#[tokio::test]
async fn a_paste_reaches_an_unaware_child_between_the_keystrokes_that_bracket_it() {
    assert_paste_arrives_between_the_keystrokes("final-sink-order-unaware", false).await;
}

// ---------------------------------------------------------------------------
// Negative controls
// ---------------------------------------------------------------------------
//
// These pin the observable contract: no delimiter is ever added around input
// that did not arrive as a bracketed paste. They are byte-exact at the child
// and mode-independent. They do not by themselves identify which internal
// helper ran; the mutation ledger covers that.

#[tokio::test]
async fn send_keys_never_wraps_its_payload_for_an_aware_child() {
    let handler = RequestHandler::new();
    let session = session_name("final-sink-send-keys");
    let literal = "send-keys-plain";
    let slot = FinalSinkSlot::new("send-keys", literal.as_bytes(), true);

    let target = started_final_sink_pane(&handler, &session, &slot, true).await;
    let response = handler
        .handle(Request::SendKeys(SendKeysRequest {
            target: target.clone(),
            keys: vec![literal.to_owned()],
        }))
        .await;
    assert!(matches!(
        response,
        Response::SendKeys(SendKeysResponse { key_count: 1 })
    ));

    slot.assert_application_bytes("send-keys must not enter the bracketed-paste sink");
}

#[tokio::test]
async fn pane_input_and_raw_attached_keystrokes_are_never_wrapped_for_an_aware_child() {
    let handler = RequestHandler::new();
    let session = session_name("final-sink-raw");
    let pane_input = "pane-input";
    let raw = b"raw-keys";
    let mut expected = pane_input.as_bytes().to_vec();
    expected.extend_from_slice(raw);
    let slot = FinalSinkSlot::new("raw", &expected, true);

    started_final_sink_pane(&handler, &session, &slot, true).await;
    let pane_id = {
        let state = handler.state.lock().await;
        state
            .sessions
            .session(&session)
            .and_then(|session| session.window_at(0))
            .and_then(|window| window.pane(0))
            .map(rmux_core::Pane::id)
            .expect("pane exists")
    };
    let response = handler
        .handle(Request::PaneInput(rmux_proto::PaneInputRequest {
            target: PaneTargetRef::by_id(session.clone(), pane_id),
            keys: vec![pane_input.to_owned()],
            literal: true,
        }))
        .await;
    assert!(
        matches!(response, Response::SendKeys(_)),
        "unexpected pane-input response: {response:?}"
    );

    let requester_pid = attach_to(&handler, &session).await;
    handler
        .handle_attached_live_input_for_test(requester_pid, raw)
        .await
        .expect("raw attached keystrokes");

    slot.assert_application_bytes(
        "SDK/raw keystrokes and pane-input must reach the child unwrapped",
    );
}
