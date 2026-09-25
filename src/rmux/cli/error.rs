use std::path::Path;

use rmux_client::{AutoStartError, ClientError, NestedContextError, default_socket_path};
use rmux_proto::{DisplayMessageDurationParseError, RmuxError};

use crate::tmux_error_surface::tmux_client_connect_error_message;

/// A CLI failure carrying the process exit code, message, and how that message is emitted.
#[derive(Debug)]
pub(crate) struct ExitFailure {
    exit_code: i32,
    message: String,
    use_stderr: bool,
    message_termination: ExitMessageTermination,
    kind: ExitFailureKind,
}

/// Whether a failure message is written with a trailing newline or exactly as given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitMessageTermination {
    Line,
    Exact,
}

/// Failure categories the dispatcher reacts to beyond printing the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitFailureKind {
    Generic,
    AmbiguousTarget,
    ServerAbsent,
    UnsupportedWireVersion,
}

impl ExitFailure {
    /// The process exit status this failure should produce.
    pub(crate) const fn exit_code(&self) -> i32 {
        self.exit_code
    }

    /// The human-readable failure text shown to the user.
    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    /// Whether the message belongs on stderr rather than stdout.
    pub(crate) const fn use_stderr(&self) -> bool {
        self.use_stderr
    }

    /// Whether the message is newline-terminated or written verbatim.
    pub(crate) const fn message_termination(&self) -> ExitMessageTermination {
        self.message_termination
    }

    /// Whether the target spec matched more than one candidate.
    pub(super) fn is_ambiguous_target(&self) -> bool {
        self.kind == ExitFailureKind::AmbiguousTarget
    }

    /// Whether the failure was caused by no daemon listening on the socket.
    pub(super) fn is_server_absent(&self) -> bool {
        self.kind == ExitFailureKind::ServerAbsent
    }

    /// Whether the daemon answered with a protocol version this client cannot speak.
    pub(super) fn is_unsupported_wire_version(&self) -> bool {
        self.kind == ExitFailureKind::UnsupportedWireVersion
    }

    /// Builds an ordinary stderr failure with the given exit code and message.
    pub(crate) fn new(exit_code: i32, message: impl Into<String>) -> Self {
        Self::new_with_kind(exit_code, message, ExitFailureKind::Generic)
    }

    /// Builds the exit-code-1 failure used when a target spec is ambiguous.
    pub(super) fn ambiguous_target(message: impl Into<String>) -> Self {
        Self::new_with_kind(1, message, ExitFailureKind::AmbiguousTarget)
    }

    /// Shared constructor for a newline-terminated stderr failure of the given kind.
    fn new_with_kind(exit_code: i32, message: impl Into<String>, kind: ExitFailureKind) -> Self {
        Self {
            exit_code,
            message: message.into(),
            use_stderr: true,
            message_termination: ExitMessageTermination::Line,
            kind,
        }
    }

    /// Builds a failure whose message goes to stdout, as some `tmux` commands expect.
    pub(super) fn new_stdout(exit_code: i32, message: impl Into<String>) -> Self {
        Self {
            exit_code,
            message: message.into(),
            use_stderr: false,
            message_termination: ExitMessageTermination::Line,
            kind: ExitFailureKind::Generic,
        }
    }

    /// Builds a stdout failure whose message is written without a trailing newline.
    pub(super) fn new_stdout_exact(exit_code: i32, message: impl Into<String>) -> Self {
        Self {
            exit_code,
            message: message.into(),
            use_stderr: false,
            message_termination: ExitMessageTermination::Exact,
            kind: ExitFailureKind::Generic,
        }
    }

    /// Converts a connect failure, preferring the `tmux`-compatible absent-server wording.
    pub(super) fn from_client_connect(socket_path: &Path, error: ClientError) -> Self {
        if let Some(message) = tmux_client_connect_error_message(socket_path, &error) {
            return Self::new_with_kind(1, message, ExitFailureKind::ServerAbsent);
        }

        Self::from(error)
    }

