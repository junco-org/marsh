//! The two identities a job has: the name it answers to, and the snapshot it runs in.
//!
//! A name carries an explicit capability scope: reusable names are session-local, while a caller
//! may opt a stable agent identity into durable ownership. Snapshot ids still distinguish every
//! generation of a job, so a retained handle can never reach that name's replacement.

use crate::policy::{Principal, durable_principal, escaped_live_principal};

/// A job's displayed name and capability identity. `From` creates an ordinary reusable name;
/// [`Self::durable`] explicitly opts a stable caller-owned identity into WAL recovery.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShellId {
    name: Principal,
    scoped_principal: Option<Principal>,
    durable: bool,
}

impl ShellId {
    /// A stable agent identity whose capabilities survive closing and reopening the seed.
    ///
    /// The embedding caller must control the identity and keep it stable for the same agent;
    /// this is an explicit authority choice, not authentication of an arbitrary pane name.
    /// Ordinary names cannot impersonate this identity, even by spelling its policy namespace.
    /// This does not reassign legacy snapshot-UID grants. Keep the typed id when reusing it:
    /// converting its displayed name through `From` deliberately selects session-local scope.
    #[must_use]
    pub fn durable(name: impl Into<Principal>) -> Self {
        let name = name.into();
        let scoped_principal = Some(durable_principal(name.as_str()));
        Self {
            name,
            scoped_principal,
            durable: true,
        }
    }

    pub(crate) fn durable_name(&self) -> Option<&Principal> {
        self.durable.then_some(&self.name)
    }

    /// The name as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.name.as_str()
    }

    /// The capability principal this name is, for the policy layer.
    #[must_use]
    pub const fn principal(&self) -> &Principal {
        match &self.scoped_principal {
            Some(principal) => principal,
            None => &self.name,
        }
    }

    /// Whether this name needs no quoting when written as `%name`: non-empty ASCII alphanumerics,
    /// `_` and `-` only.
    ///
    /// The set is the one that survives being printed in a job table and typed back without
    /// quoting.
    #[must_use]
    pub fn is_bare(&self) -> bool {
        let name = self.as_str();
        !name.is_empty()
            && name.chars().all(|character| {
                character.is_ascii_alphanumeric() || character == '_' || character == '-'
            })
    }

    /// `%name` when the name is one word, `%"a name"` (`{name:?}`) when it is not.
    ///
    /// Every `%…` a front-end prints goes through this, because a job reference is also *input*:
    /// it is what a reader types at `fg` and `stop`, so a row of a job table has to be
    /// re-typeable. `{name:?}` is exact rather than merely close: a bare name holds neither a
    /// quote nor a control character, so there is nothing for `Debug` to escape.
    #[must_use]
    pub fn reference(&self) -> String {
        if self.is_bare() {
            format!("%{}", self.as_str())
        } else {
            format!("%{:?}", self.as_str())
        }
    }
}

impl From<&str> for ShellId {
    fn from(name: &str) -> Self {
        Self::from(Principal::from(name))
    }
}

impl From<String> for ShellId {
    fn from(name: String) -> Self {
        Self::from(Principal::from(name))
    }
}

impl From<Principal> for ShellId {
    fn from(name: Principal) -> Self {
        let scoped_principal = escaped_live_principal(&name);
        Self {
            name,
            scoped_principal,
            durable: false,
        }
    }
}

impl std::fmt::Display for ShellId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The id of one snapshot: the directory it lives in under `snap/`, and the generation marker that
/// distinguishes two jobs that held the same name.
///
/// A name can be reused; a snapshot id never is. That is what makes a retained handle safe: every
/// mutation checks the pair, so a handle on a closed job cannot reach the job that took its name.
/// It is the same property the seed's log relies on, which is why this is the seed layer's type
/// re-exported and not a second one: a mux handle and a durable transaction have to be talking
/// about the same id for either guarantee to mean anything.
pub use crate::SnapshotUid;

/// The seed-relative directory a job's commands start in.
///
/// A label in the `sd NAME DIR` grammar and in the console prompt — a `/`-joined string the user
/// types, not a path — with `""` naming the seed root. Separate from a [`SnapshotUid`] because the
/// two are both strings and mean entirely different things.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobDir(String);

impl JobDir {
    /// The label as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this names the seed root.
    #[must_use]
    pub const fn is_root(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<&str> for JobDir {
    fn from(dir: &str) -> Self {
        Self(dir.to_string())
    }
}

impl From<String> for JobDir {
    fn from(dir: String) -> Self {
        Self(dir)
    }
}

impl std::fmt::Display for JobDir {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
