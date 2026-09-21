use clap::{ArgAction, Args};

use super::automation::{parse_duration, SendKeysWaitMode, DURATION_HELP};
use super::{parse_target_spec, TargetSpec};

/// Parsed arguments of `send-keys`, including the rmux-specific `--wait` synchronization flags.
#[derive(Debug, Clone, Args)]
pub(crate) struct SendKeysArgs {
    #[arg(short = 'F', action = ArgAction::SetTrue)]
    pub(crate) expand_formats: bool,
    #[arg(short = 'H', action = ArgAction::SetTrue)]
    pub(crate) hex: bool,
    #[arg(short = 'l', action = ArgAction::SetTrue)]
    pub(crate) literal: bool,
    #[arg(short = 'K', action = ArgAction::SetTrue)]
    pub(crate) key_table: bool,
    #[arg(short = 'M', action = ArgAction::SetTrue)]
    pub(crate) mouse: bool,
    #[arg(short = 'N')]
    pub(crate) repeat_count: Option<usize>,
    #[arg(short = 'p', action = ArgAction::SetTrue, hide = true)]
    pub(crate) unsupported_prefix: bool,
    #[arg(short = 'R', action = ArgAction::SetTrue)]
    pub(crate) reset_terminal: bool,
    #[arg(short = 'X', action = ArgAction::SetTrue)]
    pub(crate) copy_mode: bool,
    #[arg(short = 'c', allow_hyphen_values = true)]
    pub(crate) client_target: Option<String>,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    #[arg(long = "wait", value_parser = parse_send_keys_wait_mode)]
    pub(crate) wait: Option<SendKeysWaitMode>,
    #[arg(long = "wait-text")]
    pub(crate) wait_text: Option<String>,
    #[arg(long = "wait-visible-text")]
    pub(crate) wait_visible_text: Option<String>,
    #[arg(long = "wait-next-text")]
    pub(crate) wait_next_text: Option<String>,
    #[arg(long = "wait-pane-exit", action = ArgAction::SetTrue)]
    pub(crate) wait_pane_exit: bool,
    #[arg(long = "stable-for", value_parser = parse_duration, value_name = "DURATION", help = DURATION_HELP)]
    pub(crate) stable_for: Option<std::time::Duration>,
    #[arg(long = "timeout", value_parser = parse_duration, value_name = "DURATION", help = DURATION_HELP)]
    pub(crate) timeout: Option<std::time::Duration>,
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    pub(crate) keys: Vec<String>,
}

impl SendKeysArgs {
    /// Whether any `--wait` condition was requested, so the client must synchronize after sending.
    pub(crate) fn has_wait(&self) -> bool {
        self.wait.is_some()
            || self.wait_text.is_some()
            || self.wait_visible_text.is_some()
            || self.wait_next_text.is_some()
            || self.wait_pane_exit
    }

    /// Rejects conflicting, empty, or unsupported combinations of the `--wait` family of flags.
    pub(crate) fn validate(self) -> Result<Self, clap::Error> {
        let waits = [
            self.wait.is_some(),
            self.wait_text.is_some(),
            self.wait_visible_text.is_some(),
            self.wait_next_text.is_some(),
            self.wait_pane_exit,
        ]
        .into_iter()
        .filter(|selected| *selected)
        .count();
        let has_wait = waits > 0;
        if waits > 1 {
            return Err(value_error(
                "send-keys",
                "only one --wait condition may be selected",
            ));
        }
        if self.timeout.is_some() && !has_wait {
            return Err(value_error(
                "send-keys",
                "--timeout is valid only with a --wait condition",
            ));
        }
        reject_empty("send-keys", "--wait-text", self.wait_text.as_deref())?;
        reject_empty(
            "send-keys",
            "--wait-visible-text",
            self.wait_visible_text.as_deref(),
        )?;
        reject_empty(
            "send-keys",
            "--wait-next-text",
            self.wait_next_text.as_deref(),
        )?;
        if self.stable_for.is_some() && self.wait != Some(SendKeysWaitMode::Quiet) {
            return Err(value_error(
                "send-keys",
                "--stable-for is valid only with --wait quiet",
            ));
        }
        Ok(self)
    }
}

