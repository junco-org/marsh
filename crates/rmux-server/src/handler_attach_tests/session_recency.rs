//! Targetless attach and detach-on-destroy ranking share the core recency
//! invariant.
//!
//! Both readers used to compare whole-second `activity_at`/`created_at` pairs
//! and then fall back to a creation id or a name, so two sessions used inside
//! one second were ordered by something that is not use at all. Every fixture
//! here pins the public seconds, which is the condition the readers must
//! survive, and then asserts the reader's own outcome rather than the token.

use super::*;
use crate::test_fixtures::{SessionSpec, TestRequest};

/// One arbitrary but fixed second shared by every session in a fixture.
const PINNED_SECOND: i64 = 1_785_500_000;

/// Creates `creation_order`, marks `use_order` as used, then pins every public
/// timestamp so no whole second can order the result.
async fn same_second_handler(creation_order: &[&str], use_order: &[&str]) -> RequestHandler {
    let handler = RequestHandler::new();
    for name in creation_order {
        SessionSpec::create(&handler, *name).await;
    }
    let mut state = handler.state.lock().await;
    for name in use_order {
        state
            .sessions
            .session_mut(&session_name(name))
            .expect("used session exists")
            .touch_attached();
    }
    for name in creation_order {
        state
            .sessions
            .session_mut(&session_name(name))
            .expect("created session exists")
            .pin_public_times_for_tests(PINNED_SECOND);
    }
    assert!(
        state
            .sessions
            .iter()
            .all(|(_, session)| session.created_at() == PINNED_SECOND
                && session.activity_at() == PINNED_SECOND),
        "the fixture must leave no public second able to order the sessions"
    );
    drop(state);
    handler
}

#[tokio::test]
async fn targetless_attach_picks_the_last_used_session_not_the_lowest_id_or_name() {
    let handler = same_second_handler(&["m03", "z99", "a01"], &["z99", "m03"]).await;
    assert_eq!(
        handler.session_id_for_test("m03").await,
        rmux_proto::SessionId::new(0),
        "the truly last-used session deliberately holds the lowest creation id"
    );

    assert_eq!(
        handler
            .preferred_session_name()
            .await
            .expect("preferred session resolves"),
        session_name("m03")
    );
}

#[tokio::test]
async fn targetless_attach_prefers_an_unattached_session_over_the_most_recent_one() {
    let handler = same_second_handler(&["a01", "z99", "m03"], &["a01", "m03"]).await;
    // `finish_attach` is the teardown half of an attach, so the client has to
    // stay registered for m03 to count as attached at all.
    let _control_rx = handler.attach_client(140_001, "m03").await;
    {
        // Attaching is itself use, so m03 is now both the most recently used
        // session and the only attached one.
        let mut state = handler.state.lock().await;
        for name in ["a01", "z99", "m03"] {
            state
                .sessions
                .session_mut(&session_name(name))
                .expect("session exists")
                .pin_public_times_for_tests(PINNED_SECOND);
        }
    }

    // tmux 3.7b measured: a targetless attach lands on the unattached session
    // even when a more recently used session is available.
    assert_eq!(
        handler
            .preferred_session_name()
            .await
            .expect("preferred session resolves"),
        session_name("a01"),
        "unattached preference outranks recency, and a01 was used after z99"
    );
}

#[tokio::test]
async fn targetless_attach_ranks_a_later_creation_above_an_earlier_attach() {
    // a01 is the only session that was ever attached; z99 was merely created
    // afterwards. Attach history must not outrank the later lifetime event.
    let handler = RequestHandler::new();
    for name in ["m03", "a01"] {
        SessionSpec::create(&handler, name).await;
    }
    {
        let mut state = handler.state.lock().await;
        state
            .sessions
            .session_mut(&session_name("a01"))
            .expect("a01 exists")
            .touch_attached();
    }
    SessionSpec::create(&handler, "z99").await;
    // Targetless selection prefers sessions whose deferred pane process is
    // live before it ranks by recency. Equalize that independent precondition
    // so this fixture measures only the lifetime order it names.
    handler.wait_for_initial_panes_for_test().await;
    {
        let mut state = handler.state.lock().await;
        for name in ["m03", "a01", "z99"] {
            state
                .sessions
                .session_mut(&session_name(name))
                .expect("session exists")
                .pin_public_times_for_tests(PINNED_SECOND);
        }
        assert!(
            state
                .sessions
                .session(&session_name("a01"))
                .expect("a01 exists")
                .last_attached_at()
                .is_some(),
            "a01 must carry the only attach history in this fixture"
        );
    }

    assert_eq!(
        handler
            .preferred_session_name()
            .await
            .expect("preferred session resolves"),
        session_name("z99")
    );
}

#[tokio::test]
async fn destroy_switch_ranks_the_all_session_candidate_set_by_recency() {
    let handler = same_second_handler(&["m03", "z99", "a01", "source"], &["z99", "m03"]).await;
    handler
        .set_detach_on_destroy_for_test(&session_name("source"), "off")
        .await;

    let mut control_rx = handler.attach_client(141_001, "source").await;
    TestRequest::send_ok(&handler, KillSessionRequest::fixture("source")).await;

    let target = recv_switch_target(&mut control_rx, "detach-on-destroy off").await;
    assert_eq!(target.session_name, session_name("m03"));
}

#[tokio::test]
async fn destroy_switch_establishes_its_detached_candidate_set_before_ranking_it() {
    // c03 is the most recently used session but is already attached, so the
    // no-detached policy must rank only b02, a01 and pick b02.
    let handler = same_second_handler(&["a01", "b02", "c03", "source"], &["b02", "c03"]).await;
    handler
        .set_detach_on_destroy_for_test(&session_name("source"), "no-detached")
        .await;

    let _occupied_rx = handler.attach_client(142_001, "c03").await;
    let mut control_rx = handler.attach_client(142_002, "source").await;
    TestRequest::send_ok(&handler, KillSessionRequest::fixture("source")).await;

    let target = recv_switch_target(&mut control_rx, "detach-on-destroy no-detached").await;
    assert_eq!(target.session_name, session_name("b02"));
}
