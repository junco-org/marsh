use std::path::PathBuf;

use rmux_proto::Target;

use super::super::source_files::{ParsedSourceFileCommand, SourceSyntax};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigLoadMode {
    Execute,
    ParseOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigReadPolicy {
    Strict,
    ImportCompat,
}

/// Which trust domain asked for a configuration load.
///
/// The daemon's first configuration read happens before it has served anyone, from paths this
/// process chose for itself; the cutover plan lists startup configuration discovery among the
/// trusted runtime mechanics, and opening a shell to read the file that configures the shell is
/// the recursion that listing avoids. Every later `source-file` — explicit, queued or nested — is
/// work a *request* asked for, and reads its files through the managed `__rmux_io source` helper
/// so the bytes it is about to dispatch as commands are attributable, cancellable and refusable.
///
/// Carrying the distinction here is what stops a runtime caller from selecting the bootstrap
/// reader: [`ConfigLoadRequest::from_source_command`] is the only constructor any runtime path
/// uses and it has no parameter for this, while [`ConfigLoadRequest::for_startup_config`] is
/// reached from exactly one private method on the startup path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceReadOrigin {
    /// Daemon startup configuration discovery, before the first connection is served.
    Bootstrap,
    /// A `source-file` a request asked for, at any nesting depth.
    Runtime,
}

pub(crate) struct ConfigLoadRequest<'a> {
    pub(super) command: &'a ParsedSourceFileCommand,
    pub(super) syntax: SourceSyntax,
    pub(super) mode: ConfigLoadMode,
    pub(super) read_policy: ConfigReadPolicy,
    pub(super) quiet: bool,
    pub(super) verbose: bool,
    pub(super) caller_cwd: Option<PathBuf>,
    pub(super) current_file: Option<String>,
    pub(super) current_target: Option<Target>,
    pub(super) explicit_target: bool,
    pub(super) implicit_target_refresh: bool,
    pub(super) origin: SourceReadOrigin,
    pub(super) depth: usize,
}

impl<'a> ConfigLoadRequest<'a> {
    /// A load a request asked for: explicit, queued or nested `source-file`.
    ///
    /// Hard-codes [`SourceReadOrigin::Runtime`]. There is deliberately no parameter for the
    /// origin, so no runtime caller — present or future — can route its file reads around the
    /// managed helper by passing a different one.
    pub(crate) fn from_source_command(
        command: &'a ParsedSourceFileCommand,
        explicit_target: bool,
        implicit_target_refresh: bool,
        depth: usize,
    ) -> Self {
        Self::with_origin(
            command,
            explicit_target,
            implicit_target_refresh,
            depth,
            SourceReadOrigin::Runtime,
        )
    }

    /// The daemon's own startup configuration load.
    ///
    /// The single constructor that yields [`SourceReadOrigin::Bootstrap`], and therefore the
    /// single door to the direct reader. Its one caller is
    /// `RequestHandler::load_bootstrap_source_file_command`, which `load_startup_config_with_guard`
    /// calls before this daemon serves its first connection.
    ///
    /// Startup never addresses a pane, so the load carries no explicit target and refreshes the
    /// implicit one, exactly as the previous startup path did.
    pub(crate) fn for_startup_config(command: &'a ParsedSourceFileCommand, depth: usize) -> Self {
        Self::with_origin(command, false, true, depth, SourceReadOrigin::Bootstrap)
    }

    fn with_origin(
        command: &'a ParsedSourceFileCommand,
        explicit_target: bool,
        implicit_target_refresh: bool,
        depth: usize,
        origin: SourceReadOrigin,
    ) -> Self {
        Self {
            command,
            syntax: command.syntax,
            mode: if command.parse_only {
                ConfigLoadMode::ParseOnly
            } else {
                ConfigLoadMode::Execute
            },
            read_policy: match command.syntax {
                SourceSyntax::Rmux | SourceSyntax::Canonical => ConfigReadPolicy::Strict,
                SourceSyntax::TmuxCompat => ConfigReadPolicy::ImportCompat,
            },
            quiet: command.quiet,
            verbose: command.verbose,
            caller_cwd: command.caller_cwd.clone(),
            current_file: command.current_file.clone(),
            current_target: command.target.clone().map(Target::Pane),
            explicit_target,
            implicit_target_refresh,
            origin,
            depth,
        }
    }

    pub(crate) fn is_import_compat(&self) -> bool {
        self.read_policy == ConfigReadPolicy::ImportCompat
    }

    pub(crate) fn assert_boundary_invariants(&self) {
        debug_assert_eq!(self.syntax, self.command.syntax);
        debug_assert_eq!(self.quiet, self.command.quiet);
        debug_assert_eq!(self.verbose, self.command.verbose);
        debug_assert_eq!(self.caller_cwd, self.command.caller_cwd);
        debug_assert_eq!(self.current_file, self.command.current_file);
        debug_assert_eq!(
            self.current_target,
            self.command.target.clone().map(Target::Pane)
        );
        debug_assert_eq!(
            self.mode == ConfigLoadMode::ParseOnly,
            self.command.parse_only
        );
        debug_assert_eq!(
            self.read_policy == ConfigReadPolicy::ImportCompat,
            self.syntax == SourceSyntax::TmuxCompat
        );
        debug_assert!(
            !(self.explicit_target && self.implicit_target_refresh),
            "explicit target and implicit target refresh are mutually exclusive"
        );
        // Not a gate — the origin is already decided by which constructor ran — but a statement of
        // what a startup load looks like, so a future change that started routing request-shaped
        // work through `for_startup_config` trips here rather than quietly reading a client's
        // path outside the managed helper.
        debug_assert!(
            self.origin == SourceReadOrigin::Runtime
                || (self.depth == 1
                    && !self.command.expand_paths
                    && self.command.stdin.is_none()
                    && self.command.target.is_none()),
            "a bootstrap config load carries no target, no client stdin and no format expansion"
        );
        let _ = self.is_import_compat();
    }
}
