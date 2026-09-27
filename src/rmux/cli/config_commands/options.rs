use rmux_client::Connection;
use rmux_proto::types::OptionScopeSelector;
use rmux_proto::{PaneTarget, ResolveTargetType, SessionName, SetOptionMode, Target, WindowTarget};

/// Scope resolution for the `show-options` family of commands.
#[path = "options/show_scope.rs"]
mod show_scope;

use crate::cli::ExitFailure;
use crate::cli::target_resolution::{target_session, target_window};
use crate::cli_args::{SetOptionArgs, SetOptionCommandKind, TargetSpec};
use crate::cli_response::tmux_cli_error_message;

use super::super::{
    resolve_current_pane_target, resolve_current_session_target, resolve_target_spec,
    resolve_window_target_or_current,
};
pub(super) use show_scope::resolve_show_options_scope;
#[cfg(test)]
pub(super) use show_scope::{ShowOptionsScope, UnresolvedShowOptionsScope};

/// Validates `set-option` arguments and resolves them into a request the daemon can execute.
pub(super) fn resolve_set_option_args(
    connection: &mut Connection,
    command: SetOptionCommandKind,
    args: SetOptionArgs,
) -> Result<ResolvedSetOptionCommand, ExitFailure> {
    resolve_set_option_command(
        command,
        args,
        &mut ConnectionSetOptionTargetResolver { connection },
    )
}

/// Resolves `set-option` arguments in tests, where targets must already be exact.
#[cfg(test)]
pub(super) fn resolve_set_option_args_with_exact_targets(
    command: SetOptionCommandKind,
    args: SetOptionArgs,
) -> Result<ResolvedSetOptionCommand, ExitFailure> {
    resolve_set_option_command(command, args, &mut ExactSetOptionTargetResolver)
}

/// Validates the option name, then resolves the mutated scope and any `-F` format target.
fn resolve_set_option_command(
    command: SetOptionCommandKind,
    args: SetOptionArgs,
    resolver: &mut impl SetOptionTargetResolver,
) -> Result<ResolvedSetOptionCommand, ExitFailure> {
    validate_set_option_name(&args.option)?;
    let scope = resolve_set_option_scope(&SetOptionScopeRequest::new(command, &args), resolver)?;
    let format_target = if args.format {
        Some(resolver.pane_or_current(args.target.as_ref(), command.command_name())?)
    } else {
        None
    };
    build_resolved_set_option_command(command, args, scope, format_target)
}

/// Checks the option mutation against core rules and packages it as a request, or a no-op.
fn build_resolved_set_option_command(
    command: SetOptionCommandKind,
    args: SetOptionArgs,
    scope: ResolvedSetOptionScope,
    format_target: Option<Target>,
) -> Result<ResolvedSetOptionCommand, ExitFailure> {
    let Some(scope) = scope.into_scope() else {
        return Ok(ResolvedSetOptionCommand::NoOp);
    };

    let mode = if args.append {
        SetOptionMode::Append
    } else {
        SetOptionMode::Replace
    };
    let unset = args.unset || args.unset_pane_overrides;

    if !args.format {
        rmux_core::validate_option_name_mutation(
            &args.option,
            &scope,
            mode,
            args.value.as_deref(),
            unset,
        )
        .map_err(|error| {
            ExitFailure::new(1, tmux_cli_error_message(command.command_name(), &error))
        })?;
    }

    Ok(ResolvedSetOptionCommand::Request(ResolvedSetOptionArgs {
        scope,
        option: args.option,
        value: args.value,
        mode,
        only_if_unset: args.only_if_unset,
        unset,
        unset_pane_overrides: args.unset_pane_overrides,
        format: args.format,
        format_target,
    }))
}

/// Outcome of resolving a `set-option` invocation: a request to send, or nothing to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ResolvedSetOptionCommand {
    Request(ResolvedSetOptionArgs),
    NoOp,
}

/// Fully resolved `set-option` request: scope, option, value and mutation flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedSetOptionArgs {
    pub(super) scope: OptionScopeSelector,
    pub(super) option: String,
    pub(super) value: Option<String>,
    pub(super) mode: SetOptionMode,
    pub(super) only_if_unset: bool,
    pub(super) unset: bool,
    pub(super) unset_pane_overrides: bool,
    pub(super) format: bool,
    pub(super) format_target: Option<Target>,
}

/// Scope a `set-option` applies to, or `NoOp` when the invocation silently does nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedSetOptionScope {
    Scope(OptionScopeSelector),
    NoOp,
}

