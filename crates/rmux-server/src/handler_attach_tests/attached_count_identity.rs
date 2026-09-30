use super::*;
use crate::test_fixtures::{SessionSpec, TestRequest};

#[tokio::test]
async fn identity_refresh_counts_attach_and_control_clients() {
    let handler = RequestHandler::new();
    let session_name = session_name("identity-refresh-attached-count");
    SessionSpec::create(&handler, Quiet(&session_name)).await;
    for (option, value) in [
        (OptionName::StatusLeft, "attached=#{session_attached}"),
        (OptionName::StatusRight, ""),
    ] {
        handler
            .set_option(ScopeSelector::Session(session_name.clone()), option, value)
            .await;
    }

    let attach_pid = 91_701;
    let mut attach_rx = handler.attach_client(attach_pid, &session_name).await;
    let _control_rx = handler
        .register_control_for_test(91_702, Some(&session_name))
        .await;

    let session_id = handler.session_id_for_test(&session_name).await;
    let identity = handler.active_attach_identity_for_test(attach_pid).await;
    assert!(
        handler
            .refresh_attached_client_base_for_session_identity(identity, &session_name, session_id,)
            .await
    );

    let target = recv_switch_target(&mut attach_rx, "identity-aware attached count").await;
    let frame = String::from_utf8(target.render_frame).expect("render frame is utf-8");
    assert!(frame.contains("attached=2"), "render frame: {frame:?}");
}

#[tokio::test]
async fn identity_attached_count_rejects_a_reused_session_name() {
    let handler = RequestHandler::new();
    let original_name = session_name("identity-count-reused");
    let renamed = session_name("identity-count-renamed");
    SessionSpec::create(&handler, Quiet(&original_name)).await;

    let _old_attach_rx = handler.attach_client(91_711, &original_name).await;
    let _old_control_rx = handler
        .register_control_for_test(91_712, Some(&original_name))
        .await;
    let original_id = handler.session_id_for_test(&original_name).await;

    TestRequest::send_ok(
        &handler,
        RenameSessionRequest {
            target: original_name.clone(),
            new_name: renamed.clone(),
        },
    )
    .await;

    SessionSpec::create(&handler, Quiet(&original_name)).await;
    let _new_attach_rx = handler.attach_client(91_713, &original_name).await;
    let _new_control_rx = handler
        .register_control_for_test(91_714, Some(&original_name))
        .await;
    let replacement_id = handler.session_id_for_test(&original_name).await;
    assert_ne!(replacement_id, original_id);

    assert_eq!(
        handler
            .attached_count_for_session_identity(&original_name, replacement_id)
            .await,
        2
    );
    assert_eq!(
        handler
            .attached_count_for_session_identity(&renamed, original_id)
            .await,
        2
    );
    assert_eq!(
        handler
            .attached_count_for_session_identity(&original_name, original_id)
            .await,
        0,
        "a reused name must not make clients from the replacement count for the old identity"
    );
}
