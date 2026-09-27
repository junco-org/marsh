//! Ordinary builtin registration and logical callback I/O.

use super::execution::ManagedExtensions;

mod exec;
mod git;
pub(super) mod gitcmd;
mod gitexec;

pub use super::execution::{CommandContext, DirectoryEntry, GlobPaths, ReadDir, current_context};
pub use brush_core::builtins::{Command, DeclarationCommand, SimpleCommand};

/// A builtin registration specialized internally for the managed interpreter.
#[derive(Clone)]
pub struct Registration(pub(super) brush_core::builtins::Registration<ManagedExtensions>);
impl Registration {
    /// Enables or disables ordinary command lookup for this registration.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.0.disabled = disabled;
        self
    }
    /// Selects POSIX special-builtin semantics.
    #[must_use]
    pub const fn special_builtin(mut self, special: bool) -> Self {
        self.0.special_builtin = special;
        self
    }
    /// Selects declaration argument handling.
    #[must_use]
    pub const fn declaration_builtin(mut self, declaration: bool) -> Self {
        self.0.declaration_builtin = declaration;
        self
    }
}
/// Registers a Brush command without exposing managed extension types.
pub fn builtin<T: Command + Send + Sync>() -> Registration {
    Registration(brush_core::builtins::builtin::<T, ManagedExtensions>())
}
/// Registers a Brush simple command.
pub fn simple_builtin<T: SimpleCommand + Send + Sync>() -> Registration {
    Registration(brush_core::builtins::simple_builtin::<T, ManagedExtensions>())
}
/// Registers a Brush declaration command.
pub fn decl_builtin<T: DeclarationCommand + Send + Sync>() -> Registration {
    Registration(brush_core::builtins::decl_builtin::<T, ManagedExtensions>())
}
/// Registers a Brush command that receives raw declaration arguments.
pub fn raw_arg_builtin<T: DeclarationCommand + Default + Send + Sync>() -> Registration {
    Registration(brush_core::builtins::raw_arg_builtin::<T, ManagedExtensions>())
}

pub(super) fn managed()
-> std::collections::HashMap<String, brush_core::builtins::Registration<ManagedExtensions>> {
    std::collections::HashMap::from([
        (
            "exec".into(),
            brush_core::builtins::builtin::<exec::ExecBuiltin, ManagedExtensions>(),
        ),
        (
            "git".into(),
            brush_core::builtins::builtin::<git::GitBuiltin, ManagedExtensions>(),
        ),
    ])
}
