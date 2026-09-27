//! An ordinary persistent shell owning its complete four-stage transaction boundary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

pub use brush_core::env::ShellEnvironment;
pub use brush_core::openfiles::OpenFile;
pub use brush_core::{
    ExecutionParameters, ExecutionResult, ProfileLoadBehavior, RcLoadBehavior, ShellFd,
    ShellVariable, SourceInfo,
};
pub use brush_interactive::UIOptions;

mod access;
pub mod builtins;
mod error;
mod execution;
mod input;
mod policy;
mod publication;
mod session;
mod signal;
mod snapshot;

pub use error::{ShellError, ShellErrorKind};
pub use policy::Denial;
pub use rust_validator::Principal;
pub use signal::Signal;

use execution::{ManagedExtensions, MarshExecutor, Run};
use session::Session;
use snapshot::Snapshot;

/// Ordinary shell configuration. Execution machinery and storage choices are never public inputs.
pub struct ShellBuilder {
    options: brush_core::CreateOptions<ManagedExtensions>,
    environment: Option<ShellEnvironment>,
    pub(crate) backend: Option<Arc<dyn marsh_btrfs::Subvolumes>>,
}
impl Default for ShellBuilder {
    fn default() -> Self {
        Self {
            options: brush_core::CreateOptions {
                profile: ProfileLoadBehavior::Skip,
                rc: RcLoadBehavior::Skip,
                no_editing: true,
                builtins: brush_builtins::default_builtins(brush_builtins::BuiltinSet::BashMode),
                ..Default::default()
            },
            environment: None,
            backend: None,
        }
    }
}
impl ShellBuilder {
    /// Sets the logical initial directory; empty means the process working directory.
    #[must_use]
    pub fn working_dir(mut self, directory: PathBuf) -> Self {
        self.options.working_dir = Some(directory);
        self
    }
    /// Replaces the inherited shell environment.
    #[must_use]
    pub fn environment(mut self, environment: ShellEnvironment) -> Self {
        self.environment = Some(environment);
        self
    }
    /// Adds or replaces one initial variable.
    #[must_use]
    pub fn var(mut self, name: impl Into<String>, variable: ShellVariable) -> Self {
        self.options.vars.insert(name.into(), variable);
        self
    }
    /// Supplies ordinary shell descriptors.
    #[must_use]
    pub fn fds(mut self, fds: HashMap<ShellFd, OpenFile>) -> Self {
        self.options.fds = fds;
        self
    }
    /// Registers a builtin without exposing the private interpreter or observer.
    #[must_use]
    pub fn builtin(mut self, name: impl Into<String>, builtin: builtins::Registration) -> Self {
        self.options.builtins.insert(name.into(), builtin.0);
        self
    }
    /// Enables normal interactive shell semantics.
    #[must_use]
    pub const fn interactive(mut self, interactive: bool) -> Self {
        self.options.interactive = interactive;
        self
    }
    /// Selects the basic input editor.
    #[must_use]
    pub const fn no_editing(mut self, no_editing: bool) -> Self {
        self.options.no_editing = no_editing;
        self
    }
    /// Requests profile loading through the command boundary.
    #[must_use]
    pub const fn profile(mut self, profile: ProfileLoadBehavior) -> Self {
        self.options.profile = profile;
        self
    }
    /// Requests rc loading through the command boundary.
    #[must_use]
    pub fn rc(mut self, rc: RcLoadBehavior) -> Self {
        self.options.rc = rc;
        self
    }
    /// Enables an ordinary Brush option.
    #[must_use]
    pub fn enable_option(mut self, option: String) -> Self {
        self.options.enabled_options.push(option);
        self
    }
    /// Disables an ordinary Brush option.
    #[must_use]
    pub fn disable_option(mut self, option: String) -> Self {
        self.options.disabled_options.push(option);
        self
    }
    /// Sets `$0`.
    #[must_use]
    pub fn shell_name(mut self, name: String) -> Self {
        self.options.shell_name = Some(name);
        self
    }
    /// Sets the initial positional arguments.
    #[must_use]
    pub fn shell_args(mut self, args: Vec<String>) -> Self {
        self.options.shell_args = Some(args);
        self
    }
    /// Lets external commands lead their own terminal session.
    #[must_use]
    pub const fn external_cmd_leads_session(mut self, enabled: bool) -> Self {
        self.options.external_cmd_leads_session = enabled;
        self
    }

