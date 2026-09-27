use std::fmt::Display;

/// Post-parse checks for argument combinations `clap` cannot express on its own.
pub(super) trait Validate: Sized {
    /// Passes the arguments through, or fails with the error tmux reports for `command_name`.
    fn validate(self, command_name: &'static str) -> Result<Self, clap::Error>;
}

/// Counts how many of `flags` were selected, for checks on mutually exclusive options.
pub(super) fn selected_count<const N: usize>(flags: [bool; N]) -> usize {
    flags.into_iter().filter(|selected| *selected).count()
}

/// Builds tmux's error for an unrecognized or unsupported `flag` on `command_name`.
pub(super) fn unknown_flag_error(command_name: &str, flag: &str) -> clap::Error {
    clap::Error::raw(
        clap::error::ErrorKind::UnknownArgument,
        format!("command {command_name}: unknown flag {flag}"),
    )
}

/// Builds tmux's error for a `flag` on `command_name` that is missing its value.
pub(super) fn missing_value_error(command_name: &str, flag: &str) -> clap::Error {
    value_error(command_name, format_args!("{flag} expects an argument"))
}

/// Builds tmux's error for more than `limit` positional arguments to `command_name`.
pub(super) fn too_many_arguments_error(command_name: &str, limit: usize) -> clap::Error {
    clap::Error::raw(
        clap::error::ErrorKind::TooManyValues,
        format!("command {command_name}: too many arguments (need at most {limit})"),
    )
}

/// Builds a value-validation error prefixed with the offending command name.
pub(super) fn value_error(command_name: &str, message: impl Display) -> clap::Error {
    clap::Error::raw(
        clap::error::ErrorKind::ValueValidation,
        format!("command {command_name}: {message}"),
    )
}

/// Fails when a supplied `flag` value is present but empty.
pub(super) fn reject_empty(
    command_name: &str,
    flag: &str,
    value: Option<&str>,
) -> Result<(), clap::Error> {
    if value.is_some_and(str::is_empty) {
        return Err(value_error(
            command_name,
            format_args!("{flag} must not be empty"),
        ));
    }
    Ok(())
}

/// The error reported when a command argument is not valid UTF-8.
pub(super) fn invalid_utf8_error() -> clap::Error {
    clap::Error::raw(
        clap::error::ErrorKind::InvalidUtf8,
        "invalid UTF-8 in command argument",
    )
}
