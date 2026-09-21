use std::ffi::OsString;
use std::io::{self, ErrorKind, Write};

use rmux_core::formats::TMUX_FORMAT_TABLE_NAMES;
use rmux_proto::{capabilities_for_features, RMUX_WIRE_VERSION};
use serde_json::json;

use super::scripting_contract::{BINARY_CONTRACT_VERSION, CONTROL_NOTIFICATIONS, JSON_COMMANDS};
use super::ExitFailure;

/// Output shape selected for the `capabilities` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapabilitiesFormat {
    Human,
    Json,
}

/// A parsed `rmux capabilities` invocation, carrying only its chosen output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CapabilitiesInvocation {
    format: CapabilitiesFormat,
}

/// Recognises a `capabilities` command in raw argv, returning `None` for any other command.
pub(super) fn parse_invocation(
    arguments: &[OsString],
) -> Result<Option<CapabilitiesInvocation>, ExitFailure> {
    let Some(command_index) = split_top_level_prefix(arguments) else {
        return Ok(None);
    };
    let Some(command) = arguments
        .get(command_index)
        .and_then(|value| value.to_str())
    else {
        return Ok(None);
    };
    if command != "capabilities" {
        return Ok(None);
    }

    let format = parse_capabilities_format(&arguments[command_index + 1..])?;
    Ok(Some(CapabilitiesInvocation { format }))
}

/// Prints the capability report in the requested format and yields the process exit code.
pub(super) fn run(invocation: CapabilitiesInvocation) -> Result<i32, ExitFailure> {
    match invocation.format {
        CapabilitiesFormat::Human => write_stdout(&render_human()),
        CapabilitiesFormat::Json => write_json(),
    }
}

/// Index of the first non-global argument in argv, skipping top-level flags and their values.
fn split_top_level_prefix(arguments: &[OsString]) -> Option<usize> {
    let mut index = 0;

    while let Some(argument) = arguments.get(index) {
        let value = argument.to_str()?;
        if value == "--" {
            return Some(index + 1);
        }
        if !value.starts_with('-') || value == "-" {
            return Some(index);
        }

        match value {
            "-2" | "-D" | "-N" | "-l" | "-u" => {}
            "-C" | "-v" => {}
            "-c" | "-f" | "-L" | "-S" | "-T" => {
                index += 1;
            }
            _ if value.starts_with("-L") && value.len() > 2 => {}
            _ if value.starts_with("-S") && value.len() > 2 => {}
            _ if value.starts_with("-f") && value.len() > 2 => {}
            _ if value.starts_with("-T") && value.len() > 2 => {}
            _ if is_short_flag_cluster(value, "2CDNluv") => {}
            _ => return Some(index),
        }

        index += 1;
    }

    None
}

/// Whether `value` is a bundled short-flag run like `-2CD` whose letters are all in `allowed`.
fn is_short_flag_cluster(value: &str, allowed: &str) -> bool {
    value.len() > 2
        && value.starts_with('-')
        && !value.starts_with("--")
        && value.chars().skip(1).all(|flag| allowed.contains(flag))
}

/// Parses the `capabilities` flags, rejecting conflicts and defaulting to human output.
fn parse_capabilities_format(arguments: &[OsString]) -> Result<CapabilitiesFormat, ExitFailure> {
    let mut format = None;
    for argument in arguments {
        match argument.to_str() {
            Some("--human") => set_format(&mut format, CapabilitiesFormat::Human)?,
            Some("--json") => set_format(&mut format, CapabilitiesFormat::Json)?,
            Some("--help") => {
                return Err(ExitFailure::new_stdout(
                    0,
                    "usage: rmux capabilities [--human|--json]",
                ));
            }
            Some(other) => {
                return Err(ExitFailure::new(
                    1,
                    format!("rmux capabilities: unknown argument '{other}'"),
                ));
            }
            None => {
                return Err(ExitFailure::new(
                    1,
                    "rmux capabilities: arguments must be valid UTF-8",
                ));
            }
        }
    }

    Ok(format.unwrap_or(CapabilitiesFormat::Human))
}

