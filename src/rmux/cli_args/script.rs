use std::path::PathBuf;

use clap::{ArgAction, ArgGroup, Args};
use rmux_proto::WaitForMode;

use super::validate::unknown_flag_error;
use super::{TargetSpec, parse_command_args, parse_target_spec};

/// Parses `source-file` arguments, rejecting flags `clap` would otherwise absorb as file paths.
pub(super) fn parse_source_file_args(
    arguments: Vec<String>,
) -> Result<SourceFileArgs, clap::Error> {
    validate_source_file_options(&arguments)?;
    parse_command_args("source-file", arguments)
}

/// Scans leading flags and rejects anything outside `-F`, `-n`, `-q`, `-v`, `-t` and `--help`.
fn validate_source_file_options(arguments: &[String]) -> Result<(), clap::Error> {
    let mut expect_target = false;
    for argument in arguments {
        if expect_target {
            expect_target = false;
            continue;
        }
        if argument == "--" {
            break;
        }
        if argument == "-" || !argument.starts_with('-') {
            break;
        }
        if let Some(long) = argument.strip_prefix("--") {
            let name = long.split_once('=').map_or(long, |(name, _)| name);
            if name != "help" {
                return Err(unknown_flag_error("source-file", &format!("--{name}")));
            }
            continue;
        }

        let mut chars = argument
            .strip_prefix('-')
            .unwrap_or(argument)
            .chars()
            .peekable();
        while let Some(flag) = chars.next() {
            match flag {
                'F' | 'n' | 'q' | 'v' => {}
                't' => {
                    expect_target = chars.peek().is_none();
                    break;
                }
                _ => return Err(unknown_flag_error("source-file", &format!("-{flag}"))),
            }
        }
    }
    Ok(())
}

/// Arguments for `run-shell`, which runs a shell command and reports its output to the client.
#[derive(Debug, Clone, Args)]
pub(crate) struct RunShellArgs {
    #[arg(short = 'b', action = ArgAction::SetTrue)]
    pub(crate) background: bool,
    #[arg(short = 'C', action = ArgAction::SetTrue)]
    pub(crate) as_commands: bool,
    #[arg(short = 'E', action = ArgAction::SetTrue)]
    pub(crate) show_stderr: bool,
    #[arg(short = 'd')]
    pub(crate) delay_seconds: Option<f64>,
    #[arg(short = 'c')]
    pub(crate) start_directory: Option<PathBuf>,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    pub(crate) command: Vec<String>,
}

/// Arguments for `source-file`, which loads and executes configuration files.
#[derive(Debug, Clone, Args)]
pub(crate) struct SourceFileArgs {
    #[arg(short = 'F', action = ArgAction::SetTrue)]
    pub(crate) expand_paths: bool,
    #[arg(short = 'n', action = ArgAction::SetTrue)]
    pub(crate) parse_only: bool,
    #[arg(short = 'q', action = ArgAction::SetTrue)]
    pub(crate) quiet: bool,
    #[arg(short = 'v', action = ArgAction::SetTrue)]
    pub(crate) verbose: bool,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    #[arg(required = true, allow_hyphen_values = true)]
    pub(crate) paths: Vec<String>,
}

/// Arguments for `if-shell`, which picks a command based on a shell condition's exit status.
#[derive(Debug, Clone, Args)]
pub(crate) struct IfShellArgs {
    #[arg(short = 'b', action = ArgAction::SetTrue)]
    pub(crate) background: bool,
    #[arg(short = 'F', action = ArgAction::SetTrue)]
    pub(crate) format_mode: bool,
    #[arg(short = 't', value_parser = parse_target_spec, allow_hyphen_values = true)]
    pub(crate) target: Option<TargetSpec>,
    #[arg(allow_hyphen_values = true)]
    pub(crate) condition: String,
    #[arg(allow_hyphen_values = true)]
    pub(crate) then_command: String,
    #[arg(allow_hyphen_values = true)]
    pub(crate) else_command: Option<String>,
    #[arg(skip = String::new())]
    pub(crate) queue_command: String,
}

/// Arguments for `wait-for`, which signals, locks or waits on a named synchronisation channel.
#[derive(Debug, Clone, Args)]
#[command(group(ArgGroup::new("mode").args(["signal", "lock", "unlock"])))]
pub(crate) struct WaitForArgs {
    #[arg(short = 'S', action = ArgAction::SetTrue, group = "mode")]
    pub(crate) signal: bool,
    #[arg(short = 'L', action = ArgAction::SetTrue, group = "mode")]
    pub(crate) lock: bool,
    #[arg(short = 'U', action = ArgAction::SetTrue, group = "mode")]
    pub(crate) unlock: bool,
    #[arg(allow_hyphen_values = true)]
    pub(crate) channel: String,
}

impl WaitForArgs {
    /// Resolves the exclusive `-S`, `-L` and `-U` flags into the channel operation to perform.
    pub(crate) const fn mode(&self) -> WaitForMode {
        if self.signal {
            WaitForMode::Signal
        } else if self.lock {
            WaitForMode::Lock
        } else if self.unlock {
            WaitForMode::Unlock
        } else {
            WaitForMode::Wait
        }
    }
}
