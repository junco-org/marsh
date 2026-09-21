use std::env;
use std::time::{Duration, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use rmux_proto::WebShareCreatedResponse;

use super::{DEFAULT_WIDTH, OSC8_URL_LABEL_WIDTH};

/// Whether the share summary is painted with ANSI styling or as plain text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OutputStyle {
    Ansi,
    Plain,
}

/// Whether URLs are emitted as `OSC 8` hyperlinks or printed literally.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LinkMode {
    Osc8,
    PlainUrl,
}

impl OutputStyle {
    /// Picks `Plain` for `NO_COLOR`, a `dumb` `TERM`, or a terminal that rejects virtual output.
    pub(super) fn detect() -> Self {
        if env::var_os("NO_COLOR").is_some() {
            return Self::Plain;
        }

        if env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb")) {
            return Self::Plain;
        }

        if rmux_os::terminal::enable_virtual_terminal_output() {
            Self::Ansi
        } else {
            Self::Plain
        }
    }
}

/// How a URL is presented: an inline clickable label, or deferred to its own line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum UrlLabel {
    Clickable(String),
    PrintedBelow,
}

impl LinkMode {
    /// Whether this mode emits `OSC 8` escape sequences.
    pub(super) const fn supports_osc8(self) -> bool {
        matches!(self, Self::Osc8)
    }

    /// Detects `OSC 8` support from `TERM`, `TERM_PROGRAM`, and known terminal marker variables.
    pub(super) fn detect() -> Self {
        let term = env::var("TERM").unwrap_or_default().to_ascii_lowercase();
        let term_program = env::var("TERM_PROGRAM")
            .unwrap_or_default()
            .to_ascii_lowercase();

        if env::vars().any(|(key, _)| key.starts_with("GHOSTTY_"))
            || matches!(
                term_program.as_str(),
                "wezterm" | "ghostty" | "vscode" | "iterm.app"
            )
            || term.starts_with("xterm-kitty")
            || term.starts_with("wezterm")
            || term.starts_with("xterm-ghostty")
            || term.starts_with("foot")
            || env::var_os("KONSOLE_VERSION").is_some()
            || (env::var_os("VTE_VERSION").is_some()
                && (env::var_os("GNOME_TERMINAL_SCREEN").is_some()
                    || env::var_os("TILIX_ID").is_some()))
        {
            return Self::Osc8;
        }

        Self::PlainUrl
    }
}

