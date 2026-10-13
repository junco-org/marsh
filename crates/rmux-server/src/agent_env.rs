//! Daemon-wide agent sandboxing, chosen by the environment the daemon starts in.
//!
//! An unmodified tmux client — a benchmark harness, an agent scaffold — has no way to ask for a
//! policy, so the daemon reads two variables once, when it builds its mux profile:
//!
//! * `MARSH_AGENT_POLICY`: unset, empty or `off` keeps the default routing and no agent policy.
//!   `recommended` sandboxes every call and judges it by [`AgentPolicy::recommended`];
//!   `audit` does the same but only reports, never denies; `sandbox` sandboxes every call with no
//!   agent policy, isolating the cost of the managed route itself.
//! * `MARSH_POLICY_LOG`: a file every denied (or, under `audit`, would-be denied) capability is
//!   appended to as one JSON object per line. `policy` names the policy that decided (`git` or
//!   `agent`) and `mode` whether it enforced (`enforce`) or only reported (`audit`); the Git
//!   policy always enforces.
//!
//! Every pane is one agent: its observations are attributed to the principal `agent`, so taint
//! from one pane follows the agent into the others.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;

use marsh_core::{AgentPolicy, Event, PolicyDecision, PolicyKind, PolicyObserver, SandboxPolicy};

/// The principal every pane's observations are attributed to.
const AGENT: &str = "agent";

/// The routing policy, agent policy and decision observer the environment selects.
pub(crate) struct AgentEnvironment {
    pub sandbox_policy: SandboxPolicy,
    pub agent_policy: Option<Arc<AgentPolicy>>,
    pub policy_observer: Option<Arc<PolicyObserver>>,
}

impl AgentEnvironment {
    /// Reads `MARSH_AGENT_POLICY` and `MARSH_POLICY_LOG`.
    pub(crate) fn from_env() -> std::io::Result<Self> {
        let mode = std::env::var("MARSH_AGENT_POLICY").unwrap_or_default();
        let (sandbox_policy, agent_policy) = match mode.as_str() {
            "" | "off" => (SandboxPolicy::default(), None),
            "sandbox" => (SandboxPolicy::allow(), None),
            "recommended" | "audit" => {
                let policy = AgentPolicy::recommended()
                    .map_err(std::io::Error::other)?
                    .principal(AGENT);
                let policy = if mode == "audit" {
                    policy.audit()
                } else {
                    policy
                };
                (SandboxPolicy::allow(), Some(Arc::new(policy)))
            }
            other => {
                return Err(std::io::Error::other(format!(
                    "MARSH_AGENT_POLICY must be off, sandbox, recommended or audit, not {other:?}"
                )));
            }
        };
        let policy_observer = std::env::var_os("MARSH_POLICY_LOG")
            .filter(|path| !path.is_empty())
            .map(|path| denial_log(PathBuf::from(path), mode == "audit"))
            .transpose()?;
        Ok(Self {
            sandbox_policy,
            agent_policy,
            policy_observer,
        })
    }
}

/// An observer appending each non-grant decision to `path` from a dedicated writer thread, so the
/// authority lock it is called under never waits on the file.
fn denial_log(path: PathBuf, audit: bool) -> std::io::Result<Arc<PolicyObserver>> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let (sender, receiver) = mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("marsh-policy-log".into())
        .spawn(move || {
            for line in receiver {
                if file.write_all(line.as_bytes()).is_err() {
                    break;
                }
            }
        })?;
    let sender = std::sync::Mutex::new(sender);
    Ok(Arc::new(PolicyObserver::with_kind(
        move |kind, event, decision| {
            if let PolicyDecision::Deny {
                failed_precondition,
                ..
            } = decision
            {
                let line = record(kind, event, failed_precondition, audit);
                if let Ok(sender) = sender.lock() {
                    let _ = sender.send(line);
                }
            }
        },
    )))
}

/// One JSON line describing a denial. Only an auditing agent policy leaves it unenforced.
fn record(kind: PolicyKind, event: &Event, precondition: &str, audit: bool) -> String {
    let (policy, mode) = match kind {
        PolicyKind::Git => ("git", "enforce"),
        PolicyKind::Agent if audit => ("agent", "audit"),
        PolicyKind::Agent => ("agent", "enforce"),
    };
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    format!(
        "{{\"time_ms\":{millis},\"policy\":{},\"mode\":{},\"principal\":{},\"action\":{},\"resource\":{},\"precondition\":{}}}\n",
        json(policy),
        json(mode),
        json(event.principal.as_str()),
        json(&event.action.to_string()),
        json(&event.resource.to_string()),
        json(precondition),
    )
}

/// A JSON string literal.
fn json(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if u32::from(control) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_single_line_json() {
        let event = Event::new("p\"1", marsh_core::Action::Edit, ["net", "external"]);
        let line = record(PolicyKind::Agent, &event, "why\nnot\u{1}", true);
        assert!(line.ends_with("}\n") && line.matches('\n').count() == 1);
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["principal"], "p\"1");
        assert_eq!(value["resource"], "net/external");
        assert_eq!(value["precondition"], "why\nnot\u{1}");
        assert_eq!(value["mode"], "audit");
        assert_eq!(value["policy"], "agent");
        let line = record(PolicyKind::Git, &event, "", true);
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            (&value["policy"], &value["mode"]),
            (&"git".into(), &"enforce".into())
        );
    }
}
