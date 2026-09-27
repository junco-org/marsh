use super::RequestHandler;
use rmux_core::Utf8Config;
use rmux_proto::types::OptionScopeSelector;
use rmux_proto::{
    ErrorResponse, OptionName, PaneTarget, Request, Response, RmuxError, ScopeSelector,
    SetOptionByNameRequest, SetOptionMode, SetOptionRequest, WindowTarget,
};

use crate::test_fixtures::Fixture;
use crate::test_names::session_name;

#[tokio::test]
async fn set_option_updates_the_store_and_session_values_override_global() {
    let handler = RequestHandler::new();
    handler.create_session("alpha").await;
    handler.create_session("beta").await;

    assert_eq!(
        handler
            .handle(Request::SetOption(SetOptionRequest::fixture((
                ScopeSelector::Global,
                OptionName::Status,
                "off",
            ))))
            .await,
        Response::SetOption(rmux_proto::SetOptionResponse {
            scope: ScopeSelector::Global,
            option: OptionName::Status,
            mode: SetOptionMode::Replace,
        })
    );
    assert_eq!(
        handler
            .handle(Request::SetOption(SetOptionRequest::fixture((
                ScopeSelector::Session(session_name("alpha")),
                OptionName::Status,
                "on",
            ))))
            .await,
        Response::SetOption(rmux_proto::SetOptionResponse {
            scope: ScopeSelector::Session(session_name("alpha")),
            option: OptionName::Status,
            mode: SetOptionMode::Replace,
        })
    );

    let state = handler.state.lock().await;
    assert_eq!(state.options.global_value(OptionName::Status), Some("off"));
    assert_eq!(
        state
            .options
            .resolve(Some(&session_name("alpha")), OptionName::Status),
        Some("on")
    );
    assert_eq!(
        state
            .options
            .resolve(Some(&session_name("beta")), OptionName::Status),
        Some("off")
    );
}

#[tokio::test]
async fn typed_and_named_default_shell_mutations_reject_unsuitable_paths() {
    let handler = RequestHandler::new();
    let invalid = "/definitely/missing/rmux-shell";
    let expected = Response::Error(ErrorResponse {
        error: RmuxError::Message(format!("not a suitable shell: {invalid}")),
    });

    assert_eq!(
        handler
            .handle(Request::SetOption(SetOptionRequest::fixture((
                ScopeSelector::Global,
                OptionName::DefaultShell,
                invalid,
            ))))
            .await,
        expected
    );
    assert_eq!(
        handler
            .handle(Request::SetOptionByName(Box::new(
                SetOptionByNameRequest::fixture((
                    OptionScopeSelector::SessionGlobal,
                    "default-shell",
                    invalid,
                ))
            )))
            .await,
        expected
    );
    assert_eq!(
        handler
            .state
            .lock()
            .await
            .options
            .global_value(OptionName::DefaultShell),
        None
    );

    handler
        .set_option(ScopeSelector::Global, OptionName::DefaultShell, "/bin/sh")
        .await;
    assert_eq!(
        handler
            .handle(Request::SetOption(SetOptionRequest {
                mode: SetOptionMode::Append,
                ..Fixture::fixture((ScopeSelector::Global, OptionName::DefaultShell, ".invalid"))
            }))
            .await,
        Response::Error(ErrorResponse {
            error: RmuxError::Message("not a suitable shell: /bin/sh.invalid".to_owned()),
        })
    );
    assert_eq!(
        handler
            .state
            .lock()
            .await
            .options
            .global_value(OptionName::DefaultShell),
        Some("/bin/sh")
    );
}

#[tokio::test]
async fn terminal_features_append_preserves_order_and_invalid_requests_fail_first() {
    let handler = RequestHandler::new();
    handler.create_session("alpha").await;

    assert_eq!(
        handler
            .handle(Request::SetOption(SetOptionRequest {
                mode: SetOptionMode::Append,
                ..Fixture::fixture((
                    ScopeSelector::Global,
                    OptionName::TerminalFeatures,
                    "xterm*:RGB",
                ))
            }))
            .await,
        Response::SetOption(rmux_proto::SetOptionResponse {
            scope: ScopeSelector::Global,
            option: OptionName::TerminalFeatures,
            mode: SetOptionMode::Append,
        })
    );
    assert_eq!(
        handler
            .handle(Request::SetOption(SetOptionRequest {
                mode: SetOptionMode::Append,
                ..Fixture::fixture((
                    ScopeSelector::Global,
                    OptionName::TerminalFeatures,
                    "screen*:AX",
                ))
            }))
            .await,
        Response::SetOption(rmux_proto::SetOptionResponse {
            scope: ScopeSelector::Global,
            option: OptionName::TerminalFeatures,
            mode: SetOptionMode::Append,
        })
    );

    let scalar_append = handler
        .handle(Request::SetOption(SetOptionRequest {
            mode: SetOptionMode::Append,
            ..Fixture::fixture((ScopeSelector::Global, OptionName::Status, "off"))
        }))
        .await;
    assert_eq!(
        scalar_append,
        Response::SetOption(rmux_proto::SetOptionResponse {
            scope: ScopeSelector::Global,
            option: OptionName::Status,
            mode: SetOptionMode::Append,
        })
    );

    let invalid_value = handler
        .handle(Request::SetOption(SetOptionRequest::fixture((
            ScopeSelector::Global,
            OptionName::Status,
            "maybe",
        ))))
        .await;
    assert_eq!(
        invalid_value,
        Response::Error(ErrorResponse {
            error: RmuxError::InvalidSetOption("unknown value: maybe".to_owned()),
        })
    );

    let state = handler.state.lock().await;
    assert_eq!(
        state.options.global_value(OptionName::TerminalFeatures),
        Some(
            "xterm*:clipboard:ccolour:cstyle:focus:title,screen*:title,rxvt*:ignorefkeys,xterm*:RGB,screen*:AX"
        )
    );
    assert_eq!(state.options.global_value(OptionName::Status), Some("off"));
}

