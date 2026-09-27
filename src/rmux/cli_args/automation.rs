use std::time::Duration;

use clap::{ArgAction, Args};

use super::targets::parse_session_name;
use super::validate::{Validate, reject_empty, selected_count, value_error};
use super::{TargetSpec, parse_target_spec};

/// Shared help text for every flag parsed by [`parse_duration`].
///
/// `parse_duration` deliberately rejects bare integers so that `--timeout 8000`
/// can never be silently read as either 8 seconds or 8000 seconds. The help
/// output has to say so, otherwise the requirement is only discoverable by
/// hitting the error.
pub(crate) const DURATION_HELP: &str =
    "Duration with an explicit unit: ms, s, or m (for example 500ms, 8s, 2m)";

/// Flags for `wait-pane`, which blocks until one pane condition holds.
#[derive(Debug, Clone, Args)]
pub(crate) struct WaitPaneArgs {
    #[arg(short = 't', long = "target", value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    #[arg(long = "text")]
    pub(crate) text: Option<String>,
    #[arg(long = "next-text")]
    pub(crate) next_text: Option<String>,
    #[arg(long = "visible-text")]
    pub(crate) visible_text: Option<String>,
    #[arg(long = "quiet", action = ArgAction::SetTrue)]
    pub(crate) quiet: bool,
    #[arg(long = "stable-for", value_parser = parse_duration, value_name = "DURATION", help = DURATION_HELP)]
    pub(crate) stable_for: Option<Duration>,
    #[arg(long = "pane-exit", action = ArgAction::SetTrue)]
    pub(crate) pane_exit: bool,
    #[arg(long = "timeout", value_parser = parse_duration, value_name = "DURATION", help = DURATION_HELP)]
    pub(crate) timeout: Option<Duration>,
    #[arg(long = "json", action = ArgAction::SetTrue)]
    pub(crate) json: bool,
    #[arg(long = "get-by-text")]
    pub(crate) get_by_text: Option<String>,
}

impl Validate for WaitPaneArgs {
    /// Rejects empty patterns and anything but exactly one wait condition.
    fn validate(self, command_name: &'static str) -> Result<Self, clap::Error> {
        let conditions = selected_count([
            self.text.is_some(),
            self.next_text.is_some(),
            self.visible_text.is_some(),
            self.quiet,
            self.pane_exit,
            self.get_by_text.is_some(),
        ]);
        if conditions != 1 {
            return Err(value_error(
                command_name,
                "exactly one wait condition is required",
            ));
        }
        reject_empty(command_name, "--text", self.text.as_deref())?;
        reject_empty(command_name, "--next-text", self.next_text.as_deref())?;
        reject_empty(command_name, "--visible-text", self.visible_text.as_deref())?;
        reject_empty(command_name, "--get-by-text", self.get_by_text.as_deref())?;
        if self.stable_for.is_some() && !self.quiet {
            return Err(value_error(
                command_name,
                "--stable-for is valid only with --quiet",
            ));
        }
        Ok(self)
    }
}

/// Flags for `pane-snapshot`, which prints a pane's current screen contents.
#[derive(Debug, Clone, Args)]
pub(crate) struct PaneSnapshotArgs {
    #[arg(short = 't', long = "target", value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    #[arg(long = "json", action = ArgAction::SetTrue)]
    pub(crate) json: bool,
    #[arg(long = "style", action = ArgAction::SetTrue)]
    pub(crate) style: bool,
    #[arg(long = "region", value_parser = parse_region)]
    pub(crate) region: Option<SnapshotRegion>,
}

/// Rectangular screen area selected by `--region`, as `row,col,rows,cols`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SnapshotRegion {
    pub(crate) row: u16,
    pub(crate) col: u16,
    pub(crate) rows: u16,
    pub(crate) cols: u16,
}

/// Flags for `stream-pane`, which follows a pane's output as it arrives.
#[derive(Debug, Clone, Args)]
pub(crate) struct StreamPaneArgs {
    #[arg(short = 't', long = "target", value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    #[arg(long = "raw", action = ArgAction::SetTrue)]
    pub(crate) raw: bool,
    #[arg(long = "lines", action = ArgAction::SetTrue)]
    pub(crate) lines: bool,
}

