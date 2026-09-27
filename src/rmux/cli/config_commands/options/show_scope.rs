use rmux_client::Connection;
use rmux_proto::types::OptionScopeSelector;
use rmux_proto::{HookName, ResolveTargetType, RmuxError, Target};

use crate::cli::ExitFailure;
use crate::cli::target_resolution::{target_session, target_window};
use crate::cli_args::{ShowOptionsArgs, ShowOptionsCommandKind, TargetSpec};

use super::super::super::{
    resolve_current_pane_target, resolve_current_session_target, resolve_target_spec,
    resolve_window_target_or_current,
};
use super::{dummy_pane_target, option_name_supports_scope};

/// Picks the option scope a `show-options` run reads, from its flags, target and option name.
pub(in crate::cli::config_commands) fn resolve_show_options_scope(
    command: ShowOptionsCommandKind,
    args: &ShowOptionsArgs,
) -> Result<ShowOptionsScope, ExitFailure> {
    let force_window = matches!(command, ShowOptionsCommandKind::ShowWindowOptions);
    let hook = args.name.as_deref().and_then(show_options_hook_name);
    // A named, non-hook option that cannot be read at the requested scope falls back to the
    // scope the option itself defaults to.
    let unsupported = move |scope: &OptionScopeSelector| {
        args.name
            .as_deref()
            .filter(|name| hook.is_none() && !option_name_supports_scope(name, scope))
    };
    if args.server {
        if let Some(name) = unsupported(&OptionScopeSelector::ServerGlobal) {
            return show_named_scope_fallback(args.target.as_ref(), name);
        }
        return Ok(OptionScopeSelector::ServerGlobal.into());
    }

    match (args.window || force_window, args.pane, args.target.as_ref()) {
        (true, false, _) if args.global => Ok(OptionScopeSelector::WindowGlobal.into()),
        (true, false, target) => unsupported(&OptionScopeSelector::WindowGlobal).map_or_else(
            || {
                Ok(ShowOptionsScope::for_target(
                    target,
                    UnresolvedShowOptionsScope::Window,
                ))
            },
            |name| show_named_scope_fallback(target, name),
        ),
        (false, true, target) if args.global && hook.is_some() => Ok(ShowOptionsScope::for_target(
            target,
            UnresolvedShowOptionsScope::Pane,
        )),
        (false, true, _) if args.global => show_global_pane_options_scope(args),
        (false, true, target) => unsupported(&OptionScopeSelector::Pane(dummy_pane_target()))
            .map_or_else(
                || {
                    Ok(ShowOptionsScope::for_target(
                        target,
                        UnresolvedShowOptionsScope::Pane,
                    ))
                },
                |name| show_named_scope_fallback(target, name),
            ),
        (false, false, _) if args.global => Ok(if let Some(hook) = hook {
            global_hook_option_scope(hook)
        } else if let Some(name) = args.name.as_deref() {
            rmux_core::default_global_scope_for_option_name(name)
                .map_err(option_lookup_exit_failure)?
        } else if force_window {
            OptionScopeSelector::WindowGlobal
        } else {
            OptionScopeSelector::SessionGlobal
        }
        .into()),
        (false, false, Some(target)) => match hook {
            Some(hook) => Ok(ShowOptionsScope::unresolved(target, hook_scope_kind(hook))),
            None => show_options_scope_for_target(target, args.name.as_deref()),
        },
        (false, false, None) if force_window => Ok(ShowOptionsScope::CurrentWindow),
        (false, false, None) => Ok(hook.map_or(ShowOptionsScope::CurrentSession, |hook| {
            hook_scope_kind(hook).current()
        })),
        (true, true, _) => unreachable!("clap scope group prevents -w and -p together"),
    }
}

/// Scope a `show-options` run reads from, either already known or still needing target resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::cli::config_commands) enum ShowOptionsScope {
    Resolved(OptionScopeSelector),
    CurrentSession,
    CurrentWindow,
    CurrentPane,
    Unresolved {
        target: TargetSpec,
        kind: UnresolvedShowOptionsScope,
    },
}