impl ResolvedSetOptionScope {
    /// Yields the selector, or `None` for the silently ignored invocation.
    fn into_scope(self) -> Option<OptionScopeSelector> {
        match self {
            Self::Scope(scope) => Some(scope),
            Self::NoOp => None,
        }
    }
}

impl From<OptionScopeSelector> for ResolvedSetOptionScope {
    /// Treats any concrete selector as a scope that will be acted on.
    fn from(scope: OptionScopeSelector) -> Self {
        Self::Scope(scope)
    }
}

/// Rejects an unknown or malformed option name with tmux's `invalid option` wording.
fn validate_set_option_name(name: &str) -> Result<(), ExitFailure> {
    match rmux_core::resolve_option_name(name) {
        Ok(_) => Ok(()),
        Err(rmux_proto::RmuxError::Server(message))
            if message.starts_with("unknown option: ")
                || message.starts_with("invalid option: ") =>
        {
            Err(ExitFailure::new(1, format!("invalid option: {name}")))
        }
        Err(error) => Err(ExitFailure::new(1, error.to_string())),
    }
}

/// Borrowed view of the scope-selecting flags and target of one `set-option` invocation.
struct SetOptionScopeRequest<'a> {
    command: SetOptionCommandKind,
    option: &'a str,
    global: bool,
    server: bool,
    window: bool,
    pane: bool,
    target: Option<&'a TargetSpec>,
}

impl<'a> SetOptionScopeRequest<'a> {
    /// Borrows the scope-selecting flags out of parsed `set-option` arguments.
    fn new(command: SetOptionCommandKind, args: &'a SetOptionArgs) -> Self {
        Self {
            command,
            option: &args.option,
            global: args.global,
            server: args.server,
            window: args.window,
            pane: args.pane,
            target: args.target.as_ref(),
        }
    }
}

/// Applies tmux's flag, target and option-kind precedence to pick the scope to mutate.
fn resolve_set_option_scope(
    request: &SetOptionScopeRequest<'_>,
    resolver: &mut impl SetOptionTargetResolver,
) -> Result<ResolvedSetOptionScope, ExitFailure> {
    let command_name = request.command.command_name();
    let force_window = matches!(request.command, SetOptionCommandKind::SetWindowOption);
    let is_user = request
        .option
        .split('[')
        .next()
        .is_some_and(|base| base.starts_with('@'));
    let supports_scope =
        |scope: &OptionScopeSelector| option_name_supports_scope(request.option, scope);
    let window = request.window || force_window;

    if request.pane
        && !request.server
        && !window
        && (is_user || option_supports_pane_scope(request.option))
    {
        return Ok(resolve_pane_scope(request, resolver)?.into());
    }

    if request.global && !is_user {
        let scope = default_global_scope(request.option)?;
        if supports_scope(&scope) {
            return Ok(scope.into());
        }
        return Err(ExitFailure::new(
            1,
            "global scope is not supported for this option",
        ));
    }

    if !request.global && !is_user && (request.server || request.pane || window) {
        let scope = resolve_natural_known_set_option_scope(
            request.option,
            request.target,
            command_name,
            resolver,
        )?;
        return Ok(scope.into());
    }

    if request.server {
        let scope = OptionScopeSelector::ServerGlobal;
        if is_user || supports_scope(&scope) {
            return Ok(scope.into());
        }
        if !request.global {
            return Ok(ResolvedSetOptionScope::NoOp);
        }
    }

    if request.global && request.pane && !force_window {
        return Ok(OptionScopeSelector::WindowGlobal.into());
    }

    if request.pane {
        return Ok(resolve_pane_scope(request, resolver)?.into());
    }

    if window {
        if request.global {
            return Ok(OptionScopeSelector::WindowGlobal.into());
        }
        let window = resolver.window_or_current(request.target, command_name)?;
        return Ok(OptionScopeSelector::Window(window).into());
    }

    if request.global {
        let scope = default_global_scope(request.option)?;
        if !is_user && !supports_scope(&scope) {
            return Err(ExitFailure::new(
                1,
                "global scope is not supported for this option",
            ));
        }
        return Ok(scope.into());
    }

    let Some(target_spec) = request.target else {
        return resolve_implicit_set_option_scope(request.option, resolver);
    };

    if is_user {
        let session_name = resolver.session_or_current(Some(target_spec), command_name)?;
        return Ok(OptionScopeSelector::Session(session_name).into());
    }

    let global_scope = default_global_scope(request.option)?;
    if matches!(global_scope, OptionScopeSelector::ServerGlobal) && supports_scope(&global_scope) {
        return Ok(global_scope.into());
    }

    // The narrowest scope the option supports wins: the pane (for a pane target), then the
    // target's window, falling back to its session.
    let target = resolver.resolve_target(target_spec, target_type_for_scope(&global_scope))?;
    let pane = match &target {
        Target::Pane(pane) => Some(OptionScopeSelector::Pane(pane.clone())),
        Target::Session(_) | Target::Window(_) => None,
    };
    let session = OptionScopeSelector::Session(target.session_name().clone());
    let scope = pane
        .into_iter()
        .chain([OptionScopeSelector::Window(target_window(target))])
        .find(|scope| option_name_supports_scope(request.option, scope))
        .unwrap_or(session);
    if !supports_scope(&scope) {
        return Err(ExitFailure::new(
            1,
            "target scope is not supported for this option",
        ));
    }
    Ok(scope.into())
}

