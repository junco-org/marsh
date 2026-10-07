//! An ordinary persistent shell. Each accepted operation is routed by its sandbox policy either
//! through the complete four-stage managed transaction boundary or directly to its source.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

pub use brush_core::env::ShellEnvironment;
pub use brush_core::openfiles::OpenFile;
pub use brush_core::{
    ExecutionParameters, ExecutionResult, ProfileLoadBehavior, RcLoadBehavior, ShellFd,
    ShellVariable, SourceInfo,
};
pub use brush_interactive::UIOptions;
use marsh_lib::RecoverPoison as _;

mod access;
pub mod builtins;
mod completion;
mod error;
mod execution;
mod input;
mod policy;
mod publication;
mod sandbox_policy;
mod session;
mod signal;
mod snapshot;
mod view;

pub use error::{ShellError, ShellErrorKind};
pub use junco_policy::{Action, Principal};
pub use policy::{
    Bump, Denial, EmptyPolicy, Event, GitPolicy, Policy, PolicyDecision, PolicyObserver,
    PolicyValidator,
};
pub use sandbox_policy::{CommandContext, MarshTool, SandboxPolicy, ShellCommand};
pub use signal::Signal;

pub(crate) use execution::ExecutionProgress;
use execution::{ManagedExtensions, MarshExecutor, Run};
use session::{Admission, ExecutionResources, Route, SourceDomain, fresh_principal};
use snapshot::Snapshot;

use crate::shellmux::{JobDir, Sandbox, ShellId};

/// Ordinary shell configuration. Execution machinery and storage choices are never public inputs.
pub struct ShellBuilder {
    options: brush_core::CreateOptions<ManagedExtensions>,
    environment: Option<ShellEnvironment>,
    policy: SandboxPolicy,
    pub(crate) backend: Option<Arc<dyn marsh_btrfs::Subvolumes>>,
    /// The display name a mux reserved; a standalone shell is named by its principal.
    pub(crate) sandbox_id: Option<ShellId>,
    policy_observer: Option<Arc<PolicyObserver>>,
    /// Unset means [`GitPolicy`], resolved once in `build`.
    shell_policy: Option<Arc<dyn Policy>>,
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
            policy: SandboxPolicy::default(),
            backend: None,
            sandbox_id: None,
            policy_observer: None,
            shell_policy: None,
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
    /// Selects, per command, the managed snapshot route (true) or the direct route (false).
    /// Without one, a shell sandboxes exactly while another live shell shares its source.
    #[must_use]
    pub fn sandbox_policy(mut self, policy: SandboxPolicy) -> Self {
        self.policy = policy;
        self
    }
    /// Observes every capability decision this shell's managed commands reach.
    #[must_use]
    pub fn policy_observer(mut self, observer: Option<Arc<PolicyObserver>>) -> Self {
        self.policy_observer = observer;
        self
    }
    /// Selects the authorization policy this shell's managed commands are checked against.
    /// Without one, the shell uses [`GitPolicy`].
    #[must_use]
    pub fn shell_policy(mut self, policy: Arc<dyn Policy>) -> Self {
        self.shell_policy = Some(policy);
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

    /// Starts the shared native tracer, discovers the source, joins the live-shell set, then
    /// constructs the persistent shell. Durable storage is opened only when a command's route
    /// first needs it.
    pub async fn build(mut self) -> Result<Shell, ShellError> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|error| ShellError::infrastructure(error.to_string()))?;
        let initial = self.options.working_dir.take().unwrap_or_default();
        let tracing = marsh_instrument::Tracing::shared()?;
        let discovered = {
            let scope = tracing.internal_scope()?;
            let _guard = scope.enter();
            SourceDomain::discover(&initial, self.backend, Arc::clone(&tracing))?
        };
        let uid = fresh_principal()?;
        let sandbox = sandbox_record(&discovered, self.sandbox_id, &uid)?;
        let mut resources = ExecutionResources::new(
            discovered.domain,
            discovered.coverage,
            uid.clone(),
            self.policy,
            self.shell_policy.unwrap_or_else(|| Arc::new(GitPolicy)),
            self.policy_observer,
        );
        let executor = MarshExecutor::new();
        let profile = self.options.profile;
        let rc = self.options.rc;
        let startup = !profile.skip() || !rc.skip();
        let mut builder = brush_core::Shell::builder_with_extensions::<ManagedExtensions>()
            .external_command_spawner(executor.clone())
            .execution_observer(executor.clone())
            .working_dir(discovered.cwd.clone())
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
            ("GIT_AUTHOR_NAME", format!("marsh-{uid}")),
            ("GIT_COMMITTER_NAME", format!("marsh-{uid}")),
            ("GIT_AUTHOR_EMAIL", format!("{uid}@marsh.local")),
            ("GIT_COMMITTER_EMAIL", format!("{uid}@marsh.local")),
        ] {
            let mut variable = ShellVariable::new(value);
            variable.export();
            builder = builder.var(name, variable);
        }
        let scope = tracing.internal_scope()?;
        let mut interpreter =
            Box::pin(marsh_instrument::Scoped::new(builder.build(), scope)).await?;
        for (name, registration) in builtins::managed() {
            interpreter.register_builtin(name, registration);
        }
        executor.idle();
        resources.register(sandbox.clone());
        let observed = Observed {
            cwd: discovered.cwd,
            environment: ShellEnvironment::default(),
            status: 0,
        };
        let shell = Shell {
            sandbox,
            shared: Arc::new(Shared {
                live: tokio::sync::Mutex::new(Some(Live {
                    interpreter,
                    resources,
                })),
                observed: Mutex::new(observed),
                executor,
                runtime,
                close_failure: Mutex::new(None),
                busy: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                force: AtomicBool::new(false),
                cancelled: tokio::sync::Notify::new(),
                finished: tokio::sync::Notify::new(),
            }),
        };
        if startup
            && let Err(error) = shell
                .execute(Command::Startup(profile, rc), None)
                .await
        {
            // Membership ends before the caller sees the failure, not when a detached drop runs.
            let _ = shell.close(true).await;
            return Err(error);
        }
        Ok(shell)
    }
}

