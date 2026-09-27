//! Presentation names and opaque instance identities are distinct from policy authority.

/// A reusable job name, used only for display and lookup.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShellId(String);
impl ShellId {
    /// The name as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Whether `%name` needs no quoting.
    pub fn is_bare(&self) -> bool {
        !self.0.is_empty()
            && self.0.chars().all(|character| {
                character.is_ascii_alphanumeric() || character == '_' || character == '-'
            })
    }
    /// A shell-safe job reference for display and command input.
    pub fn reference(&self) -> String {
        if self.is_bare() {
            format!("%{}", self.0)
        } else {
            format!("%{:?}", self.0)
        }
    }
}
impl From<&str> for ShellId {
    fn from(name: &str) -> Self {
        Self(name.to_owned())
    }
}
impl From<String> for ShellId {
    fn from(name: String) -> Self {
        Self(name)
    }
}
impl std::fmt::Display for ShellId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub use crate::Principal;

/// A source-relative directory label in the mux command grammar.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobDir(String);
impl JobDir {
    /// The label as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Whether the label names the source root.
    pub const fn is_root(&self) -> bool {
        self.0.is_empty()
    }
}
impl From<&str> for JobDir {
    fn from(directory: &str) -> Self {
        Self(directory.to_owned())
    }
}
impl From<String> for JobDir {
    fn from(directory: String) -> Self {
        Self(directory)
    }
}
impl std::fmt::Display for JobDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
