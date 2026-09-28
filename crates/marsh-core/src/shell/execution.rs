//! Stage 2: one evaluation, scoped Brush work, transitive producers, and final native drain.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};

use brush_core::extensions::{
    DefaultErrorFormatter, DefaultExternalCommandSpawner, ExecutionObserver,
    ExternalCommandSpawner, ShellExtensions,
};
use brush_core::{CommandArg, ExecutionResult};
use futures_util::FutureExt;
use marsh_instrument::{
    InvocationId, PollScope, Scoped, TraceRun, TraceScope, TraceScopeGuard, Tracing,
};
use marsh_lib::RecoverPoison as _;

use super::completion::{Completion, Finalize};
use super::snapshot::{CommandEvidence, PreparedCommand, Snapshot};
use super::{ShellError, ShellErrorKind};

#[derive(Clone, Default)]
pub(super) struct ManagedExtensions;
impl ShellExtensions for ManagedExtensions {
    type ErrorFormatter = DefaultErrorFormatter;
    type ExternalCommandSpawner = MarshExecutor;
    type ExecutionObserver = MarshExecutor;
}

#[derive(Clone, Default)]
pub(super) struct MarshExecutor {
    snapshot: Weak<Snapshot>,
}
impl MarshExecutor {
    pub fn new(snapshot: &Arc<Snapshot>) -> Self {
        Self {
            snapshot: Arc::downgrade(snapshot),
        }
    }
    /// `path` inside the attached snapshot, or unchanged once the snapshot is gone.
    pub fn physical(&self, path: PathBuf) -> PathBuf {
        match self.snapshot.upgrade() {
            Some(snapshot) => snapshot.physical(&path),
            None => path,
        }
    }
    fn context(&self) -> Result<CommandContext, ShellError> {
        if let Some(context) = current_context() {
            if Weak::ptr_eq(&context.snapshot, &self.snapshot) {
                context.check()?;
                return Ok(context);
            }
        }
        let snapshot = self
            .snapshot
            .upgrade()
            .ok_or_else(|| ShellError::new(ShellErrorKind::Closed))?;
        let run = snapshot.active()?;
        run.check()?;
        Ok(CommandContext {
            snapshot: self.snapshot.clone(),
            run,
            cwd: Arc::new(snapshot.session.persistence.seed.clone()),
            builtin: None,
        })
    }
}
impl ExternalCommandSpawner for MarshExecutor {
    fn spawn(
        &self,
        command: std::process::Command,
        kill_on_drop: bool,
    ) -> std::io::Result<brush_core::sys::process::Child> {
        let context = self.context().map_err(std::io::Error::other)?;
        let _guard = context.enter().map_err(std::io::Error::other)?;
        DefaultExternalCommandSpawner.spawn(command, kill_on_drop)
    }
}

pub(super) struct BuiltinToken(Result<CommandContext, ShellError>);
impl ExecutionObserver for MarshExecutor {
    type Builtin = BuiltinToken;
    type SyncGuard = SyncGuard;
    fn enter_sync(&self) -> Result<Self::SyncGuard, brush_core::Error> {
        self.context()
            .and_then(|context| context.enter())
            .map_err(brush_error)
    }
    fn begin_builtin(&self, _name: &str, _args: &[CommandArg], cwd: &Path) -> Self::Builtin {
        BuiltinToken(self.context().and_then(|mut context| {
            let snapshot = context
                .snapshot
                .upgrade()
                .ok_or_else(|| ShellError::new(ShellErrorKind::Closed))?;
            context.cwd = Arc::new(snapshot.logical(cwd));
            context.builtin = Some(context.run.tracing.invocation(context.run.trace)?);
            Ok(context)
        }))
    }
    fn run_builtin<F, Make>(
        &self,
        token: Self::Builtin,
        make: Make,
    ) -> impl Future<Output = Result<ExecutionResult, brush_core::Error>> + Send + use<F, Make>
    where
        F: Future<Output = Result<ExecutionResult, brush_core::Error>> + Send,
        Make: FnOnce() -> F + Send,
    {
        async move {
            let context = token.0.map_err(brush_error)?;
            let scoped = context
                .wrap(async move { make().await })
                .map_err(brush_error)?;
            scoped.await
        }
    }
    fn scope_future<F>(
        &self,
        future: F,
    ) -> Result<impl Future<Output = F::Output> + Send + use<F>, brush_core::Error>
    where
        F: Future + Send,
    {
        self.context()
            .and_then(|context| context.wrap(future))
            .map_err(brush_error)
    }
    fn spawn_task<F>(
        &self,
        future: F,
    ) -> Result<tokio::task::JoinHandle<F::Output>, brush_core::Error>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let context = self.context().map_err(brush_error)?;
        let lease = context.run.lease().map_err(brush_error)?;
        let id = lease.id;
        let scoped = context.wrap(future).map_err(brush_error)?;
        let internal = context
            .run
            .tracing
            .internal_scope()
            .map_err(|error| brush_error(error.into()))?;
        let _guard = internal.enter();
        let handle = context.run.runtime.spawn(async move {
            let result = scoped.await;
            lease.complete();
            result
        });
        context.run.install(id, handle.abort_handle());
        Ok(handle)
    }
    fn spawn_blocking_task<F, T>(
        &self,
        operation: F,
    ) -> Result<tokio::task::JoinHandle<T>, brush_core::Error>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.context()
            .and_then(|context| context.blocking(operation))
            .map_err(brush_error)
    }
}