/// A new shell's live-shell record: its display name (the principal unless a mux reserved one),
/// canonical source root, initial directory relative to that root, and principal.
fn sandbox_record(
    discovered: &session::Discovered,
    id: Option<ShellId>,
    uid: &Principal,
) -> Result<Sandbox, ShellError> {
    let dir = discovered
        .cwd
        .strip_prefix(&discovered.root)
        .ok()
        .and_then(Path::to_str)
        .ok_or_else(|| {
            ShellError::infrastructure(
                "initial directory is not a representable source-relative path",
            )
        })?;
    Ok(Sandbox {
        id: id.unwrap_or_else(|| ShellId::from(uid.as_str())),
        seed: discovered.root.clone(),
        dir: JobDir::from(dir),
        uid: uid.clone(),
    })
}

struct Live {
    interpreter: brush_core::Shell<ManagedExtensions>,
    resources: ExecutionResources,
}
impl Live {
    /// The interpreter's directory as the caller names it, whichever view it is in.
    fn logical_cwd(&self) -> PathBuf {
        let cwd = self.interpreter.working_dir();
        self.resources
            .snapshot
            .as_ref()
            .map_or_else(|| cwd.to_path_buf(), |snapshot| snapshot.logical(cwd))
    }
    /// A directory variable as the caller names it.
    fn logical_variable(&self, name: &str, variable: ShellVariable) -> ShellVariable {
        match &self.resources.snapshot {
            Some(snapshot) => view::remap_variable(
                name,
                variable,
                snapshot.path(),
                &snapshot.session.persistence.seed,
            ),
            None => variable,
        }
    }
    fn logical_environment(&self) -> ShellEnvironment {
        let mut environment = self.interpreter.env().clone();
        if let Some(snapshot) = &self.resources.snapshot {
            view::remap_environment(
                &mut environment,
                snapshot.path(),
                &snapshot.session.persistence.seed,
            );
        }
        environment
    }
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
    /// The interpreter's spawner and observer; its state names the admitted span's run.
    executor: MarshExecutor,
    runtime: tokio::runtime::Handle,
    busy: AtomicBool,
    closed: AtomicBool,
    force: AtomicBool,
    /// Wakes admission waits when `force` is set.
    cancelled: tokio::sync::Notify,
    finished: tokio::sync::Notify,
}

