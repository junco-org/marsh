use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use rmux_client::{connect, resolve_socket_path};
use rmux_proto::Response;

use super::ExitFailure;

/// Selects whether `rmux diagnose` prints its human summary or a JSON object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagnoseFormat {
    Human,
    Json,
}

/// Command-line state `rmux diagnose` needs: output format plus tmux-style top-level flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DiagnoseInvocation {
    format: DiagnoseFormat,
    socket_name: Option<OsString>,
    socket_path: Option<PathBuf>,
    config_files: Vec<PathBuf>,
    terminal_features: Vec<String>,
    assume_256_colors: bool,
    utf8: bool,
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

/// Recognizes `rmux diagnose` in an argument vector, returning `None` for any other command.
pub(super) fn parse_invocation(
    arguments: &[OsString],
) -> Result<Option<DiagnoseInvocation>, ExitFailure> {
    let Some((command_index, prefix)) = split_top_level_prefix(arguments) else {
        return Ok(None);
    };
    let Some(command) = arguments
        .get(command_index)
        .and_then(|value| value.to_str())
    else {
        return Ok(None);
    };
    if command != "diagnose" {
        return Ok(None);
    }

    let format = parse_diagnose_format(&arguments[command_index + 1..])?;
    Ok(Some(DiagnoseInvocation {
        format,
        socket_name: prefix.socket_name,
        socket_path: prefix.socket_path,
        config_files: prefix.config_files,
        terminal_features: prefix.terminal_features,
        assume_256_colors: prefix.assume_256_colors,
        utf8: prefix.utf8,
    }))
}

/// Collects the report and writes it to stdout in the requested format, yielding the exit code.
pub(super) fn run(invocation: DiagnoseInvocation) -> Result<i32, ExitFailure> {
    let format = invocation.format;
    let report = DiagnoseReport::collect(invocation)?;
    let output = match format {
        DiagnoseFormat::Human => report.render_human(),
        DiagnoseFormat::Json => report.render_json(),
    };
    write_stdout(&output)
}

/// Tmux top-level flags that affect diagnose output, gathered before the command word.
#[derive(Default)]
struct TopLevelPrefix {
    socket_name: Option<OsString>,
    socket_path: Option<PathBuf>,
    config_files: Vec<PathBuf>,
    terminal_features: Vec<String>,
    assume_256_colors: bool,
    utf8: bool,
}

/// Scans leading top-level flags, returning the command word's index and the flags that matter.
fn split_top_level_prefix(arguments: &[OsString]) -> Option<(usize, TopLevelPrefix)> {
    let mut prefix = TopLevelPrefix::default();
    let mut index = 0;

    while let Some(argument) = arguments.get(index) {
        let value = argument.to_str()?;
        if value == "--" {
            return Some((index + 1, prefix));
        }
        if !value.starts_with('-') || value == "-" {
            return Some((index, prefix));
        }

        match value {
            "-2" => prefix.assume_256_colors = true,
            "-u" => prefix.utf8 = true,
            "-D" | "-N" | "-l" => {}
            "-C" | "-v" => {}
            "-L" => {
                index += 1;
                prefix.socket_name = arguments.get(index).cloned();
            }
            "-S" => {
                index += 1;
                prefix.socket_path = arguments.get(index).map(PathBuf::from);
            }
            "-f" => {
                index += 1;
                if let Some(path) = arguments.get(index) {
                    prefix.config_files.push(PathBuf::from(path));
                }
            }
            "-T" => {
                index += 1;
                if let Some(features) = arguments.get(index).and_then(|value| value.to_str()) {
                    push_terminal_features(&mut prefix.terminal_features, features);
                }
            }
            _ if value.starts_with("-L") && value.len() > 2 => {
                prefix.socket_name =
                    Some(OsString::from(value.strip_prefix("-L").unwrap_or_default()));
            }
            _ if value.starts_with("-S") && value.len() > 2 => {
                prefix.socket_path =
                    Some(PathBuf::from(value.strip_prefix("-S").unwrap_or_default()));
            }
            _ if value.starts_with("-f") && value.len() > 2 => {
                prefix
                    .config_files
                    .push(PathBuf::from(value.strip_prefix("-f").unwrap_or_default()));
            }
            _ if value.starts_with("-T") && value.len() > 2 => {
                push_terminal_features(
                    &mut prefix.terminal_features,
                    value.strip_prefix("-T").unwrap_or_default(),
                );
            }
            _ if is_short_flag_cluster(value, "2CDNluv") => {
                prefix.assume_256_colors |= value.contains('2');
                prefix.utf8 |= value.contains('u');
            }
            _ => return Some((index, prefix)),
        }

        index += 1;
    }

    None
}