#[derive(Default)]
struct Workers {
    next: u64,
    // Counter allocation and producer registration are serialized by the enclosing mutex.
    handles: HashMap<WorkerId, Option<tokio::task::AbortHandle>>,
    failure: bool,
    closing: bool,
}

#[repr(u8)]
enum RunPhase {
    Running,
    Cancelled,
    Publishing,
}

pub(super) struct Run {
    pub runtime: tokio::runtime::Handle,
    pub tracing: Arc<Tracing>,
    pub trace: TraceRun,
    workers: Mutex<Workers>,
    pub closed: AtomicBool,
    phase: AtomicU8,
    changed: tokio::sync::Notify,
}
impl Run {
    pub fn new(runtime: tokio::runtime::Handle, tracing: Arc<Tracing>, trace: TraceRun) -> Self {
        Self {
            runtime,
            tracing,
            trace,
            workers: Mutex::new(Workers::default()),
            closed: AtomicBool::new(false),
            phase: AtomicU8::new(RunPhase::Running as u8),
            changed: tokio::sync::Notify::new(),
        }
    }
    pub fn check(&self) -> Result<(), ShellError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(ShellError::new(ShellErrorKind::Closed));
        }
        if self.is_cancelled() {
            return Err(ShellError::new(ShellErrorKind::Interrupted));
        }
        self.tracing.health(self.trace)?;
        Ok(())
    }
    fn lease(self: &Arc<Self>) -> Result<Completion<Lease>, ShellError> {
        self.check()?;
        let mut workers = self.workers.lock().recover();
        if workers.closing {
            return Err(ShellError::new(ShellErrorKind::Closed));
        }
        workers.next = workers
            .next
            .checked_add(1)
            .ok_or_else(|| ShellError::infrastructure("worker identity exhausted"))?;
        let id = WorkerId(workers.next);
        workers.handles.insert(id, None);
        drop(workers);
        Ok(Completion::new(Lease {
            run: Arc::clone(self),
            id,
        }))
    }
    fn install(&self, id: WorkerId, abort: tokio::task::AbortHandle) {
        let mut workers = self.workers.lock().recover();
        if self.is_cancelled() {
            abort.abort();
        }
        if let Some(slot) = workers.handles.get_mut(&id) {
            *slot = Some(abort);
        }
    }
    pub fn cancel(&self) {
        if self
            .phase
            .compare_exchange(
                RunPhase::Running as u8,
                RunPhase::Cancelled as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        let workers = self.workers.lock().recover();
        for abort in workers.handles.values().flatten() {
            abort.abort();
        }
        drop(workers);
        let _ = self.tracing.cancel(self.trace);
        self.changed.notify_waiters();
    }
    pub fn is_cancelled(&self) -> bool {
        self.phase.load(Ordering::Acquire) == RunPhase::Cancelled as u8
    }
    pub fn seal_publication(&self) -> Result<(), ShellError> {
        self.phase
            .compare_exchange(
                RunPhase::Running as u8,
                RunPhase::Publishing as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| ShellError::new(ShellErrorKind::Interrupted))
    }
    pub async fn cancelled(&self) {
        loop {
            let changed = self.changed.notified();
            if self.is_cancelled() || self.closed.load(Ordering::Acquire) {
                return;
            }
            changed.await;
        }
    }
    async fn finish(&self) -> Result<(), ShellError> {
        loop {
            let changed = self.changed.notified();
            let state = {
                let mut workers = self.workers.lock().recover();
                if workers.handles.is_empty() {
                    workers.closing = true;
                    Some(workers.failure)
                } else {
                    None
                }
            };
            if let Some(failed) = state {
                self.closed.store(true, Ordering::Release);
                self.changed.notify_waiters();
                if failed {
                    return Err(ShellError::infrastructure(
                        "an owned producer did not complete",
                    ));
                }
                return Ok(());
            }
            changed.await;
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct WorkerId(u64);
struct Lease {
    run: Arc<Run>,
    id: WorkerId,
}
impl Finalize for Lease {
    fn finalize(&mut self, completed: bool) {
        let mut workers = self.run.workers.lock().recover();
        workers.handles.remove(&self.id);
        workers.failure |= !completed && !self.run.is_cancelled();
        drop(workers);
        self.run.changed.notify_waiters();
    }
}

thread_local! { static CURRENT: RefCell<Option<CommandContext>> = const { RefCell::new(None) }; }
struct ContextGuard(Option<CommandContext>);
impl ContextGuard {
    fn enter(context: CommandContext) -> Self {
        Self(CURRENT.with(|current| current.replace(Some(context))))
    }
}
impl Drop for ContextGuard {
    fn drop(&mut self) {
        CURRENT.with(|current| {
            current.replace(self.0.take());
        });
    }
}

/// Normal callback I/O and cancellation facilities. Retaining this never retains a work tree.
#[derive(Clone)]
pub struct CommandContext {
    snapshot: Weak<Snapshot>,
    run: Arc<Run>,
    cwd: Arc<PathBuf>,
    builtin: Option<InvocationId>,
}
impl std::fmt::Debug for CommandContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandContext")
            .field("working_dir", &self.cwd)
            .field("cancelled", &self.cancellation_requested())
            .finish_non_exhaustive()
    }
}
/// Returns the context of the builtin currently executing on this thread or task poll.
pub fn current_context() -> Option<CommandContext> {
    CURRENT.with(|current| current.borrow().clone())
}

impl CommandContext {
    pub(super) const fn invocation(&self) -> Option<InvocationId> {
        self.builtin
    }
    pub(super) fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.check().ok()?;
        self.snapshot.upgrade()
    }
    fn check(&self) -> Result<(), ShellError> {
        self.run.check()
    }
    fn enter(&self) -> Result<SyncGuard, ShellError> {
        self.check()?;
        let scope = self.run.tracing.scope(self.run.trace, self.builtin)?;
        Ok(SyncGuard {
            _trace: scope.enter(),
            _context: ContextGuard::enter(self.clone()),
        })
    }
    fn wrap<F: Future>(&self, future: F) -> Result<Scoped<F, ContextScope>, ShellError> {
        self.check()?;
        Ok(Scoped::new(
            future,
            ContextScope {
                scope: self.run.tracing.scope(self.run.trace, self.builtin)?,
                context: self.clone(),
            },
        ))
    }
    /// Logical working directory of the callback invocation.
    pub fn working_dir(&self) -> &Path {
        &self.cwd
    }
    /// Whether its owner requested cancellation or already ended the run.
    pub fn cancellation_requested(&self) -> bool {
        self.run.is_cancelled() || self.run.closed.load(Ordering::Acquire)
    }
    /// Resolves when cancellation is requested or the owning run ends.
    pub async fn cancelled(&self) {
        self.run.cancelled().await;
    }
    fn blocking<F, T>(&self, operation: F) -> Result<tokio::task::JoinHandle<T>, ShellError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let lease = self.run.lease()?;
        let id = lease.id;
        let context = self.clone();
        let scope = self.run.tracing.scope(self.run.trace, self.builtin)?;
        let internal = self.run.tracing.internal_scope()?;
        let _guard = internal.enter();
        let handle = self.run.runtime.spawn_blocking(move || {
            let _trace = scope.enter();
            let _context = ContextGuard::enter(context);
            let result = operation();
            lease.complete();
            result
        });
        self.run.install(id, handle.abort_handle());
        Ok(handle)
    }
    /// Registers blocking work before scheduling it; Shell joins it even if the receiver is dropped.
    pub fn spawn_blocking<F, T>(
        &self,
        operation: F,
    ) -> Result<tokio::sync::oneshot::Receiver<T>, ShellError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.blocking(move || {
            let _ = send.send(operation());
        })?;
        Ok(receive)
    }
    /// Runs `operation` against the live work generation as one registered, scoped step of the
    /// owning run; `ended` is the failure once the snapshot has gone.
    fn step<T>(&self, ended: &str, operation: impl FnOnce(&Snapshot) -> T) -> std::io::Result<T> {
        let mut lease = self.run.lease().map_err(std::io::Error::other)?;
        let _guard = self.enter().map_err(std::io::Error::other)?;
        let snapshot = self
            .snapshot
            .upgrade()
            .ok_or_else(|| std::io::Error::other(ended))?;
        let result = operation(&snapshot);
        lease.completed = true;
        Ok(result)
    }
    fn io<T>(
        &self,
        path: &Path,
        operation: impl FnOnce(&Path) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let logical = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        self.step("command context has ended", |snapshot| {
            operation(&snapshot.physical(&logical))
        })?
    }
    /// Opens a logical path inside the owning command's work generation.
    pub fn open(
        &self,
        path: &Path,
        options: &std::fs::OpenOptions,
    ) -> std::io::Result<std::fs::File> {
        self.io(path, |path| options.open(path))
    }
    /// Reads metadata through the command's logical filesystem view.
    pub fn metadata(&self, path: &Path) -> std::io::Result<std::fs::Metadata> {
        self.io(path, |path| std::fs::metadata(path))
    }
    /// Creates directories through the command's logical filesystem view.
    pub fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.io(path, |path| std::fs::create_dir_all(path))
    }
    /// Opens a directory iterator; each subsequent call still checks owning-run liveness.
    pub fn read_dir(&self, path: &Path) -> std::io::Result<ReadDir> {
        let inner = self.io(path, |path| std::fs::read_dir(path))?;
        Ok(ReadDir {
            inner,
            context: self.clone(),
        })
    }
    /// Expands a glob using the existing glob implementation and returns logical paths.
    pub fn glob(&self, pattern: &str, cwd: Option<&Path>) -> Result<GlobPaths, ShellError> {
        self.check()?;
        let _guard = self.enter()?;
        let snapshot = self
            .snapshot
            .upgrade()
            .ok_or_else(|| ShellError::new(ShellErrorKind::Closed))?;
        let not_utf8 = || ShellError::unsupported("glob path is not UTF-8");
        // Only the caller's pattern is glob syntax; the directory it is relative to is a literal
        // path, and a `[` or `*` in its name must not turn it into a character class or wildcard.
        let pattern = if Path::new(pattern).is_absolute() {
            let physical = snapshot.physical(Path::new(pattern));
            physical.to_str().ok_or_else(not_utf8)?.to_owned()
        } else {
            let base = snapshot.physical(cwd.unwrap_or(&self.cwd));
            format!(
                "{}/{pattern}",
                glob::Pattern::escape(base.to_str().ok_or_else(not_utf8)?)
            )
        };
        let inner =
            glob::glob(&pattern).map_err(|error| ShellError::unsupported(error.to_string()))?;
        Ok(GlobPaths {
            inner,
            context: self.clone(),
        })
    }
}