    /// Names the startup operation behind a generic failure, keeping its original cause.
    ///
    /// Classified failures are left alone: absent-server wording drives cold startup and
    /// wire-version wording carries recovery advice, and both are matched on elsewhere.
    pub(super) fn with_startup_context(
        mut self,
        stage: &'static str,
        socket_path: Option<&Path>,
    ) -> Self {
        if self.message.is_empty() || self.kind != ExitFailureKind::Generic {
            return self;
        }

        self.message = match socket_path {
            Some(socket_path) => format!(
                "rmux: startup failed during {stage} (socket '{}'): {}",
                socket_path.display(),
                self.message
            ),
            None => format!("rmux: startup failed during {stage}: {}", self.message),
        };
        self
    }

    /// Upgrades a wire-version mismatch into advice naming the socket and how to stop it.
    pub(super) fn with_socket_context(self, socket_path: &Path) -> Self {
        if self.kind == ExitFailureKind::UnsupportedWireVersion {
            return Self::incompatible_daemon(socket_path);
        }
        self
    }

    /// Builds the message telling the user to stop the incompatible daemon on this socket.
    fn incompatible_daemon(socket_path: &Path) -> Self {
        Self::new(
            1,
            format!(
                "rmux: running daemon on '{}' uses an incompatible protocol.\nrmux: run `{}` to stop it, then retry.",
                socket_path.display(),
                incompatible_daemon_kill_server_command(socket_path)
            ),
        )
    }
}

/// Whether the client error is a protocol wire-version mismatch.
const fn unsupported_wire_version(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::Protocol(RmuxError::UnsupportedWireVersion { .. })
    )
}

/// The `rmux kill-server` invocation to suggest, with `-S` only for a non-default socket.
fn incompatible_daemon_kill_server_command(socket_path: &Path) -> String {
    if default_socket_path()
        .ok()
        .as_deref()
        .is_some_and(|default_path| default_path == socket_path)
    {
        return "rmux kill-server".to_owned();
    }

    format!("rmux -S {} kill-server", shell_quote_path(socket_path))
}

