use std::collections::HashMap;

use rmux_proto::request::SwitchClientExt3Request;
use rmux_proto::{PaneTarget, Response, SessionName, SplitDirection, SplitWindowRequest};
use tokio::sync::mpsc;

use super::RequestHandler;
use crate::client_names::control_client_name;
use crate::control::ControlServerEvent;
use crate::test_fixtures::{Fixture, SessionSpec, TestRequest};

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProtocolSelectionModel {
    client_sessions: HashMap<String, String>,
    session_windows: HashMap<String, String>,
    window_panes: HashMap<String, String>,
    self_client_name: String,
}

impl ProtocolSelectionModel {
    async fn capture(handler: &RequestHandler, self_client_name: String) -> Self {
        let (session_windows, window_panes) = {
            let state = handler.state.lock().await;
            let mut session_windows = HashMap::new();
            let mut window_panes = HashMap::new();
            for (_session_name, session) in state.sessions.iter() {
                session_windows.insert(session.id().to_string(), session.window().id().to_string());
                for window in session.windows().values() {
                    let pane = window.active_pane().expect("test windows have panes");
                    window_panes.insert(window.id().to_string(), pane.id().to_string());
                }
            }
            (session_windows, window_panes)
        };
        let mut client_sessions = {
            let active_attach = handler.active_attach.lock().await;
            active_attach
                .by_pid
                .values()
                .map(|active| (active.client_name.clone(), active.session_id.to_string()))
                .collect::<HashMap<_, _>>()
        };
        {
            let active_control = handler.active_control.lock().await;
            client_sessions.extend(active_control.by_pid.iter().filter_map(|(pid, active)| {
                active
                    .session_id
                    .map(|session_id| (control_client_name(*pid), session_id.to_string()))
            }));
        }
        Self {
            client_sessions,
            session_windows,
            window_panes,
            self_client_name,
        }
    }

    fn apply(&mut self, notifications: &[String]) {
        for line in notifications {
            let mut fields = line.split_whitespace();
            match fields.next() {
                Some("%session-changed") => {
                    let session_id = fields.next().expect("session-changed session id");
                    self.client_sessions
                        .insert(self.self_client_name.clone(), session_id.to_owned());
                }
                Some("%client-session-changed") => {
                    let client_name = fields.next().expect("client-session-changed client");
                    let session_id = fields.next().expect("client-session-changed session id");
                    self.client_sessions
                        .insert(client_name.to_owned(), session_id.to_owned());
                }
                Some("%session-window-changed") => {
                    let session_id = fields.next().expect("session-window-changed session id");
                    let window_id = fields.next().expect("session-window-changed window id");
                    self.session_windows
                        .insert(session_id.to_owned(), window_id.to_owned());
                }
                Some("%window-pane-changed") => {
                    let window_id = fields.next().expect("window-pane-changed window id");
                    let pane_id = fields.next().expect("window-pane-changed pane id");
                    self.window_panes
                        .insert(window_id.to_owned(), pane_id.to_owned());
                }
                _ => {}
            }
        }
    }

    async fn assert_current(&self, handler: &RequestHandler, context: &str) {
        let actual = Self::capture(handler, self.self_client_name.clone()).await;
        assert_eq!(
            self.client_sessions, actual.client_sessions,
            "{context}: protocol consumer client sessions"
        );
        assert_eq!(
            self.session_windows, actual.session_windows,
            "{context}: protocol consumer active windows"
        );
        assert_eq!(
            self.window_panes, actual.window_panes,
            "{context}: protocol consumer active panes"
        );
    }
}

struct SwitchFixture {
    handler: RequestHandler,
    source: SessionName,
    target: SessionName,
}

impl SwitchFixture {
    async fn new(label: &str) -> Self {
        let handler = RequestHandler::new();
        let source = SessionSpec::create(&handler, format!("{label}-source")).await;
        let target = SessionSpec::create(&handler, format!("{label}-target")).await;
        TestRequest::send_ok(
            &handler,
            SplitWindowRequest {
                direction: SplitDirection::Horizontal,
                ..Fixture::fixture(PaneTarget::with_window(target.clone(), 0, 0))
            },
        )
        .await;
        let second_window = handler.create_window(&target).await.window_index();
        assert_eq!(second_window, 1);
        TestRequest::send_ok(
            &handler,
            SplitWindowRequest {
                direction: SplitDirection::Horizontal,
                ..Fixture::fixture(PaneTarget::with_window(target.clone(), second_window, 0))
            },
        )
        .await;

        {
            let mut state = handler.state.lock().await;
            let target_session = state
                .sessions
                .session_mut(&target)
                .expect("target session exists");
            target_session
                .select_pane_in_window(0, 0)
                .expect("first pane selected");
            target_session
                .select_pane_in_window(1, 0)
                .expect("first pane selected in inactive window");
            target_session
                .select_window(0)
                .expect("first window selected");
        }

        Self {
            handler,
            source,
            target,
        }
    }
}

fn drain_notifications(rx: &mut mpsc::Receiver<ControlServerEvent>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            ControlServerEvent::Notification(line) => lines.push(line),
            ControlServerEvent::SessionChanged(_)
            | ControlServerEvent::SessionChangedAt { .. }
            | ControlServerEvent::Refresh => {}
            ControlServerEvent::Exit(reason) => panic!("unexpected control exit: {reason:?}"),
        }
    }
    lines
}