/// Logical directory entries with lifetime-checked iteration.
pub struct ReadDir {
    inner: std::fs::ReadDir,
    context: CommandContext,
}
impl Iterator for ReadDir {
    type Item = std::io::Result<DirectoryEntry>;
    fn next(&mut self) -> Option<Self::Item> {
        self.context
            .step("ended directory context", |snapshot| {
                self.inner.next().map(|entry| {
                    entry.map(|entry| DirectoryEntry {
                        path: snapshot.logical(&entry.path()),
                        name: entry.file_name(),
                        context: self.context.clone(),
                    })
                })
            })
            .unwrap_or_else(|error| Some(Err(error)))
    }
}
/// A directory entry containing only logical names.
pub struct DirectoryEntry {
    path: PathBuf,
    name: std::ffi::OsString,
    context: CommandContext,
}
impl DirectoryEntry {
    /// Logical full pathname.
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }
    /// Final pathname component.
    pub fn file_name(&self) -> std::ffi::OsString {
        self.name.clone()
    }
    /// Metadata without following a symlink leaf, matching standard directory-entry semantics.
    pub fn metadata(&self) -> std::io::Result<std::fs::Metadata> {
        self.context
            .io(&self.path, |path| std::fs::symlink_metadata(path))
    }
    /// The entry's kind without following a symlink leaf.
    pub fn file_type(&self) -> std::io::Result<std::fs::FileType> {
        self.metadata().map(|metadata| metadata.file_type())
    }
}
/// Logical results from a managed glob.
pub struct GlobPaths {
    inner: glob::Paths,
    context: CommandContext,
}
impl Iterator for GlobPaths {
    type Item = std::io::Result<PathBuf>;
    fn next(&mut self) -> Option<Self::Item> {
        self.context
            .step("ended glob context", |snapshot| {
                self.inner.next().map(|path| {
                    path.map(|path| snapshot.logical(&path)).map_err(|error| {
                        std::io::Error::new(error.error().kind(), error.error().to_string())
                    })
                })
            })
            .unwrap_or_else(|error| Some(Err(error)))
    }
}

