use clap::Args;

/// Parsed arguments of `list-commands`, optionally limited to one command and a format string.
#[derive(Debug, Clone, Args)]
pub(crate) struct ListCommandsArgs {
    #[arg(short = 'F', allow_hyphen_values = true)]
    pub(crate) format: Option<String>,
    #[arg(allow_hyphen_values = true)]
    pub(crate) command: Option<String>,
}
