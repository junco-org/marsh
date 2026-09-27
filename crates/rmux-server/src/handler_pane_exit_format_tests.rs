use super::RequestHandler;
use rmux_proto::{OptionName, PaneTarget, RespawnPaneRequest, ScopeSelector};
use tokio::time::Duration;

use crate::test_fixtures::{wait_until, Fixture};
use crate::test_names::session_name;

#[tokio::test]
async fn display_message_pane_dead_observes_exited_child_promptly() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");
    let target = PaneTarget::with_window(alpha.clone(), 0, 0);

    handler.create_session(&alpha).await;
    handler
        .set_option(
            ScopeSelector::Pane(target.clone()),
            OptionName::RemainOnExit,
            "on",
        )
        .await;
    handler
        .handle_ok(RespawnPaneRequest {
            command: Some(vec!["true".to_owned()]),
            ..Fixture::fixture(&target)
        })
        .await;

    wait_until(
        Duration::from_secs(5),
        Duration::from_millis(20),
        async || {
            let output = handler.display_print(&target, "#{pane_dead}").await;
            let dead = String::from_utf8(output)
                .expect("pane_dead output is utf8")
                .trim()
                .to_owned();
            if dead == "1" {
                Ok(())
            } else {
                Err(dead)
            }
        },
    )
    .await
    .unwrap_or_else(|dead| panic!("pane_dead did not flip promptly, last value was {dead:?}"));
}