pub(super) struct SyncGuard {
    _trace: TraceScopeGuard,
    _context: ContextGuard,
}
/// Per-poll command context: TLS is entered before tracing and restored after it.
struct ContextScope {
    scope: TraceScope,
    context: CommandContext,
}
impl PollScope for ContextScope {
    type Guard = SyncGuard;
    fn enter(&self) -> Self::Guard {
        let context = ContextGuard::enter(self.context.clone());
        SyncGuard {
            _trace: self.scope.enter(),
            _context: context,
        }
    }
}
/// A managed-context failure as the interpreter reports one.
#[expect(
    clippy::needless_pass_by_value,
    reason = "a `map_err` adapter, which is handed each error by value"
)]
pub(super) fn brush_error(error: ShellError) -> brush_core::Error {
    brush_core::ErrorKind::InternalError(error.to_string()).into()
}

pub(super) struct ExecutedCommand {
    pub prepared: Completion<PreparedCommand>,
    pub result: Option<ExecutionResult>,
    pub failure: Option<ShellError>,
    pub evidence: CommandEvidence,
    pub command: String,
}

pub(super) async fn run(
    interpreter: &mut brush_core::Shell<ManagedExtensions>,
    prepared: Completion<PreparedCommand>,
    command: super::Command,
) -> ExecutedCommand {
    let context = CommandContext {
        snapshot: Arc::downgrade(&prepared.snapshot),
        run: Arc::clone(&prepared.run),
        cwd: Arc::new(prepared.snapshot.logical(interpreter.working_dir())),
        builtin: None,
    };
    let text = command.description();
    let mut result = None;
    let mut failure = None;
    match context.wrap(command.evaluate(interpreter)) {
        Err(error) => failure = Some(error),
        Ok(evaluation) => {
            let evaluated = tokio::select! {
                outcome = std::panic::AssertUnwindSafe(evaluation).catch_unwind() => match outcome {
                    Ok(result) => result.map_err(ShellError::from),
                    Err(_) => Err(ShellError::infrastructure("command producer panicked")),
                },
                () = prepared.run.cancelled() => Err(ShellError::new(ShellErrorKind::Interrupted)),
                error = std::future::poll_fn(|context| prepared.run.tracing.poll_failure(prepared.run.trace, context)) => Err(error.into()),
            };
            match evaluated {
                Ok(value) => result = Some(value),
                Err(error) => failure = Some(error),
            }
        }
    }
    complete(prepared, result, failure, text).await
}

