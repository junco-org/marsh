//! Which tunnel provider to run, found and read as managed work.
//!
//! A preset is a file a user installed, selected by name in a `web-share --tunnel-provider`
//! request. Both halves of finding one — enumerating the configured directories and reading the
//! chosen file — are therefore *requested* filesystem work, and both go through `__rmux_io`
//! inside a managed job rather than [`std::fs`].
//!
//! There is no trusted bootstrap path here to hold apart from that. The shipped presets are
//! compiled in by the build script and never touch a filesystem at all, and [`load`] has exactly
//! one caller: a client's web-share request. A runtime caller cannot select a bypass because
//! there is none to select.
//!
//! # Why the read has to be complete and approved
//!
//! A preset's content becomes the program name and argument list this daemon executes. Bytes
//! from a job the gate discarded or denied are bytes nobody checked, and running a program named
//! by unchecked bytes is precisely the outcome this boundary exists to prevent — so the read uses
//! the helper that requires publication, and a refusal surfaces as the ordinary preset error
//! rather than as an empty list of available providers.

use std::collections::BTreeSet;
use std::env;
use std::path::{Path, PathBuf};

use regex::Regex;
use rmux_proto::RmuxError;
use serde::Deserialize;

use crate::io::ShellIo;
use crate::managed_workload;

include!(concat!(env!("OUT_DIR"), "/tunnel_presets.rs"));

const USER_PRESET_ENV: &str = "RMUX_TUNNEL_PRESET_DIR";
const DEFAULT_READY_TIMEOUT_SECS: u64 = 30;
const MAX_READY_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TunnelPreset {
    pub(super) name: String,
    pub(super) program: String,
    #[serde(default)]
    pub(super) args: Vec<String>,
    pub(super) url_pattern: String,
    #[serde(default)]
    pub(super) ready_pattern: Option<String>,
    #[serde(default)]
    pub(super) url_source: UrlSource,
    #[serde(default = "default_ready_timeout_secs")]
    pub(super) ready_timeout_secs: u64,
    #[serde(default)]
    pub(super) install_hint: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(super) enum UrlSource {
    #[default]
    Both,
    Stderr,
    Stdout,
}

impl UrlSource {
    pub(super) const fn accepts(self, source: ProcessOutput) -> bool {
        matches!(
            (self, source),
            (Self::Both, _)
                | (Self::Stderr, ProcessOutput::Stderr)
                | (Self::Stdout, ProcessOutput::Stdout)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProcessOutput {
    Stderr,
    Stdout,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum PresetSource {
    Embedded,
    File,
}

/// Loads the preset named `name`, preferring a configured file over a shipped one.
///
/// # Errors
///
/// Fails when the name is not a legal provider name, when the managed enumeration or read was
/// refused or not approved, when a configured file exists but could not be read, when its content
/// does not parse, and when no preset of that name exists at all.
pub(super) async fn load(io: &ShellIo, name: &str) -> Result<TunnelPreset, RmuxError> {
    if !valid_name(name) {
        return Err(RmuxError::Server(
            "web-share tunnel provider names may contain only ASCII letters, digits, '-' and '_'"
                .to_owned(),
        ));
    }
    let directories = preset_dirs();
    let cwd = helper_directory(io);
    // One managed enumeration answers both questions this function has: whether a configured file
    // for this name exists, and what to list as available if it does not. Probing each candidate
    // path with `Path::is_file` would be the same requested filesystem work, done outside the
    // boundary and attributed to nobody.
    let configured = managed_workload::preset_names(io, &cwd, &directories).await?;
    if configured.iter().any(|candidate| candidate == name) {
        let mut failure = None;
        for path in preset_paths(name) {
            match managed_workload::read_file(io, &cwd, &path).await {
                Ok(content) => return parse_file(name, &path, &content),
                Err(error) => {
                    failure = Some(RmuxError::Server(format!(
                        "failed to read web-share tunnel preset '{}': {error}",
                        path.display()
                    )));
                }
            }
        }
        // The enumeration said this name is configured, so every candidate failing is a real
        // failure. Falling through to a shipped preset here would silently start a different
        // program from the one the user installed.
        if let Some(error) = failure {
            return Err(error);
        }
    }
    if let Some((_, content)) = embedded()
        .iter()
        .find(|(preset_name, _)| *preset_name == name)
    {
        return parse(name, PresetSource::Embedded, content);
    }
    Err(RmuxError::Server(no_preset_message(
        name,
        &configured,
        &directories,
    )))
}

/// The directory the managed preset helpers run in.
///
/// This host's default, so a relative `RMUX_TUNNEL_PRESET_DIR` resolves where the daemon was
/// started rather than wherever a request happened to come from. Every shipped preset directory
/// is absolute and does not depend on it.
fn helper_directory(io: &ShellIo) -> PathBuf {
    io.default_dir().to_path_buf()
}

/// Parses a configured preset file's raw bytes.
///
/// Invalid UTF-8 is reported as a read failure rather than repaired: a lossy conversion would
/// hand [`parse`] a program name and argument list that are not the ones in the file.
fn parse_file(name: &str, path: &Path, content: &[u8]) -> Result<TunnelPreset, RmuxError> {
    let content = std::str::from_utf8(content).map_err(|error| {
        RmuxError::Server(format!(
            "failed to read web-share tunnel preset '{}': {error}",
            path.display()
        ))
    })?;
    parse(name, PresetSource::File, content)
}

#[cfg(test)]
pub(super) fn embedded() -> &'static [(&'static str, &'static str)] {
    SHIPPED_TUNNEL_PRESETS
}

#[cfg(not(test))]
fn embedded() -> &'static [(&'static str, &'static str)] {
    SHIPPED_TUNNEL_PRESETS
}

pub(super) fn parse(
    expected_name: &str,
    _source: PresetSource,
    content: &str,
) -> Result<TunnelPreset, RmuxError> {
    let preset: TunnelPreset = toml::from_str(content).map_err(|error| {
        RmuxError::Server(format!(
            "failed to parse web-share tunnel preset '{expected_name}': {error}"
        ))
    })?;
    if preset.name != expected_name {
        return Err(RmuxError::Server(format!(
            "web-share tunnel preset '{expected_name}' declares name '{}'",
            preset.name
        )));
    }
    validate(&preset)?;
    Ok(preset)
}

fn validate(preset: &TunnelPreset) -> Result<(), RmuxError> {
    if !valid_name(&preset.name) {
        return Err(RmuxError::Server(format!(
            "web-share tunnel preset '{}' has an invalid name",
            preset.name
        )));
    }
    if preset.program.trim().is_empty() || preset.program.contains('\0') {
        return Err(RmuxError::Server(format!(
            "web-share tunnel preset '{}' must define a program",
            preset.name
        )));
    }
    if preset.args.iter().any(|arg| arg.contains('\0')) {
        return Err(RmuxError::Server(format!(
            "web-share tunnel preset '{}' contains an invalid argument",
            preset.name
        )));
    }
    if preset.ready_timeout_secs == 0 || preset.ready_timeout_secs > MAX_READY_TIMEOUT_SECS {
        return Err(RmuxError::Server(format!(
            "web-share tunnel preset '{}' ready_timeout_secs must be between 1 and {MAX_READY_TIMEOUT_SECS}",
            preset.name
        )));
    }
    Regex::new(&preset.url_pattern).map_err(|error| {
        RmuxError::Server(format!(
            "web-share tunnel preset '{}' has an invalid url_pattern: {error}",
            preset.name
        ))
    })?;
    if let Some(pattern) = preset.ready_pattern.as_deref() {
        Regex::new(pattern).map_err(|error| {
            RmuxError::Server(format!(
                "web-share tunnel preset '{}' has an invalid ready_pattern: {error}",
                preset.name
            ))
        })?;
    }
    Ok(())
}

fn preset_paths(name: &str) -> Vec<PathBuf> {
    preset_dirs()
        .into_iter()
        .map(|dir| dir.join(format!("{name}.toml")))
        .collect()
}

fn preset_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = env::var_os(USER_PRESET_ENV) {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
        dirs.push(PathBuf::from(config_home).join("rmux/tunnels"));
    } else if let Some(home) = env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".config/rmux/tunnels"));
    }
    dirs.push(PathBuf::from("/usr/local/share/rmux/tunnels"));
    dirs.push(PathBuf::from("/usr/share/rmux/tunnels"));
    dirs
}

