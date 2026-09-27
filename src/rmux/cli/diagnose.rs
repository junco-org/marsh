use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use rmux_client::{connect, resolve_socket_path};
use rmux_proto::Response;

use super::ExitFailure;
use super::aux_command::{AuxCommand, OutputFormat, TopLevelFlag, command_word, stdout_written};

/// Command-line state `rmux diagnose` needs: output format plus tmux-style top-level flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct DiagnoseInvocation {
    format: OutputFormat,
    socket_name: Option<OsString>,
    socket_path: Option<PathBuf>,
    config_files: Vec<PathBuf>,
    terminal_features: Vec<String>,
    assume_256_colors: bool,
}

/// The collected, already-redacted diagnostic facts rendered by either output format.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiagnoseReport {
    version: String,
    os_name: String,
    os_arch: String,
    os_version: String,
    terminal_host: String,
    term: String,
    term_program: String,
    shell: String,
    config_mode: String,
    config_paths: Vec<String>,
    config_messages: Vec<String>,
    socket_path: String,
    terminal_features: Vec<String>,
    osc52: String,
}

impl AuxCommand for DiagnoseInvocation {
    /// Recognizes `diagnose` after the top-level flags, keeping the ones that shape its report.
    fn parse(arguments: &[OsString]) -> Result<Option<Self>, ExitFailure> {
        let mut invocation = Self::default();
        let Some(("diagnose", rest)) =
            command_word(arguments, "fLST", "fLST", |flag| invocation.record(flag))
        else {
            return Ok(None);
        };
        invocation.format = OutputFormat::parse("diagnose", rest)?;
        Ok(Some(invocation))
    }

    /// Collects the report and writes it to stdout in the requested format.
    fn run(self, _argv: &[OsString]) -> Result<i32, ExitFailure> {
        let format = self.format;
        let report = DiagnoseReport::collect(self)?;
        let output = match format {
            OutputFormat::Human => report.render_human(),
            OutputFormat::Json => report.render_json(),
        };
        stdout_written(io::stdout().lock().write_all(output.as_bytes()), |error| {
            error.to_string()
        })
    }
}

impl DiagnoseInvocation {
    /// Keeps a top-level flag that affects the report: `-2`, `-L`, `-S`, `-f`, and `-T`.
    fn record(&mut self, flag: TopLevelFlag<'_>) {
        match flag {
            TopLevelFlag::Switches(switches) => self.assume_256_colors |= switches.contains('2'),
            TopLevelFlag::Value('L', name) => self.socket_name = Some(name.to_owned()),
            TopLevelFlag::Value('S', path) => self.socket_path = Some(PathBuf::from(path)),
            TopLevelFlag::Value('f', path) => self.config_files.push(PathBuf::from(path)),
            TopLevelFlag::Value(_, features) => {
                if let Some(features) = features.to_str() {
                    push_terminal_features(&mut self.terminal_features, features);
                }
            }
        }
    }
}

impl DiagnoseReport {
    /// Gathers version, OS, terminal, shell, socket, and config facts with paths already redacted.
    fn collect(invocation: DiagnoseInvocation) -> Result<Self, ExitFailure> {
        let socket_path = resolve_socket_path(
            invocation.socket_name.as_deref(),
            invocation.socket_path.as_deref(),
        )
        .map_err(ExitFailure::from)?;
        let mut terminal_features = invocation.terminal_features;
        if invocation.assume_256_colors {
            push_unique(&mut terminal_features, "256".to_owned());
        }

        let term = env_value("TERM");
        let term_program = env_value("TERM_PROGRAM");
        let terminal_host = detect_terminal_host(&term, &term_program);
        let custom_config_files = !invocation.config_files.is_empty();
        let config_paths = if custom_config_files {
            invocation.config_files
        } else {
            default_config_paths()
        };
        let config_paths = config_paths
            .iter()
            .map(|path| redact_path(path))
            .collect::<Vec<_>>();
        let config_messages = collect_config_messages(&socket_path);
        let osc52 = if terminal_looks_clipboard_capable(&term, &term_program, &terminal_features) {
            "available-when-requested"
        } else {
            "not-advertised"
        };

        Ok(Self {
            version: rmux_server::VERSION.to_owned(),
            os_name: std::env::consts::OS.to_owned(),
            os_arch: std::env::consts::ARCH.to_owned(),
            os_version: command_output("uname", &["-sr"]),
            terminal_host,
            term,
            term_program,
            shell: env_value("SHELL"),
            config_mode: if custom_config_files {
                "custom".to_owned()
            } else {
                "default".to_owned()
            },
            config_paths,
            config_messages,
            socket_path: redact_path(&socket_path),
            terminal_features,
            osc52: osc52.to_owned(),
        })
    }

