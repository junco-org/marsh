use clap::{ArgAction, Args};

use super::validate::{Validate, unknown_flag_error};

/// Arguments for `choose-tree`.
#[derive(Debug, Clone, Args)]
pub(crate) struct ChooseTreeArgs {
    #[arg(short = 'G', action = ArgAction::SetTrue)]
    pub(crate) show_all_group_members: bool,
    #[arg(short = 'N', action = ArgAction::Count)]
    pub(crate) preview: u8,
    #[arg(short = 'r', action = ArgAction::SetTrue)]
    pub(crate) reversed: bool,
    #[arg(short = 's', action = ArgAction::SetTrue)]
    pub(crate) sessions_collapsed: bool,
    #[arg(short = 'w', action = ArgAction::SetTrue)]
    pub(crate) windows_collapsed: bool,
    #[arg(short = 'y', action = ArgAction::SetTrue, hide = true)]
    unsupported_auto_accept: bool,
    #[arg(short = 'Z', action = ArgAction::SetTrue)]
    pub(crate) zoom: bool,
    #[arg(short = 'F', allow_hyphen_values = true)]
    pub(crate) row_format: Option<String>,
    #[arg(short = 'f', allow_hyphen_values = true)]
    pub(crate) filter_format: Option<String>,
    #[arg(short = 'K', allow_hyphen_values = true)]
    pub(crate) key_format: Option<String>,
    #[arg(short = 'O', allow_hyphen_values = true)]
    pub(crate) sort_order: Option<String>,
    #[arg(short = 't', allow_hyphen_values = true)]
    pub(crate) target_pane: Option<String>,
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    pub(crate) template: Vec<String>,
    #[arg(skip = String::new())]
    pub(crate) queue_command: String,
}

/// Defines the argument structs of the choosers sharing one flag surface. Each doc comment is
/// kept on its struct because clap renders it as that command's `--help` summary.
macro_rules! list_chooser_args {
    ($($(#[$doc:meta])* $name:ident;)+) => {$(
        $(#[$doc])*
        #[derive(Debug, Clone, Args)]
        pub(crate) struct $name {
            #[arg(short = 'N', action = ArgAction::Count)]
            pub(crate) preview: u8,
            #[arg(short = 'r', action = ArgAction::SetTrue)]
            pub(crate) reversed: bool,
            #[arg(short = 'y', action = ArgAction::SetTrue, hide = true)]
            unsupported_auto_accept: bool,
            #[arg(short = 'Z', action = ArgAction::SetTrue)]
            pub(crate) zoom: bool,
            #[arg(short = 'F', allow_hyphen_values = true)]
            pub(crate) row_format: Option<String>,
            #[arg(short = 'f', allow_hyphen_values = true)]
            pub(crate) filter_format: Option<String>,
            #[arg(short = 'K', allow_hyphen_values = true)]
            pub(crate) key_format: Option<String>,
            #[arg(short = 'O', allow_hyphen_values = true)]
            pub(crate) sort_order: Option<String>,
            #[arg(short = 't', allow_hyphen_values = true)]
            pub(crate) target_pane: Option<String>,
            #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
            pub(crate) template: Vec<String>,
            #[arg(skip = String::new())]
            pub(crate) queue_command: String,
        }
    )+};
}

list_chooser_args! {
    /// Arguments for `choose-buffer`.
    ChooseBufferArgs;
    /// Arguments for `choose-client`.
    ChooseClientArgs;
}

/// Arguments for `customize-mode`.
#[derive(Debug, Clone, Args)]
pub(crate) struct CustomizeModeArgs {
    #[arg(short = 'N', action = ArgAction::Count)]
    pub(crate) preview: u8,
    #[arg(short = 'Z', action = ArgAction::SetTrue)]
    pub(crate) zoom: bool,
    #[arg(short = 'F', allow_hyphen_values = true)]
    pub(crate) row_format: Option<String>,
    #[arg(short = 'f', allow_hyphen_values = true)]
    pub(crate) filter_format: Option<String>,
    #[arg(short = 't', allow_hyphen_values = true)]
    pub(crate) target_pane: Option<String>,
    #[arg(skip = String::new())]
    pub(crate) queue_command: String,
}

/// Implements [`Validate`] for choosers by rejecting `-y`, which tmux accepts but `rmux` does not
/// implement.
macro_rules! reject_auto_accept {
    ($($args:ty),+) => {$(
        impl Validate for $args {
            fn validate(self, command_name: &'static str) -> Result<Self, clap::Error> {
                if self.unsupported_auto_accept {
                    return Err(unknown_flag_error(command_name, "-y"));
                }
                Ok(self)
            }
        }
    )+};
}

reject_auto_accept! { ChooseTreeArgs, ChooseBufferArgs, ChooseClientArgs }
