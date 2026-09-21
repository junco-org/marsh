use std::path::PathBuf;

use clap::{ArgAction, ArgGroup, Args};
use rmux_proto::RotateWindowDirection;

use super::{parse_command_args, parse_target_spec, QueuedCommand, TargetSpec};

/// Parses `rename-window` arguments, rejecting anything but exactly one new name.
pub(super) fn parse_rename_window_args(
    arguments: Vec<String>,
) -> Result<RenameWindowArgs, clap::Error> {
    parse_command_args::<RawRenameWindowArgs>("rename-window", arguments)?.validate()
}

/// Parses `select-window` arguments and rejects the unsupported `-Z` flag.
pub(super) fn parse_select_window_args(
    arguments: Vec<String>,
) -> Result<SelectWindowArgs, clap::Error> {
    parse_command_args::<SelectWindowArgs>("select-window", arguments)?.validate()
}

/// Parses `swap-window` arguments and rejects the unsupported `-a` flag.
pub(super) fn parse_swap_window_args(
    arguments: Vec<String>,
) -> Result<SwapWindowArgs, clap::Error> {
    parse_command_args::<SwapWindowArgs>("swap-window", arguments)?.validate()
}

/// Arguments of `new-window`: placement, naming, environment and the command to run.
#[derive(Debug, Clone, Args)]
#[command(group(
    ArgGroup::new("placement")
        .required(false)
        .multiple(false)
        .args(["after", "before"])
))]
pub(crate) struct NewWindowArgs {
    #[arg(short = 'a', action = ArgAction::SetTrue)]
    pub(crate) after: bool,
    #[arg(short = 'b', action = ArgAction::SetTrue)]
    pub(crate) before: bool,
    #[arg(short = 'c', allow_hyphen_values = true)]
    pub(crate) start_directory: Option<PathBuf>,
    #[arg(short = 'e')]
    pub(crate) environment: Vec<String>,
    #[arg(short = 'F', allow_hyphen_values = true)]
    pub(crate) format: Option<String>,
    #[arg(short = 'P', action = ArgAction::SetTrue)]
    pub(crate) print_target: bool,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    #[arg(short = 'k', action = ArgAction::SetTrue)]
    pub(crate) kill_existing: bool,
    #[arg(short = 'S', action = ArgAction::SetTrue)]
    pub(crate) select_existing: bool,
    #[arg(short = 'n')]
    pub(crate) name: Option<String>,
    #[arg(short = 'd', action = ArgAction::SetTrue)]
    pub(crate) detached: bool,
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    pub(crate) command: Vec<String>,
    #[arg(skip = String::new())]
    pub(crate) queue_command: String,
}

impl QueuedCommand for NewWindowArgs {
    /// Records the original `new-window` line so the dispatcher can queue it.
    fn set_queue_command(&mut self, queue_command: String) {
        self.queue_command = queue_command;
    }
}

/// Arguments of `kill-window`, including `-a` to kill every other window instead.
#[derive(Debug, Clone, Args)]
pub(crate) struct KillWindowArgs {
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    #[arg(short = 'a', action = ArgAction::SetTrue)]
    pub(crate) kill_others: bool,
}

/// Arguments of commands whose only option is a `-t` window target.
#[derive(Debug, Clone, Args)]
pub(crate) struct WindowTargetArgs {
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
}

/// Arguments of `select-window`: relative navigation flags plus an explicit target.
#[derive(Debug, Clone, Args)]
#[command(group(
    ArgGroup::new("navigation")
        .required(false)
        .multiple(false)
        .args(["last", "next", "previous"])
))]
pub(crate) struct SelectWindowArgs {
    #[arg(short = 'l', action = ArgAction::SetTrue, group = "navigation")]
    pub(crate) last: bool,
    #[arg(short = 'n', action = ArgAction::SetTrue, group = "navigation")]
    pub(crate) next: bool,
    #[arg(short = 'p', action = ArgAction::SetTrue, group = "navigation")]
    pub(crate) previous: bool,
    #[arg(short = 'T', action = ArgAction::SetTrue)]
    pub(crate) toggle_last: bool,
    #[arg(short = 'Z', action = ArgAction::SetTrue, hide = true)]
    reject_zoom: bool,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
}

impl SelectWindowArgs {
    /// Rejects `select-window -Z`, which `rmux` does not implement.
    fn validate(self) -> Result<Self, clap::Error> {
        if self.reject_zoom {
            return Err(tmux_unknown_flag_error("select-window", "-Z"));
        }
        Ok(self)
    }
}