impl Validate for StreamPaneArgs {
    /// Rejects `--raw` combined with `--lines`, which select different framings.
    fn validate(self, command_name: &'static str) -> Result<Self, clap::Error> {
        if self.raw && self.lines {
            return Err(value_error(
                command_name,
                "--raw and --lines are mutually exclusive",
            ));
        }
        Ok(self)
    }
}

/// Flags for `collect-pane-output`, which buffers a pane's output until it exits.
#[derive(Debug, Clone, Args)]
pub(crate) struct CollectPaneOutputArgs {
    #[arg(short = 't', long = "target", value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    #[arg(long = "until-pane-exit", action = ArgAction::SetTrue)]
    pub(crate) until_pane_exit: bool,
    #[arg(long = "max-bytes", value_parser = parse_positive_usize)]
    pub(crate) max_bytes: usize,
    #[arg(long = "json", action = ArgAction::SetTrue)]
    pub(crate) json: bool,
}

impl Validate for CollectPaneOutputArgs {
    /// Requires `--until-pane-exit`, the only supported collection stop condition.
    fn validate(self, command_name: &'static str) -> Result<Self, clap::Error> {
        if !self.until_pane_exit {
            return Err(value_error(command_name, "--until-pane-exit is required"));
        }
        Ok(self)
    }
}

/// Flags for `locator`, which resolves a pane by the text it displays.
#[derive(Debug, Clone, Args)]
pub(crate) struct LocatorArgs {
    #[arg(short = 't', long = "target", value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    #[arg(long = "get-by-text")]
    pub(crate) get_by_text: String,
    #[arg(long = "json", action = ArgAction::SetTrue)]
    pub(crate) json: bool,
}

impl Validate for LocatorArgs {
    /// Rejects an empty `--get-by-text` pattern, which would match everything.
    fn validate(self, command_name: &'static str) -> Result<Self, clap::Error> {
        reject_empty(command_name, "--get-by-text", Some(&self.get_by_text))?;
        Ok(self)
    }
}

/// Flags for `expect-pane`, which asserts visibility or match count of pane text.
#[derive(Debug, Clone, Args)]
pub(crate) struct ExpectPaneArgs {
    #[arg(short = 't', long = "target", value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    #[arg(long = "get-by-text")]
    pub(crate) get_by_text: String,
    #[arg(long = "visible", action = ArgAction::SetTrue)]
    pub(crate) visible: bool,
    #[arg(long = "hidden", action = ArgAction::SetTrue)]
    pub(crate) hidden: bool,
    #[arg(long = "count")]
    pub(crate) count: Option<usize>,
    #[arg(long = "json", action = ArgAction::SetTrue)]
    pub(crate) json: bool,
}

impl Validate for ExpectPaneArgs {
    /// Requires a nonempty pattern and exactly one of the three assertions.
    fn validate(self, command_name: &'static str) -> Result<Self, clap::Error> {
        reject_empty(command_name, "--get-by-text", Some(&self.get_by_text))?;
        if selected_count([self.visible, self.hidden, self.count.is_some()]) != 1 {
            return Err(value_error(
                command_name,
                "exactly one assertion is required",
            ));
        }
        Ok(self)
    }
}

/// Filters for `find-panes`, which lists panes matching title, command, or cwd.
#[derive(Debug, Clone, Args)]
pub(crate) struct FindPanesArgs {
    #[arg(long = "title")]
    pub(crate) title: Option<String>,
    #[arg(long = "title-prefix")]
    pub(crate) title_prefix: Option<String>,
    #[arg(long = "current-command")]
    pub(crate) current_command: Option<String>,
    #[arg(long = "cwd")]
    pub(crate) cwd: Option<String>,
    #[arg(long = "json", action = ArgAction::SetTrue)]
    pub(crate) json: bool,
}

/// Filters for `find-sessions`, which lists sessions matching an exact name or prefix.
#[derive(Debug, Clone, Args)]
pub(crate) struct FindSessionsArgs {
    #[arg(long = "name")]
    pub(crate) name: Option<String>,
    #[arg(long = "name-prefix")]
    pub(crate) name_prefix: Option<String>,
    #[arg(long = "json", action = ArgAction::SetTrue)]
    pub(crate) json: bool,
}