/// Parsed arguments of `bind-key`, attaching a command to a key in a key table.
#[derive(Debug, Clone, Args)]
pub(crate) struct BindKeyArgs {
    #[arg(short = 'n', action = ArgAction::SetTrue)]
    pub(crate) root_table: bool,
    #[arg(short = 'r', action = ArgAction::SetTrue)]
    pub(crate) repeat: bool,
    #[arg(short = 'N')]
    pub(crate) note: Option<String>,
    #[arg(short = 'T')]
    pub(crate) table_name: Option<String>,
    #[arg(allow_hyphen_values = true)]
    pub(crate) key: String,
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    pub(crate) command: Vec<String>,
}

/// Parsed arguments of `unbind-key`, removing one key or an entire key table.
#[derive(Debug, Clone, Args)]
pub(crate) struct UnbindKeyArgs {
    #[arg(short = 'a', action = ArgAction::SetTrue)]
    pub(crate) all: bool,
    #[arg(short = 'n', action = ArgAction::SetTrue)]
    pub(crate) root_table: bool,
    #[arg(short = 'q', action = ArgAction::SetTrue)]
    pub(crate) quiet: bool,
    #[arg(short = 'T')]
    pub(crate) table_name: Option<String>,
    #[arg(allow_hyphen_values = true)]
    pub(crate) key: Option<String>,
}

/// Parsed arguments of `list-keys`, selecting which bindings to print and how to format them.
#[derive(Debug, Clone, Args)]
pub(crate) struct ListKeysArgs {
    #[arg(short = '1', action = ArgAction::SetTrue)]
    pub(crate) first_only: bool,
    #[arg(short = 'a', action = ArgAction::SetTrue)]
    pub(crate) include_unnoted: bool,
    #[arg(short = 'N', action = ArgAction::SetTrue)]
    pub(crate) notes: bool,
    #[arg(short = 'r', action = ArgAction::SetTrue)]
    pub(crate) reversed: bool,
    #[arg(short = 'F')]
    pub(crate) format: Option<String>,
    #[arg(short = 'O')]
    pub(crate) sort_order: Option<String>,
    #[arg(short = 'P')]
    pub(crate) prefix: Option<String>,
    #[arg(short = 'T')]
    pub(crate) table_name: Option<String>,
    #[arg(allow_hyphen_values = true)]
    pub(crate) key: Option<String>,
}

impl ListKeysArgs {
    /// Accepts the parsed arguments unchanged; `list-keys` has no cross-flag constraints.
    pub(crate) fn validate(self) -> Result<Self, clap::Error> {
        Ok(self)
    }
}

/// Parsed arguments of `send-prefix`, sending the primary or secondary prefix key to a pane.
#[derive(Debug, Clone, Args)]
pub(crate) struct SendPrefixArgs {
    #[arg(short = '2', action = ArgAction::SetTrue)]
    pub(crate) secondary: bool,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
}

impl BindKeyArgs {
    /// The key table to bind into: the explicit `-T` name, else `root` with `-n`, else `prefix`.
    pub(crate) fn table_name(&self) -> String {
        if let Some(table_name) = &self.table_name {
            table_name.clone()
        } else if self.root_table {
            "root".to_owned()
        } else {
            "prefix".to_owned()
        }
    }
}

impl UnbindKeyArgs {
    /// The key table to unbind from: the explicit `-T` name, else `root` with `-n`, else `prefix`.
    pub(crate) fn table_name(&self) -> String {
        if let Some(table_name) = &self.table_name {
            table_name.clone()
        } else if self.root_table {
            "root".to_owned()
        } else {
            "prefix".to_owned()
        }
    }
}

/// Parses the `--wait` mode argument; `quiet` is the only supported mode.
fn parse_send_keys_wait_mode(value: &str) -> Result<SendKeysWaitMode, String> {
    match value {
        "quiet" => Ok(SendKeysWaitMode::Quiet),
        _ => Err("supported wait modes: quiet".to_owned()),
    }
}

/// Fails when a present flag value is the empty string.
fn reject_empty(command_name: &str, flag: &str, value: Option<&str>) -> Result<(), clap::Error> {
    if value.is_some_and(str::is_empty) {
        return Err(value_error(
            command_name,
            format!("{flag} must not be empty"),
        ));
    }
    Ok(())
}

/// Builds a clap value-validation error prefixed with the offending command name.
fn value_error(command_name: &str, message: impl std::fmt::Display) -> clap::Error {
    clap::Error::raw(
        clap::error::ErrorKind::ValueValidation,
        format!("command {command_name}: {message}"),
    )
}