/// Resolves the pane a `-p` mutation applies to, failing when the target names no pane.
fn resolve_pane_scope(
    request: &SetOptionScopeRequest<'_>,
    resolver: &mut impl SetOptionTargetResolver,
) -> Result<OptionScopeSelector, ExitFailure> {
    let command_name = request.command.command_name();
    match resolver.pane_or_current(request.target, command_name)? {
        Target::Pane(target) => Ok(OptionScopeSelector::Pane(target)),
        Target::Session(_) | Target::Window(_) => Err(ExitFailure::new(
            1,
            format!("{command_name} -p requires a pane target"),
        )),
    }
}

/// Picks the scope an option naturally lives in, filling globals from the target or current focus.
fn resolve_natural_known_set_option_scope(
    option: &str,
    target: Option<&TargetSpec>,
    command_name: &str,
    resolver: &mut impl SetOptionTargetResolver,
) -> Result<OptionScopeSelector, ExitFailure> {
    Ok(match default_global_scope(option)? {
        OptionScopeSelector::SessionGlobal => {
            OptionScopeSelector::Session(resolver.session_or_current(target, command_name)?)
        }
        OptionScopeSelector::WindowGlobal => {
            OptionScopeSelector::Window(resolver.window_or_current(target, command_name)?)
        }
        scope => scope,
    })
}

/// The global scope an option lives in by default, reporting an unknown name as a failure.
fn default_global_scope(option: &str) -> Result<OptionScopeSelector, ExitFailure> {
    rmux_core::default_global_scope_for_option_name(option)
        .map_err(|error| ExitFailure::new(1, error.to_string()))
}

/// Reports whether the named option can be set at pane scope.
fn option_supports_pane_scope(option: &str) -> bool {
    option_name_supports_scope(option, &OptionScopeSelector::Pane(dummy_pane_target()))
}

/// Builds a throwaway pane used only to probe an option's or hook's supported scopes.
#[allow(
    clippy::expect_used,
    reason = "a fixed literal session name is valid by construction"
)]
fn dummy_pane_target() -> PaneTarget {
    PaneTarget::with_window(
        SessionName::new("set-option").expect("valid session name"),
        0,
        0,
    )
}

/// Reports whether the named option exists and accepts `scope`; unknown names are `false`.
fn option_name_supports_scope(option: &str, scope: &OptionScopeSelector) -> bool {
    rmux_core::resolve_option_name(option).is_ok_and(|query| query.supports_scope(scope))
}

/// Maps an option's default global scope to the target kind a spec must resolve to.
const fn target_type_for_scope(scope: &OptionScopeSelector) -> ResolveTargetType {
    match scope {
        OptionScopeSelector::WindowGlobal | OptionScopeSelector::Window(_) => {
            ResolveTargetType::Window
        }
        OptionScopeSelector::Pane(_) => ResolveTargetType::Pane,
        OptionScopeSelector::ServerGlobal
        | OptionScopeSelector::SessionGlobal
        | OptionScopeSelector::Session(_) => ResolveTargetType::Session,
    }
}

/// Resolves the scope for an invocation with no flags and no target, using the current focus.
fn resolve_implicit_set_option_scope(
    option: &str,
    resolver: &mut impl SetOptionTargetResolver,
) -> Result<ResolvedSetOptionScope, ExitFailure> {
    Ok(match default_global_scope(option)? {
        OptionScopeSelector::WindowGlobal => {
            OptionScopeSelector::Window(resolver.current_window("set-option")?)
        }
        OptionScopeSelector::SessionGlobal => {
            OptionScopeSelector::Session(resolver.current_session("set-option")?)
        }
        scope => scope,
    }
    .into())
}

