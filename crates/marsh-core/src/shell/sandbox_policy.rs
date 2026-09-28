//! The embedded routing language: whether one accepted command runs in the managed snapshot view
//! (true) or directly against its source directory (false).
//!
//! A policy is a pure Boolean expression over one [`CommandContext`]. It decides the route only;
//! whether a managed command's effects may be published remains the capability gate's decision.

use std::sync::Arc;

use super::policy::PolicyValidator;
use crate::shellmux::Sandbox;

/// Everything a routing decision may inspect about one accepted command.
pub struct CommandContext<'a> {
    /// The accepted top-level input: the submitted string byte for byte, a script path, a function
    /// name, or empty text for startup, prompt and end-of-input spans.
    pub command: &'a str,
    /// The live-shell record of the shell running the command.
    pub current: &'a Sandbox,
    /// Every live shell in this process, the current one and idle peers included, in no
    /// particular order.
    pub sessions: &'a [Sandbox],
    /// The current source's shared policy authority, the same one managed publication uses.
    pub validator: Arc<PolicyValidator>,
}

/// A routing policy: true selects the managed, snapshot-isolated route, false the direct one.
///
/// Evaluation is left to right and short-circuits; it never allocates, performs I/O itself, or
/// refreshes the metadata it was given. [`SandboxPolicy::Base`] predicates must be synchronous and
/// read-only, and must not call back into a shell.
#[derive(Clone, Default)]
pub enum SandboxPolicy {
    /// True iff another live shell, distinguished by its principal, shares the current shell's
    /// source root.
    #[default]
    SharedSource,
    /// A caller-supplied predicate over the command context.
    Base(for<'a> fn(&CommandContext<'a>) -> bool),
    /// True iff both operands are; the right operand is not evaluated when the left is false.
    And(Arc<(Self, Self)>),
    /// True iff either operand is; the right operand is not evaluated when the left is true.
    Or(Arc<(Self, Self)>),
}

impl SandboxPolicy {
    /// Always sandbox: every command takes the managed route.
    pub fn allow() -> Self {
        Self::Base(|_| true)
    }

    /// Never sandbox: every command runs directly against its source.
    pub fn forbid() -> Self {
        Self::Base(|_| false)
    }

    /// The conjunction of `left` and `right`.
    pub fn and(left: Self, right: Self) -> Self {
        Self::And(Arc::new((left, right)))
    }

    /// The disjunction of `left` and `right`.
    pub fn or(left: Self, right: Self) -> Self {
        Self::Or(Arc::new((left, right)))
    }

    /// Whether `ctx`'s command takes the managed route.
    pub fn eval(&self, ctx: &CommandContext<'_>) -> bool {
        match self {
            Self::SharedSource => ctx
                .sessions
                .iter()
                .any(|session| session.uid != ctx.current.uid && session.seed == ctx.current.seed),
            Self::Base(predicate) => predicate(ctx),
            Self::And(operands) => operands.0.eval(ctx) && operands.1.eval(ctx),
            Self::Or(operands) => operands.0.eval(ctx) || operands.1.eval(ctx),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::PathBuf;

    use super::*;
    use crate::shellmux::{JobDir, ShellId};

    fn record(id: &str, seed: &str, dir: &str, uid: &str) -> Sandbox {
        Sandbox {
            id: ShellId::from(id),
            seed: PathBuf::from(seed),
            dir: JobDir::from(dir),
            uid: crate::Principal::from(uid.to_owned()),
        }
    }

    fn eval(
        policy: &SandboxPolicy,
        command: &str,
        current: &Sandbox,
        sessions: &[Sandbox],
    ) -> bool {
        policy.eval(&CommandContext {
            command,
            current,
            sessions,
            validator: Arc::new(PolicyValidator::new()),
        })
    }

    thread_local! { static EVALUATED: Cell<u32> = const { Cell::new(0) }; }

    fn counted() -> SandboxPolicy {
        SandboxPolicy::Base(|_| {
            EVALUATED.with(|count| count.set(count.get() + 1));
            true
        })
    }

    #[test]
    fn connectives_follow_boolean_truth_tables() {
        let current = record("a", "/src", "", "u1");
        let sessions = [current.clone()];
        let leaf = |value: bool| {
            if value {
                SandboxPolicy::allow()
            } else {
                SandboxPolicy::forbid()
            }
        };
        for left in [false, true] {
            for right in [false, true] {
                let and = SandboxPolicy::and(leaf(left), leaf(right));
                let or = SandboxPolicy::or(leaf(left), leaf(right));
                assert_eq!(eval(&and, "", &current, &sessions), left && right);
                assert_eq!(eval(&or, "", &current, &sessions), left || right);
            }
        }
        let nested = SandboxPolicy::or(
            SandboxPolicy::and(SandboxPolicy::allow(), SandboxPolicy::forbid()),
            SandboxPolicy::and(SandboxPolicy::allow(), SandboxPolicy::allow()),
        );
        assert!(eval(&nested, "", &current, &sessions));
    }

    #[test]
    fn right_operand_is_skipped_once_the_left_decides() {
        let current = record("a", "/src", "", "u1");
        let sessions = [current.clone()];
        EVALUATED.with(|count| count.set(0));
        assert!(!eval(
            &SandboxPolicy::and(SandboxPolicy::forbid(), counted()),
            "",
            &current,
            &sessions
        ));
        assert!(eval(
            &SandboxPolicy::or(SandboxPolicy::allow(), counted()),
            "",
            &current,
            &sessions
        ));
        assert_eq!(EVALUATED.with(Cell::get), 0);
        assert!(eval(
            &SandboxPolicy::and(SandboxPolicy::allow(), counted()),
            "",
            &current,
            &sessions
        ));
        assert_eq!(EVALUATED.with(Cell::get), 1);
    }

    #[test]
    fn base_predicates_see_the_command_and_its_records() {
        let current = record("a", "/src", "sub", "u1");
        let sessions = [current.clone()];
        let exact = SandboxPolicy::Base(|ctx| ctx.command == "printf  x\n");
        assert!(eval(&exact, "printf  x\n", &current, &sessions));
        assert!(!eval(&exact, "printf x\n", &current, &sessions));
        let nested = SandboxPolicy::Base(|ctx| ctx.current.dir.as_str() == "sub");
        assert!(eval(&nested, "", &current, &sessions));
    }

    #[test]
    fn shared_source_compares_principal_and_seed_only() {
        let current = record("same", "/src", "a", "u1");
        let policy = SandboxPolicy::SharedSource;
        assert!(!eval(&policy, "", &current, &[]));
        assert!(!eval(&policy, "", &current, std::slice::from_ref(&current)));
        // A second record with the same principal is still this shell, whatever else differs.
        let alias = record("other", "/src", "b", "u1");
        assert!(!eval(&policy, "", &current, &[current.clone(), alias]));
        // Same display name and directory label on another source never counts.
        let elsewhere = record("same", "/other", "a", "u2");
        assert!(!eval(&policy, "", &current, &[current.clone(), elsewhere]));
        // An idle peer on the same source counts whatever its name or initial directory.
        let peer = record("different", "/src", "c", "u2");
        assert!(eval(&policy, "", &current, &[peer, current.clone()]));
    }
}
