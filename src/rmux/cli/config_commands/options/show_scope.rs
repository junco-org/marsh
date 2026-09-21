use rmux_client::Connection;
use rmux_proto::types::OptionScopeSelector;
use rmux_proto::{PaneTarget, ResolveTargetType, RmuxError, SessionName, Target, WindowTarget};

use crate::cli::ExitFailure;
use crate::cli_args::{ShowOptionsArgs, ShowOptionsCommandKind, TargetSpec};

use super::super::super::{
    resolve_current_pane_target, resolve_current_session_target, resolve_target_spec,
    resolve_window_target_or_current,
};

/// Picks the option scope a `show-options` run reads, from its flags, target and option name.
pub(in crate::cli::config_commands) fn resolve_show_options_scope(
    command: ShowOptionsCommandKind,
    args: &ShowOptionsArgs,
) -> Result<ShowOptionsScope, ExitFailure> {
    let force_window = matches!(command, ShowOptionsCommandKind::ShowWindowOptions);
    let hook = args.name.as_deref().and_then(show_options_hook_name);
    if args.server {
        if let Some(name) = args.name.as_deref() {
            if hook.is_none()
                && !option_supports_show_scope(name, &OptionScopeSelector::ServerGlobal)
            {
                return show_named_scope_fallback(args.target.as_ref(), name);
            }
        }
        return Ok(OptionScopeSelector::ServerGlobal.into());
    }

    match (args.window || force_window, args.pane, args.target.as_ref()) {
        (true, false, _) if args.global => Ok(OptionScopeSelector::WindowGlobal.into()),
        (true, false, Some(target)) => {
            if let Some(name) = args.name.as_deref() {
                if hook.is_none()
                    && !option_supports_show_scope(name, &OptionScopeSelector::WindowGlobal)
                {
                    return show_options_scope_for_target(target, Some(name));
                }
            }
            Ok(ShowOptionsScope::Unresolved {
                target: target.clone(),
                kind: UnresolvedShowOptionsScope::Window,
            })
        }
        (true, false, None) => {
            if let Some(name) = args.name.as_deref() {
                if hook.is_none()
                    && !option_supports_show_scope(name, &OptionScopeSelector::WindowGlobal)
                {
                    return show_named_scope_fallback(None, name);
                }
            }
            Ok(ShowOptionsScope::CurrentWindow)
        }
        (false, true, _) if args.global && hook.is_some() => {
            Ok(show_pane_scope(args.target.as_ref()))
        }
        (false, true, _) if args.global => show_global_pane_options_scope(args),
        (false, true, Some(target)) => {
            if let Some(name) = args.name.as_deref() {
                if hook.is_none() && !option_supports_show_scope(name, &dummy_pane_scope()) {
                    return show_options_scope_for_target(target, Some(name));
                }
            }
            Ok(ShowOptionsScope::Unresolved {
                target: target.clone(),
                kind: UnresolvedShowOptionsScope::Pane,
            })
        }
        (false, true, None) => {
            if let Some(name) = args.name.as_deref() {
                if hook.is_none() && !option_supports_show_scope(name, &dummy_pane_scope()) {
                    return show_named_scope_fallback(None, name);
                }
            }
            Ok(ShowOptionsScope::CurrentPane)
        }
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
            Some(hook) => Ok(ShowOptionsScope::Unresolved {
                target: target.clone(),
                kind: hook_scope_kind(hook),
            }),
            None => show_options_scope_for_target(target, args.name.as_deref()),
        },
        (false, false, None) if force_window => Ok(ShowOptionsScope::CurrentWindow),
        (false, false, None) => Ok(match hook {
            Some(hook) => current_hook_scope(hook),
            None => ShowOptionsScope::CurrentSession,
        }),
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

impl ShowOptionsScope {
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
    let pane_scope = dummy_pane_scope();
    if let Some(name) = args.name.as_deref() {
        match rmux_core::resolve_option_name(name) {
            Ok(query) if query.is_user() || query.supports_scope(&pane_scope) => {
                return Ok(show_pane_scope(args.target.as_ref()));
            }
            Ok(_) => {
                return Ok(rmux_core::default_global_scope_for_option_name(name)
                    .map_err(option_lookup_exit_failure)?
                    .into());
            }
            Err(error) => return Err(option_lookup_exit_failure(error)),
        }
    }

    Ok(show_pane_scope(args.target.as_ref()))
}

/// Yields the given target's pane scope, or the client's current pane when no target was named.
fn show_pane_scope(target: Option<&TargetSpec>) -> ShowOptionsScope {
    match target {
        Some(target) => ShowOptionsScope::Unresolved {
            target: target.clone(),
            kind: UnresolvedShowOptionsScope::Pane,
        },
        None => ShowOptionsScope::CurrentPane,
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
    let target = resolve_target_spec(connection, target, target_type, false, false)?;
    match (kind, target) {
        (UnresolvedShowOptionsScope::Pane, Target::Pane(target)) => {
            Ok(OptionScopeSelector::Pane(target))
        }
        (UnresolvedShowOptionsScope::Pane, _) => Err(ExitFailure::new(
            1,
            "show-options -p requires a pane target",
        )),
        (UnresolvedShowOptionsScope::Session, Target::Session(session_name)) => {
            Ok(OptionScopeSelector::Session(session_name))
        }
        (UnresolvedShowOptionsScope::Session, Target::Window(target)) => {
            Ok(OptionScopeSelector::Session(target.session_name().clone()))
        }
        (UnresolvedShowOptionsScope::Session, Target::Pane(target)) => {
            Ok(OptionScopeSelector::Session(target.session_name().clone()))
        }
        (UnresolvedShowOptionsScope::Window, Target::Session(session_name)) => {
            Ok(OptionScopeSelector::Window(WindowTarget::new(session_name)))
        }
        (UnresolvedShowOptionsScope::Window, Target::Window(target)) => {
            Ok(OptionScopeSelector::Window(target))
        }
        (UnresolvedShowOptionsScope::Window, Target::Pane(target)) => {
            Ok(OptionScopeSelector::Window(WindowTarget::with_window(
                target.session_name().clone(),
                target.window_index(),
            )))
        }
    }
}

/// Derives the deferred scope kind for a target from the named option's default global scope.
fn show_options_scope_for_target(
    target: &TargetSpec,
    name: Option<&str>,
) -> Result<ShowOptionsScope, ExitFailure> {
    let Some(name) = name else {
        return Ok(ShowOptionsScope::Unresolved {
            target: target.clone(),
            kind: UnresolvedShowOptionsScope::Session,
        });
    };

    match rmux_core::default_global_scope_for_option_name(name)
        .map_err(option_lookup_exit_failure)?
    {
        OptionScopeSelector::ServerGlobal => Ok(OptionScopeSelector::ServerGlobal.into()),
        OptionScopeSelector::WindowGlobal | OptionScopeSelector::Window(_) => {
            Ok(ShowOptionsScope::Unresolved {
                target: target.clone(),
                kind: UnresolvedShowOptionsScope::Window,
            })
        }
        OptionScopeSelector::Pane(_) => Ok(ShowOptionsScope::Unresolved {
            target: target.clone(),
            kind: UnresolvedShowOptionsScope::Pane,
        }),
        OptionScopeSelector::SessionGlobal | OptionScopeSelector::Session(_) => {
            Ok(ShowOptionsScope::Unresolved {
                target: target.clone(),
                kind: UnresolvedShowOptionsScope::Session,
            })
        }
    }
}

/// Falls back to the named option's default scope when the requested scope does not support it.
fn show_named_scope_fallback(
    target: Option<&TargetSpec>,
    name: &str,
) -> Result<ShowOptionsScope, ExitFailure> {
    if let Some(target) = target {
        return show_options_scope_for_target(target, Some(name));
    }

    match rmux_core::default_global_scope_for_option_name(name)
        .map_err(option_lookup_exit_failure)?
    {
        OptionScopeSelector::ServerGlobal => Ok(OptionScopeSelector::ServerGlobal.into()),
        OptionScopeSelector::WindowGlobal | OptionScopeSelector::Window(_) => {
            Ok(ShowOptionsScope::CurrentWindow)
        }
        OptionScopeSelector::Pane(_) => Ok(ShowOptionsScope::CurrentPane),
        OptionScopeSelector::SessionGlobal | OptionScopeSelector::Session(_) => {
            Ok(ShowOptionsScope::CurrentSession)
        }
    }
}

/// Reports whether the named option may be read at `scope`, treating unknown names as unsupported.
fn option_supports_show_scope(name: &str, scope: &OptionScopeSelector) -> bool {
    rmux_core::resolve_option_name(name).is_ok_and(|query| query.supports_scope(scope))
}

/// Builds a throwaway pane scope used only to test whether an option is pane scoped.
#[allow(
    clippy::expect_used,
    reason = "a fixed literal session name is valid by construction"
)]
fn dummy_pane_scope() -> OptionScopeSelector {
    OptionScopeSelector::Pane(PaneTarget::with_window(
        SessionName::new("show-scope").expect("valid session name"),
        0,
        0,
    ))
}

