//! Per-invocation namespace for Claude Code's tmux-shim calls.
//!
//! Claude Code's teammate mode drives a `tmux` binary. `rmux claude` puts a shim named `tmux`
//! first on the workload's `PATH`, so those calls come back into this executable. They address
//! two fixed logical names — session `rmux-claude` for the leader and session `claude-swarm`
//! on socket label `claude-swarm-<pid>` for teammates — which upstream could serve because each
//! invocation owned a private daemon.
//!
//! There is only one daemon now, and one seed lease with it. Fixed names on a shared daemon
//! would make two concurrent `rmux claude` invocations fight over the same two sessions, so
//! each invocation instead owns a `marsh-io-<nonce>` pair and passes it to its workload through
//! three internal environment variables. This module is the *only* consumer of those variables:
//! it redirects the swarm socket label onto the shared endpoint and rewrites the two logical
//! session names onto the owned pair, for the duration of one shim invocation.
//!
//! # Scope, deliberately narrow
//!
//! * All three variables must be present and valid. A partial or malformed set is an error —
//!   never a silent fall-through that would create a second daemon or address the wrong one.
//! * Only tmux-shim invocations are adapted. A plain `rmux ...` invocation, even inside a Claude
//!   workload, is left exactly as the user wrote it.
//! * An explicit `-S` wins. If it names the shared endpoint the rewrites stay on; if it names a
//!   different one, **both** the endpoint redirect and the name rewrites are disabled, because
//!   sending an owned session name to a foreign socket would address a stranger's session.
//! * Only whole session-name components are rewritten. `:window.pane` suffixes and `=` exact
//!   syntax are preserved; command bodies, format strings and arbitrary option values are never
//!   touched, so `send-keys 'echo claude-swarm'` still types what it says.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use rmux_proto::SessionName;

use super::managed_io::{
    CLAUDE_ENDPOINT_ENV, CLAUDE_MAIN_SESSION_ENV, CLAUDE_SWARM_SESSION_ENV,
};
use super::ExitFailure;

/// Socket-label prefix Claude Code derives from its own process id for teammate calls.
const SWARM_SOCKET_PREFIX: &str = "claude-swarm-";

/// The logical session name Claude Code uses for the leader window.
const LOGICAL_MAIN_SESSION: &str = "rmux-claude";

/// The logical session name Claude Code creates its teammates in.
const LOGICAL_SWARM_SESSION: &str = "claude-swarm";

thread_local! {
    /// The namespace in force for this invocation, installed after endpoint selection.
    ///
    /// A process-wide value would be wrong for the parallel test harness, and threading the
    /// namespace through every target-resolution callsite would add a parameter to thirty
    /// functions that have nothing to do with Claude. This mirrors the command-connection cache
    /// next door: one invocation, one thread, one value, installed once.
    static ACTIVE: RefCell<Option<ClaudeNamespace>> = const { RefCell::new(None) };
}

/// The owned endpoint and session pair one Claude invocation published to its workload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClaudeNamespace {
    /// Absolute path of the shared daemon socket.
    endpoint: PathBuf,
    /// Owned session standing in for [`LOGICAL_MAIN_SESSION`].
    main: SessionName,
    /// Owned session standing in for [`LOGICAL_SWARM_SESSION`].
    swarm: SessionName,
}

impl ClaudeNamespace {
    /// Reads the namespace this invocation was launched with, if any.
    ///
    /// Returns `Ok(None)` when none of the three variables is present, which is the ordinary
    /// case for every invocation that is not a Claude workload's shim call.
    ///
    /// # Errors
    ///
    /// Fails when the set is partial or a session name is invalid. Treating that as "no
    /// namespace" would send Claude's fixed names to the shared daemon, where they would
    /// collide with another invocation.
    fn from_environment() -> Result<Option<Self>, ExitFailure> {
        let endpoint = std::env::var_os(CLAUDE_ENDPOINT_ENV);
        let main = std::env::var_os(CLAUDE_MAIN_SESSION_ENV);
        let swarm = std::env::var_os(CLAUDE_SWARM_SESSION_ENV);
        if endpoint.is_none() && main.is_none() && swarm.is_none() {
            return Ok(None);
        }

        let endpoint = required(endpoint, CLAUDE_ENDPOINT_ENV)?;
        let main = required(main, CLAUDE_MAIN_SESSION_ENV)?;
        let swarm = required(swarm, CLAUDE_SWARM_SESSION_ENV)?;
        Ok(Some(Self {
            endpoint: PathBuf::from(endpoint),
            main: internal_session_name(&main, CLAUDE_MAIN_SESSION_ENV)?,
            swarm: internal_session_name(&swarm, CLAUDE_SWARM_SESSION_ENV)?,
        }))
    }

    /// Returns the shared endpoint this invocation must talk to.
    fn endpoint(&self) -> &Path {
        &self.endpoint
    }

    /// Rewrites one whole session-name component onto the owned pair.
    fn rewrite_component(&self, component: &str) -> Option<&SessionName> {
        match component {
            LOGICAL_MAIN_SESSION => Some(&self.main),
            LOGICAL_SWARM_SESSION => Some(&self.swarm),
            _ => None,
        }
    }
}

/// Requires one internal variable to be present and UTF-8.
fn required(value: Option<std::ffi::OsString>, name: &str) -> Result<String, ExitFailure> {
    let value = value.ok_or_else(|| {
        ExitFailure::new(
            1,
            format!("rmux: internal Claude namespace is incomplete: {name} is not set"),
        )
    })?;
    value.into_string().map_err(|_| {
        ExitFailure::new(
            1,
            format!("rmux: internal Claude namespace variable {name} is not valid UTF-8"),
        )
    })
}

