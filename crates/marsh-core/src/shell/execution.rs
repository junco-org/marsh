//! Stage 2: one evaluation, scoped Brush work, transitive producers, and final native drain.
//!
//! Both routes run the same persistent interpreter under one [`Run`]. A managed run observes its
//! snapshot through the shared tracer; a direct run executes against the source and only owns the
//! lifetimes of the children and tasks it starts.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::OwnedFd;
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
use tokio::io::unix::AsyncFd;

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

/// Which run, if any, the interpreter's hooks currently belong to.
#[derive(Default)]
pub(super) enum ExecutorState {
    /// The interpreter is being constructed; no command owns its hooks yet.
    Initializing,
    /// Between spans: no hook has an owner.
    #[default]
    Idle,
    /// The admitted span's run.
    Running(Weak<Run>),
}

/// The interpreter's spawner and observer. Every clone shares one state cell.
#[derive(Clone, Default)]
pub(super) struct MarshExecutor {
    state: Arc<Mutex<ExecutorState>>,
}
impl MarshExecutor {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ExecutorState::Initializing)),
        }
    }
    /// Makes `run` the owner of every hook until [`Self::idle`].
    pub fn install(&self, run: &Arc<Run>) {
        *self.state.lock().recover() = ExecutorState::Running(Arc::downgrade(run));
    }
    pub fn idle(&self) {
        *self.state.lock().recover() = ExecutorState::Idle;
    }
    /// The live run owning the interpreter's hooks.
    pub fn running(&self) -> Option<Arc<Run>> {
        match &*self.state.lock().recover() {
            ExecutorState::Running(run) => run.upgrade(),
            ExecutorState::Initializing | ExecutorState::Idle => None,
        }
    }
    /// `path` in the active run's filesystem view.
    pub fn physical(&self, path: PathBuf) -> Result<PathBuf, ShellError> {
        let context = self.context()?;
        Ok(match context.snapshot()? {
            Some(snapshot) => snapshot.physical(&path),
            None => path,
        })
    }
    fn context(&self) -> Result<BuiltinContext, ShellError> {
        let run = self
            .running()
            .ok_or_else(|| ShellError::new(ShellErrorKind::Closed))?;
        if let Some(context) = current_context()
            && Arc::ptr_eq(&context.run, &run)
        {
            context.check()?;
            return Ok(context);
        }
        run.check()?;
        Ok(BuiltinContext {
            cwd: Arc::clone(&run.cwd),
            run,
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
        let child = DefaultExternalCommandSpawner.spawn(command, kill_on_drop)?;
        context.run.adopt(child)
    }
}

pub(super) struct BuiltinToken(Result<BuiltinContext, ShellError>);
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
            context.cwd = Arc::new(match context.snapshot()? {
                Some(snapshot) => snapshot.logical(cwd),
                None => cwd.to_path_buf(),
            });
            context.builtin = match &context.run.backend {
                Backend::Managed { tracing, trace, .. } => Some(tracing.invocation(*trace)?),
                Backend::Direct { .. } => None,
            };
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
        let internal = context.run.internal_scope().map_err(brush_error)?;
        let _guard = internal.as_ref().map(TraceScope::enter);
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

/// A child process of a direct run, pinned by its pidfd; readable once it has exited.
type DirectChild = Arc<AsyncFd<OwnedFd>>;

/// How a run's work is observed and ended.
pub(super) enum Backend {
    /// Traced work in a private snapshot, published only through authorization.
    Managed {
        tracing: Arc<Tracing>,
        trace: TraceRun,
        snapshot: Weak<Snapshot>,
    },
    /// Work against the source itself: only the children this run spawned are owned.
    Direct { children: Mutex<Vec<DirectChild>> },
}

pub(super) struct Run {
    pub runtime: tokio::runtime::Handle,
    pub backend: Backend,
    /// The span's logical initial directory, for contexts not created by a builtin.
    pub cwd: Arc<PathBuf>,
    workers: Mutex<Workers>,
    pub closed: AtomicBool,
    phase: AtomicU8,
    changed: tokio::sync::Notify,
}
impl Run {
    fn new(runtime: tokio::runtime::Handle, backend: Backend, cwd: Arc<PathBuf>) -> Self {
        Self {
            runtime,
            backend,
            cwd,
            workers: Mutex::new(Workers::default()),
            closed: AtomicBool::new(false),
            phase: AtomicU8::new(RunPhase::Running as u8),
            changed: tokio::sync::Notify::new(),
        }
    }
    pub fn managed(
        runtime: tokio::runtime::Handle,
        tracing: Arc<Tracing>,
        trace: TraceRun,
        snapshot: Weak<Snapshot>,
        cwd: Arc<PathBuf>,
    ) -> Self {
        Self::new(
            runtime,
            Backend::Managed {
                tracing,
                trace,
                snapshot,
            },
            cwd,
        )
    }
    pub fn direct(runtime: tokio::runtime::Handle, cwd: Arc<PathBuf>) -> Self {
        Self::new(
            runtime,
            Backend::Direct {
                children: Mutex::new(Vec::new()),
            },
            cwd,
        )
    }
    /// The tracer and trace of a managed run; a direct run has neither.
    pub fn trace(&self) -> Result<(&Arc<Tracing>, TraceRun), ShellError> {
        match &self.backend {
            Backend::Managed { tracing, trace, .. } => Ok((tracing, *trace)),
            Backend::Direct { .. } => Err(ShellError::infrastructure(
                "a direct command has no managed trace",
            )),
        }
    }
    /// A scope marking implementation work; direct runs are never traced.
    fn internal_scope(&self) -> Result<Option<TraceScope>, ShellError> {
        match &self.backend {
            Backend::Managed { tracing, .. } => Ok(Some(tracing.internal_scope()?)),
            Backend::Direct { .. } => Ok(None),
        }
    }
    pub fn check(&self) -> Result<(), ShellError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(ShellError::new(ShellErrorKind::Closed));
        }
        if self.is_cancelled() {
            return Err(ShellError::new(ShellErrorKind::Interrupted));
        }
        if let Backend::Managed { tracing, trace, .. } = &self.backend {
            tracing.health(*trace)?;
        }
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
    /// Takes ownership of a freshly spawned child. A direct run pins it by pidfd, or kills and
    /// reaps it when it cannot, so no child it spawned is ever left untracked.
    fn adopt(
        &self,
        child: brush_core::sys::process::Child,
    ) -> std::io::Result<brush_core::sys::process::Child> {
        let Backend::Direct { children } = &self.backend else {
            return Ok(child);
        };
        let pinned = child
            .id()
            .ok_or_else(|| std::io::Error::other("spawned child has no process id"))
            .and_then(|pid| i32::try_from(pid).map_err(std::io::Error::other))
            .and_then(marsh_instrument::open_process)
            .and_then(|pidfd| {
                let _runtime = self.runtime.enter();
                AsyncFd::new(pidfd)
            });
        match pinned {
            Ok(pidfd) => {
                if self.is_cancelled() {
                    let _ = marsh_instrument::signal_process(pidfd.get_ref(), libc::SIGKILL);
                }
                children.lock().recover().push(Arc::new(pidfd));
                Ok(child)
            }
            Err(error) => {
                let mut child = child;
                let _ = child.start_kill();
                self.runtime.spawn(async move {
                    let _ = child.wait().await;
                });
                Err(error)
            }
        }
    }
    /// Signals the run's live processes; returns how many received it.
    pub fn signal(&self, signal: i32) -> Result<usize, ShellError> {
        match &self.backend {
            Backend::Managed { tracing, trace, .. } => Ok(tracing.signal(*trace, signal)?),
            Backend::Direct { children } => {
                let children = children.lock().recover().clone();
                let mut signalled = 0;
                for child in &children {
                    if marsh_instrument::signal_process(child.get_ref(), signal)? {
                        signalled += 1;
                    }
                }
                Ok(signalled)
            }
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
        match &self.backend {
            Backend::Managed { tracing, trace, .. } => {
                let _ = tracing.cancel(*trace);
            }
            Backend::Direct { .. } => {
                let _ = self.signal(libc::SIGKILL);
            }
        }
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
    /// Resolves with the native service's failure; a direct run has no such service.
    async fn failure(&self) -> ShellError {
        match &self.backend {
            Backend::Managed { tracing, trace, .. } => {
                std::future::poll_fn(|context| tracing.poll_failure(*trace, context))
                    .await
                    .into()
            }
            Backend::Direct { .. } => std::future::pending().await,
        }
    }
    /// Waits for every owned producer. `Err(true)` means some producer's end cannot be proven.
    async fn finish(&self) -> Result<(), (ShellError, bool)> {
        let failed = loop {
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
                break failed;
            }
            changed.await;
        };
        let children = match &self.backend {
            Backend::Direct { children } => children.lock().recover().clone(),
            Backend::Managed { .. } => Vec::new(),
        };
        let mut uncertain = None;
        for child in &children {
            if let Err(error) = child.readable().await {
                uncertain.get_or_insert(error);
            }
        }
        self.closed.store(true, Ordering::Release);
        self.changed.notify_waiters();
        if let Some(error) = uncertain {
            return Err((error.into(), true));
        }
        if failed {
            return Err((
                ShellError::infrastructure("an owned producer did not complete"),
                false,
            ));
        }
        Ok(())
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

thread_local! { static CURRENT: RefCell<Option<BuiltinContext>> = const { RefCell::new(None) }; }
struct ContextGuard(Option<BuiltinContext>);
impl ContextGuard {
    fn enter(context: BuiltinContext) -> Self {
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

/// Normal callback I/O and cancellation facilities. Retaining this never retains a work tree,
/// an admission, source membership or any later command's view.
#[derive(Clone)]
pub struct BuiltinContext {
    run: Arc<Run>,
    cwd: Arc<PathBuf>,
    builtin: Option<InvocationId>,
}
impl std::fmt::Debug for BuiltinContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltinContext")
            .field("working_dir", &self.cwd)
            .field("cancelled", &self.cancellation_requested())
            .finish_non_exhaustive()
    }
}
/// Returns the context of the builtin currently executing on this thread or task poll.
pub fn current_context() -> Option<BuiltinContext> {
    CURRENT.with(|current| current.borrow().clone())
}

/// `path` in `view`, or `path` itself on the direct route.
fn physical(view: Option<&Snapshot>, path: &Path) -> PathBuf {
    view.map_or_else(|| path.to_path_buf(), |snapshot| snapshot.physical(path))
}
/// `path` as the caller names it, from `view` or from the source itself.
fn logical(view: Option<&Snapshot>, path: &Path) -> PathBuf {
    view.map_or_else(|| path.to_path_buf(), |snapshot| snapshot.logical(path))
}

impl BuiltinContext {
    pub(super) const fn invocation(&self) -> Option<InvocationId> {
        self.builtin
    }
    /// The owning run, as a nested call's admission sees its caller.
    pub(super) fn parent(&self) -> Weak<Run> {
        Arc::downgrade(&self.run)
    }
    /// The owning run, while it still accepts work.
    pub(super) fn run(&self) -> Result<&Arc<Run>, ShellError> {
        self.check()?;
        Ok(&self.run)
    }
    /// The managed view of the owning run; `None` only for a direct run.
    pub(super) fn snapshot(&self) -> Result<Option<Arc<Snapshot>>, ShellError> {
        self.check()?;
        match &self.run.backend {
            Backend::Managed { snapshot, .. } => snapshot
                .upgrade()
                .map(Some)
                .ok_or_else(|| ShellError::new(ShellErrorKind::Closed)),
            Backend::Direct { .. } => Ok(None),
        }
    }
    fn check(&self) -> Result<(), ShellError> {
        self.run.check()
    }
    /// The workload scope of a managed run; direct work is never attributed.
    fn scope(&self) -> Result<Option<TraceScope>, ShellError> {
        match &self.run.backend {
            Backend::Managed { tracing, trace, .. } => {
                Ok(Some(tracing.scope(*trace, self.builtin)?))
            }
            Backend::Direct { .. } => Ok(None),
        }
    }
    fn enter(&self) -> Result<SyncGuard, ShellError> {
        self.check()?;
        let scope = self.scope()?;
        let context = ContextGuard::enter(self.clone());
        Ok(SyncGuard {
            _trace: scope.as_ref().map(TraceScope::enter),
            _context: context,
        })
    }
    fn wrap<F: Future>(&self, future: F) -> Result<Scoped<F, ContextScope>, ShellError> {
        self.check()?;
        Ok(Scoped::new(
            future,
            ContextScope {
                scope: self.scope()?,
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
        let scope = self.scope()?;
        let internal = self.run.internal_scope()?;
        let _guard = internal.as_ref().map(TraceScope::enter);
        let handle = self.run.runtime.spawn_blocking(move || {
            let _trace = scope.as_ref().map(TraceScope::enter);
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
    /// Runs `operation` against the owning run's filesystem view as one registered, scoped step;
    /// `ended` is the failure once a managed view has gone.
    fn step<T>(
        &self,
        ended: &str,
        operation: impl FnOnce(Option<&Snapshot>) -> T,
    ) -> std::io::Result<T> {
        let mut lease = self.run.lease().map_err(std::io::Error::other)?;
        let _guard = self.enter().map_err(std::io::Error::other)?;
        let view = match &self.run.backend {
            Backend::Managed { snapshot, .. } => Some(
                snapshot
                    .upgrade()
                    .ok_or_else(|| std::io::Error::other(ended))?,
            ),
            Backend::Direct { .. } => None,
        };
        let result = operation(view.as_deref());
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
        self.step("command context has ended", |view| {
            operation(&physical(view, &logical))
        })?
    }
    /// Opens a logical path in the owning command's filesystem view.
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
        let view = self.snapshot()?;
        let _guard = self.enter()?;
        let not_utf8 = || ShellError::unsupported("glob path is not UTF-8");
        // Only the caller's pattern is glob syntax; the directory it is relative to is a literal
        // path, and a `[` or `*` in its name must not turn it into a character class or wildcard.
        let pattern = if Path::new(pattern).is_absolute() {
            let physical = physical(view.as_deref(), Path::new(pattern));
            physical.to_str().ok_or_else(not_utf8)?.to_owned()
        } else {
            let base = physical(view.as_deref(), cwd.unwrap_or(&self.cwd));
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
    context: BuiltinContext,
}
impl Iterator for ReadDir {
    type Item = std::io::Result<DirectoryEntry>;
    fn next(&mut self) -> Option<Self::Item> {
        self.context
            .step("ended directory context", |view| {
                self.inner.next().map(|entry| {
                    entry.map(|entry| DirectoryEntry {
                        path: logical(view, &entry.path()),
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
    context: BuiltinContext,
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
/// Logical results from a glob in the command's view.
pub struct GlobPaths {
    inner: glob::Paths,
    context: BuiltinContext,
}
impl Iterator for GlobPaths {
    type Item = std::io::Result<PathBuf>;
    fn next(&mut self) -> Option<Self::Item> {
        self.context
            .step("ended glob context", |view| {
                self.inner.next().map(|path| {
                    path.map(|path| logical(view, &path)).map_err(|error| {
                        std::io::Error::new(error.error().kind(), error.error().to_string())
                    })
                })
            })
            .unwrap_or_else(|error| Some(Err(error)))
    }
}

pub(super) struct SyncGuard {
    _trace: Option<TraceScopeGuard>,
    _context: ContextGuard,
}
/// Per-poll command context: TLS is entered before tracing and restored after it.
struct ContextScope {
    scope: Option<TraceScope>,
    context: BuiltinContext,
}
impl PollScope for ContextScope {
    type Guard = SyncGuard;
    fn enter(&self) -> Self::Guard {
        let context = ContextGuard::enter(self.context.clone());
        SyncGuard {
            _trace: self.scope.as_ref().map(TraceScope::enter),
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

/// Evaluates `command` exactly once under `run`, which every producer it starts inherits.
pub(super) async fn evaluate(
    interpreter: &mut brush_core::Shell<ManagedExtensions>,
    run: &Arc<Run>,
    command: super::Command,
) -> (Option<ExecutionResult>, Option<ShellError>) {
    let context = BuiltinContext {
        run: Arc::clone(run),
        cwd: Arc::clone(&run.cwd),
        builtin: None,
    };
    let evaluation = match context.wrap(command.evaluate(interpreter)) {
        Ok(evaluation) => evaluation,
        Err(error) => return (None, Some(error)),
    };
    let evaluated = tokio::select! {
        outcome = std::panic::AssertUnwindSafe(evaluation).catch_unwind() => match outcome {
            Ok(result) => result.map_err(ShellError::from),
            Err(_) => Err(ShellError::infrastructure("command producer panicked")),
        },
        () = run.cancelled() => Err(ShellError::new(ShellErrorKind::Interrupted)),
        error = run.failure() => Err(error),
    };
    match evaluated {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(error)),
    }
}

pub(super) struct ExecutedCommand {
    pub prepared: Completion<PreparedCommand>,
    pub result: Option<ExecutionResult>,
    pub failure: Option<ShellError>,
    pub evidence: CommandEvidence,
    pub command: String,
    /// Producer quiescence could not be proven; the view and its coverage must be retained.
    pub uncertain: bool,
}

pub(super) async fn run(
    interpreter: &mut brush_core::Shell<ManagedExtensions>,
    prepared: Completion<PreparedCommand>,
    command: super::Command,
    text: String,
) -> ExecutedCommand {
    let (result, failure) = evaluate(interpreter, &prepared.run, command).await;
    complete(prepared, result, failure, text).await
}

/// Drains a direct run: its tasks, and the children it spawned. Returns the native result, or
/// the failure and whether producer quiescence is uncertain.
pub(super) async fn complete_direct(
    run: &Run,
    result: Option<ExecutionResult>,
    mut failure: Option<ShellError>,
) -> (Result<ExecutionResult, ShellError>, bool) {
    if failure.is_some() {
        run.cancel();
    }
    let mut uncertain = false;
    if let Err((error, unproven)) = run.finish().await {
        uncertain = unproven;
        failure.get_or_insert(error);
    }
    if run.is_cancelled() {
        failure.get_or_insert_with(|| ShellError::new(ShellErrorKind::Interrupted));
    }
    let outcome = match (failure, result) {
        (None, Some(result)) => Ok(result),
        (Some(failure), result) => Err(failure.with_result(result)),
        (None, None) => Err(ShellError::infrastructure(
            "accepted execution has no native result",
        )),
    };
    (outcome, uncertain)
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
    if let Err((error, _)) = prepared.run.finish().await {
        failure.get_or_insert(error);
    }
    let mut uncertain = false;
    let mut evidence = CommandEvidence::default();
    match prepared.run.trace() {
        Err(error) => {
            failure.get_or_insert(error);
        }
        Ok((tracing, trace)) => {
            let tracing = Arc::clone(tracing);
            let drain = {
                let internal = tracing.internal_scope();
                let _guard = internal.as_ref().ok().map(TraceScope::enter);
                let tracing = Arc::clone(&tracing);
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
                    uncertain = true;
                }
                Err(error) => {
                    failure.get_or_insert_with(|| ShellError::infrastructure(error.to_string()));
                    prepared.snapshot.retained.store(true, Ordering::Release);
                    uncertain = true;
                }
            }
            if prepared.run.is_cancelled() {
                failure.get_or_insert_with(|| ShellError::new(ShellErrorKind::Interrupted));
            }
            match prepared.snapshot.take_evidence(trace) {
                Ok(taken) => evidence = taken,
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
            if tracing.health(trace).is_ok()
                && let Err(error) = tracing.end_run(trace)
            {
                failure.get_or_insert_with(|| error.into());
            }
        }
    }
    ExecutedCommand {
        prepared,
        result,
        failure,
        evidence,
        command: text,
        uncertain,
    }
}