/// One admitted span: its route stays fixed until the admission is released.
struct Span {
    admission: completion::Completion<Admission>,
    route: SpanRoute,
}
enum SpanRoute {
    Managed(completion::Completion<snapshot::PreparedCommand>),
    Direct(Arc<Run>),
}

/// A persistent in-process shell. Each accepted operation owns all four private stages.
pub struct Shell {
    sandbox: Sandbox,
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
        self.execute(Command::Line(line.to_owned()), None).await
    }
    /// [`Self::run`], reporting through `progress` when its execution begins.
    pub(crate) async fn run_with_progress(
        &self,
        line: &str,
        progress: ExecutionProgress,
    ) -> Result<ExecutionResult, ShellError> {
        self.execute(Command::Line(line.to_owned()), Some(progress))
            .await
    }
    /// Executes a program string with ordinary Brush source and descriptor parameters.
    pub async fn run_string<S: Into<String>>(
        &self,
        command: S,
        source: &SourceInfo,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, ShellError> {
        self.execute(
            Command::String(command.into(), source.clone(), params.clone()),
            None,
        )
        .await
    }
    /// Executes a script in the filesystem view its route selects.
    pub async fn run_script(
        &self,
        path: &Path,
        args: &[String],
    ) -> Result<ExecutionResult, ShellError> {
        self.execute(Command::Script(path.to_path_buf(), args.to_vec()), None)
            .await
    }
    /// Sources a script without replacing the persistent shell state.
    pub async fn source_script(
        &self,
        path: &Path,
        args: &[String],
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, ShellError> {
        self.execute(
            Command::Source(path.to_path_buf(), args.to_vec(), params.clone()),
            None,
        )
        .await
    }
    /// Invokes a shell function through the same boundary as a command string.
    pub async fn invoke_function(
        &self,
        name: &str,
        args: &[String],
        params: &ExecutionParameters,
    ) -> Result<u8, ShellError> {
        self.execute(
            Command::Function(name.to_owned(), args.to_vec(), params.clone()),
            None,
        )
        .await
        .map(|result| result.exit_code.into())
    }
    /// Runs `operation` as one accepted call of `tool`, routed by this shell's policy over the
    /// tool and the [`Action`] it converts to, like any command.
    ///
    /// `operation` runs once, on a registered, trace-scoped blocking thread of the call's run. On
    /// the managed route it sees the private snapshot: its changes are evidence only when made
    /// through the context's I/O (`open`, `create_dir_all`, `remove_file`), are authorized and
    /// published like a command's, and `Ok` means they were published. Any other change to the
    /// snapshot refuses publication. Engines that read the view directly map paths with
    /// [`builtins::BuiltinContext::physical_path`] and
    /// [`logical_path`](builtins::BuiltinContext::logical_path). On the direct route it acts on
    /// the source itself. A refused, failed or interrupted call drops `operation`'s result.
    pub async fn run_tool<T, R, F>(&self, tool: T, operation: F) -> Result<R, ShellError>
    where
        T: MarshTool,
        for<'a> &'a T: Into<Action>,
        R: Send + 'static,
        F: FnOnce(&builtins::BuiltinContext) -> R + Send + 'static,
    {
        let action: Action = (&tool).into();
        let description = tool.description().into_owned();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let command = Command::Tool {
            tool: Box::new(tool),
            action,
            description,
            operation: Box::new(move |context| {
                let _ = send.send(operation(context));
            }),
        };
        self.execute(command, None).await?;
        receive
            .try_recv()
            .map_err(|_| ShellError::infrastructure("tool operation produced no result"))
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
        self.observe(Live::logical_cwd, |observed| observed.cwd.clone())
            .await
    }
    /// Changes the logical working directory without manufacturing a command transaction.
    pub async fn set_working_dir(&self, path: &Path) -> Result<(), ShellError> {
        self.modify(|live| {
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                live.logical_cwd().join(path)
            };
            match &live.resources.snapshot {
                Some(snapshot) => {
                    let scope = snapshot.session.tracing.internal_scope()?;
                    let _guard = scope.enter();
                    live.interpreter.set_working_dir(snapshot.physical(&path))?;
                }
                None => live.interpreter.set_working_dir(path)?,
            }
            Ok(())
        })
        .await
    }
    /// Returns an owned snapshot of the ordinary environment.
    pub async fn env(&self) -> ShellEnvironment {
        self.observe(Live::logical_environment, |observed| {
            observed.environment.clone()
        })
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
                Some(
                    live.logical_variable(
                        name,
                        live.interpreter
                            .env()
                            .get_using_policy(name, lookup)?
                            .clone(),
                    ),
                )
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
    /// Signals this operation's live processes without waiting for the interpreter lock.
    pub fn signal_running(&self, signal: Signal) -> Result<usize, ShellError> {
        self.shared
            .executor
            .running()
            .map_or(Ok(0), |run| run.signal(signal.number()))
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
        &self.sandbox.uid
    }
    /// Canonical source root: the Git work-tree root, or the initial directory outside one.
    pub fn source_dir(&self) -> &Path {
        &self.sandbox.seed
    }
    /// This shell's live-shell record, fixed for its lifetime.
    pub(crate) const fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }
    /// Owns interactive prompt, line, completion and EOF/EXIT boundaries automatically.
    pub async fn run_interactively(
        &self,
        options: UIOptions,
    ) -> Result<ExecutionResult, ShellError> {
        input::run(self, options).await
    }

    async fn execute(
        &self,
        command: Command,
        progress: Option<ExecutionProgress>,
    ) -> Result<ExecutionResult, ShellError> {
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
        // A call made from inside another command must never wait on work that waits on it.
        let parent = execution::current_context().map(|context| context.parent());
        let shared = Arc::clone(&self.shared);
        let (send, receive) = tokio::sync::oneshot::channel();
        self.shared.runtime.spawn(async move {
            let result = shared.evaluate(command, parent, progress).await;
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
        live.as_ref()
            .map_or_else(|| released(&self.shared.observed.lock().recover()), current)
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
        self.cancelled.notify_waiters();
        if let Some(run) = self.executor.running() {
            run.cancel();
        }
    }
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the interpreter stays locked until its command is published, so no accessor sees it mid-publication"
    )]
    async fn evaluate(
        &self,
        mut command: Command,
        parent: Option<Weak<Run>>,
        progress: Option<ExecutionProgress>,
    ) -> Result<ExecutionResult, ShellError> {
        let mut live = self.live.lock().await;
        let live = live
            .as_mut()
            .ok_or_else(|| ShellError::new(ShellErrorKind::Closed))?;
        if let Command::Interactive(options) = command {
            return input::run_owned(self, live, options, parent).await;
        }
        // A stored tool is routed as its original concrete value; any other command is a shell
        // span. Only these two references, never the command itself, are held across admission.
        let mut shell_call = None;
        let (tool, action): (&(dyn std::any::Any + Send + Sync), &Action) =
            if let Command::Tool { tool, action, .. } = &command {
                (tool.as_ref(), action)
            } else {
                let tool = ShellCommand {
                    command: command.description().into_owned(),
                };
                let action: Action = (&tool).into();
                let (tool, action) = shell_call.insert((tool, action));
                (&*tool, &*action)
            };
        let Span { admission, route } = self
            .begin_span(
                &mut live.interpreter,
                &mut live.resources,
                tool,
                action,
                parent.as_ref(),
                command.params(),
            )
            .await?;
        // Admission and view preparation are over: from here the command is executing.
        let outcome = match route {
            SpanRoute::Managed(prepared) => {
                if let Some(progress) = &progress {
                    progress.attach(&prepared.run);
                }
                let text = match (&mut command, shell_call) {
                    (Command::Tool { description, .. }, _) => std::mem::take(description),
                    (_, Some((tool, _))) => tool.command,
                    (_, None) => unreachable!("non-tool command has a shell descriptor"),
                };
                let executed = execution::run(&mut live.interpreter, prepared, command, text).await;
                self.exit_on(executed.result.as_ref());
                Self::publish(&live.resources, executed)
            }
            SpanRoute::Direct(run) => {
                if let Some(progress) = &progress {
                    progress.attach(&run);
                }
                let (result, failure) =
                    execution::evaluate(&mut live.interpreter, &run, command).await;
                self.exit_on(result.as_ref());
                Self::conclude_direct(&live.resources, &run, result, failure).await
            }
        };
        self.executor.idle();
        drop(admission);
        outcome
    }
    /// Closes the shell when the command asked it to exit.
    fn exit_on(&self, result: Option<&ExecutionResult>) {
        if result.is_some_and(|result| {
            matches!(
                result.next_control_flow,
                brush_core::ExecutionControlFlow::ExitShell
            )
        }) {
            self.closed.store(true, Ordering::Release);
        }
    }
    /// Admits one span for `tool`, classified as `action`, moves the interpreter into the view its
    /// route selects, and installs the span's run as the owner of every interpreter hook.
    async fn begin_span<SE: brush_core::ShellExtensions>(
        &self,
        interpreter: &mut brush_core::Shell<SE>,
        resources: &mut ExecutionResources,
        tool: &(dyn std::any::Any + Send + Sync),
        action: &Action,
        parent: Option<&Weak<Run>>,
        params: Option<&ExecutionParameters>,
    ) -> Result<Span, ShellError> {
        let (admission, route) = resources
            .admit(tool, action, &self.cancelled, &self.force, parent)
            .await?;
        let route = match route {
            Route::Managed => {
                if resources.snapshot.is_none() {
                    let session = resources.domain.managed().await?;
                    let snapshot = Snapshot::new(session, resources.principal.clone())?;
                    view::Transition::plan(
                        interpreter,
                        &snapshot.session.persistence.seed,
                        snapshot.path(),
                    )?
                    .apply(interpreter)?;
                    resources.snapshot = Some(snapshot);
                }
                let number = resources.number()?;
                let snapshot = resources
                    .snapshot
                    .as_ref()
                    .ok_or_else(|| ShellError::infrastructure("managed view missing"))?;
                let cwd = Arc::new(snapshot.logical(interpreter.working_dir()));
                let prepared = snapshot::prepare(snapshot, self.runtime.clone(), number, cwd)?;
                self.executor.install(&prepared.run);
                SpanRoute::Managed(prepared)
            }
            Route::Direct => {
                if let Some(snapshot) = &resources.snapshot {
                    if snapshot.retained.load(Ordering::Acquire) {
                        return Err(ShellError::infrastructure(
                            "command resources retained because recovery or quiescence is required",
                        ));
                    }
                    let transition = view::Transition::plan(
                        interpreter,
                        snapshot.path(),
                        &snapshot.session.persistence.seed,
                    )?;
                    snapshot.close()?;
                    resources.snapshot = None;
                    transition.apply(interpreter)?;
                }
                if let Some(session) = resources.domain.session() {
                    view::refuse_private(params, &session.persistence.snap())?;
                }
                let run = Arc::new(Run::direct(
                    self.runtime.clone(),
                    Arc::new(interpreter.working_dir().to_path_buf()),
                ));
                self.executor.install(&run);
                SpanRoute::Direct(run)
            }
        };
        if self.force.load(Ordering::Acquire) {
            match &route {
                SpanRoute::Managed(prepared) => prepared.run.cancel(),
                SpanRoute::Direct(run) => run.cancel(),
            }
        }
        Ok(Span { admission, route })
    }
    /// Completes a span begun outside [`Self::evaluate`]: drains its producers, publishes a
    /// managed span, then returns every hook to idle and releases the admission.
    async fn finish_span(
        &self,
        resources: &ExecutionResources,
        span: Span,
        result: ExecutionResult,
        failure: Option<ShellError>,
        command: String,
    ) -> Result<ExecutionResult, ShellError> {
        let Span { admission, route } = span;
        let outcome = match route {
            SpanRoute::Managed(prepared) => {
                let executed = execution::complete(prepared, Some(result), failure, command).await;
                Self::publish(resources, executed)
            }
            SpanRoute::Direct(run) => {
                Self::conclude_direct(resources, &run, Some(result), failure).await
            }
        };
        self.executor.idle();
        drop(admission);
        outcome
    }
    fn publish(
        resources: &ExecutionResources,
        executed: execution::ExecutedCommand,
    ) -> Result<ExecutionResult, ShellError> {
        if executed.uncertain {
            resources.domain.fail(
                &resources.coverage,
                "a command's producers could not be proven to have finished",
            );
        }
        let session = Arc::clone(&executed.prepared.snapshot.session);
        let authorized = policy::authorize(
            &session,
            executed,
            resources.shell_policy.as_ref(),
            resources.policy_observer.as_deref(),
        )?;
        publication::commit(authorized)
    }
    async fn conclude_direct(
        resources: &ExecutionResources,
        run: &Run,
        result: Option<ExecutionResult>,
        failure: Option<ShellError>,
    ) -> Result<ExecutionResult, ShellError> {
        let (outcome, uncertain) = execution::complete_direct(run, result, failure).await;
        if uncertain {
            resources.domain.fail(
                &resources.coverage,
                "a direct command's children could not be proven to have exited",
            );
        }
        outcome
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
                cwd: live.logical_cwd(),
                environment: live.logical_environment(),
                status: live.interpreter.last_exit_status(),
            };
            *self.observed.lock().recover() = observed;
            let Live {
                interpreter,
                mut resources,
            } = live;
            drop(interpreter);
            if let Some(snapshot) = resources.snapshot.take()
                && let Err(error) = snapshot.close()
            {
                self.close_failure
                    .lock()
                    .recover()
                    .get_or_insert_with(|| Arc::new(error));
            }
            // Membership ends here, even while a closed handle still retains this state.
            drop(resources);
        }
        self.close_failure
            .lock()
            .recover()
            .as_ref()
            .map_or(Ok(()), |error| {
                Err(ShellError::caused(
                    ShellErrorKind::Infrastructure,
                    Arc::clone(error),
                ))
            })
    }
}

