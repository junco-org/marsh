//! Client-side terminal capability detection shared by the full and tiny CLIs.

use std::io::IsTerminal;

use rmux_proto::ClientTerminalContext;

/// Error text tmux prints when an attach is attempted without a terminal on stdin.
pub(crate) const ATTACH_TERMINAL_REQUIRED_MESSAGE: &str = "open terminal failed: not a terminal";

/// Refuses an attach when the real stdin is not a terminal.
pub(crate) fn require_attach_terminal() -> Result<(), &'static str> {
    require_attach_terminal_from(std::io::stdin().is_terminal())
}

/// Attach admission decision for a given stdin terminal state, split out for testing.
const fn require_attach_terminal_from(stdin_is_terminal: bool) -> Result<(), &'static str> {
    if stdin_is_terminal {
        Ok(())
    } else {
        Err(ATTACH_TERMINAL_REQUIRED_MESSAGE)
    }
}

/// Builds the client terminal context from the client's reported features.
pub(crate) const fn client_terminal_context_from_parts(
    terminal_features: Vec<String>,
    utf8: bool,
) -> ClientTerminalContext {
    ClientTerminalContext {
        terminal_features,
        utf8,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::{ATTACH_TERMINAL_REQUIRED_MESSAGE, require_attach_terminal_from};

    #[test]
    fn attach_terminal_preflight_rejects_redirected_stdin() {
        assert_eq!(
            require_attach_terminal_from(false),
            Err(ATTACH_TERMINAL_REQUIRED_MESSAGE)
        );
        assert_eq!(require_attach_terminal_from(true), Ok(()));
    }
}
