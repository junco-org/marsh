use rmux_client::Connection;
use rmux_proto::Target;

use super::{ExitFailure, expect_command_output, write_command_output};

/// Renders a format template against a target by asking the daemon to display it, then prints it.
pub(in crate::cli) fn print_target_format(
    connection: &mut Connection,
    _command_name: &str,
    target: Target,
    template: &str,
) -> Result<(), ExitFailure> {
    let response = connection
        .display_message(Some(target), true, Some(template.to_owned()))
        .map_err(ExitFailure::from)?;
    let output = expect_command_output(&response, "display-message")?;
    write_command_output(output)
}
