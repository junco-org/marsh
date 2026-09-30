//! The embedded routing language: whether one accepted call — a shell span or an embedder's
//! [`MarshTool`] — runs in the managed snapshot view (true) or directly against its source
//! directory (false).
//!
//! A policy is a pure Boolean expression over one routed call, seen through its
//! [`CommandContext`]. It decides the route only; whether a managed call's effects may be
//! published remains the capability gate's decision.

use std::any::Any;
use std::borrow::Cow;
use std::sync::Arc;

use super::Action;
use super::policy::PolicyValidator;
use crate::shellmux::Sandbox;

/// One accepted call a routing policy decides: a shell span or an embedder's tool.
///
/// The owned and borrowed conversions into [`Action`] must agree, and be total, deterministic and
/// side-effect-free: a call is classified once, from a borrow, before its concrete type is erased.
pub trait MarshTool: Any + Send + Sync + Into<Action> {
    /// The accepted input as traces and published WAL metadata record it.
    fn description(&self) -> Cow<'_, str>;
}

/// Every shell span: the accepted top-level text (the submitted string byte for byte, a script
/// path, a function name, or empty text for startup, prompt and end-of-input spans).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellCommand {
    /// The exact input text submitted for this shell span.
    pub command: String,
}
/// Every shell span may write: conservative routing metadata, not a claim about what the text
/// does. Observed effects and Git capabilities remain what publication authorizes.
impl From<&ShellCommand> for Action {
    fn from(_: &ShellCommand) -> Self {
        Self::Edit
    }
}
impl From<ShellCommand> for Action {
    fn from(tool: ShellCommand) -> Self {
        Self::from(&tool)
    }
}
impl MarshTool for ShellCommand {
    fn description(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.command)
    }
}

/// Everything a routing decision may inspect about one accepted call.
pub struct CommandContext<'a> {
    /// The original concrete call, borrowed for type-specific inspection.
    pub tool: &'a (dyn Any + Send + Sync),
    /// The call's stable routing classification, converted before type erasure.
    pub action: &'a Action,
    /// The live-shell record of the shell running the call.
    pub current: &'a Sandbox,
    /// Every live shell in this process, the current one and idle peers included, in no
    /// particular order.
    pub sessions: &'a [Sandbox],
    /// The current source's shared policy authority, the same one managed publication uses.
    pub validator: Arc<PolicyValidator>,
}
impl<'a> CommandContext<'a> {
    /// The routed call as `T`, or `None` when it is another tool type.
    pub fn tool_as<T: MarshTool>(&self) -> Option<&'a T> {
        self.tool.downcast_ref()
    }
}

/// A routing policy: a pure Boolean expression over one routed call. True selects the managed,
/// snapshot-isolated route, false the direct one.
///
/// Evaluation is left to right and short-circuits; it never allocates, performs I/O itself, or
/// refreshes the metadata it was given. [`SandboxPolicy::Base`] predicates must be synchronous and
/// read-only, and must not call back into a shell.
#[derive(Clone, Default)]
pub enum SandboxPolicy {
    /// True iff another live shell, distinguished by its principal, shares the current shell's
    /// source root. The call itself is not inspected.
    #[default]
    SharedSource,
    /// A caller-supplied predicate over the call's context.
    Base(for<'a> fn(&CommandContext<'a>) -> bool),
    /// True iff both operands are; the right operand is not evaluated when the left is false.
    And(Arc<(Self, Self)>),
    /// True iff either operand is; the right operand is not evaluated when the left is true.
    Or(Arc<(Self, Self)>),
}

impl SandboxPolicy {
    /// Always sandbox: every call takes the managed route.
    pub fn allow() -> Self {
        Self::Base(|_| true)
    }

    /// Never sandbox: every call runs directly against its source.
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

    /// Whether `ctx`'s call takes the managed route.
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

    /// An embedder's call that writes exactly when `writes` says so.
    struct Foreign {
        writes: bool,
    }
    impl From<&Foreign> for Action {
        fn from(tool: &Foreign) -> Self {
            if tool.writes { Self::Edit } else { Self::Read }
        }
    }
    impl From<Foreign> for Action {
        fn from(tool: Foreign) -> Self {
            Self::from(&tool)
        }
    }
    impl MarshTool for Foreign {
        fn description(&self) -> Cow<'_, str> {
            Cow::Borrowed("foreign")
        }
    }

    fn context<'a>(
        tool: &'a (dyn Any + Send + Sync),
        action: &'a Action,
        current: &'a Sandbox,
        sessions: &'a [Sandbox],
    ) -> CommandContext<'a> {
        CommandContext {
            tool,
            action,
            current,
            sessions,
            validator: Arc::new(PolicyValidator::new()),
        }
    }

    /// Evaluates `policy` for `tool`, classified from a borrow before erasure as admission does.
    fn eval_tool<T: MarshTool>(
        policy: &SandboxPolicy,
        tool: &T,
        current: &Sandbox,
        sessions: &[Sandbox],
    ) -> bool
    where
        for<'t> &'t T: Into<Action>,
    {
        let action: Action = tool.into();
        policy.eval(&context(tool, &action, current, sessions))
    }

    fn eval(
        policy: &SandboxPolicy,
        command: &str,
        current: &Sandbox,
        sessions: &[Sandbox],
    ) -> bool {
        let tool = ShellCommand {
            command: command.to_owned(),
        };
        eval_tool(policy, &tool, current, sessions)
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
        let exact = SandboxPolicy::Base(|ctx| {
            ctx.tool_as::<ShellCommand>()
                .is_some_and(|shell| shell.command == "printf  x\n")
        });
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

    #[test]
    fn tool_as_downcasts_only_to_the_concrete_type() {
        let current = record("a", "/src", "", "u1");
        let shell = ShellCommand {
            command: "printf x > a.txt".to_owned(),
        };
        let shell_action: Action = (&shell).into();
        let shell_call = context(&shell, &shell_action, &current, &[]);
        assert!(
            shell_call
                .tool_as::<ShellCommand>()
                .is_some_and(|seen| std::ptr::eq(seen, &raw const shell))
        );
        assert!(shell_call.tool_as::<Foreign>().is_none());
        let foreign = Foreign { writes: true };
        let foreign_action: Action = (&foreign).into();
        let foreign_call = context(&foreign, &foreign_action, &current, &[]);
        assert!(foreign_call.tool_as::<ShellCommand>().is_none());
        assert!(
            foreign_call
                .tool_as::<Foreign>()
                .is_some_and(|seen| std::ptr::eq(seen, &raw const foreign) && seen.writes)
        );
    }

    #[test]
    fn shared_source_and_write_actions_route_writers_beside_a_peer() {
        let policy = SandboxPolicy::and(
            SandboxPolicy::SharedSource,
            SandboxPolicy::Base(|ctx| ctx.action.is_write()),
        );
        let current = record("a", "/src", "", "u1");
        let alone = [current.clone()];
        let shared = [current.clone(), record("b", "/src", "", "u2")];
        for (sessions, peer) in [(&alone[..], false), (&shared[..], true)] {
            // Every shell span may write, whatever its text looks like.
            for command in ["", "cat a.txt", "printf x > a.txt"] {
                assert_eq!(
                    eval(&policy, command, &current, sessions),
                    peer,
                    "{command:?} with peer={peer}"
                );
            }
            for writes in [false, true] {
                assert_eq!(
                    eval_tool(&policy, &Foreign { writes }, &current, sessions),
                    peer && writes,
                    "foreign writes={writes} with peer={peer}"
                );
            }
        }
    }
}
