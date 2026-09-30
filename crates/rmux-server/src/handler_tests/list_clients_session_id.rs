use super::*;
use crate::test_fixtures::{SessionSpec, Sizeless, TestRequest};

const CLIENT_SESSION_FORMAT: &str =
    "#{session_id}|#{session_name}|#{client_session}|#{client_control_mode}";

async fn list_clients(handler: &RequestHandler, format: &str, filter: Option<&str>) -> String {
    let response = handler
        .handle(Request::ListClients(Box::new(
            rmux_proto::ListClientsRequest {
                format: Some(format.to_owned()),
                target_session: None,
                filter: filter.map(str::to_owned),
                sort_order: None,
                reversed: false,
            },
        )))
        .await;
    let Response::ListClients(response) = response else {
        panic!("expected list-clients response");
    };
    String::from_utf8(response.output.stdout().to_vec()).expect("utf-8 list-clients output")
}

#[tokio::test]
async fn list_clients_uses_stable_session_identity_for_attach_and_control_formats() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    SessionSpec::create(&handler, Sizeless(&alpha)).await;

    let session_id = handler.session_id_for_test(&alpha).await;
    let attach_pid = 93_401;
    let _attach_rx = handler.attach_client(attach_pid, &alpha).await;
    handler
        .register_control_for_test(93_402, Some(&alpha))
        .await;

    let expected = format!("{session_id}|alpha|alpha|0\n{session_id}|alpha|alpha|1\n");
    assert_eq!(
        list_clients(&handler, CLIENT_SESSION_FORMAT, None).await,
        expected
    );
    assert_eq!(
        list_clients(
            &handler,
            "#{?session_id,#{session_id},missing}|#{session_name}",
            None,
        )
        .await,
        format!("{session_id}|alpha\n{session_id}|alpha\n")
    );
    assert_eq!(
        list_clients(
            &handler,
            CLIENT_SESSION_FORMAT,
            Some(&format!("#{{==:#{{session_id}},{session_id}}}")),
        )
        .await,
        expected
    );

    let renamed = session_name("renamed");
    TestRequest::send_ok(
        &handler,
        RenameSessionRequest {
            target: alpha,
            new_name: renamed,
        },
    )
    .await;
    assert_eq!(
        list_clients(&handler, CLIENT_SESSION_FORMAT, None).await,
        format!(
            "{session_id}|renamed|renamed|0\n\
             {session_id}|renamed|renamed|1\n"
        )
    );
}
