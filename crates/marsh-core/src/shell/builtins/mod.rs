//! The builtins an attached shell registers over stock `brush-builtins`, one file each.
//!
//! `git` forwards to the system git through the shell's recorded spawner and, in an attached
//! shell, observes what it did; `exec` runs its program through the spawner and exits the shell
//! instead of replacing the process. [`all`] is the table
//! [`Shell::attach`](super::Shell::attach) registers; adding a builtin is one file and one
//! line here.

use std::collections::HashMap;

use brush_core::ShellExtensions;
use brush_core::builtins::Registration;

mod exec;
mod git;
pub mod gitcmd;
mod gitexec;

pub use exec::exec_builtins;
pub(crate) use git::managed_registration;
pub use git::{SNAPSHOT_ROOT_VAR, git_builtins};

/// Every builtin of this module, keyed by name.
#[must_use]
pub fn all<SE: ShellExtensions>() -> HashMap<String, Registration<SE>> {
    let mut builtins = git_builtins();
    builtins.extend(exec_builtins());
    builtins
}
