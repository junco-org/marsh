use clap::{ArgAction, Args};

use super::{parse_target_spec, TargetSpec};

/// Parsed arguments of `capture-pane`, selecting the line range and output encoding.
#[derive(Debug, Clone, Args)]
pub(crate) struct CapturePaneArgs {
    #[arg(short = 'a', action = ArgAction::SetTrue)]
    pub(crate) alternate: bool,
    #[arg(short = 'e', action = ArgAction::SetTrue)]
    pub(crate) escape_ansi: bool,
    #[arg(short = 'C', action = ArgAction::SetTrue)]
    pub(crate) escape_sequences: bool,
    #[arg(short = 'F', action = ArgAction::SetTrue)]
    pub(crate) include_format: bool,
    #[arg(short = 'H', action = ArgAction::SetTrue)]
    pub(crate) hyperlinks: bool,
    #[arg(short = 'J', action = ArgAction::SetTrue)]
    pub(crate) join_wrapped: bool,
    #[arg(short = 'L', action = ArgAction::SetTrue)]
    pub(crate) line_numbers: bool,
    #[arg(short = 'M', action = ArgAction::SetTrue)]
    pub(crate) use_mode_screen: bool,
    #[arg(short = 'N', action = ArgAction::SetTrue)]
    pub(crate) do_not_trim_spaces: bool,
    #[arg(short = 'T', action = ArgAction::SetTrue)]
    pub(crate) preserve_trailing_spaces: bool,
    #[arg(short = 'P', action = ArgAction::SetTrue)]
    pub(crate) pending_input: bool,
    #[arg(short = 'q', action = ArgAction::SetTrue)]
    pub(crate) quiet: bool,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    #[arg(short = 'S', allow_hyphen_values = true)]
    pub(crate) start: Option<String>,
    #[arg(short = 'E', allow_hyphen_values = true)]
    pub(crate) end: Option<String>,
    #[arg(short = 'p', action = ArgAction::SetTrue)]
    pub(crate) print: bool,
    #[arg(short = 'b')]
    pub(crate) buffer_name: Option<String>,
}

impl CapturePaneArgs {
    /// Accepts the parsed arguments unchanged; `capture-pane` has no cross-flag constraints.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "queue dispatch uses `and_then(CapturePaneArgs::validate)` alongside fallible validators"
    )]
    pub(crate) const fn validate(self) -> Result<Self, clap::Error> {
        Ok(self)
    }
}

/// Parsed arguments of `clear-history`, emptying one pane's scrollback.
#[derive(Debug, Clone, Args)]
pub(crate) struct ClearHistoryArgs {
    #[arg(short = 'H', action = ArgAction::SetTrue)]
    pub(crate) reset_hyperlinks: bool,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
}