fn transition_order(notifications: &[String]) -> Vec<&str> {
    notifications
        .iter()
        .filter_map(|line| {
            let event = line.split_whitespace().next()?;
            matches!(
                event,
                "%window-pane-changed"
                    | "%session-window-changed"
                    | "%client-session-changed"
                    | "%session-changed"
            )
            .then_some(event)
        })
        .collect()
}

fn switch_request(target: String) -> SwitchClientExt3Request {
    SwitchClientExt3Request {
        target_client: None,
        target: Some(target),
        key_table: None,
        last_session: false,
        next_session: false,
        previous_session: false,
        toggle_read_only: false,
        sort_order: None,
        skip_environment_update: true,
        zoom: false,
    }
}

#[tokio::test]
async fn pty_switch_selection_notifications_keep_protocol_model_current() {
    let cases = [
        (
            "window",
            ":1",
            vec!["%session-window-changed", "%client-session-changed"],
        ),
        (
            "pane",
            ":0.1",
            vec!["%window-pane-changed", "%client-session-changed"],
        ),
        (
            "window-pane",
            ":1.1",
            vec![
                "%window-pane-changed",
                "%session-window-changed",
                "%client-session-changed",
            ],
        ),
        ("active", ":0.0", vec!["%client-session-changed"]),
        ("session", "", vec!["%client-session-changed"]),
    ];

    for (offset, (label, suffix, expected_order)) in cases.into_iter().enumerate() {
        let fixture = SwitchFixture::new(&format!("pty-switch-{label}")).await;
        let attach_pid = 71_000 + u32::try_from(offset).expect("small test offset");
        let observer_pid = attach_pid + 1_000;
        let (_observer_id, mut observer_rx) = fixture
            .handler
            .register_control_for_test(observer_pid, Some(&fixture.target))
            .await;
        let mut attach_rx = fixture
            .handler
            .attach_client(attach_pid, &fixture.source)
            .await;
        let _ = drain_notifications(&mut observer_rx);
        while attach_rx.try_recv().is_ok() {}

        let mut model =
            ProtocolSelectionModel::capture(&fixture.handler, control_client_name(observer_pid))
                .await;
        let response = fixture
            .handler
            .handle_switch_client_ext3(
                attach_pid,
                switch_request(format!("{}{suffix}", fixture.target)),
            )
            .await;
        assert!(
            matches!(response, Response::SwitchClient(_)),
            "{label}: {response:?}"
        );

        let notifications = drain_notifications(&mut observer_rx);
        assert_eq!(
            transition_order(&notifications),
            expected_order,
            "{label}: tmux 3.7b transition order"
        );
        model.apply(&notifications);
        model.assert_current(&fixture.handler, label).await;

        if label == "window-pane" {
            let repeated = fixture
                .handler
                .handle_switch_client_ext3(
                    attach_pid,
                    switch_request(format!("{}:1.1", fixture.target)),
                )
                .await;
            assert!(
                matches!(repeated, Response::SwitchClient(_)),
                "{repeated:?}"
            );
            let repeated_notifications = drain_notifications(&mut observer_rx);
            assert_eq!(
                transition_order(&repeated_notifications),
                vec!["%client-session-changed"],
                "a repeated switch emits no selection transition"
            );
            model.apply(&repeated_notifications);
            model
                .assert_current(&fixture.handler, "window-pane repetition")
                .await;
        }
    }
}

#[tokio::test]
async fn control_self_switch_selection_notifications_keep_protocol_model_current() {
    let fixture = SwitchFixture::new("control-self-switch").await;
    let requester_pid = 72_000;
    let (control_id, mut event_rx) = fixture
        .handler
        .register_control_for_test(requester_pid, Some(&fixture.source))
        .await;
    let _ = drain_notifications(&mut event_rx);
    let mut model =
        ProtocolSelectionModel::capture(&fixture.handler, control_client_name(requester_pid)).await;

    let command = format!("switch-client -t {}:1.1", fixture.target);
    let commands = fixture
        .handler
        .parse_control_commands(&command)
        .await
        .expect("control switch parses");
    let result = fixture
        .handler
        .execute_control_commands_identity(requester_pid, control_id, commands)
        .await;
    assert!(result.error.is_none(), "{:?}", result.error);

    let notifications = drain_notifications(&mut event_rx);
    assert_eq!(
        transition_order(&notifications),
        vec![
            "%window-pane-changed",
            "%session-window-changed",
            "%session-changed",
        ],
        "tmux 3.7b emits pane, window, then the switching control client"
    );
    model.apply(&notifications);
    model
        .assert_current(&fixture.handler, "control self switch")
        .await;

    let repeated = fixture
        .handler
        .parse_control_commands(&command)
        .await
        .expect("repeated control switch parses");
    let result = fixture
        .handler
        .execute_control_commands_identity(requester_pid, control_id, repeated)
        .await;
    assert!(result.error.is_none(), "{:?}", result.error);
    let repeated_notifications = drain_notifications(&mut event_rx);
    assert_eq!(
        transition_order(&repeated_notifications),
        vec!["%session-changed"],
        "a repeated control switch emits no selection transition"
    );
    model.apply(&repeated_notifications);
    model
        .assert_current(&fixture.handler, "control self repetition")
        .await;
}