/// Kind of target a deferred `show-options` scope resolves to once the server answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::cli::config_commands) enum UnresolvedShowOptionsScope {
    Session,
    Window,
    Pane,
}

impl UnresolvedShowOptionsScope {
    /// The client's current scope of this kind.
    const fn current(self) -> ShowOptionsScope {
        match self {
            Self::Session => ShowOptionsScope::CurrentSession,
            Self::Window => ShowOptionsScope::CurrentWindow,
            Self::Pane => ShowOptionsScope::CurrentPane,
        }
    }
}

impl ShowOptionsScope {
    /// Defers resolving `target` as a `kind` scope until a connection is available.
    fn unresolved(target: &TargetSpec, kind: UnresolvedShowOptionsScope) -> Self {
        Self::Unresolved {
            target: target.clone(),
            kind,
        }
    }

    /// The `kind` scope of `target`, or the client's current one when no target was named.
    fn for_target(target: Option<&TargetSpec>, kind: UnresolvedShowOptionsScope) -> Self {
        target.map_or_else(|| kind.current(), |target| Self::unresolved(target, kind))
    }

    /// Resolves a deferred scope into a concrete `OptionScopeSelector` via the server.
    pub(in crate::cli::config_commands) fn resolve(
        self,
        connection: &mut Connection,
        command_name: &str,
    ) -> Result<OptionScopeSelector, ExitFailure> {
        match self {
            Self::Resolved(scope) => Ok(scope),
            Self::CurrentSession => {
                resolve_current_session_target(connection).map(OptionScopeSelector::Session)
            }
            Self::CurrentWindow => resolve_window_target_or_current(connection, None, command_name)
                .map(OptionScopeSelector::Window),
            Self::CurrentPane => {
                resolve_current_pane_target(connection, command_name).map(OptionScopeSelector::Pane)
            }
            Self::Unresolved { target, kind } => {
                resolve_unresolved_show_options_scope(connection, &target, kind)
            }
        }
    }
}

impl From<OptionScopeSelector> for ShowOptionsScope {
    /// Wraps an already concrete `OptionScopeSelector` as a resolved scope.
    fn from(scope: OptionScopeSelector) -> Self {
        Self::Resolved(scope)
    }
}

/// Chooses between pane scope and the option's default global scope for `show-options -gp`.
fn show_global_pane_options_scope(args: &ShowOptionsArgs) -> Result<ShowOptionsScope, ExitFailure> {
    let pane_scope =
        || ShowOptionsScope::for_target(args.target.as_ref(), UnresolvedShowOptionsScope::Pane);
    let Some(name) = args.name.as_deref() else {
        return Ok(pane_scope());
    };
    match rmux_core::resolve_option_name(name) {
        Ok(query)
            if query.is_user()
                || query.supports_scope(&OptionScopeSelector::Pane(dummy_pane_target())) =>
        {
            Ok(pane_scope())
        }
        Ok(_) => Ok(rmux_core::default_global_scope_for_option_name(name)
            .map_err(option_lookup_exit_failure)?
            .into()),
        Err(error) => Err(option_lookup_exit_failure(error)),
    }
}

/// Resolves a target spec to a session, window or pane scope, widening pane targets as needed.
fn resolve_unresolved_show_options_scope(
    connection: &mut Connection,
    target: &TargetSpec,
    kind: UnresolvedShowOptionsScope,
) -> Result<OptionScopeSelector, ExitFailure> {
    let target_type = match kind {
        UnresolvedShowOptionsScope::Session => ResolveTargetType::Session,
        UnresolvedShowOptionsScope::Window => ResolveTargetType::Window,
        UnresolvedShowOptionsScope::Pane => ResolveTargetType::Pane,
    };
    match (
        kind,
        resolve_target_spec(connection, target, target_type, false, false)?,
    ) {
        (UnresolvedShowOptionsScope::Pane, Target::Pane(target)) => {
            Ok(OptionScopeSelector::Pane(target))
        }
        (UnresolvedShowOptionsScope::Pane, _) => Err(ExitFailure::new(
            1,
            "show-options -p requires a pane target",
        )),
        (UnresolvedShowOptionsScope::Session, target) => {
            Ok(OptionScopeSelector::Session(target_session(target)))
        }
        (UnresolvedShowOptionsScope::Window, target) => {
            Ok(OptionScopeSelector::Window(target_window(target)))
        }
    }
}