    /// Renders the report as the plain-text `key: value` listing.
    fn render_human(&self) -> String {
        let mut output = String::new();
        output.push_str("rmux diagnose\n");
        let _ = writeln!(output, "version: {}", self.version);
        let _ = writeln!(output, "os: {} ({})", self.os_name, self.os_arch);
        let _ = writeln!(output, "os_version: {}", self.os_version);
        let _ = writeln!(output, "terminal_host: {}", self.terminal_host);
        let _ = writeln!(output, "term: {}", self.term);
        let _ = writeln!(output, "term_program: {}", self.term_program);
        let _ = writeln!(output, "shell: {}", self.shell);
        let _ = writeln!(output, "socket_path: {}", self.socket_path);
        let _ = writeln!(output, "config_mode: {}", self.config_mode);
        output.push_str("config_paths:\n");
        for path in &self.config_paths {
            let _ = writeln!(output, "  - {path}");
        }
        output.push_str("config_messages:\n");
        if self.config_messages.is_empty() {
            output.push_str("  - none\n");
        } else {
            for message in &self.config_messages {
                let _ = writeln!(output, "  - {message}");
            }
        }
        output.push_str("capabilities:\n");
        let _ = writeln!(output, "  osc52: {}", self.osc52);
        let _ = writeln!(
            output,
            "  terminal_features: {}",
            render_feature_list(&self.terminal_features)
        );
        output.push_str("privacy: environment values are summarized or redacted\n");
        output
    }

    /// Renders the report as a JSON object with the same fields as the human listing.
    fn render_json(&self) -> String {
        format!(
            concat!(
                "{{\n",
                "  \"version\": {},\n",
                "  \"os\": {{\"name\": {}, \"arch\": {}, \"version\": {}}},\n",
                "  \"terminal\": {{\"host\": {}, \"term\": {}, \"term_program\": {}}},\n",
                "  \"shell\": {},\n",
                "  \"socket_path\": {},\n",
                "  \"config\": {{\"mode\": {}, \"paths\": {}, \"messages\": {}}},\n",
                "  \"capabilities\": {{\"osc52\": {}, \"terminal_features\": {}}},\n",
                "  \"privacy\": {{\"environment_values\": \"summarized-or-redacted\"}}\n",
                "}}\n"
            ),
            json_string(&self.version),
            json_string(&self.os_name),
            json_string(&self.os_arch),
            json_string(&self.os_version),
            json_string(&self.terminal_host),
            json_string(&self.term),
            json_string(&self.term_program),
            json_string(&self.shell),
            json_string(&self.socket_path),
            json_string(&self.config_mode),
            json_array(&self.config_paths),
            json_array(&self.config_messages),
            json_string(&self.osc52),
            json_array(&self.terminal_features),
        )
    }
}

/// Asks a running server for its `show-messages` history, keeping only config diagnostics.
fn collect_config_messages(socket_path: &Path) -> Vec<String> {
    let Ok(mut connection) = connect(socket_path) else {
        return Vec::new();
    };
    let Ok(Response::ShowMessages(response)) = connection.show_messages(false, false, None) else {
        return Vec::new();
    };
    let Ok(output) = std::str::from_utf8(response.command_output().stdout()) else {
        return Vec::new();
    };
    output
        .lines()
        .filter_map(config_message_from_show_messages_line)
        .collect()
}

/// Keeps a `show-messages` line if it reports a config problem, redacted against real homes.
fn config_message_from_show_messages_line(line: &str) -> Option<String> {
    config_message_from_show_messages_line_against(line, &home_prefixes())
}

/// `config_message_from_show_messages_line` over explicit home prefixes, for testability.
fn config_message_from_show_messages_line_against(line: &str, homes: &[PathBuf]) -> Option<String> {
    let message = line.split_once(": ").map_or(line, |(_, message)| message);
    if !is_config_diagnostic_message(message) {
        return None;
    }
    Some(truncate_diagnose_line(
        &redact_text_paths_against(line, homes),
        512,
    ))
}

/// Reports whether a message is a config diagnostic worth surfacing in the report.
fn is_config_diagnostic_message(message: &str) -> bool {
    message.starts_with("config ignored:")
        || message.starts_with("config error:")
        || source_location_config_diagnostic(message)
}

/// Reports whether a message looks like `path:line: detail` naming a known config parse failure.
fn source_location_config_diagnostic(message: &str) -> bool {
    for (colon_index, _) in message.match_indices(':') {
        let (Some(path), Some(after_colon)) =
            (message.get(..colon_index), message.get(colon_index + 1..))
        else {
            continue;
        };
        let after_digits = after_colon.trim_start_matches(|ch: char| ch.is_ascii_digit());
        if after_digits.len() == after_colon.len() {
            continue;
        }
        let Some(detail) = after_digits.strip_prefix(':') else {
            continue;
        };
        if path.trim().is_empty() {
            continue;
        }
        if source_location_config_detail(detail.trim_start()) {
            return true;
        }
    }
    false
}

/// Reports whether a `path:line:` suffix is one of the known config parse or load failures.
fn source_location_config_detail(detail: &str) -> bool {
    detail == "unmatched }"
        || detail == "missing }"
        || detail.starts_with("unknown command:")
        || detail.starts_with("unknown option:")
        || detail.starts_with("invalid option:")
        || detail.starts_with("ambiguous option:")
        || detail.starts_with("No such file or directory")
}