/// Parses an option name as a hook name, tolerating a trailing `[index]` array subscript.
fn show_options_hook_name(value: &str) -> Option<rmux_proto::HookName> {
    let name = match value.rsplit_once('[') {
        Some((name, index)) if index.strip_suffix(']')?.parse::<u32>().is_ok() => name,
        Some(_) => return None,
        None => value,
    };
    rmux_proto::HookName::from_str(name)
}

/// Maps a hook to the global scope its options live in, either session or window global.
const fn global_hook_option_scope(hook: rmux_proto::HookName) -> OptionScopeSelector {
    match rmux_core::hook_global_root(hook) {
        rmux_core::HookGlobalRoot::Session => OptionScopeSelector::SessionGlobal,
        rmux_core::HookGlobalRoot::Window => OptionScopeSelector::WindowGlobal,
    }
}

/// Reports whether a hook naturally resolves against a session, window or pane target.
#[allow(
    clippy::expect_used,
    reason = "a fixed literal session name is valid by construction"
)]
fn hook_scope_kind(hook: rmux_proto::HookName) -> UnresolvedShowOptionsScope {
    let target = Target::Pane(PaneTarget::with_window(
        SessionName::new("show-hook-scope").expect("valid session name"),
        0,
        0,
    ));
    match rmux_core::hook_natural_scope_for_target(hook, target) {
        rmux_proto::ScopeSelector::Session(_) => UnresolvedShowOptionsScope::Session,
        rmux_proto::ScopeSelector::Window(_) => UnresolvedShowOptionsScope::Window,
        rmux_proto::ScopeSelector::Pane(_) => UnresolvedShowOptionsScope::Pane,
        rmux_proto::ScopeSelector::Global => unreachable!("natural hook scope is local"),
    }
}

/// Maps a hook to the client's matching current session, window or pane scope.
fn current_hook_scope(hook: rmux_proto::HookName) -> ShowOptionsScope {
    match hook_scope_kind(hook) {
        UnresolvedShowOptionsScope::Session => ShowOptionsScope::CurrentSession,
        UnresolvedShowOptionsScope::Window => ShowOptionsScope::CurrentWindow,
        UnresolvedShowOptionsScope::Pane => ShowOptionsScope::CurrentPane,
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