/// Parses one internal session name.
fn internal_session_name(value: &str, name: &str) -> Result<SessionName, ExitFailure> {
    SessionName::new(value.to_owned()).map_err(|error| {
        ExitFailure::new(
            1,
            format!("rmux: internal Claude namespace variable {name} is invalid: {error}"),
        )
    })
}

/// Selects the endpoint and installs the namespace for the rest of this invocation.
///
/// Call this immediately before every endpoint-resolution branch, with that branch's own
/// `-L`/`-S` selection and whether this invocation was reached through the tmux shim. Returns
/// the socket path to resolve against instead of `-L`, or `None` to leave resolution alone.
pub(super) fn select_endpoint(
    invoked_as_tmux: bool,
    socket_name: Option<&std::ffi::OsStr>,
    socket_path: Option<&Path>,
) -> Result<Option<PathBuf>, ExitFailure> {
    if !invoked_as_tmux {
        return Ok(None);
    }
    let Some(namespace) = ClaudeNamespace::from_environment()? else {
        return Ok(None);
    };

    // `-S` wins over `-L`, exactly as ordinary endpoint resolution does. An unrelated `-S`
    // disables name rewriting too: an owned session name means nothing on a foreign socket.
    if let Some(socket_path) = socket_path {
        if !same_endpoint(socket_path, namespace.endpoint()) {
            return Ok(None);
        }
        install(namespace);
        return Ok(None);
    }

    let redirect = socket_name
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|name| name.starts_with(SWARM_SOCKET_PREFIX))
        .then(|| namespace.endpoint().to_path_buf());
    install(namespace);
    Ok(redirect)
}

/// Compares two endpoint selections without requiring either to exist yet.
///
/// The socket may not be present on disk when a shim call runs, so this normalizes to absolute
/// paths rather than canonicalizing.
fn same_endpoint(left: &Path, right: &Path) -> bool {
    let absolute = |path: &Path| std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    absolute(left) == absolute(right)
}

/// Installs the namespace for this thread's invocation.
fn install(namespace: ClaudeNamespace) {
    ACTIVE.with(|active| {
        *active.borrow_mut() = Some(namespace);
    });
}

/// Rewrites the session-name component of a structural target.
///
/// Applied to every target string on its way into `resolve-target`, so `rmux-claude:1.0` from
/// the shim becomes `marsh-io-<nonce>:1.0` and everything else passes through untouched.
pub(super) fn rewrite_target(raw: &str) -> String {
    ACTIVE.with(|active| {
        active
            .borrow()
            .as_ref()
            .map_or_else(|| raw.to_owned(), |namespace| rewrite_with(namespace, raw))
    })
}

/// Rewrites a session name used to create or address a session directly.
///
/// `new-session -s claude-swarm` is the case that matters: the shim creates the teammate
/// session by exact name rather than by resolving a target.
pub(super) fn rewrite_session_name(session_name: SessionName) -> SessionName {
    ACTIVE.with(|active| {
        active.borrow().as_ref().map_or_else(
            || session_name.clone(),
            |namespace| {
                namespace
                    .rewrite_component(session_name.as_str())
                    .cloned()
                    .unwrap_or_else(|| session_name.clone())
            },
        )
    })
}

/// Rewrites one target string against an active namespace.
///
/// Splits off the leading `=` of exact-target syntax and everything from the first `:` or `.`,
/// then replaces the remaining session component only when it matches a logical name whole.
fn rewrite_with(namespace: &ClaudeNamespace, raw: &str) -> String {
    let (exact, rest) = match raw.strip_prefix('=') {
        Some(rest) => ("=", rest),
        None => ("", raw),
    };
    let split = rest.find([':', '.']).unwrap_or(rest.len());
    let (component, suffix) = rest.split_at(split);
    match namespace.rewrite_component(component) {
        Some(owned) => format!("{exact}{owned}{suffix}"),
        None => raw.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rmux_proto::SessionName;

    use super::{rewrite_with, same_endpoint, ClaudeNamespace};

    fn namespace() -> ClaudeNamespace {
        ClaudeNamespace {
            endpoint: PathBuf::from("/run/marsh/rmux.sock"),
            main: SessionName::new("marsh-io-abc").expect("valid session name"),
            swarm: SessionName::new("marsh-io-abc-swarm").expect("valid session name"),
        }
    }

    #[test]
    fn whole_session_components_are_rewritten_with_their_suffixes() {
        let namespace = namespace();

        assert_eq!(rewrite_with(&namespace, "rmux-claude"), "marsh-io-abc");
        assert_eq!(rewrite_with(&namespace, "claude-swarm:0"), "marsh-io-abc-swarm:0");
        assert_eq!(
            rewrite_with(&namespace, "=rmux-claude:1.2"),
            "=marsh-io-abc:1.2"
        );
    }

    #[test]
    fn partial_matches_and_foreign_targets_are_left_alone() {
        let namespace = namespace();

        // A longer name that merely starts with a logical one is a different session.
        assert_eq!(rewrite_with(&namespace, "claude-swarm-2"), "claude-swarm-2");
        assert_eq!(
            rewrite_with(&namespace, "echo claude-swarm"),
            "echo claude-swarm"
        );
        assert_eq!(rewrite_with(&namespace, "%7"), "%7");
        assert_eq!(rewrite_with(&namespace, "other:0.1"), "other:0.1");
    }

    #[test]
    fn a_different_explicit_socket_is_not_the_shared_endpoint() {
        assert!(same_endpoint(
            &PathBuf::from("/run/marsh/rmux.sock"),
            &PathBuf::from("/run/marsh/rmux.sock")
        ));
        assert!(!same_endpoint(
            &PathBuf::from("/run/other.sock"),
            &PathBuf::from("/run/marsh/rmux.sock")
        ));
    }
}