    /// Opens or shares the source automatically, then constructs the persistent managed shell.
    pub async fn build(mut self) -> Result<Shell, ShellError> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|error| ShellError::infrastructure(error.to_string()))?;
        let initial = self.options.working_dir.take().unwrap_or_default();
        let (session, cwd) = {
            let tracing = marsh_instrument::Tracing::shared();
            let scope = tracing.internal_scope()?;
            let _guard = scope.enter();
            Session::open(&initial, self.backend)?
        };
        let snapshot = Snapshot::new(session)?;
        let executor = MarshExecutor::new(&snapshot);
        let profile = self.options.profile;
        let rc = self.options.rc;
        let startup = !profile.skip() || !rc.skip();
        let mut builder = brush_core::Shell::builder_with_extensions::<ManagedExtensions>()
            .external_command_spawner(executor.clone())
            .execution_observer(executor)
            .working_dir(snapshot.physical(&cwd))
            .interactive(self.options.interactive)
            .no_editing(self.options.no_editing)
            .profile(ProfileLoadBehavior::Skip)
            .rc(RcLoadBehavior::Skip)
            .do_not_inherit_env(self.environment.is_some())
            .fds(self.options.fds)
            .external_cmd_leads_session(self.options.external_cmd_leads_session)
            .shell_name(self.options.shell_name.unwrap_or_else(|| "marsh".into()))
            .shell_args(self.options.shell_args.unwrap_or_default())
            .enable_options(self.options.enabled_options)
            .disable_options(self.options.disabled_options)
            .builtins(self.options.builtins);
        if let Some(environment) = self.environment {
            for (name, variable) in environment.iter() {
                builder = builder.var(name.clone(), variable.clone());
            }
        }
        for (name, variable) in self.options.vars {
            builder = builder.var(name, variable);
        }
        // Git identity is an instance property, never a reusable mux name or recovered principal.
        for (name, value) in [
            ("GIT_AUTHOR_NAME", format!("marsh-{}", snapshot.uid)),
            ("GIT_COMMITTER_NAME", format!("marsh-{}", snapshot.uid)),
            ("GIT_AUTHOR_EMAIL", format!("{}@marsh.local", snapshot.uid)),
            (
                "GIT_COMMITTER_EMAIL",
                format!("{}@marsh.local", snapshot.uid),
            ),
        ] {
            let mut variable = ShellVariable::new(value);
            variable.export();
            builder = builder.var(name, variable);
        }
        let scope = snapshot.session.tracing.internal_scope()?;
        let mut interpreter =
            Box::pin(marsh_instrument::Scoped::new(builder.build(), scope)).await?;
        for (name, registration) in builtins::managed() {
            interpreter.register_builtin(name, registration);
        }
        let id = snapshot.uid.clone();
        let source = snapshot.session.persistence.seed.clone();
        let observed = Observed {
            cwd,
            environment: ShellEnvironment::default(),
            status: 0,
        };
        let shell = Shell {
            id,
            source,
            shared: Arc::new(Shared {
                live: tokio::sync::Mutex::new(Some(Live {
                    interpreter,
                    snapshot,
                })),
                observed: Mutex::new(observed),
                active: Mutex::new(Weak::new()),
                runtime,
                close_failure: Mutex::new(None),
                busy: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                force: AtomicBool::new(false),
                finished: tokio::sync::Notify::new(),
            }),
        };
        if startup {
            shell.execute(Command::Startup(profile, rc)).await?;
        }
        Ok(shell)
    }
}

struct Live {
    interpreter: brush_core::Shell<ManagedExtensions>,
    snapshot: Arc<Snapshot>,
}
struct Observed {
    cwd: PathBuf,
    environment: ShellEnvironment,
    status: u8,
}
struct Shared {
    live: tokio::sync::Mutex<Option<Live>>,
    observed: Mutex<Observed>,
    close_failure: Mutex<Option<Arc<ShellError>>>,
    active: Mutex<Weak<Run>>,
    runtime: tokio::runtime::Handle,
    busy: AtomicBool,
    closed: AtomicBool,
    force: AtomicBool,
    finished: tokio::sync::Notify,
}

