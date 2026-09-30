use super::*;

use crate::test_fixtures::SessionSpec;
use rmux_core::PaneGeometry;

#[derive(Clone, Copy)]
enum StatusCase {
    Bottom,
    Top,
    MultiBottom,
    Off,
}

impl StatusCase {
    const fn label(self) -> &'static str {
        match self {
            Self::Bottom => "bottom",
            Self::Top => "top",
            Self::MultiBottom => "multi-bottom",
            Self::Off => "off",
        }
    }

    const fn expected_geometry(self) -> PaneGeometry {
        // Frozen tmux 3.7b, measured on an 80x24 client: mode-trees receive
        // the terminal rectangle minus the configured status rows, then the
        // top-status offset is applied.
        match self {
            Self::Bottom => PaneGeometry::new(0, 0, 80, 23),
            Self::Top => PaneGeometry::new(0, 1, 80, 23),
            Self::MultiBottom => PaneGeometry::new(0, 0, 80, 21),
            Self::Off => PaneGeometry::new(0, 0, 80, 24),
        }
    }

    fn assert_frame_rows(self, frame: &[u8], context: &str) {
        match self {
            Self::Bottom => {
                assert!(frame_visits_row(frame, 23), "{context}");
                assert!(!frame_visits_row(frame, 24), "{context}");
            }
            Self::Top => {
                assert!(!frame_visits_row(frame, 1), "{context}");
                assert!(frame_visits_row(frame, 2), "{context}");
                assert!(frame_visits_row(frame, 24), "{context}");
            }
            Self::MultiBottom => {
                assert!(frame_visits_row(frame, 21), "{context}");
                for row in 22..=24 {
                    assert!(!frame_visits_row(frame, row), "{context}: row {row}");
                }
            }
            Self::Off => {
                assert!(frame_visits_row(frame, 1), "{context}");
                assert!(frame_visits_row(frame, 24), "{context}");
            }
        }
    }
}

#[derive(Clone, Copy)]
enum OriginCase {
    AttachedPty,
    // A control-mode command has no entry in `active_attach`; its requester
    // PID therefore exercises the detached-request dispatch branch while the
    // registered PTY receives the resulting overlay frame.
    Control,
}

impl OriginCase {
    const fn label(self) -> &'static str {
        match self {
            Self::AttachedPty => "pty",
            Self::Control => "control",
        }
    }
}

async fn set_status_case(handler: &RequestHandler, status_case: StatusCase) {
    let status = match status_case {
        StatusCase::MultiBottom => "3",
        StatusCase::Off => "off",
        StatusCase::Bottom | StatusCase::Top => "on",
    };
    handler
        .set_option(ScopeSelector::Global, OptionName::Status, status)
        .await;
    let position = if matches!(status_case, StatusCase::Top) {
        "top"
    } else {
        "bottom"
    };
    handler
        .set_option(ScopeSelector::Global, OptionName::StatusPosition, position)
        .await;
}

async fn overlay_geometry_case(
    status_case: StatusCase,
    kind: ModeTreeKind,
    origin_case: OriginCase,
    pid_offset: u32,
) {
    let handler = RequestHandler::new();
    let label = format!(
        "mode-tree-status-{}-{}-{}",
        status_case.label(),
        kind.command_name(),
        origin_case.label()
    );
    let session_name = SessionName::new(label).expect("valid session");
    SessionSpec::create(&handler, &session_name).await;
    set_status_case(&handler, status_case).await;

    let observer_pid = std::process::id().saturating_add(pid_offset);
    let second_pid = observer_pid.saturating_add(1);
    let mut observer_rx = handler.attach_client(observer_pid, &session_name).await;
    let _second_rx = handler.attach_client(second_pid, &session_name).await;
    while observer_rx.try_recv().is_ok() {}

    let target = format!("{}:0.0", session_name.as_str());
    let arguments = match kind {
        ModeTreeKind::Tree => vec!["choose-tree", "-t", target.as_str()],
        ModeTreeKind::Client => vec!["choose-client"],
        other => panic!("unexpected mode-tree kind: {other:?}"),
    };
    let requester_pid = match origin_case {
        OriginCase::AttachedPty => observer_pid,
        OriginCase::Control => second_pid.saturating_add(100),
    };
    open_mode_tree(&handler, requester_pid, &arguments).await;

    let mode = handler
        .active_attach
        .lock()
        .await
        .by_pid
        .get(&observer_pid)
        .and_then(|active| active.mode_tree.clone())
        .expect("observer has active mode-tree state");
    let geometry = handler
        .mode_tree_content_geometry(&mode)
        .await
        .expect("mode-tree geometry resolves");
    let context = format!(
        "{} / {} / {}",
        status_case.label(),
        kind.command_name(),
        origin_case.label()
    );
    assert_eq!(geometry, status_case.expected_geometry(), "{context}");

    let overlay = std::iter::from_fn(|| observer_rx.try_recv().ok())
        .find_map(|control| match control {
            AttachControl::Overlay(overlay) if !overlay.frame.is_empty() => Some(overlay.frame),
            _ => None,
        })
        .expect("observer receives a rendered overlay");
    status_case.assert_frame_rows(&overlay, &context);
}

#[tokio::test]
async fn mode_tree_geometry_reserves_status_for_tree_and_client_from_pty_and_control_origins() {
    let mut pid_offset = 1_000_u32;
    for status_case in [
        StatusCase::Bottom,
        StatusCase::Top,
        StatusCase::MultiBottom,
        StatusCase::Off,
    ] {
        for kind in [ModeTreeKind::Tree, ModeTreeKind::Client] {
            for origin_case in [OriginCase::AttachedPty, OriginCase::Control] {
                overlay_geometry_case(status_case, kind, origin_case, pid_offset).await;
                pid_offset = pid_offset.saturating_add(10);
            }
        }
    }
}