/// Arguments of `rename-window`: the target window and its single new name.
#[derive(Debug, Clone, Args)]
pub(crate) struct RenameWindowArgs {
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    #[arg(allow_hyphen_values = true)]
    pub(crate) new_name: String,
}

/// Raw `rename-window` parse with unbounded names, used to emit tmux arity errors.
#[derive(Debug, Clone, Args)]
struct RawRenameWindowArgs {
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    target: Option<TargetSpec>,
    #[arg(allow_hyphen_values = true, num_args = 1..)]
    names: Vec<String>,
}

impl RawRenameWindowArgs {
    /// Rejects zero or several names with tmux's arity wording, else yields one rename.
    fn validate(self) -> Result<RenameWindowArgs, clap::Error> {
        match self.names.as_slice() {
            [new_name] => Ok(RenameWindowArgs {
                target: self.target,
                new_name: new_name.clone(),
            }),
            [] => Err(clap::Error::raw(
                clap::error::ErrorKind::TooFewValues,
                "command rename-window: too few arguments (need at least 1)",
            )),
            _ => Err(clap::Error::raw(
                clap::error::ErrorKind::TooManyValues,
                "command rename-window: too many arguments (need at most 1)",
            )),
        }
    }
}

/// Builds the tmux-style `unknown flag` parse error for an accepted-but-unsupported flag.
fn tmux_unknown_flag_error(command_name: &str, flag: &str) -> clap::Error {
    clap::Error::raw(
        clap::error::ErrorKind::UnknownArgument,
        format!("command {command_name}: unknown flag {flag}"),
    )
}

/// Arguments of `list-windows`: scope, formatting, filtering and sort order.
#[derive(Debug, Clone, Args)]
pub(crate) struct ListWindowsArgs {
    #[arg(short = 'a', action = ArgAction::SetTrue)]
    pub(crate) all_sessions: bool,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    #[arg(short = 'F', conflicts_with = "json")]
    pub(crate) format: Option<String>,
    #[arg(long = "json", action = ArgAction::SetTrue)]
    pub(crate) json: bool,
    #[arg(short = 'f', allow_hyphen_values = true)]
    pub(crate) filter: Option<String>,
    #[arg(short = 'O')]
    pub(crate) sort_order: Option<String>,
    #[arg(short = 'r', action = ArgAction::SetTrue)]
    pub(crate) reversed: bool,
}

/// Arguments of `move-window`: source, destination and placement or reindex behavior.
#[derive(Debug, Clone, Args)]
#[command(group(
    ArgGroup::new("position")
        .required(false)
        .multiple(false)
        .args(["after", "before"])
))]
pub(crate) struct MoveWindowArgs {
    #[arg(short = 'a', action = ArgAction::SetTrue, group = "position")]
    pub(crate) after: bool,
    #[arg(short = 'b', action = ArgAction::SetTrue, group = "position")]
    pub(crate) before: bool,
    #[arg(short = 'r', action = ArgAction::SetTrue)]
    pub(crate) reindex: bool,
    #[arg(short = 'k', action = ArgAction::SetTrue, conflicts_with = "reindex")]
    pub(crate) kill_target: bool,
    #[arg(short = 'd', action = ArgAction::SetTrue)]
    pub(crate) detached: bool,
    #[arg(short = 's', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) source: Option<TargetSpec>,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
}

/// Arguments of `swap-window`: the two windows to exchange and detach behavior.
#[derive(Debug, Clone, Args)]
pub(crate) struct SwapWindowArgs {
    #[arg(short = 'a', action = ArgAction::SetTrue, hide = true)]
    reject_after: bool,
    #[arg(short = 'd', action = ArgAction::SetTrue)]
    pub(crate) detached: bool,
    #[arg(short = 's', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) source: Option<TargetSpec>,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
}

impl SwapWindowArgs {
    /// Rejects `swap-window -a`, which `rmux` does not implement.
    fn validate(self) -> Result<Self, clap::Error> {
        if self.reject_after {
            return Err(tmux_unknown_flag_error("swap-window", "-a"));
        }
        Ok(self)
    }
}

/// Arguments of `link-window`: source, destination and placement or kill behavior.
#[derive(Debug, Clone, Args)]
#[command(group(
    ArgGroup::new("position")
        .required(false)
        .multiple(false)
        .args(["after", "before"])
))]
pub(crate) struct LinkWindowArgs {
    #[arg(short = 'a', action = ArgAction::SetTrue, group = "position")]
    pub(crate) after: bool,
    #[arg(short = 'b', action = ArgAction::SetTrue, group = "position")]
    pub(crate) before: bool,
    #[arg(short = 'd', action = ArgAction::SetTrue)]
    pub(crate) detached: bool,
    #[arg(short = 'k', action = ArgAction::SetTrue)]
    pub(crate) kill_target: bool,
    #[arg(short = 's', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) source: Option<TargetSpec>,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
}

