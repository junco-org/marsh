/// Version reported for the binary control-mode contract this CLI implements.
pub(super) const BINARY_CONTRACT_VERSION: u32 = 1;

/// Command names whose output this CLI can render as JSON via `--json`.
pub(super) const JSON_COMMANDS: &[&str] = &[
    "capabilities",
    "display-message",
    "list-clients",
    "list-panes",
    "list-sessions",
    "list-windows",
];

/// Control-mode notification tags this CLI recognises on a `%`-prefixed line.
pub(super) const CONTROL_NOTIFICATIONS: &[&str] = &[
    "%begin",
    "%end",
    "%error",
    "%output",
    "%extended-output",
    "%pause",
    "%continue",
    "%exit",
    "%message",
    "%config-error",
    "%window-add",
    "%window-close",
    "%window-renamed",
    "%unlinked-window-add",
    "%unlinked-window-close",
    "%unlinked-window-renamed",
    "%window-pane-changed",
    "%pane-mode-changed",
    "%layout-change",
    "%session-changed",
    "%session-renamed",
    "%session-window-changed",
    "%sessions-changed",
    "%client-session-changed",
    "%client-detached",
    "%paste-buffer-changed",
    "%paste-buffer-deleted",
];

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeSet;

    use super::{CONTROL_NOTIFICATIONS, JSON_COMMANDS};

    #[test]
    fn json_commands_are_sorted_unique_and_stable() {
        assert_eq!(
            JSON_COMMANDS,
            [
                "capabilities",
                "display-message",
                "list-clients",
                "list-panes",
                "list-sessions",
                "list-windows",
            ]
        );
        assert_sorted_unique(JSON_COMMANDS);
    }

    #[test]
    fn control_notifications_are_unique_and_stable() {
        assert_eq!(
            CONTROL_NOTIFICATIONS,
            [
                "%begin",
                "%end",
                "%error",
                "%output",
                "%extended-output",
                "%pause",
                "%continue",
                "%exit",
                "%message",
                "%config-error",
                "%window-add",
                "%window-close",
                "%window-renamed",
                "%unlinked-window-add",
                "%unlinked-window-close",
                "%unlinked-window-renamed",
                "%window-pane-changed",
                "%pane-mode-changed",
                "%layout-change",
                "%session-changed",
                "%session-renamed",
                "%session-window-changed",
                "%sessions-changed",
                "%client-session-changed",
                "%client-detached",
                "%paste-buffer-changed",
                "%paste-buffer-deleted",
            ]
        );
        assert_unique(CONTROL_NOTIFICATIONS);
    }

    fn assert_sorted_unique(values: &[&str]) {
        for pair in values.windows(2) {
            assert!(pair[0] < pair[1], "{values:?} is not sorted");
        }
        assert_unique(values);
    }

    fn assert_unique(values: &[&str]) {
        let unique = values.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), values.len(), "{values:?} contains duplicates");
    }
}