pub(super) async fn complete(
    prepared: Completion<PreparedCommand>,
    result: Option<ExecutionResult>,
    mut failure: Option<ShellError>,
    text: String,
) -> ExecutedCommand {
    if failure.is_some() {
        prepared.run.cancel();
    }
    if let Err(error) = prepared.run.finish().await {
        failure.get_or_insert(error);
    }
    let tracing = Arc::clone(&prepared.run.tracing);
    let trace = prepared.run.trace;
    let drain = {
        let internal = tracing.internal_scope();
        let _guard = internal.as_ref().ok().map(TraceScope::enter);
        prepared
            .run
            .runtime
            .spawn_blocking(move || tracing.quiesce(trace))
    };
    match drain.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            failure.get_or_insert_with(|| error.into());
            prepared.snapshot.retained.store(true, Ordering::Release);
        }
        Err(error) => {
            failure.get_or_insert_with(|| ShellError::infrastructure(error.to_string()));
            prepared.snapshot.retained.store(true, Ordering::Release);
        }
    }
    if prepared.run.is_cancelled() {
        failure.get_or_insert_with(|| ShellError::new(ShellErrorKind::Interrupted));
    }
    let evidence = match prepared.snapshot.take_evidence(trace) {
        Ok(evidence) => evidence,
        Err(error) => {
            failure.get_or_insert(error);
            CommandEvidence::default()
        }
    };
    if prepared.run.tracing.health(trace).is_ok() {
        if let Err(error) = prepared.run.tracing.end_run(trace) {
            failure.get_or_insert_with(|| error.into());
        }
    }
    ExecutedCommand {
        prepared,
        result,
        failure,
        evidence,
        command: text,
    }
}