/// Records the chosen format, rejecting a second conflicting format flag.
fn set_format(
    current: &mut Option<CapabilitiesFormat>,
    next: CapabilitiesFormat,
) -> Result<(), ExitFailure> {
    if current.is_some_and(|current| current != next) {
        return Err(ExitFailure::new(
            1,
            "rmux capabilities: choose only one of --human or --json",
        ));
    }
    *current = Some(next);
    Ok(())
}

/// Renders the plain-text capability report: versions, contracts, and JSON command names.
fn render_human() -> String {
    let mut output = String::new();
    output.push_str("rmux capabilities\n");
    output.push_str(&format!("version: {}\n", rmux_server::VERSION));
    output.push_str(&format!(
        "binary_contract_version: {BINARY_CONTRACT_VERSION}\n"
    ));
    output.push_str(&format!("wire_version: {RMUX_WIRE_VERSION}\n"));
    output.push_str("public_contract:\n");
    output.push_str("  - cli\n  - json-output\n  - format-tokens\n  - control-mode\n");
    output.push_str("json_commands:\n");
    for command in JSON_COMMANDS {
        output.push_str(&format!("  - {command}\n"));
    }
    output
}

/// Writes the full machine-readable capability report as pretty-printed JSON.
fn write_json() -> Result<i32, ExitFailure> {
    let capabilities = compiled_protocol_capabilities()
        .into_iter()
        .chain(["scripting.binary_contract.v1", "scripting.json.v1"])
        .collect::<Vec<_>>();
    let report = json!({
        "version": rmux_server::VERSION,
        "binary_contract_version": BINARY_CONTRACT_VERSION,
        "wire_version": RMUX_WIRE_VERSION,
        "public_contract": ["cli", "json-output", "format-tokens", "control-mode"],
        "capabilities": capabilities,
        "json_commands": JSON_COMMANDS,
        "format_tokens": &TMUX_FORMAT_TABLE_NAMES[..],
        "control_notifications": CONTROL_NOTIFICATIONS,
        "control_mode": control_mode_contract(),
    });

    write_json_value(&report)
}

/// The wire capability names compiled into this build, reflecting the `web` feature.
fn compiled_protocol_capabilities() -> Vec<&'static str> {
    capabilities_for_features(cfg!(all(any(unix, windows), feature = "web")))
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

/// Writes a JSON value plus a newline to stdout, treating a broken pipe as success.
fn write_json_value(value: &serde_json::Value) -> Result<i32, ExitFailure> {
    let mut stdout = io::stdout().lock();
    match serde_json::to_writer_pretty(&mut stdout, value) {
        Ok(()) => Ok(0),
        Err(error) if error.is_io() && error.io_error_kind() == Some(ErrorKind::BrokenPipe) => {
            Ok(0)
        }
        Err(error) => Err(ExitFailure::new(
            1,
            format!("failed to write capabilities JSON: {error}"),
        )),
    }?;
    match stdout.write_all(b"\n") {
        Ok(()) => Ok(0),
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(0),
        Err(error) => Err(ExitFailure::new(
            1,
            format!("failed to write capabilities JSON: {error}"),
        )),
    }
}

/// Writes text to stdout, treating a broken pipe as success.
fn write_stdout(output: &str) -> Result<i32, ExitFailure> {
    match io::stdout().lock().write_all(output.as_bytes()) {
        Ok(()) => Ok(0),
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(0),
        Err(error) => Err(ExitFailure::new(
            1,
            format!("failed to write capabilities output: {error}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{compiled_protocol_capabilities, parse_invocation, CapabilitiesFormat};
    use rmux_proto::CAPABILITY_WEB_SHARE;
    use std::ffi::OsString;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_after_top_level_socket_flags() {
        let invocation = parse_invocation(&args(&["-Ldemo", "capabilities", "--json"]))
            .expect("parse succeeds")
            .expect("capabilities invocation");

        assert_eq!(invocation.format, CapabilitiesFormat::Json);
    }

    #[test]
    fn ignores_other_commands() {
        assert!(parse_invocation(&args(&["list-sessions"]))
            .expect("parse succeeds")
            .is_none());
    }

    #[test]
    fn local_inventory_reports_compiled_web_capability() {
        assert_eq!(
            compiled_protocol_capabilities().contains(&CAPABILITY_WEB_SHARE),
            cfg!(all(any(unix, windows), feature = "web"))
        );
    }
}
