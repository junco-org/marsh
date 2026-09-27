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

/// Implements `PartialEq` against proto targets as "this spec statically resolved to exactly that
/// target": either one bare `Target` variant, or a `Session`-or-`$variant` enum such as the
/// session or window a `move-window` names.
macro_rules! target_spec_eq {
    ($($other:ident => $variant:ident),+ $(,)?) => {$(
        impl PartialEq<$other> for TargetSpec {
            fn eq(&self, other: &$other) -> bool {
                matches!(self.exact(), Some(Target::$variant(target)) if target == other)
            }
        }
    )+};
    ($($other:ident: Session | $variant:ident),+ $(,)?) => {$(
        impl PartialEq<$other> for TargetSpec {
            fn eq(&self, other: &$other) -> bool {
                match (self.exact(), other) {
                    (Some(Target::Session(target)), $other::Session(other)) => target == other,
                    (Some(Target::$variant(target)), $other::$variant(other)) => target == other,
                    _ => false,
                }
            }
        }
    )+};
}

target_spec_eq! { SessionName => Session, WindowTarget => Window, PaneTarget => Pane }
target_spec_eq! {
    MoveWindowTarget: Session | Window,
    SelectLayoutTarget: Session | Window,
    SplitWindowTarget: Session | Pane,
}

impl PartialEq<Target> for TargetSpec {
    /// True when this spec statically resolved to exactly that target.
    fn eq(&self, other: &Target) -> bool {
        self.exact().is_some_and(|target| target == other)
    }
}

/// Validates a session name typed on the command line, returning the error text `clap` prints.
pub(super) fn parse_session_name(value: &str) -> Result<SessionName, String> {
    SessionName::new(value.to_owned()).map_err(|error| error.to_string())
}

/// Parses a `-t` value, deferring to the server when it names a runtime id or an unknown shape.
pub(crate) fn parse_target_spec(value: &str) -> Result<TargetSpec, String> {
    // A leading `=` requests an exact rather than prefix match.
    let parse_value = value.strip_prefix('=').unwrap_or(value);
    // Live `$`/`@` ids, and any other nonempty text the static parser rejects, are left for the
    // running server to resolve.
    let exact = if contains_runtime_target_id(parse_value) {
        None
    } else {
        match Target::parse(parse_value) {
            Ok(target) => Some(target),
            Err(_) if !parse_value.is_empty() => None,
            Err(error) => return Err(error.to_string()),
        }
    };
    Ok(TargetSpec {
        raw: value.to_owned(),
        exact,
    })
}

/// True when a colon or dot component starts with `$` or `@`, naming a live session or window id.
fn contains_runtime_target_id(value: &str) -> bool {
    value
        .split([':', '.'])
        .any(|part| part.starts_with(['$', '@']))
}
