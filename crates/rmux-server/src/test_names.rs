use rmux_proto::SessionName;

/// Builds a [`SessionName`] for tests, panicking on an invalid literal.
///
/// Test fixtures construct session names from hard-coded literals, so an
/// invalid name is a bug in the test rather than a runtime condition worth
/// propagating.
pub(crate) fn session_name(value: &str) -> SessionName {
    SessionName::new(value).expect("valid session name")
}