/// Reports whether `value` is a bundled short-flag group such as `-2u` drawn only from `allowed`.
use super::is_short_flag_cluster;

/// Parses the `--human` / `--json` / `--help` arguments that follow the `diagnose` command word.
fn parse_diagnose_format(arguments: &[OsString]) -> Result<DiagnoseFormat, ExitFailure> {
    let mut format = None;
    for argument in arguments {
        match argument.to_str() {
            Some("--human") => set_format(&mut format, DiagnoseFormat::Human)?,
            Some("--json") => set_format(&mut format, DiagnoseFormat::Json)?,
            Some("--help") => {
                return Err(ExitFailure::new_stdout(
                    0,
                    "usage: rmux diagnose [--human|--json]",
                ));
            }
            Some(other) => {
                return Err(ExitFailure::new(
                    1,
                    format!("rmux diagnose: unknown argument '{other}'"),
                ));
            }
            None => {
                return Err(ExitFailure::new(
                    1,
                    "rmux diagnose: arguments must be valid UTF-8",
                ));
            }
        }
    }

    Ok(format.unwrap_or(DiagnoseFormat::Human))
}

/// Records a format choice, rejecting a second conflicting one.
fn set_format(
    current: &mut Option<DiagnoseFormat>,
    next: DiagnoseFormat,
) -> Result<(), ExitFailure> {
    if current.is_some_and(|current| current != next) {
        return Err(ExitFailure::new(
            1,
            "rmux diagnose: choose only one of --human or --json",
        ));
    }
    *current = Some(next);
    Ok(())
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
            os_version: os_version(),
            terminal_host,
            term,
            term_program,
            shell: detected_shell(),
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
            redacted = redacted.replace("~\\", "~/");
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
fn push_unique(values: &mut Vec<String>, value: String) {
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

/// The user's shell, from `SHELL`.
fn detected_shell() -> String {
    env_value("SHELL")
}

/// The operating system version string, from `uname -sr`.
fn os_version() -> String {
    command_output("uname", &["-sr"])
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

/// The config files `rmux` would load when no `-f` was given, in platform lookup order.
fn default_config_paths() -> Vec<PathBuf> {
    unix_default_config_paths()
}

/// The Unix config lookup order: `/etc/rmux.conf`, `~/.rmux.conf`, then the `XDG` locations.
fn unix_default_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    push_unique_path(&mut paths, PathBuf::from("/etc/rmux.conf"));
    if let Some(home) = nonempty_env_os("HOME") {
        let home = PathBuf::from(home);
        push_unique_path(&mut paths, home.join(".rmux.conf"));
    }
    if let Some(xdg_config_home) = nonempty_env_os("XDG_CONFIG_HOME") {
        push_unique_path(
            &mut paths,
            PathBuf::from(xdg_config_home)
                .join("rmux")
                .join("rmux.conf"),
        );
    }
    if let Some(home) = nonempty_env_os("HOME") {
        let home = PathBuf::from(home);
        push_unique_path(
            &mut paths,
            home.join(".config").join("rmux").join("rmux.conf"),
        );
    }
    paths
}

/// Reads an environment variable as an `OsString`, treating an empty value as absent.
fn nonempty_env_os(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

/// Appends a path only when it is not already in the list, preserving lookup order.
fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
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

/// The home directories diagnose output must never leak, in tmux's lookup order.
fn home_prefixes() -> Vec<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(nonempty_env_os)
        .map(PathBuf::from)
        .collect()
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

/// Writes the rendered report to stdout, treating a broken pipe as success.
fn write_stdout(output: &str) -> Result<i32, ExitFailure> {
    match io::stdout().lock().write_all(output.as_bytes()) {
        Ok(()) => Ok(0),
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(0),
        Err(error) => Err(ExitFailure::new(1, error.to_string())),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[path = "diagnose_tests.rs"]
mod tests;
