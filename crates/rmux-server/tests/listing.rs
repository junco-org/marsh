use std::error::Error;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod common;

use common::{
    create_session, send, send_ok, send_request, session_name, shell_quote, shell_quote_str,
    start_server, wait_for_file_contents, Fixture, TestHarness,
};
use rmux_proto::{
    HasSessionRequest, HookName, ListPanesRequest, ListSessionsRequest, NewWindowRequest,
    PaneTarget, Request, Response, ScopeSelector, SendKeysRequest, SetEnvironmentRequest,
    SetHookRequest, SetOptionRequest, ShowEnvironmentRequest, ShowOptionsRequest,
    SplitWindowRequest, TerminalSize, WindowTarget,
};

const FILE_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread")]
async fn list_sessions_uses_shared_formatter_through_real_socket() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("listing-list-sessions");
    let handle = start_server(&harness).await?;
    let alpha = session_name("alpha");
    let beta = session_name("beta");

    for (session_name, size) in [
        (
            beta.clone(),
            TerminalSize {
                cols: 120,
                rows: 40,
            },
        ),
        (alpha.clone(), TerminalSize { cols: 80, rows: 24 }),
    ] {
        create_session(harness.socket_path(), (session_name, size)).await?;
    }

    send_ok(
        harness.socket_path(),
        NewWindowRequest {
            name: Some("logs".to_owned()),
            ..Fixture::fixture(alpha)
        },
    )
    .await?;

    let listed = send(
        harness.socket_path(),
        ListSessionsRequest::fixture(
            "#{session_name}:#{session_windows}:#{session_attached}:#{session_width}x#{session_height}",
        ),
    )
    .await?;

    let output = listed
        .command_output()
        .expect("list-sessions returns command output");
    assert_eq!(
        std::str::from_utf8(output.stdout()).expect("list-sessions output is utf-8"),
        "alpha:2:0:x\nbeta:1:0:x\n"
    );

    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn list_panes_uses_shared_formatter_through_real_socket() -> Result<(), Box<dyn Error>> {
    let harness = TestHarness::new("listing-list-panes");
    let handle = start_server(&harness).await?;
    let alpha = session_name("alpha");

    create_session(harness.socket_path(), &alpha).await?;
    send_ok(harness.socket_path(), SplitWindowRequest::fixture(&alpha)).await?;
    send_ok(
        harness.socket_path(),
        NewWindowRequest {
            name: Some("logs".to_owned()),
            ..Fixture::fixture(&alpha)
        },
    )
    .await?;

    let listed = send(
        harness.socket_path(),
        ListPanesRequest::fixture((
            alpha,
            "#{session_name}:#{window_index}:#{pane_index}:#{pane_id}:#{pane_active}",
        )),
    )
    .await?;

    let output = listed
        .command_output()
        .expect("list-panes returns command output");
    assert_eq!(
        std::str::from_utf8(output.stdout()).expect("list-panes output is utf-8"),
        "alpha:0:0:%0:0\nalpha:0:1:%1:1\nalpha:1:0:%2:1\n"
    );

    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_session_round_trips_and_migrates_session_scoped_state() -> Result<(), Box<dyn Error>>
{
    let harness = TestHarness::new("listing-rename-session");
    let handle = start_server(&harness).await?;
    let alpha = session_name("alpha");
    let gamma = session_name("gamma");
    let hook_path = std::env::temp_dir().join(format!(
        "rmux-rename-hook-{}-{}.txt",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the unix epoch")
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&hook_path);

    create_session(harness.socket_path(), &alpha).await?;
    send_ok(
        harness.socket_path(),
        SetEnvironmentRequest::fixture((ScopeSelector::Session(alpha.clone()), "TERM", "screen")),
    )
    .await?;
    send_ok(
        harness.socket_path(),
        SetOptionRequest::fixture((
            ScopeSelector::Window(WindowTarget::new(alpha.clone())),
            rmux_proto::OptionName::PaneBorderStyle,
            "red",
        )),
    )
    .await?;
    send_ok(
        harness.socket_path(),
        SetHookRequest::fixture((
            ScopeSelector::Session(alpha.clone()),
            HookName::AfterSendKeys,
            format!(
                "run-shell {}",
                shell_quote_str(&format!(
                    "printf renamed-hook > {}",
                    shell_quote(&hook_path)
                ))
            ),
        )),
    )
    .await?;

    let renamed = send_request(
        harness.socket_path(),
        &Request::RenameSession(rmux_proto::RenameSessionRequest {
            target: alpha.clone(),
            new_name: gamma.clone(),
        }),
    )
    .await?;
    assert_eq!(
        renamed,
        Response::RenameSession(rmux_proto::RenameSessionResponse {
            session_name: gamma.clone(),
        })
    );

    assert_eq!(
        send_request(
            harness.socket_path(),
            &Request::HasSession(HasSessionRequest {
                target: alpha.clone(),
            }),
        )
        .await?,
        Response::HasSession(rmux_proto::HasSessionResponse { exists: false })
    );
    assert_eq!(
        send_request(
            harness.socket_path(),
            &Request::HasSession(HasSessionRequest {
                target: gamma.clone(),
            }),
        )
        .await?,
        Response::HasSession(rmux_proto::HasSessionResponse { exists: true })
    );

    let environment = send_request(
        harness.socket_path(),
        &Request::ShowEnvironment(ShowEnvironmentRequest {
            scope: ScopeSelector::Session(gamma.clone()),
            name: None,
            hidden: false,
            shell_format: false,
        }),
    )
    .await?;
    let environment_output = environment
        .command_output()
        .expect("show-environment returns command output");
    assert_eq!(
        std::str::from_utf8(environment_output.stdout()).expect("environment output is utf-8"),
        "TERM=screen\n"
    );

    let options = send_request(
        harness.socket_path(),
        &Request::ShowOptions(ShowOptionsRequest {
            scope: rmux_proto::OptionScopeSelector::Window(WindowTarget::new(gamma.clone())),
            name: None,
            value_only: false,
            include_inherited: true,
            quiet: false,
            include_hooks: false,
        }),
    )
    .await?;
    let options_output = options
        .command_output()
        .expect("show-options returns command output");
    assert!(std::str::from_utf8(options_output.stdout())
        .expect("options output is utf-8")
        .contains("pane-border-style red"));

    send_ok(
        harness.socket_path(),
        SendKeysRequest::fixture((
            PaneTarget::with_window(gamma, 0, 0),
            ["printf noop", "Enter"],
        )),
    )
    .await?;
    wait_for_file_contents(&hook_path, "renamed-hook", FILE_WAIT_TIMEOUT).await?;

    handle.shutdown().await?;
    let _ = std::fs::remove_file(&hook_path);
    Ok(())
}