/// The deferred scope kind the named option's default global scope implies, `None` for a
/// server option, which needs no target.
fn option_scope_kind(name: &str) -> Result<Option<UnresolvedShowOptionsScope>, ExitFailure> {
    Ok(
        match rmux_core::default_global_scope_for_option_name(name)
            .map_err(option_lookup_exit_failure)?
        {
            OptionScopeSelector::ServerGlobal => None,
            OptionScopeSelector::WindowGlobal | OptionScopeSelector::Window(_) => {
                Some(UnresolvedShowOptionsScope::Window)
            }
            OptionScopeSelector::Pane(_) => Some(UnresolvedShowOptionsScope::Pane),
            OptionScopeSelector::SessionGlobal | OptionScopeSelector::Session(_) => {
                Some(UnresolvedShowOptionsScope::Session)
            }
        },
    )
}

/// Derives the deferred scope kind for a target from the named option's default global scope.
fn show_options_scope_for_target(
    target: &TargetSpec,
    name: Option<&str>,
) -> Result<ShowOptionsScope, ExitFailure> {
    let kind = match name {
        Some(name) => option_scope_kind(name)?,
        None => Some(UnresolvedShowOptionsScope::Session),
    };
    Ok(kind.map_or(
        ShowOptionsScope::Resolved(OptionScopeSelector::ServerGlobal),
        |kind| ShowOptionsScope::unresolved(target, kind),
    ))
}

/// Falls back to the named option's default scope when the requested scope does not support it.
fn show_named_scope_fallback(
    target: Option<&TargetSpec>,
    name: &str,
) -> Result<ShowOptionsScope, ExitFailure> {
    if let Some(target) = target {
        return show_options_scope_for_target(target, Some(name));
    }
    Ok(option_scope_kind(name)?.map_or(
        ShowOptionsScope::Resolved(OptionScopeSelector::ServerGlobal),
        UnresolvedShowOptionsScope::current,
    ))
}

/// Parses an option name as a hook name, tolerating a trailing `[index]` array subscript.
fn show_options_hook_name(value: &str) -> Option<HookName> {
    let name = match value.rsplit_once('[') {
        Some((name, index)) if index.strip_suffix(']')?.parse::<u32>().is_ok() => name,
        Some(_) => return None,
        None => value,
    };
    HookName::from_str(name)
}

/// Maps a hook to the global scope its options live in, either session or window global.
const fn global_hook_option_scope(hook: HookName) -> OptionScopeSelector {
    match rmux_core::hook_global_root(hook) {
        rmux_core::HookGlobalRoot::Session => OptionScopeSelector::SessionGlobal,
        rmux_core::HookGlobalRoot::Window => OptionScopeSelector::WindowGlobal,
    }
}

/// Reports whether a hook naturally resolves against a session, window or pane target.
fn hook_scope_kind(hook: HookName) -> UnresolvedShowOptionsScope {
    match rmux_core::hook_natural_scope_for_target(hook, Target::Pane(dummy_pane_target())) {
        rmux_proto::ScopeSelector::Session(_) => UnresolvedShowOptionsScope::Session,
        rmux_proto::ScopeSelector::Window(_) => UnresolvedShowOptionsScope::Window,
        rmux_proto::ScopeSelector::Pane(_) => UnresolvedShowOptionsScope::Pane,
        rmux_proto::ScopeSelector::Global => unreachable!("natural hook scope is local"),
    }
}

/// Converts an option-name lookup error into an exit failure without its `server error: ` prefix.
fn option_lookup_exit_failure(error: RmuxError) -> ExitFailure {
    match error {
        RmuxError::Server(message) | RmuxError::Message(message) => {
            let normalized = message.strip_prefix("server error: ").unwrap_or(&message);
            ExitFailure::new(1, normalized.to_owned())
        }
        error => ExitFailure::new(1, error.to_string()),
    }
}