/// A persistent in-process shell. Each accepted operation owns all four private stages.
pub struct Shell {
    id: Principal,
    source: PathBuf,
    shared: Arc<Shared>,
}
impl Shell {
    /// Opens the source containing `initial_dir`, automatically recovering its durable authority.
    pub async fn new(initial_dir: &Path) -> Result<Self, ShellError> {
        Self::builder()
            .working_dir(initial_dir.to_path_buf())
            .build()
            .await
    }
    /// Configures a shell using only ordinary shell inputs.
    pub fn builder() -> ShellBuilder {
        ShellBuilder::default()
    }
    /// Executes one command exactly once. Stale work is reported, never replayed.
    pub async fn run(&self, line: &str) -> Result<ExecutionResult, ShellError> {
        self.execute(Command::Line(line.to_owned())).await
    }
    /// Executes a program string with ordinary Brush source and descriptor parameters.
    pub async fn run_string<S: Into<String>>(
        &self,
        command: S,
        source: &SourceInfo,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, ShellError> {
        self.execute(Command::String(
            command.into(),
            source.clone(),
            params.clone(),
        ))
        .await
    }
    /// Executes a script inside this shell's managed filesystem view.
    pub async fn run_script(
        &self,
        path: &Path,
        args: &[String],
    ) -> Result<ExecutionResult, ShellError> {
        self.execute(Command::Script(path.to_path_buf(), args.to_vec()))
            .await
    }
    /// Sources a script without replacing the persistent shell state.
    pub async fn source_script(
        &self,
        path: &Path,
        args: &[String],
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, ShellError> {
        self.execute(Command::Source(
            path.to_path_buf(),
            args.to_vec(),
            params.clone(),
        ))
        .await
    }
    /// Invokes a shell function through the same boundary as a command string.
    pub async fn invoke_function(
        &self,
        name: &str,
        args: &[String],
        params: &ExecutionParameters,
    ) -> Result<u8, ShellError> {
        self.execute(Command::Function(
            name.to_owned(),
            args.to_vec(),
            params.clone(),
        ))
        .await
        .map(|result| result.exit_code.into())
    }
    /// Returns the shell's current ordinary execution parameters.
    pub async fn default_exec_params(&self) -> ExecutionParameters {
        self.shared
            .live
            .lock()
            .await
            .as_ref()
            .map_or_else(ExecutionParameters::default, |live| {
                live.interpreter.default_exec_params()
            })
    }
    /// Returns the logical source working directory, never a snapshot pathname.
    pub async fn working_dir(&self) -> PathBuf {
        self.observe(
            |live| live.snapshot.logical(live.interpreter.working_dir()),
            |observed| observed.cwd.clone(),
        )
        .await
    }
    /// Changes the logical working directory without manufacturing a command transaction.
    pub async fn set_working_dir(&self, path: &Path) -> Result<(), ShellError> {
        self.modify(|live| {
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                live.snapshot
                    .logical(live.interpreter.working_dir())
                    .join(path)
            };
            let scope = live.snapshot.session.tracing.internal_scope()?;
            let _guard = scope.enter();
            live.interpreter
                .set_working_dir(live.snapshot.physical(&path))?;
            Ok(())
        })
        .await
    }
    /// Returns an owned snapshot of the ordinary environment.
    pub async fn env(&self) -> ShellEnvironment {
        self.observe(logical_environment, |observed| observed.environment.clone())
            .await
    }
    /// Replaces the in-memory environment.
    pub async fn set_env(&self, environment: ShellEnvironment) -> Result<(), ShellError> {
        self.modify(|live| {
            *live.interpreter.env_mut() = environment;
            Ok(())
        })
        .await
    }
    /// Returns one variable without cloning the whole environment.
    pub async fn env_var(&self, name: &str) -> Option<ShellVariable> {
        let lookup = brush_core::env::EnvironmentLookup::Anywhere;
        self.observe(
            |live| {
                Some(logical_variable(
                    live,
                    name,
                    live.interpreter
                        .env()
                        .get_using_policy(name, lookup)?
                        .clone(),
                ))
            },
            |observed| observed.environment.get_using_policy(name, lookup).cloned(),
        )
        .await
    }
    /// Sets one global variable.
    pub async fn set_var(&self, name: &str, variable: ShellVariable) -> Result<(), ShellError> {
        self.modify(|live| {
            live.interpreter
                .env_mut()
                .set_global(name, variable)
                .map_err(ShellError::from)
        })
        .await
    }
    /// Returns the last native shell status, including a nonzero successful execution.
    pub async fn last_exit_status(&self) -> u8 {
        self.observe(
            |live| live.interpreter.last_exit_status(),
            |observed| observed.status,
        )
        .await
    }
    /// Checks shell syntax without executing code.
    pub async fn input_is_complete(&self, line: &str) -> Result<bool, ShellError> {
        let live = self.shared.live.lock().await;
        let parsed = live
            .as_ref()
            .ok_or_else(|| ShellError::new(ShellErrorKind::Closed))?
            .interpreter
            .parse_string(line.to_owned());
        drop(live);
        Ok(!matches!(
            parsed,
            Err(brush_core::parser::ParseError::ParsingAtEndOfInput
                | brush_core::parser::ParseError::Tokenizing { .. })
        ))
    }
    /// Signals this operation's verified live descendants without waiting for the interpreter lock.
    pub fn signal_running(&self, signal: Signal) -> Result<usize, ShellError> {
        let active = self
            .shared
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .upgrade();
        active.map_or(Ok(0), |run| {
            run.tracing
                .signal(run.trace, signal.number())
                .map_err(ShellError::from)
        })
    }
    /// Signals an ordinary Brush job specification between operations.
    pub async fn signal_job(&self, job: &str, signal: Signal) -> Result<(), ShellError> {
        let mut guard = self.shared.live.lock().await;
        let live = guard
            .as_mut()
            .ok_or_else(|| ShellError::new(ShellErrorKind::Closed))?;
        let signal = brush_core::traps::TrapSignal::try_from(signal.as_str())?;
        live.interpreter
            .jobs_mut()
            .resolve_job_spec(job)
            .ok_or_else(|| ShellError::infrastructure(format!("no such job: {job}")))?
            .kill(signal)?;
        drop(guard);
        Ok(())
    }
    /// Prevents new admission, optionally cancels the accepted operation, and releases live resources.
    pub async fn close(&self, force: bool) -> Result<(), ShellError> {
        self.shared.closed.store(true, Ordering::Release);
        if force {
            self.shared.cancel();
        }
        self.shared.release().await
    }
    /// Whether the shell has stopped accepting commands.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }
    /// Stable policy principal of this shell, independent of its display name.
    pub const fn principal(&self) -> &Principal {
        &self.id
    }
    /// Logical canonical source directory.
    pub fn source_dir(&self) -> &Path {
        &self.source
    }
    /// Owns interactive prompt, line, completion and EOF/EXIT boundaries automatically.
    pub async fn run_interactively(
        &self,
        options: UIOptions,
    ) -> Result<ExecutionResult, ShellError> {
        input::run(self, options).await
    }

    async fn execute(&self, command: Command) -> Result<ExecutionResult, ShellError> {
        if self.is_closed() {
            return Err(ShellError::new(ShellErrorKind::Closed));
        }
        if self
            .shared
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(ShellError::new(ShellErrorKind::Busy));
        }
        let shared = Arc::clone(&self.shared);
        let (send, receive) = tokio::sync::oneshot::channel();
        self.shared.runtime.spawn(async move {
            let result = shared.evaluate(command).await;
            shared.busy.store(false, Ordering::Release);
            shared.finished.notify_waiters();
            if shared.closed.load(Ordering::Acquire) {
                let _ = shared.release().await;
            }
            let _ = send.send(result);
        });
        receive.await.map_err(|error| {
            ShellError::infrastructure(format!("owning runtime stopped: {error}"))
        })?
    }
    /// `current` of the live interpreter, or `released` of what it last showed before release.
    async fn observe<T>(
        &self,
        current: impl FnOnce(&Live) -> T + Send,
        released: impl FnOnce(&Observed) -> T + Send,
    ) -> T {
        let live = self.shared.live.lock().await;
        live.as_ref().map_or_else(
            || {
                released(
                    &self
                        .shared
                        .observed
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner),
                )
            },
            current,
        )
    }
    /// Applies `change` to the live interpreter of a shell that still admits operations.
    async fn modify<T>(
        &self,
        change: impl FnOnce(&mut Live) -> Result<T, ShellError> + Send,
    ) -> Result<T, ShellError> {
        let mut guard = self.shared.live.lock().await;
        let live = guard
            .as_mut()
            .ok_or_else(|| ShellError::new(ShellErrorKind::Closed))?;
        if self.is_closed() {
            return Err(ShellError::new(ShellErrorKind::Closed));
        }
        let changed = change(live);
        drop(guard);
        changed
    }
}
impl Drop for Shell {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.cancel();
        let shared = Arc::clone(&self.shared);
        self.shared.runtime.spawn(async move {
            let _ = shared.release().await;
        });
    }
}
impl Shared {
    /// Forces shutdown: the accepted operation is cancelled, and so is any span begun after it.
    fn cancel(&self) {
        self.force.store(true, Ordering::Release);
        let active = self
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .upgrade();
        if let Some(run) = active {
            run.cancel();
        }
    }
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the interpreter stays locked until its command is published, so no accessor sees it mid-publication"
    )]
    async fn evaluate(&self, command: Command) -> Result<ExecutionResult, ShellError> {
        let mut live = self.live.lock().await;
        let live = live
            .as_mut()
            .ok_or_else(|| ShellError::new(ShellErrorKind::Closed))?;
        if let Command::Interactive(options) = command {
            return input::run_owned(self, live, options).await;
        }
        let session = Arc::clone(&live.snapshot.session);
        let prepared = self.begin_span(&live.snapshot)?;
        let executed = execution::run(&mut live.interpreter, prepared, command).await;
        if executed.result.as_ref().is_some_and(|result| {
            matches!(
                result.next_control_flow,
                brush_core::ExecutionControlFlow::ExitShell
            )
        }) {
            self.closed.store(true, Ordering::Release);
        }
        let result = Self::publish_span(&session, executed);
        *self.active.lock().unwrap_or_else(PoisonError::into_inner) = Weak::new();
        result
    }
    fn begin_span(
        &self,
        snapshot: &Arc<Snapshot>,
    ) -> Result<snapshot::PreparedCommand, ShellError> {
        let prepared = snapshot::prepare(&snapshot.session, snapshot, self.runtime.clone())?;
        *self.active.lock().unwrap_or_else(PoisonError::into_inner) = Arc::downgrade(&prepared.run);
        if self.force.load(Ordering::Acquire) {
            prepared.run.cancel();
        }
        Ok(prepared)
    }
    async fn finish_span(
        &self,
        prepared: snapshot::PreparedCommand,
        result: ExecutionResult,
        command: String,
    ) -> Result<ExecutionResult, ShellError> {
        let session = Arc::clone(&prepared.snapshot.session);
        let executed = execution::complete(prepared, Some(result), None, command).await;
        Self::publish_span(&session, executed)
    }
    fn publish_span(
        session: &Session,
        executed: execution::ExecutedCommand,
    ) -> Result<ExecutionResult, ShellError> {
        let authorized = policy::authorize(session, executed)?;
        publication::commit(authorized)
    }
    #[expect(
        clippy::significant_drop_tightening,
        reason = "readers must not see the interpreter gone before its last observation is recorded"
    )]
    async fn release(&self) -> Result<(), ShellError> {
        loop {
            let finished = self.finished.notified();
            if !self.busy.load(Ordering::Acquire) {
                break;
            }
            finished.await;
        }
        let mut live = self.live.lock().await;
        if let Some(live) = live.take() {
            let observed = Observed {
                cwd: live.snapshot.logical(live.interpreter.working_dir()),
                environment: logical_environment(&live),
                status: live.interpreter.last_exit_status(),
            };
            *self.observed.lock().unwrap_or_else(PoisonError::into_inner) = observed;
            let Live {
                interpreter,
                snapshot,
            } = live;
            drop(interpreter);
            if let Err(error) = snapshot.close() {
                self.close_failure
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get_or_insert_with(|| Arc::new(error));
            }
        }
        self.close_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map_or(Ok(()), |error| {
                Err(ShellError::caused(
                    ShellErrorKind::Infrastructure,
                    Arc::clone(error),
                ))
            })
    }
}
fn logical_variable(live: &Live, name: &str, variable: ShellVariable) -> ShellVariable {
    if !matches!(name, "PWD" | "OLDPWD") || variable.is_treated_as_nameref() {
        return variable;
    }
    let brush_core::ShellValue::String(value) = variable.value() else {
        return variable;
    };
    let Ok(relative) = Path::new(value).strip_prefix(live.snapshot.path()) else {
        return variable;
    };
    let logical = live.snapshot.session.persistence.seed.join(relative);
    let mut mapped = ShellVariable::new(logical.to_string_lossy().into_owned());
    if variable.is_exported() {
        mapped.export();
    }
    if variable.is_readonly() {
        mapped.set_readonly();
    }
    if variable.is_trace_enabled() {
        mapped.enable_trace();
    }
    if !variable.is_enumerable() {
        mapped.hide_from_enumeration();
    }
    if variable.is_treated_as_integer() {
        mapped.treat_as_integer();
    }
    mapped.set_update_transform(variable.get_update_transform());
    mapped
}
fn logical_environment(live: &Live) -> ShellEnvironment {
    let mut environment = live.interpreter.env().clone();
    for name in ["PWD", "OLDPWD"] {
        if let Some(variable) =
            environment.get_mut_using_policy(name, brush_core::env::EnvironmentLookup::Anywhere)
        {
            *variable = logical_variable(live, name, std::mem::take(variable));
        }
    }
    environment
}