/// Current terminal column count, falling back to `COLUMNS` and then `DEFAULT_WIDTH`.
pub(super) fn terminal_width() -> u16 {
    rmux_os::terminal::current_size()
        .map(|size| size.cols)
        .or_else(|| {
            env::var("COLUMNS")
                .ok()
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(DEFAULT_WIDTH)
}

/// Whether the ambient `TERM_PROGRAM` mis-renders half-block QR codes and needs the safe form.
pub(super) fn terminal_needs_qr_fallback() -> bool {
    terminal_program_needs_qr_fallback(&env::var("TERM_PROGRAM").unwrap_or_default())
}

/// Whether the named terminal program, such as Apple Terminal, needs the QR fallback rendering.
fn terminal_program_needs_qr_fallback(term_program: &str) -> bool {
    matches!(
        term_program.to_ascii_lowercase().as_str(),
        "apple_terminal" | "terminal.app"
    )
}

/// Tunnel provider name for the share, falling back to the public URL host then `localhost`.
pub(super) fn provider_label(created: &WebShareCreatedResponse) -> String {
    created
        .tunnel_provider
        .clone()
        .or_else(|| {
            created
                .tunnel_public_url
                .as_deref()
                .and_then(host_from_url)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "localhost".to_owned())
}

/// Host serving the share front end, falling back to `share.rmux.io`.
pub(super) fn frontend_label(created: &WebShareCreatedResponse) -> String {
    created
        .spectator_url
        .as_deref()
        .or(created.operator_url.as_deref())
        .and_then(host_from_url)
        .map(str::to_owned)
        .unwrap_or_else(|| "share.rmux.io".to_owned())
}

/// Share expiry as an `RFC 3339` timestamp, or `no expiry` when unbounded.
pub(super) fn expiry_label(created: &WebShareCreatedResponse) -> String {
    created
        .expires_at_unix
        .map(format_unix_rfc3339)
        .unwrap_or_else(|| "no expiry".to_owned())
}

/// Pluralized participant-limit phrase for `role`, such as `limit: 2 spectators`.
pub(super) fn role_limit(limit: u16, role: &str) -> String {
    if limit == 1 {
        format!("limit: 1 {role}")
    } else {
        format!("limit: {limit} {role}s")
    }
}

/// The URL text to paint inline: compacted for `OSC 8` labels, verbatim otherwise.
pub(super) fn display_url(url: &str, max_chars: usize, link_mode: LinkMode) -> String {
    match link_mode {
        LinkMode::Osc8 => compact_url(url, max_chars.min(OSC8_URL_LABEL_WIDTH)),
        LinkMode::PlainUrl => url.to_owned(),
    }
}

/// Decides whether `url` fits inline as a clickable label or must be printed below.
pub(super) fn url_label(url: &str, max_chars: usize, link_mode: LinkMode) -> UrlLabel {
    match link_mode {
        LinkMode::Osc8 => UrlLabel::Clickable(display_url(url, max_chars, link_mode)),
        LinkMode::PlainUrl if url.chars().count() <= max_chars => {
            UrlLabel::Clickable(url.to_owned())
        }
        LinkMode::PlainUrl => UrlLabel::PrintedBelow,
    }
}

/// Shortens `url` to `max_chars`, dropping `https://` and preferring share-token elision.
pub(super) fn compact_url(url: &str, max_chars: usize) -> String {
    let visible = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("HTTPS://"))
        .unwrap_or(url);
    if let Some(label) = compact_share_url(visible, max_chars) {
        return label;
    }
    compact_middle(visible, max_chars)
}

/// Truncates `value` to `max_chars` by eliding its middle with `…`, or its tail when very narrow.
pub(super) fn compact_middle(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    if max_chars <= 8 {
        return value.chars().take(max_chars).collect();
    }
    let left_len = (max_chars - 1) / 2;
    let right_len = max_chars - 1 - left_len;
    let left = value.chars().take(left_len).collect::<String>();
    let right = value
        .chars()
        .rev()
        .take(right_len)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{left}…{right}")
}

/// Extracts the host authority from `url`, ignoring any scheme and path.
fn host_from_url(url: &str) -> Option<&str> {
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    without_scheme
        .split('/')
        .next()
        .filter(|host| !host.is_empty())
}

/// Formats a Unix-seconds timestamp as an `RFC 3339` UTC string.
fn format_unix_rfc3339(value: u64) -> String {
    DateTime::<Utc>::from(UNIX_EPOCH + Duration::from_secs(value)).to_rfc3339()
}

/// Compacts a share URL of the `base/#t=token` shape, or `None` when it has no such token.
fn compact_share_url(visible: &str, max_chars: usize) -> Option<String> {
    let (base, fragment) = visible.split_once("/#")?;
    let token = fragment
        .split('&')
        .find_map(|param| param.strip_prefix("t="))?;
    Some(compact_share_token(base, token, max_chars))
}

/// Elides the middle of `token` so that `base/#t=token` fits within `max_chars`.
fn compact_share_token(base: &str, token: &str, max_chars: usize) -> String {
    let prefix = format!("{base}/#t=");
    let full = format!("{prefix}{token}");
    if full.chars().count() <= max_chars {
        return full;
    }
    let prefix_len = prefix.chars().count();
    if max_chars <= prefix_len + 1 {
        return compact_middle(base, max_chars);
    }
    let token_budget = max_chars.saturating_sub(prefix_len + 1);
    if token_budget <= 8 {
        return format!("{prefix}…");
    }
    let left_len = token_budget / 2;
    let right_len = token_budget - left_len;
    let left = token.chars().take(left_len).collect::<String>();
    let right = token
        .chars()
        .rev()
        .take(right_len)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{prefix}{left}…{right}")
}