/// Supplies the target lookups scope resolution needs, so it can be driven without a daemon.
trait SetOptionTargetResolver {
    /// Resolves a user-written target spec to a concrete target of the requested kind.
    fn resolve_target(
        &mut self,
        target: &TargetSpec,
        target_type: ResolveTargetType,
    ) -> Result<Target, ExitFailure>;

    /// Names the session the command is being run from.
    fn current_session(&mut self, command_name: &str) -> Result<SessionName, ExitFailure>;

    /// Names the pane the command is being run from.
    fn current_pane(&mut self, command_name: &str) -> Result<PaneTarget, ExitFailure>;

    /// Names the window the command is being run from.
    fn current_window(&mut self, command_name: &str) -> Result<WindowTarget, ExitFailure>;

    /// Resolves `target` as a pane-typed spec, or names the current pane when none was given.
    fn pane_or_current(
        &mut self,
        target: Option<&TargetSpec>,
        command_name: &str,
    ) -> Result<Target, ExitFailure> {
        match target {
            Some(target) => self.resolve_target(target, ResolveTargetType::Pane),
            None => self.current_pane(command_name).map(Target::Pane),
        }
    }

    /// The window `target` names, or the current window when none was given.
    fn window_or_current(
        &mut self,
        target: Option<&TargetSpec>,
        command_name: &str,
    ) -> Result<WindowTarget, ExitFailure> {
        match target {
            Some(target) => self
                .resolve_target(target, ResolveTargetType::Window)
                .map(target_window),
            None => self.current_window(command_name),
        }
    }

    /// The session `target` belongs to, or the current session when none was given.
    fn session_or_current(
        &mut self,
        target: Option<&TargetSpec>,
        command_name: &str,
    ) -> Result<SessionName, ExitFailure> {
        match target {
            Some(target) => self
                .resolve_target(target, ResolveTargetType::Session)
                .map(target_session),
            None => self.current_session(command_name),
        }
    }
}

/// Resolver that answers every lookup by asking the daemon over a client connection.
struct ConnectionSetOptionTargetResolver<'a> {
    connection: &'a mut Connection,
}

impl SetOptionTargetResolver for ConnectionSetOptionTargetResolver<'_> {
    /// Asks the daemon to resolve the spec, without creating or matching unattached targets.
    fn resolve_target(
        &mut self,
        target: &TargetSpec,
        target_type: ResolveTargetType,
    ) -> Result<Target, ExitFailure> {
        resolve_target_spec(self.connection, target, target_type, false, false)
    }

    /// Asks the daemon which session the client is attached to.
    fn current_session(&mut self, _command_name: &str) -> Result<SessionName, ExitFailure> {
        resolve_current_session_target(self.connection)
    }

    /// Asks the daemon which pane the client is attached to.
    fn current_pane(&mut self, command_name: &str) -> Result<PaneTarget, ExitFailure> {
        resolve_current_pane_target(self.connection, command_name)
    }

    /// Asks the daemon for the client's current window.
    fn current_window(&mut self, command_name: &str) -> Result<WindowTarget, ExitFailure> {
        resolve_window_target_or_current(self.connection, None, command_name)
    }
}

/// Test resolver that accepts only already-exact targets and has no current session or pane.
#[cfg(test)]
struct ExactSetOptionTargetResolver;

#[cfg(test)]
impl SetOptionTargetResolver for ExactSetOptionTargetResolver {
    /// Accepts a spec only when it already carries an exact target; anything else fails.
    fn resolve_target(
        &mut self,
        target: &TargetSpec,
        _target_type: ResolveTargetType,
    ) -> Result<Target, ExitFailure> {
        target
            .exact()
            .cloned()
            .ok_or_else(|| ExitFailure::new(1, "test target requires daemon resolution"))
    }

    /// Fails: the test path has no attached client to take a session from.
    fn current_session(&mut self, _command_name: &str) -> Result<SessionName, ExitFailure> {
        Err(ExitFailure::new(
            1,
            "test path does not provide a current session",
        ))
    }

    /// Fails: the test path has no attached client to take a pane from.
    fn current_pane(&mut self, _command_name: &str) -> Result<PaneTarget, ExitFailure> {
        Err(ExitFailure::new(
            1,
            "test path does not provide a current pane",
        ))
    }

    /// Fails: the test path has no attached client to take a window from.
    fn current_window(&mut self, _command_name: &str) -> Result<WindowTarget, ExitFailure> {
        Err(ExitFailure::new(
            1,
            "test path does not provide a current window",
        ))
    }
}
