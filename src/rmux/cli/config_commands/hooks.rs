use std::path::Path;

use rmux_client::Connection;
use rmux_proto::{
    HookLifecycle, HookName, ResolveTargetType, ScopeSelector, SessionName, Target, WindowTarget,
};

use crate::cli::target_resolution::{
    resolve_active_pane_index, resolve_active_window_index, target_session, target_window,
};
use crate::cli::{
    CommandTarget, ExitFailure, resolve_current_pane_target, resolve_current_session_target,
    resolve_target_spec, run_command_resolved, run_payload_command_resolved,
};
use crate::cli_args::{SetHookArgs, ShowHooksArgs, TargetSpec};

/// Runs the `set-hook` CLI command, registering or unsetting a hook in the requested scope.
pub(crate) fn run_set_hook(args: SetHookArgs, socket_path: &Path) -> Result<i32, ExitFailure> {
    let SetHookArgs {
        append,
        global,
        pane,
        run_immediately,
        target,
        unset,
        window,
        hook,
        command,
    } = args;
    let scope = resolve_hook_scope(ResolveHookScopeInput {
        command: "set-hook",
        global,
        window,
        pane,
        target,
        hook: Some(hook.hook),
        run_immediately,
    })?;

    run_command_resolved(socket_path, "set-hook", move |connection| {
        let scope = scope.resolve(connection, "set-hook")?;
        validate_hook_registration(hook.hook, &scope)?;
        connection
            .set_hook_mutation(
                scope,
                hook.hook,
                command,
                HookLifecycle::Persistent,
                append,
                unset,
                run_immediately,
                hook.index,
            )
            .map_err(ExitFailure::from)
    })
}

/// Runs the `show-hooks` CLI command, printing the hooks registered in the requested scope.
pub(crate) fn run_show_hooks(args: ShowHooksArgs, socket_path: &Path) -> Result<i32, ExitFailure> {
    let hook = args.hook;
    let scope = if args.global {
        reject_target("show-hooks", args.target.as_ref(), "-g")?;
        HookScope::Resolved(ScopeSelector::Global)
    } else {
        resolve_hook_scope(ResolveHookScopeInput {
            command: "show-hooks",
            global: false,
            window: args.window,
            pane: args.pane,
            target: args.target,
            hook,
            run_immediately: false,
        })?
    };

    run_payload_command_resolved(socket_path, "show-hooks", move |connection| {
        let scope = scope.resolve(connection, "show-hooks")?;
        if let Some(hook) = hook {
            rmux_core::validate_hook_scope(hook, &scope)
                .map_err(|error| ExitFailure::new(1, error.to_string()))?;
        }
        connection
            .show_hooks(scope, args.window, args.pane, hook)
            .map_err(ExitFailure::from)
    })
}

/// Flags and target of a hook command, gathered so scope resolution reads one argument.
struct ResolveHookScopeInput<'a> {
    command: &'a str,
    global: bool,
    window: bool,
    pane: bool,
    target: Option<TargetSpec>,
    hook: Option<HookName>,
    run_immediately: bool,
}

/// Maps hook command flags and target onto the scope the hook will be registered in.
fn resolve_hook_scope(input: ResolveHookScopeInput<'_>) -> Result<HookScope, ExitFailure> {
    let ResolveHookScopeInput {
        command,
        global,
        window,
        pane,
        target,
        hook,
        run_immediately,
    } = input;
    if run_immediately {
        return Ok(scope_or_current(
            target,
            HookTargetKind::Pane,
            HookScope::CurrentPane,
        ));
    }
    if window && pane {
        return Err(ExitFailure::new(
            1,
            format!("{command} does not support combining -w and -p"),
        ));
    }
    if global {
        return Ok(target.map_or(
            HookScope::Resolved(ScopeSelector::Global),
            HookScope::TargetCheckedGlobal,
        ));
    }

    Ok(match (window, pane, target) {
        (true, _, target) => {
            scope_or_current(target, HookTargetKind::Window, HookScope::CurrentWindow)
        }
        (false, true, target) => {
            scope_or_current(target, HookTargetKind::Pane, HookScope::CurrentPane)
        }
        (false, false, Some(target)) => HookScope::Unresolved {
            target,
            kind: HookTargetKind::Natural(hook),
        },
        (false, false, None) => hook.map_or(HookScope::CurrentSession, HookScope::CurrentNatural),
    })
}

/// Narrows to `target` as `kind` when one was given, else falls back to the `current` scope.
fn scope_or_current(
    target: Option<TargetSpec>,
    kind: HookTargetKind,
    current: HookScope,
) -> HookScope {
    target.map_or(current, |target| HookScope::Unresolved { target, kind })
}