/// Replaces every occurrence of a home directory inside free text with `~`.
fn redact_text_paths_against(text: &str, homes: &[PathBuf]) -> String {
    let mut redacted = text.to_owned();
    for home in homes {
        let home = home.to_string_lossy();
        if !home.is_empty() {
            redacted = redacted.replace(home.as_ref(), "~");
        }
    }
    redacted
}

/// Shortens a line to at most `max_bytes` on a character boundary, appending `...` when cut.
fn truncate_diagnose_line(line: &str, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", line.get(..end).unwrap_or_default())
}

/// Splits a comma-separated `-T` value and appends each distinct feature name.
fn push_terminal_features(features: &mut Vec<String>, raw: &str) {
    for feature in raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        push_unique(features, feature.to_owned());
    }
}

/// Appends `value` only when it is not already present, keeping first-seen order.
fn push_unique<T: PartialEq>(values: &mut Vec<T>, value: T) {
    if !values.contains(&value) {
        values.push(value);
    }
}

/// Reads an environment variable, reporting an absent or empty one as the literal `unset`.
fn env_value(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unset".to_owned())
}

/// Names the enclosing terminal emulator: `WT_SESSION`, then `TERM_PROGRAM`, then `TERM`.
fn detect_terminal_host(term: &str, term_program: &str) -> String {
    if std::env::var_os("WT_SESSION").is_some() {
        return "windows-terminal".to_owned();
    }
    if term_program != "unset" {
        return term_program.to_owned();
    }
    if term != "unset" {
        return term.to_owned();
    }
    "unknown".to_owned()
}

/// Runs a helper program and returns its trimmed stdout, or `unknown` on any failure.
fn command_output(program: &str, args: &[&str]) -> String {
    ProcessCommand::new(program)
        .args(args)
        .output()
        .ok()
        .and_then(|output| {
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Guesses whether the terminal would honour an `OSC 52` clipboard write.
fn terminal_looks_clipboard_capable(
    term: &str,
    term_program: &str,
    terminal_features: &[String],
) -> bool {
    terminal_features
        .iter()
        .any(|feature| feature.eq_ignore_ascii_case("clipboard"))
        || term.starts_with("xterm")
        || term.starts_with("tmux")
        || term.contains("mintty")
        || term.starts_with("foot")
        || term.starts_with("iterm")
        || term_program.eq_ignore_ascii_case("iTerm.app")
        || term_program.eq_ignore_ascii_case("mintty")
}

/// The config files `rmux` would load when no `-f` was given, in lookup order:
/// `/etc/rmux.conf`, `~/.rmux.conf`, then the `XDG` locations.
fn default_config_paths() -> Vec<PathBuf> {
    let home = nonempty_env_os("HOME").map(PathBuf::from);
    let xdg_config_home = nonempty_env_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let mut paths = Vec::new();
    for path in [
        Some(PathBuf::from("/etc/rmux.conf")),
        home.as_ref().map(|home| home.join(".rmux.conf")),
        xdg_config_home.map(|config| config.join("rmux").join("rmux.conf")),
        home.map(|home| home.join(".config").join("rmux").join("rmux.conf")),
    ]
    .into_iter()
    .flatten()
    {
        push_unique(&mut paths, path);
    }
    paths
}

/// Reads an environment variable as an `OsString`, treating an empty value as absent.
fn nonempty_env_os(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

/// Renders a path with the current user's home directory replaced by `~`.
fn redact_path(path: &Path) -> String {
    redact_path_against(path, &home_prefixes())
}

/// `redact_path` over explicit home prefixes.
///
/// Redaction is a pure function of the caller's home directories, so the environment read is a
/// separate step: tests state the home they mean instead of mutating the process environment the
/// rest of the harness shares.
fn redact_path_against(path: &Path, homes: &[PathBuf]) -> String {
    for home in homes {
        if let Ok(rest) = path.strip_prefix(home) {
            if rest.as_os_str().is_empty() {
                return "~".to_owned();
            }
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// The nonempty HOME prefix diagnose output must redact.
fn home_prefixes() -> Vec<PathBuf> {
    nonempty_env_os("HOME").map(PathBuf::from).into_iter().collect()
}

/// Joins terminal feature names for human output, printing `none` when there are none.
fn render_feature_list(features: &[String]) -> String {
    if features.is_empty() {
        "none".to_owned()
    } else {
        features.join(",")
    }
}

/// Encodes a string list as a JSON array.
fn json_array(values: &[String]) -> String {
    let mut output = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            output.push_str(", ");
        }
        output.push_str(&json_string(value));
    }
    output.push(']');
    output
}

/// Encodes a string as a JSON literal, escaping quotes, backslashes, and control characters.
fn json_string(value: &str) -> String {
    let mut output = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch.is_control() => {
                let _ = write!(output, "\\u{:04x}", ch as u32);
            }
            ch => output.push(ch),
        }
    }
    output.push('"');
    output
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[path = "diagnose_tests.rs"]
mod tests;