/// A tool call's work, run once on a registered, trace-scoped blocking thread of its run.
type ToolOperation = Box<dyn FnOnce(&execution::BuiltinContext) + Send>;

enum Command {
    Line(String),
    String(String, SourceInfo, ExecutionParameters),
    Script(PathBuf, Vec<String>),
    Source(PathBuf, Vec<String>, ExecutionParameters),
    Function(String, Vec<String>, ExecutionParameters),
    Startup(ProfileLoadBehavior, RcLoadBehavior),
    Interactive(UIOptions),
    /// An embedder's call: the original concrete tool, with the action and description taken
    /// from it while its type was known, and the work it runs.
    Tool {
        tool: Box<dyn std::any::Any + Send + Sync>,
        action: Action,
        description: String,
        operation: ToolOperation,
    },
}
impl Command {
    /// The accepted top-level input: the submitted text, a script path, a function name or a
    /// tool's description.
    fn description(&self) -> Cow<'_, str> {
        match self {
            Self::Line(line) | Self::String(line, ..) => Cow::Borrowed(line),
            Self::Script(path, _) | Self::Source(path, ..) => path.to_string_lossy(),
            Self::Function(name, ..) => Cow::Borrowed(name),
            Self::Startup(..) | Self::Interactive(_) => Cow::Borrowed(""),
            Self::Tool { description, .. } => Cow::Borrowed(description),
        }
    }
    /// Caller-supplied descriptor parameters, when the command carries any.
    const fn params(&self) -> Option<&ExecutionParameters> {
        match self {
            Self::String(_, _, params)
            | Self::Source(_, _, params)
            | Self::Function(_, _, params) => Some(params),
            Self::Line(_)
            | Self::Script(..)
            | Self::Startup(..)
            | Self::Interactive(_)
            | Self::Tool { .. } => None,
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
                let path = shell
                    .external_command_spawner()
                    .physical(path)
                    .map_err(execution::brush_error)?;
                shell.run_script(path, args.into_iter()).await
            }
            Self::Source(path, args, params) => {
                let path = shell
                    .external_command_spawner()
                    .physical(path)
                    .map_err(execution::brush_error)?;
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
            Self::Tool { operation, .. } => {
                let context = execution::current_context().ok_or_else(|| {
                    brush_core::Error::from(brush_core::ErrorKind::InternalError(
                        "tool operation has no command context".into(),
                    ))
                })?;
                let done = context
                    .spawn_blocking({
                        let context = context.clone();
                        move || operation(&context)
                    })
                    .map_err(execution::brush_error)?;
                done.await.map_err(|_| {
                    brush_core::Error::from(brush_core::ErrorKind::InternalError(
                        "tool operation panicked".into(),
                    ))
                })?;
                Ok(ExecutionResult::success())
            }
        }
    }
}

#[cfg(test)]
mod tests;