/// Hook scope selected by the command line, possibly still needing server-side resolution.
#[derive(Debug, Clone)]
enum HookScope {
    Resolved(ScopeSelector),
    TargetCheckedGlobal(TargetSpec),
    CurrentSession,
    CurrentNatural(HookName),
    CurrentWindow,
    CurrentPane,
    Unresolved {
        target: TargetSpec,
        kind: HookTargetKind,
    },
}

/// How an explicit target narrows: to its window, its pane, or the hook's natural scope.
#[derive(Debug, Clone, Copy)]
enum HookTargetKind {
    Window,
    Pane,
    Natural(Option<HookName>),
}

impl HookScope {
    /// Turns this selection into a concrete scope selector, querying the server when needed.
    fn resolve(
        self,
        connection: &mut Connection,
        command: &str,
    ) -> Result<ScopeSelector, ExitFailure> {
        match self {
            Self::Resolved(scope) => Ok(scope),
            Self::TargetCheckedGlobal(target) => {
                let _ = resolve_target_spec(
                    connection,
                    &target,
                    ResolveTargetType::Pane,
                    false,
                    false,
                )?;
                Ok(ScopeSelector::Global)
            }
            Self::CurrentSession => {
                resolve_current_session_target(connection).map(ScopeSelector::Session)
            }
            Self::CurrentNatural(hook) => {
                let session_name = resolve_current_session_target(connection)?;
                resolve_natural_hook_scope_for_session_target(
                    connection,
                    command,
                    hook,
                    session_name,
                )
            }
            Self::CurrentWindow => {
                WindowTarget::resolve_fallback(connection, command).map(ScopeSelector::Window)
            }
            Self::CurrentPane => {
                resolve_current_pane_target(connection, command).map(ScopeSelector::Pane)
            }
            Self::Unresolved { target, kind } => {
                resolve_unresolved_hook_scope(connection, command, &target, kind)
            }
        }
    }
}

/// Resolves an explicit target spec into the scope selector implied by `kind`.
fn resolve_unresolved_hook_scope(
    connection: &mut Connection,
    command: &str,
    target: &TargetSpec,
    kind: HookTargetKind,
) -> Result<ScopeSelector, ExitFailure> {
    let target = resolve_target_spec(connection, target, ResolveTargetType::Pane, false, false)?;
    match (kind, target) {
        (HookTargetKind::Pane, Target::Pane(target)) => Ok(ScopeSelector::Pane(target)),
        (HookTargetKind::Pane, _) => Err(ExitFailure::new(
            1,
            format!("{command} -p requires a pane target"),
        )),
        (HookTargetKind::Window, target) => Ok(ScopeSelector::Window(target_window(target))),
        (HookTargetKind::Natural(Some(hook)), Target::Session(session_name)) => {
            resolve_natural_hook_scope_for_session_target(connection, command, hook, session_name)
        }
        (HookTargetKind::Natural(Some(hook)), target) => {
            Ok(rmux_core::hook_natural_scope_for_target(hook, target))
        }
        (HookTargetKind::Natural(None), target) => {
            Ok(ScopeSelector::Session(target_session(target)))
        }
    }
}

/// Picks the scope a hook naturally lives in when only a session was named.
fn resolve_natural_hook_scope_for_session_target(
    connection: &mut Connection,
    command: &str,
    hook: HookName,
    session_name: SessionName,
) -> Result<ScopeSelector, ExitFailure> {
    if matches!(
        rmux_core::hook_global_root(hook),
        rmux_core::HookGlobalRoot::Session
    ) {
        return Ok(ScopeSelector::Session(session_name));
    }
    let window_index = resolve_active_window_index(connection, &session_name, command)?;
    let pane_index = resolve_active_pane_index(connection, &session_name, window_index, command)?;
    Ok(rmux_core::hook_natural_scope_for_session_target(
        hook,
        session_name,
        window_index,
        pane_index,
    ))
}

/// Rejects a hook that may not be registered in `scope`, surfacing the core rule as a failure.
fn validate_hook_registration(hook: HookName, scope: &ScopeSelector) -> Result<(), ExitFailure> {
    rmux_core::validate_hook_registration(hook, scope)
        .map_err(|error| ExitFailure::new(1, error.to_string()))
}

/// Rejects a target argument that the given flag does not accept.
fn reject_target(
    command: &str,
    target: Option<&TargetSpec>,
    flag: &str,
) -> Result<(), ExitFailure> {
    if target.is_some() {
        Err(ExitFailure::new(
            1,
            format!("{command} {flag} does not accept a target"),
        ))
    } else {
        Ok(())
    }
}