/// The diagnostic for a provider name that matches nothing.
///
/// `configured` is the enumeration [`load`] already performed, passed in rather than repeated: a
/// second managed job to build an error message would double the work for every mistyped name.
fn no_preset_message(name: &str, configured: &[String], directories: &[PathBuf]) -> String {
    let names = available_from(
        embedded().iter().map(|(name, _)| (*name).to_owned()),
        configured.iter().cloned(),
    );
    let locations = directories
        .iter()
        .map(|path| format!("  {}", path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let available = if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    };
    format!(
        "no web-share tunnel preset named '{name}'. Available: {available}. Preset locations:\n{locations}"
    )
}

/// Merges the shipped preset names with the configured ones, sorted and without duplicates.
///
/// Configured names are filtered by the provider-name grammar, which is where that check lived
/// when this function walked the directories itself: a `*.toml` whose stem could never be
/// requested must not be advertised as available.
pub(super) fn available_from(
    embedded: impl IntoIterator<Item = String>,
    configured: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut names = embedded.into_iter().collect::<BTreeSet<_>>();
    names.extend(configured.into_iter().filter(|name| valid_name(name)));
    names.into_iter().collect()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

const fn default_ready_timeout_secs() -> u64 {
    DEFAULT_READY_TIMEOUT_SECS
}

#[cfg(test)]
mod tests {
    use super::{parse, PresetSource, UrlSource};

    #[test]
    fn parse_rejects_mismatched_name() {
        let error = parse(
            "expected",
            PresetSource::Embedded,
            r#"
name = "other"
program = "tool"
url_pattern = "https://example\\.test"
"#,
        )
        .expect_err("name mismatch rejected");
        assert!(error.to_string().contains("declares name"));
    }

    #[test]
    fn parse_uses_safe_defaults() {
        let preset = parse(
            "tool",
            PresetSource::Embedded,
            r#"
name = "tool"
program = "tool"
args = ["--port", "{port}"]
url_pattern = "https://example\\.test"
"#,
        )
        .expect("preset parses");
        assert_eq!(preset.url_source, UrlSource::Both);
        assert_eq!(preset.ready_timeout_secs, 30);
    }
}