#[tokio::test]
async fn set_option_by_name_refreshes_existing_transcripts_for_server_utf8_options() {
    let handler = RequestHandler::new();
    handler.create_session("alpha").await;
    let alpha = session_name("alpha");

    let before = {
        let state = handler.state.lock().await;
        state
            .transcript_utf8_config(&alpha, 0, 0)
            .expect("initial transcript exists")
    };

    assert_eq!(
        handler
            .handle(Request::SetOptionByName(Box::new(
                SetOptionByNameRequest::fixture((
                    OptionScopeSelector::ServerGlobal,
                    "variation-selector-always-wide",
                    "off",
                ))
            )))
            .await,
        Response::SetOptionByName(rmux_proto::SetOptionByNameResponse {
            scope: OptionScopeSelector::ServerGlobal,
            name: "variation-selector-always-wide".to_owned(),
            mode: SetOptionMode::Replace,
        })
    );

    let state = handler.state.lock().await;
    let after = state
        .transcript_utf8_config(&alpha, 0, 0)
        .expect("transcript still exists");
    let expected = Utf8Config::from_options(&state.options);

    assert_ne!(before, after);
    assert_eq!(after, expected);
}

#[tokio::test]
async fn pane_style_options_resolve_session_then_global_for_supported_variants() {
    let handler = RequestHandler::new();
    handler.create_session("alpha").await;
    handler.create_session("beta").await;
    let alpha_window = WindowTarget::with_window(session_name("alpha"), 0);
    let alpha_pane = PaneTarget::with_window(session_name("alpha"), 0, 0);

    handler
        .set_option(
            ScopeSelector::Global,
            OptionName::PaneBorderStyle,
            "fg=colour1",
        )
        .await;
    handler
        .set_option(
            ScopeSelector::Global,
            OptionName::PaneActiveBorderStyle,
            "fg=colour2",
        )
        .await;
    handler
        .set_option(
            ScopeSelector::Window(alpha_window),
            OptionName::PaneBorderStyle,
            "fg=colour3",
        )
        .await;
    handler
        .set_option(
            ScopeSelector::Pane(alpha_pane.clone()),
            OptionName::PaneBorderStyle,
            "fg=colour4",
        )
        .await;
    handler
        .set_option(
            ScopeSelector::Pane(alpha_pane),
            OptionName::PaneActiveBorderStyle,
            "fg=colour5",
        )
        .await;

    let state = handler.state.lock().await;
    assert_eq!(
        state
            .options
            .resolve_for_window(&session_name("alpha"), 0, OptionName::PaneBorderStyle,),
        Some("fg=colour3")
    );
    assert_eq!(
        state
            .options
            .resolve_for_pane(&session_name("alpha"), 0, 0, OptionName::PaneBorderStyle),
        Some("fg=colour4")
    );
    assert_eq!(
        state.options.resolve_for_pane(
            &session_name("alpha"),
            0,
            0,
            OptionName::PaneActiveBorderStyle,
        ),
        Some("fg=colour5")
    );
    assert_eq!(
        state.options.resolve_for_window(
            &session_name("alpha"),
            0,
            OptionName::PaneActiveBorderStyle,
        ),
        Some("fg=colour2")
    );
    assert_eq!(
        state.options.resolve(None, OptionName::DefaultTerminal),
        Some("tmux-256color")
    );
    assert_eq!(
        state
            .options
            .resolve_for_window(&session_name("beta"), 0, OptionName::PaneBorderStyle),
        Some("fg=colour1")
    );
    assert_eq!(
        state.options.resolve_for_window(
            &session_name("beta"),
            0,
            OptionName::PaneActiveBorderStyle,
        ),
        Some("fg=colour2")
    );
}

#[tokio::test]
async fn set_option_to_nonexistent_session_returns_session_not_found() {
    let handler = RequestHandler::new();

    let response = handler
        .handle(Request::SetOption(SetOptionRequest::fixture((
            ScopeSelector::Session(session_name("missing")),
            OptionName::Status,
            "off",
        ))))
        .await;

    assert_eq!(
        response,
        Response::Error(ErrorResponse {
            error: RmuxError::SessionNotFound("missing".to_owned()),
        })
    );
}

#[tokio::test]
async fn set_option_append_empty_string_is_noop() {
    let handler = RequestHandler::new();

    handler
        .handle_ok(SetOptionRequest {
            mode: SetOptionMode::Append,
            ..Fixture::fixture((ScopeSelector::Global, OptionName::TerminalFeatures, ""))
        })
        .await;

    let state = handler.state.lock().await;
    assert_eq!(
        state.options.global_value(OptionName::TerminalFeatures),
        None
    );
}
