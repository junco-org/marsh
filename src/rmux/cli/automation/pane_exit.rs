use serde_json::{json, Value};

use super::super::ExitFailure;

/// What the daemon reported about how a pane's process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::cli) struct PaneExitStatus {
    stale: bool,
    exit_status: Option<i32>,
    exit_signal: Option<i32>,
}

impl PaneExitStatus {
    /// Observation for a target that no longer exists, so no exit evidence will ever arrive.
    pub(in crate::cli) const fn stale() -> Self {
        Self {
            stale: true,
            exit_status: None,
            exit_signal: None,
        }
    }

    /// Observation for a live target whose status and signal are whatever the daemon retained.
    pub(in crate::cli) const fn known(exit_status: Option<i32>, exit_signal: Option<i32>) -> Self {
        Self {
            stale: false,
            exit_status,
            exit_signal,
        }
    }

    /// Renders this observation as the JSON object the automation commands emit.
    pub(super) fn json_value(self) -> Value {
        json!({
            "stale": self.stale,
            "exit_status": self.exit_status,
            "exit_signal": self.exit_signal,
        })
    }

    /// Whether this observation actually settles how the process ended.
    ///
    /// A pane can be reported dead a moment before the daemon has retained *how* it died, so an
    /// observation carrying neither a status nor a signal is not yet an answer — only a stale
    /// target makes the absence final, because nothing further will ever be recorded for it.
    pub(in crate::cli) const fn is_conclusive(self) -> bool {
        self.stale || self.exit_status.is_some() || self.exit_signal.is_some()
    }

    /// Exit code `send-keys` reports for an observed pane exit.
    pub(super) fn send_keys_exit_code(observation: Option<Self>) -> Result<i32, ExitFailure> {
        Self::resolved_exit_code(observation, "send-keys")
    }

    /// Maps an observed pane exit onto this process' exit code.
    ///
    /// Fails closed: a pane whose process status was never captured, or whose only evidence is
    /// a stale target, does not become a successful exit. `command_name` names the caller in
    /// the resulting diagnostic.
    pub(in crate::cli) fn resolved_exit_code(
        observation: Option<Self>,
        command_name: &str,
    ) -> Result<i32, ExitFailure> {
        let observation = observation.ok_or_else(|| {
            ExitFailure::new(
                1,
                format!("{command_name} observed pane exit without process exit metadata"),
            )
        })?;
        if let Some(exit_status) = observation.exit_status {
            return Ok(exit_status);
        }
        if let Some(exit_signal) = observation.exit_signal.filter(|signal| *signal > 0) {
            return 128_i32.checked_add(exit_signal).ok_or_else(|| {
                ExitFailure::new(
                    1,
                    format!("{command_name} observed invalid pane exit signal {exit_signal}"),
                )
            });
        }

        let detail = if observation.stale {
            " because the pane target became stale"
        } else {
            ""
        };
        Err(ExitFailure::new(
            1,
            format!("{command_name} could not determine the pane process exit status{detail}"),
        ))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::PaneExitStatus;

    #[test]
    fn normal_exit_codes_are_preserved_without_cli_truncation() {
        for exit_status in [0, 7, 513] {
            assert_eq!(
                PaneExitStatus::send_keys_exit_code(Some(PaneExitStatus::known(
                    Some(exit_status),
                    None,
                )))
                .expect("normal exit status must be available"),
                exit_status
            );
        }
    }

    #[test]
    fn unix_signal_uses_conventional_shell_exit_code() {
        assert_eq!(
            PaneExitStatus::send_keys_exit_code(Some(PaneExitStatus::known(None, Some(15))))
                .expect("valid signal must map to a shell exit code"),
            143
        );
    }

    #[test]
    fn missing_or_unknown_exit_metadata_fails_closed() {
        for observation in [
            None,
            Some(PaneExitStatus::stale()),
            Some(PaneExitStatus::known(None, None)),
            Some(PaneExitStatus::known(None, Some(0))),
        ] {
            let error = PaneExitStatus::send_keys_exit_code(observation)
                .expect_err("unknown exit outcome must not become success");
            assert_eq!(error.exit_code(), 1);
        }
    }

    #[test]
    fn serialized_exit_metadata_keeps_the_existing_json_shape() {
        let value = PaneExitStatus::known(Some(7), None).json_value();

        assert_eq!(
            value,
            json!({
                "stale": false,
                "exit_status": 7,
                "exit_signal": null,
            })
        );
    }
}