/// Quotes a path for shell display, leaving already-safe characters untouched.
fn shell_quote_path(path: &Path) -> String {
    let text = path.display().to_string();
    if !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._-+=:@".contains(&byte))
    {
        return text;
    }

    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Rewrites a `clap` error into the terser wording real `tmux` prints for the same mistake.
fn tmux_compat_clap_message(error: &clap::Error) -> String {
    let message = error.to_string().trim_end().to_owned();
    let first_line = message.lines().next().unwrap_or(message.as_str());
    if message == "error: size missing"
        || message == "error: command join-pane: size missing"
        || message == "error: command move-pane: size missing"
    {
        return "size missing".to_owned();
    }
    if first_line.contains("invalid session name: session names must be non-empty") {
        return "invalid session: ".to_owned();
    }
    if let Some((_, detail)) = first_line.rsplit_once(": ") {
        if let Some(normalized) = normalized_invalid_value_detail(detail) {
            return normalized;
        }
    }
    if let Some(stripped) = message.strip_prefix("error: ") {
        if normalized_invalid_value_detail(stripped).is_some() {
            return stripped.to_owned();
        }
    }
    if let Some(stripped) = message.strip_prefix("error: command ") {
        return format!("command {stripped}");
    }
    if let Some((_, option)) = message.rsplit_once(": invalid option: ") {
        let option = option.lines().next().unwrap_or(option);
        return format!("invalid option: {option}");
    }
    message
}

/// Returns the detail text when it is one of the size or delay messages `tmux` passes through.
fn normalized_invalid_value_detail(detail: &str) -> Option<String> {
    let is_display_message_delay_error = DisplayMessageDurationParseError::ALL
        .into_iter()
        .any(|error| error.as_str() == detail);
    if is_display_message_delay_error
        || matches!(
            detail,
            "width too small"
                | "width invalid"
                | "width too large"
                | "height too small"
                | "height invalid"
                | "height too large"
                | "adjustment invalid"
                | "adjustment too small"
                | "adjustment too large"
        )
    {
        return Some(detail.to_owned());
    }

    None
}

impl From<NestedContextError> for ExitFailure {
    /// Reports a nested-client context error as an exit-code-1 failure.
    fn from(error: NestedContextError) -> Self {
        Self::new(1, error.to_string())
    }
}

impl From<clap::Error> for ExitFailure {
    /// Converts a `clap` parse error, mapping help and version requests to exit code `0`.
    fn from(error: clap::Error) -> Self {
        let exit_code = match error.kind() {
            clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
            _ => 1,
        };
        let message = tmux_compat_clap_message(&error);

        Self {
            exit_code,
            message,
            use_stderr: error.use_stderr(),
            message_termination: ExitMessageTermination::Line,
            kind: ExitFailureKind::Generic,
        }
    }
}

impl From<ClientError> for ExitFailure {
    /// Converts a client error, tagging unsupported wire versions for later socket context.
    fn from(error: ClientError) -> Self {
        let kind = if unsupported_wire_version(&error) {
            ExitFailureKind::UnsupportedWireVersion
        } else {
            ExitFailureKind::Generic
        };
        Self::new_with_kind(1, error.to_string(), kind)
    }
}

impl From<AutoStartError> for ExitFailure {
    /// Converts a daemon auto-start failure into a plain exit-code-1 failure.
    fn from(error: AutoStartError) -> Self {
        Self::new(1, error.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::tmux_compat_clap_message;

    #[test]
    fn display_message_delay_errors_keep_single_tmux_line() {
        for parse_error in [
            rmux_proto::DisplayMessageDurationParseError::Invalid,
            rmux_proto::DisplayMessageDurationParseError::TooSmall,
            rmux_proto::DisplayMessageDurationParseError::TooLarge,
        ] {
            let message = parse_error.to_string();
            let error = clap::Error::raw(clap::error::ErrorKind::InvalidValue, message.as_str());

            assert_eq!(tmux_compat_clap_message(&error), message);
        }
    }

    #[test]
    fn unrelated_clap_errors_keep_their_normal_prefix() {
        let error = clap::Error::raw(
            clap::error::ErrorKind::InvalidValue,
            "unrelated invalid value",
        );

        assert_eq!(
            tmux_compat_clap_message(&error),
            "error: unrelated invalid value"
        );
    }

    #[test]
    fn clap_invalid_option_value_errors_keep_single_tmx_line() {
        let error = clap::Error::raw(
            clap::error::ErrorKind::ValueValidation,
            "error: invalid value 'no-such-hook' for '<HOOK>': invalid option: no-such-hook\n\nFor more information, try '--help'.",
        );

        assert_eq!(
            tmux_compat_clap_message(&error),
            "invalid option: no-such-hook"
        );
    }

    #[test]
    fn resize_pane_dimension_errors_keep_single_tmux_line() {
        let error = clap::Error::raw(clap::error::ErrorKind::ValueValidation, "width too small");

        assert_eq!(tmux_compat_clap_message(&error), "width too small");
    }

    #[test]
    fn resize_pane_adjustment_errors_keep_single_tmux_line() {
        for message in [
            "adjustment invalid",
            "adjustment too small",
            "adjustment too large",
        ] {
            let error = clap::Error::raw(clap::error::ErrorKind::ValueValidation, message);

            assert_eq!(tmux_compat_clap_message(&error), message);
        }
    }

    #[test]
    fn invalid_value_dimension_errors_keep_single_tmux_line() {
        let error = clap::Error::raw(
            clap::error::ErrorKind::ValueValidation,
            "error: invalid value '70000' for '-x <COLS>': width too large\n\nFor more information, try '--help'.",
        );

        assert_eq!(tmux_compat_clap_message(&error), "width too large");
    }

    #[test]
    fn empty_session_name_errors_keep_tmux_line() {
        let error = clap::Error::raw(
            clap::error::ErrorKind::ValueValidation,
            "error: invalid value '' for '<NEW_NAME>': invalid session name: session names must be non-empty\n\nFor more information, try '--help'.",
        );

        assert_eq!(tmux_compat_clap_message(&error), "invalid session: ");
    }
}