enum Command {
    Line(String),
    String(String, SourceInfo, ExecutionParameters),
    Script(PathBuf, Vec<String>),
    Source(PathBuf, Vec<String>, ExecutionParameters),
    Function(String, Vec<String>, ExecutionParameters),
    Startup(ProfileLoadBehavior, RcLoadBehavior),
    Interactive(UIOptions),
}
impl Command {
    fn description(&self) -> String {
        match self {
            Self::Line(line) | Self::String(line, ..) => line.clone(),
            Self::Script(path, _) | Self::Source(path, ..) => path.display().to_string(),
            Self::Function(name, ..) => name.clone(),
            Self::Startup(..) | Self::Interactive(_) => String::new(),
        }
    }
    async fn evaluate(
        self,
        shell: &mut brush_core::Shell<ManagedExtensions>,
    ) -> Result<ExecutionResult, brush_core::Error> {
        match self {
            Self::Line(line) => {
                let params = shell.default_exec_params();
                shell
                    .run_string(line, &SourceInfo::from("marsh"), &params)
                    .await
            }
            Self::String(line, source, params) => shell.run_string(line, &source, &params).await,
            Self::Script(path, args) => {
                let path = shell.external_command_spawner().physical(path);
                shell.run_script(path, args.into_iter()).await
            }
            Self::Source(path, args, params) => {
                let path = shell.external_command_spawner().physical(path);
                shell.source_script(path, args.into_iter(), &params).await
            }
            Self::Function(name, args, params) => shell
                .invoke_function(&name, &args, &params)
                .await
                .map(ExecutionResult::new),
            Self::Startup(profile, rc) => {
                shell.load_config(&profile, &rc).await?;
                Ok(ExecutionResult::new(shell.last_exit_status()))
            }
            Self::Interactive(_) => Err(brush_core::ErrorKind::InternalError(
                "interactive command must be driven by its private span coordinator".into(),
            )
            .into()),
        }
    }
}

#[cfg(test)]
mod tests;
