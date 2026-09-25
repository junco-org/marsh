use std::path::Path;

pub(crate) fn command_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

pub(crate) fn sh_quote(value: &str) -> String {
    command_quote(value)
}

pub(crate) fn sh_quote_path(path: &Path) -> String {
    sh_quote(&path.display().to_string())
}

pub(crate) fn stdin_discard_command() -> String {
    platform_stdin_discard_command()
}

fn platform_stdin_discard_command() -> String {
    "cat >/dev/null".to_owned()
}

/// The real-child final-sink harness.
///
/// Split out of this module because it carries a slot protocol, two child
/// scripts and their diagnostics, none of which the shell-quoting helpers
/// above need to know about.
#[path = "test_shell/final_sink.rs"]
pub(crate) mod final_sink;