/// Arguments of `unlink-window`, including `-k` to kill the last linked window.
#[derive(Debug, Clone, Args)]
pub(crate) struct UnlinkWindowArgs {
    #[arg(short = 'k', action = ArgAction::SetTrue)]
    pub(crate) kill_if_last: bool,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
}

/// Arguments of `rotate-window`: rotation direction, zoom restore and target.
#[derive(Debug, Clone, Args)]
#[command(group(
    ArgGroup::new("direction")
        .required(false)
        .multiple(false)
        .args(["down", "up"])
))]
pub(crate) struct RotateWindowArgs {
    #[arg(short = 'D', action = ArgAction::SetTrue, group = "direction")]
    pub(crate) down: bool,
    #[arg(short = 'U', action = ArgAction::SetTrue, group = "direction")]
    pub(crate) up: bool,
    #[arg(short = 'Z', action = ArgAction::SetTrue)]
    pub(crate) restore_zoom: bool,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
}

impl RotateWindowArgs {
    /// Rotation direction requested, defaulting to `Up` when `-D` was not given.
    pub(crate) fn direction(&self) -> RotateWindowDirection {
        if self.down {
            RotateWindowDirection::Down
        } else {
            RotateWindowDirection::Up
        }
    }
}

/// Arguments of `resize-window`: directional or absolute sizing and the adjustment step.
#[derive(Debug, Clone, Args)]
#[command(group(
    ArgGroup::new("balanced")
        .required(false)
        .multiple(false)
        .args(["expand", "shrink"])
))]
pub(crate) struct ResizeWindowArgs {
    #[arg(short = 'A', action = ArgAction::SetTrue, group = "balanced")]
    pub(crate) expand: bool,
    #[arg(short = 'a', action = ArgAction::SetTrue, group = "balanced")]
    pub(crate) shrink: bool,
    #[arg(short = 'D', action = ArgAction::SetTrue)]
    pub(crate) down: bool,
    #[arg(short = 'U', action = ArgAction::SetTrue)]
    pub(crate) up: bool,
    #[arg(short = 'L', action = ArgAction::SetTrue)]
    pub(crate) left: bool,
    #[arg(short = 'R', action = ArgAction::SetTrue)]
    pub(crate) right: bool,
    #[arg(short = 'x')]
    pub(crate) width: Option<u16>,
    #[arg(short = 'y')]
    pub(crate) height: Option<u16>,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    /// Adjustment amount (default 1).
    pub(crate) adjustment: Option<u16>,
}

/// Arguments of `respawn-window`: start directory, environment and replacement command.
#[derive(Debug, Clone, Args)]
pub(crate) struct RespawnWindowArgs {
    #[arg(short = 'c', allow_hyphen_values = true)]
    pub(crate) start_directory: Option<PathBuf>,
    #[arg(short = 'k', action = ArgAction::SetTrue)]
    pub(crate) kill: bool,
    #[arg(short = 'e')]
    pub(crate) environment: Vec<String>,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    pub(crate) command: Vec<String>,
}

/// Arguments of `find-window`: the match string and which pane fields to search.
#[derive(Debug, Clone, Args)]
pub(crate) struct FindWindowArgs {
    #[arg(short = 'i', action = ArgAction::SetTrue)]
    pub(crate) case_insensitive: bool,
    #[arg(short = 'C', action = ArgAction::SetTrue)]
    pub(crate) search_content: bool,
    #[arg(short = 'N', action = ArgAction::SetTrue)]
    pub(crate) search_name: bool,
    #[arg(short = 'r', action = ArgAction::SetTrue)]
    pub(crate) regex: bool,
    #[arg(short = 'T', action = ArgAction::SetTrue)]
    pub(crate) search_title: bool,
    #[arg(short = 'Z', action = ArgAction::SetTrue)]
    pub(crate) zoom: bool,
    #[arg(short = 't', allow_hyphen_values = true)]
    pub(crate) target_pane: Option<String>,
    #[arg(allow_hyphen_values = true)]
    pub(crate) match_string: String,
    #[arg(skip = String::new())]
    pub(crate) queue_command: String,
}

impl QueuedCommand for FindWindowArgs {
    /// Records the original `find-window` line so the dispatcher can queue it.
    fn set_queue_command(&mut self, queue_command: String) {
        self.queue_command = queue_command;
    }
}
