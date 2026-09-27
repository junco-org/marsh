use std::ffi::OsString;
use std::fmt::Write as _;

use rmux_core::formats::TMUX_FORMAT_TABLE_NAMES;
use rmux_proto::{RMUX_WIRE_VERSION, capabilities_for_features};
use serde_json::json;

use super::ExitFailure;
use super::aux_command::{AuxCommand, OutputFormat, command_word, write_stdout};
use super::json_output::write_json_value;
use super::scripting_contract::{BINARY_CONTRACT_VERSION, CONTROL_NOTIFICATIONS, JSON_COMMANDS};

/// A parsed `rmux capabilities` invocation, carrying only its chosen output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CapabilitiesInvocation {
    format: OutputFormat,
}

impl AuxCommand for CapabilitiesInvocation {
    /// Recognises `capabilities` after the top-level flags, rejecting unknown or conflicting flags.
    fn parse(arguments: &[OsString]) -> Result<Option<Self>, ExitFailure> {
        match command_word(arguments, "cfLST", "fLST", |_| {}) {
            Some(("capabilities", rest)) => Ok(Some(Self {
                format: OutputFormat::parse("capabilities", rest)?,
            })),
            _ => Ok(None),
        }
    }

    /// Prints the capability report in the requested format.
    fn run(self, _argv: &[OsString]) -> Result<i32, ExitFailure> {
        match self.format {
            OutputFormat::Human => write_stdout(render_human().as_bytes(), "capabilities"),
            OutputFormat::Json => write_json_value(&render_json(), "capabilities", true),
        }
    }
}

/// Renders the plain-text capability report: versions, contracts, and JSON command names.
fn render_human() -> String {
    let mut output = String::new();
    output.push_str("rmux capabilities\n");
    let _ = writeln!(output, "version: {}", rmux_server::VERSION);
    let _ = writeln!(output, "binary_contract_version: {BINARY_CONTRACT_VERSION}");
    let _ = writeln!(output, "wire_version: {RMUX_WIRE_VERSION}");
    output.push_str("public_contract:\n");
    output.push_str("  - cli\n  - json-output\n  - format-tokens\n  - control-mode\n");
    output.push_str("json_commands:\n");
    for command in JSON_COMMANDS {
        let _ = writeln!(output, "  - {command}");
    }
    output
}

/// The full machine-readable capability report.
fn render_json() -> serde_json::Value {
    let capabilities = compiled_protocol_capabilities()
        .into_iter()
        .chain(["scripting.binary_contract.v1", "scripting.json.v1"])
        .collect::<Vec<_>>();
    json!({
        "version": rmux_server::VERSION,
        "binary_contract_version": BINARY_CONTRACT_VERSION,
        "wire_version": RMUX_WIRE_VERSION,
        "public_contract": ["cli", "json-output", "format-tokens", "control-mode"],
        "capabilities": capabilities,
        "json_commands": JSON_COMMANDS,
        "format_tokens": &TMUX_FORMAT_TABLE_NAMES[..],
        "control_notifications": CONTROL_NOTIFICATIONS,
        "control_mode": control_mode_contract(),
    })
}

/// The wire capability names compiled into this build, reflecting the `web` feature.
fn compiled_protocol_capabilities() -> Vec<&'static str> {
    capabilities_for_features(cfg!(feature = "web"))
}

/// The JSON description of control-mode framing: guard lines, escapes, and line shapes.
fn control_mode_contract() -> serde_json::Value {
    json!({
        "entrypoint": "rmux -C",
        "line_ending": "\\n",
        "unknown_percent_lines": "ignore",
        "output_escape": {
            "encoding": "tmux-octal",
            "pattern": "\\ooo",
            "applies_to": ["%output", "%extended-output"]
        },
        "guard_lines": {
            "%begin": ["%begin", "timestamp", "sequence", "flags"],
            "%end": ["%end", "timestamp", "sequence", "flags"],
            "%error": ["%error", "timestamp", "sequence", "flags"]
        },
        "line_shapes": {
            "%output": ["%output", "pane_id", "octal_escaped_bytes"],
            "%extended-output": ["%extended-output", "pane_id", "age", "octal_escaped_bytes"],
            "%pause": ["%pause", "pane_id"],
            "%continue": ["%continue", "pane_id"],
            "%exit": ["%exit", "reason"],
            "%message": ["%message", "message"],
            "%config-error": ["%config-error", "message"]
        }
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::{CapabilitiesInvocation, compiled_protocol_capabilities};
    use crate::cli::aux_command::{AuxCommand, OutputFormat, args};
    use rmux_proto::CAPABILITY_WEB_SHARE;

    #[test]
    fn parses_after_top_level_socket_flags() {
        let invocation =
            CapabilitiesInvocation::parse(&args(&["-Ldemo", "capabilities", "--json"]))
                .expect("parse succeeds")
                .expect("capabilities invocation");

        assert_eq!(invocation.format, OutputFormat::Json);
    }

    #[test]
    fn ignores_other_commands() {
        assert!(
            CapabilitiesInvocation::parse(&args(&["list-sessions"]))
                .expect("parse succeeds")
                .is_none()
        );
    }

    #[test]
    fn local_inventory_reports_compiled_web_capability() {
        assert_eq!(
            compiled_protocol_capabilities().contains(&CAPABILITY_WEB_SHARE),
            cfg!(feature = "web")
        );
    }
}
