use std::error::Error;

mod common;

use common::{create_session, send, session_name, start_server, Fixture, Sizeless, TestHarness};
use rmux_proto::{
    OptionName, Response, RmuxError, ScopeSelector, SetEnvironmentRequest, SetOptionMode,
    SetOptionRequest,
};

#[tokio::test(flavor = "multi_thread")]
async fn set_option_round_trips_and_invalid_variants_fail_cleanly() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("set-option");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;

    create_session(&socket_path, Sizeless("alpha")).await?;

    let global_status = send(
        &socket_path,
        SetOptionRequest::fixture((ScopeSelector::Global, OptionName::Status, "off")),
    )
    .await?;
    assert_eq!(
        global_status,
        Response::SetOption(rmux_proto::SetOptionResponse {
            scope: ScopeSelector::Global,
            option: OptionName::Status,
            mode: SetOptionMode::Replace,
        })
    );

    let session_status = send(
        &socket_path,
        SetOptionRequest::fixture((
            ScopeSelector::Session(session_name("alpha")),
            OptionName::Status,
            "on",
        )),
    )
    .await?;
    assert_eq!(
        session_status,
        Response::SetOption(rmux_proto::SetOptionResponse {
            scope: ScopeSelector::Session(session_name("alpha")),
            option: OptionName::Status,
            mode: SetOptionMode::Replace,
        })
    );

    let scalar_append = send(
        &socket_path,
        SetOptionRequest {
            mode: SetOptionMode::Append,
            ..Fixture::fixture((ScopeSelector::Global, OptionName::Status, "off"))
        },
    )
    .await?;
    assert_eq!(
        scalar_append,
        Response::SetOption(rmux_proto::SetOptionResponse {
            scope: ScopeSelector::Global,
            option: OptionName::Status,
            mode: SetOptionMode::Append,
        })
    );

    let explicit_local_scope = send(
        &socket_path,
        SetOptionRequest::fixture((
            ScopeSelector::Session(session_name("alpha")),
            OptionName::TerminalFeatures,
            "xterm*:RGB",
        )),
    )
    .await?;
    assert_eq!(
        explicit_local_scope,
        Response::SetOption(rmux_proto::SetOptionResponse {
            scope: ScopeSelector::Session(session_name("alpha")),
            option: OptionName::TerminalFeatures,
            mode: SetOptionMode::Replace,
        })
    );

    let invalid_value = send(
        &socket_path,
        SetOptionRequest::fixture((ScopeSelector::Global, OptionName::Status, "maybe")),
    )
    .await?;
    assert_eq!(
        invalid_value,
        Response::Error(rmux_proto::ErrorResponse {
            error: RmuxError::InvalidSetOption("unknown value: maybe".to_owned()),
        })
    );

    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn set_environment_round_trips_and_requires_existing_sessions() -> Result<(), Box<dyn Error>>
{
    let harness = TestHarness::new("set-environment");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;

    create_session(&socket_path, Sizeless("alpha")).await?;

    let global = send(
        &socket_path,
        SetEnvironmentRequest::fixture((ScopeSelector::Global, "TERM", "screen")),
    )
    .await?;
    assert_eq!(
        global,
        Response::SetEnvironment(rmux_proto::SetEnvironmentResponse {
            scope: ScopeSelector::Global,
            name: "TERM".to_owned(),
        })
    );

    let session = send(
        &socket_path,
        SetEnvironmentRequest::fixture((
            ScopeSelector::Session(session_name("alpha")),
            "TERM",
            "tmux-256color",
        )),
    )
    .await?;
    assert_eq!(
        session,
        Response::SetEnvironment(rmux_proto::SetEnvironmentResponse {
            scope: ScopeSelector::Session(session_name("alpha")),
            name: "TERM".to_owned(),
        })
    );

    let missing_session = send(
        &socket_path,
        SetEnvironmentRequest::fixture((
            ScopeSelector::Session(session_name("missing")),
            "TERM",
            "screen",
        )),
    )
    .await?;
    assert_eq!(
        missing_session,
        Response::Error(rmux_proto::ErrorResponse {
            error: RmuxError::SessionNotFound("missing".to_owned()),
        })
    );

    handle.shutdown().await?;
    Ok(())
}