/// Flags for `broadcast-keys`, which sends the same keys to several panes.
#[derive(Debug, Clone, Args)]
pub(crate) struct BroadcastKeysArgs {
    #[arg(short = 't', long = "target", value_parser = parse_target_spec, allow_hyphen_values = true, action = ArgAction::Append)]
    pub(crate) targets: Vec<TargetSpec>,
    #[arg(short = 'l', action = ArgAction::SetTrue)]
    pub(crate) literal: bool,
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    pub(crate) keys: Vec<String>,
}

impl Validate for BroadcastKeysArgs {
    /// Requires at least one `--target` and at least one key to send.
    fn validate(self, command_name: &'static str) -> Result<Self, clap::Error> {
        if self.targets.is_empty() {
            return Err(value_error(
                command_name,
                "at least one --target is required",
            ));
        }
        if self.keys.is_empty() {
            return Err(value_error(command_name, "at least one key is required"));
        }
        Ok(self)
    }
}

/// Flags for `with-session`, which runs a child command against a scoped session.
#[derive(Debug, Clone, Args)]
pub(crate) struct WithSessionArgs {
    #[arg(value_parser = parse_session_name)]
    pub(crate) session_name: rmux_proto::SessionName,
    #[arg(long = "kill-on-owner-exit", action = ArgAction::SetTrue)]
    pub(crate) kill_on_owner_exit: bool,
    #[arg(long = "ttl", value_parser = parse_duration, default_value = "30s", value_name = "DURATION", help = DURATION_HELP)]
    pub(crate) ttl: Duration,
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    pub(crate) command: Vec<String>,
}

impl Validate for WithSessionArgs {
    /// Requires a child command, since `with-session` exists to run one.
    fn validate(self, command_name: &'static str) -> Result<Self, clap::Error> {
        if self.command.is_empty() {
            return Err(value_error(command_name, "a child command is required"));
        }
        Ok(self)
    }
}

/// Quiescence mode selecting how `send-keys` waits for the pane to settle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendKeysWaitMode {
    Quiet,
}

/// Parses a positive duration with a mandatory `ms`, `s`, or `m` unit suffix.
pub(crate) fn parse_duration(value: &str) -> Result<Duration, String> {
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000)
    } else {
        return Err(
            "duration requires an explicit unit: ms, s, or m (for example 500ms, 8s, 2m)"
                .to_owned(),
        );
    };
    if number.is_empty() || number.starts_with('-') {
        return Err("duration must be positive".to_owned());
    }
    let amount = number
        .parse::<u64>()
        .map_err(|_| "duration must be an integer".to_owned())?;
    if amount == 0 {
        return Err("duration must be positive".to_owned());
    }
    amount
        .checked_mul(multiplier)
        .map(Duration::from_millis)
        .ok_or_else(|| "duration is too large".to_owned())
}

/// Parses a `row,col,rows,cols` region, requiring positive extents.
fn parse_region(value: &str) -> Result<SnapshotRegion, String> {
    let parts = value.split(',').collect::<Vec<_>>();
    let [row, col, rows, cols] = parts.as_slice() else {
        return Err("region must use row,col,rows,cols".to_owned());
    };
    Ok(SnapshotRegion {
        row: parse_u16(row, "row")?,
        col: parse_u16(col, "col")?,
        rows: parse_positive_u16(rows, "rows")?,
        cols: parse_positive_u16(cols, "cols")?,
    })
}

/// Parses one region component as a `u16`, naming the field in the error.
fn parse_u16(value: &str, field: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .map_err(|_| format!("{field} must be a u16"))
}

/// Parses one region extent as a `u16` that must be greater than zero.
fn parse_positive_u16(value: &str, field: &str) -> Result<u16, String> {
    let parsed = parse_u16(value, field)?;
    if parsed == 0 {
        return Err(format!("{field} must be positive"));
    }
    Ok(parsed)
}

/// Parses a byte or item count that must be greater than zero.
fn parse_positive_usize(value: &str) -> Result<usize, String> {
    value
        .parse::<std::num::NonZeroUsize>()
        .map(std::num::NonZeroUsize::get)
        .map_err(|_| "value must be a positive integer".to_owned())
}
