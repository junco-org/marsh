use std::fmt;

use rmux_proto::{
    MoveWindowTarget, PaneTarget, SelectLayoutTarget, SessionName, SplitWindowTarget, Target,
    WindowTarget,
};

/// A `-t` argument keeping its original text alongside the target it parsed to, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TargetSpec {
    raw: String,
    exact: Option<Target>,
}

impl TargetSpec {
    /// The target exactly as typed, including any `=` prefix, for server-side resolution.
    pub(crate) fn raw(&self) -> &str {
        &self.raw
    }

    /// The statically parsed target, or `None` when only the server can resolve the text.
    pub(crate) const fn exact(&self) -> Option<&Target> {
        self.exact.as_ref()
    }
}

impl fmt::Display for TargetSpec {
    /// Writes the target back in the form the user typed.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.raw)
    }
}

impl PartialEq<SessionName> for TargetSpec {
    /// True when this spec statically resolved to exactly that session.
    fn eq(&self, other: &SessionName) -> bool {
        matches!(self.exact(), Some(Target::Session(session_name)) if session_name == other)
    }
}

impl PartialEq<WindowTarget> for TargetSpec {
    /// True when this spec statically resolved to exactly that window.
    fn eq(&self, other: &WindowTarget) -> bool {
        matches!(self.exact(), Some(Target::Window(target)) if target == other)
    }
}

impl PartialEq<PaneTarget> for TargetSpec {
    /// True when this spec statically resolved to exactly that pane.
    fn eq(&self, other: &PaneTarget) -> bool {
        matches!(self.exact(), Some(Target::Pane(target)) if target == other)
    }
}

impl PartialEq<Target> for TargetSpec {
    /// True when this spec statically resolved to exactly that target.
    fn eq(&self, other: &Target) -> bool {
        self.exact().is_some_and(|target| target == other)
    }
}

impl PartialEq<MoveWindowTarget> for TargetSpec {
    /// True when this spec statically resolved to the same session or window to move.
    fn eq(&self, other: &MoveWindowTarget) -> bool {
        match (self.exact(), other) {
            (Some(Target::Session(session_name)), MoveWindowTarget::Session(other)) => {
                session_name == other
            }
            (Some(Target::Window(target)), MoveWindowTarget::Window(other)) => target == other,
            _ => false,
        }
    }
}

impl PartialEq<SelectLayoutTarget> for TargetSpec {
    /// True when this spec statically resolved to the same session or window to lay out.
    fn eq(&self, other: &SelectLayoutTarget) -> bool {
        match (self.exact(), other) {
            (Some(Target::Session(session_name)), SelectLayoutTarget::Session(other)) => {
                session_name == other
            }
            (Some(Target::Window(target)), SelectLayoutTarget::Window(other)) => target == other,
            _ => false,
        }
    }
}

impl PartialEq<SplitWindowTarget> for TargetSpec {
    /// True when this spec statically resolved to the same session or pane to split.
    fn eq(&self, other: &SplitWindowTarget) -> bool {
        match (self.exact(), other) {
            (Some(Target::Session(session_name)), SplitWindowTarget::Session(other)) => {
                session_name == other
            }
            (Some(Target::Pane(target)), SplitWindowTarget::Pane(other)) => target == other,
            _ => false,
        }
    }
}

/// Validates a session name typed on the command line, returning the error text `clap` prints.
pub(super) fn parse_session_name(value: &str) -> Result<SessionName, String> {
    SessionName::new(value.to_owned()).map_err(|error| error.to_string())
}

/// Parses a `-t` value, deferring to the server when it names a runtime id or an unknown shape.
pub(crate) fn parse_target_spec(value: &str) -> Result<TargetSpec, String> {
    let parse_value = exact_match_target(value);

    if contains_runtime_target_id(parse_value) {
        return Ok(TargetSpec {
            raw: value.to_owned(),
            exact: None,
        });
    }

    match Target::parse(parse_value) {
        Ok(target) => Ok(TargetSpec {
            raw: value.to_owned(),
            exact: Some(target),
        }),
        Err(_) if is_runtime_resolved_target_shape(parse_value) => Ok(TargetSpec {
            raw: value.to_owned(),
            exact: None,
        }),
        Err(error) => Err(error.to_string()),
    }
}

/// Strips the leading `=` that requests an exact rather than prefix match.
fn exact_match_target(value: &str) -> &str {
    value.strip_prefix('=').unwrap_or(value)
}

/// True for any nonempty value, which the running server may still resolve.
const fn is_runtime_resolved_target_shape(value: &str) -> bool {
    !value.is_empty()
}

/// True when a colon or dot component starts with `$` or `@`, naming a live session or window id.
fn contains_runtime_target_id(value: &str) -> bool {
    value
        .split([':', '.'])
        .any(|part| part.starts_with(['$', '@']))
}
