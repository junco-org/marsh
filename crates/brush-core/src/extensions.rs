//! Definition of shell behavior traits and defaults.

use std::path::Path;

use crate::{CommandArg, ExecutionResult, Shell, error, extensions, sys};

/// Trait for static shell extensions. Collects all associated types needed to
/// instantiate a shell into a single containing struct.
pub trait ShellExtensions: Clone + Default + Send + Sync + 'static {
    /// Type of the error behavior implementation.
    type ErrorFormatter: ErrorFormatter;
    /// Type of the external command spawner implementation.
    type ExternalCommandSpawner: ExternalCommandSpawner;
    /// Type of the execution observer implementation.
    type ExecutionObserver: ExecutionObserver;
}

/// Shell extensions implementation constructed from component types.
#[derive(Clone, Default)]
pub struct ShellExtensionsImpl<
    EF: ErrorFormatter = DefaultErrorFormatter,
    ECS: ExternalCommandSpawner = DefaultExternalCommandSpawner,
    EO: ExecutionObserver = DefaultExecutionObserver,
> {
    _marker: std::marker::PhantomData<(EF, ECS, EO)>,
}

impl<EF: ErrorFormatter, ECS: ExternalCommandSpawner, EO: ExecutionObserver> ShellExtensions
    for ShellExtensionsImpl<EF, ECS, EO>
{
    type ErrorFormatter = EF;
    type ExternalCommandSpawner = ECS;
    type ExecutionObserver = EO;
}

/// Default shell extensions implementation.
/// This is a type alias for the most common shell configuration.
pub type DefaultShellExtensions = ShellExtensionsImpl<DefaultErrorFormatter>;

/// Trait for defining shell error behaviors.
pub trait ErrorFormatter: Clone + Default + Send + Sync + 'static {
    /// Format the given error for display within the context of the provided shell.
    ///
    /// # Arguments
    ///
    /// * `error` - The error to format
    /// * `shell` - The shell context in which the error occurred.
    fn format_error(
        &self,
        error: &error::Error,
        shell: &Shell<impl extensions::ShellExtensions>,
    ) -> String {
        let _ = shell;
        std::format!("error: {error:#}\n")
    }
}

/// Trait for spawning the processes that run external commands.
///
/// The shell resolves a command name to a builtin, shell function, or external program on
/// its own; only the last of these reaches the spawner. By then the shell has composed a
/// [`std::process::Command`] carrying the resolved executable path, `argv`, environment,
/// working directory, file descriptors, and process-group settings. An implementation may
/// spawn it as-is, or build and spawn a different command in its place (e.g. wrapping the
/// program in a tracer, or running it on another host).
///
/// The returned [`Child`](sys::process::Child) is what the shell waits on and reports through
/// `$?`. A Tokio-backed child (`Child::from(tokio::process::Child)`) is reaped by the shell,
/// which also detects its stops with a `waitid` on its pid alone. A
/// [`Child::hosted`](sys::process::Child::hosted) child is never waited on by the shell: its
/// stops and exit are taken from the [`HostedChild`](sys::process::HostedChild)'s event channel,
/// so an embedder that ptraces the command from this process keeps sole ownership of its wait
/// status. An `Err` is mapped to the command's exit status:
/// [`NotFound`](std::io::ErrorKind::NotFound) is reported as command-not-found (127), anything
/// else as failed-to-execute (126).
///
/// An implementation is selected statically as the [`ShellExtensions::ExternalCommandSpawner`]
/// associated type; the instance the shell runs with is supplied via
/// [`CreateOptions::external_command_spawner`](crate::CreateOptions::external_command_spawner)
/// and cloned along with the shell (pipeline stages, subshells, command substitutions).
pub trait ExternalCommandSpawner: Clone + Default + Send + Sync + 'static {
    /// Spawns the given command.
    ///
    /// # Arguments
    ///
    /// * `command` - The fully composed command to spawn.
    /// * `kill_on_drop` - Whether the child should be killed when its handle is dropped. The
    ///   shell always passes `false`.
    fn spawn(
        &self,
        command: std::process::Command,
        kill_on_drop: bool,
    ) -> std::io::Result<sys::process::Child>;
}

/// Default external command spawner; spawns the command exactly as composed.
#[derive(Clone, Default)]
pub struct DefaultExternalCommandSpawner;

impl ExternalCommandSpawner for DefaultExternalCommandSpawner {
    fn spawn(
        &self,
        command: std::process::Command,
        kill_on_drop: bool,
    ) -> std::io::Result<sys::process::Child> {
        sys::process::spawn(command, kill_on_drop)
    }
}

/// One filesystem access the interpreter itself performed on a host thread, reported after it
/// happened through [`ExecutionObserver::host_access`].
///
/// Paths are absolute (already joined with the shell's working directory).
#[non_exhaustive]
pub enum HostAccess<'a> {
    /// An open; `Ok(fd)` is the raw descriptor of the just-opened file (still open during the
    /// call), `Err(errno)` otherwise.
    Open {
        /// The path that was opened.
        path: &'a Path,
        /// The opened descriptor, or the errno the open failed with.
        result: Result<std::os::fd::RawFd, i32>,
    },
    /// A metadata/existence probe (stat when `follow`, lstat otherwise). `errno` when it
    /// failed.
    Metadata {
        /// The path that was probed.
        path: &'a Path,
        /// Whether a final symbolic link was followed.
        follow: bool,
        /// The errno the probe failed with, if it failed.
        errno: Option<i32>,
    },
    /// A directory enumeration. `errno` when opening the directory failed.
    ReadDir {
        /// The directory that was enumerated.
        path: &'a Path,
        /// The errno opening the directory failed with, if it failed.
        errno: Option<i32>,
    },
    /// A file descriptor the shell kept from an earlier command (for example, from
    /// `exec 3> file`) is handed to this command's host-side reads and writes, which are not
    /// reported one by one.
    Descriptor {
        /// The raw descriptor, open during the call.
        fd: std::os::fd::RawFd,
    },
}

/// Trait for observing the work a shell performs in its host process.
///
/// The shell interprets commands in-process: builtins, shell functions, expansions, prompts,
/// completions and sourced files all run on the host's threads, and some constructs schedule
/// further work on other Tokio tasks or blocking threads. An observer is consulted at each of
/// those points so that an embedder can attribute everything the shell does to an operation of
/// its own and account for every piece of scheduled work until it finishes. Beyond *where* the
/// shell runs code, the observer is also told about each filesystem access the interpreter
/// itself performs on a host thread ([`host_access`](Self::host_access)), since such accesses
/// are invisible to any tracer of the external commands the shell spawns. It has no say over
/// the shell's semantics other than refusing to run a unit of work.
///
/// The hooks are:
///
/// * [`scope_future`](Self::scope_future) wraps a future the shell is about to drive on the
///   caller's task: running a program string, program or sourced script, invoking a function,
///   composing a prompt, generating completions, loading startup (profile/rc) files, and
///   running the `EXIT` trap. These are the public entry points an embedder, the shell's own
///   builder or an interactive front end calls on its own initiative. They also re-enter one
///   another (`eval`, `source`, trap handlers, completion functions, sourced startup files),
///   so a scope may be entered while an enclosing scope of the same shell is being polled.
/// * [`enter_sync`](Self::enter_sync) brackets synchronous host work that happens outside such
///   a future: loading and saving the history file, and opening and parsing a sourced script.
///   The guard is always dropped before the shell reaches an `.await`, so an implementation may
///   make it `!Send`.
/// * [`begin_builtin`](Self::begin_builtin) and [`run_builtin`](Self::run_builtin) bracket
///   every builtin invocation, including the synchronous prefix of its execute function.
/// * [`spawn_task`](Self::spawn_task) and [`spawn_blocking_task`](Self::spawn_blocking_task)
///   schedule the shell's concurrent work: background (`&`) lists, coprocesses, process
///   substitutions, command substitutions, and builtins run as a stage of a multi-command
///   pipeline. The shell never schedules work through any other path.
/// * [`host_access`](Self::host_access) reports, after the fact, each filesystem access the
///   interpreter performs itself: opens for redirections and sourced scripts, `test`/`[[`
///   probes, glob directory enumeration, `PATH` searches, working-directory changes.
///
/// The futures returned by [`run_builtin`](Self::run_builtin) and
/// [`scope_future`](Self::scope_future) capture exactly their type parameters
/// (`use<Self, F, Make>` / `use<Self, F>`): they borrow whatever the wrapped work borrows and
/// never borrow the observer, so an implementation moves any state it needs into them (and
/// writes the same captures, `use<F, Make>` / `use<F>`, on its impl). They carry no separate
/// lifetime parameter: an explicit `'a` with `F: 'a` bounds makes the shell's own futures fail
/// to prove `Send` once a concrete observer is selected (rust-lang/rust#100013).
///
/// Every fallible hook refuses by returning `Err`. A refused unit of work does not run at all
/// (neither observed nor unobserved); the error propagates exactly like any other error raised
/// by the construct that needed it, so the shell reports it and sets `$?` accordingly. An
/// implementation that wants a refusal to terminate a non-interactive shell returns an error
/// marked with [`Error::into_fatal`](error::Error::into_fatal).
///
/// An implementation is selected statically as the [`ShellExtensions::ExecutionObserver`]
/// associated type; the instance the shell runs with is supplied via
/// [`CreateOptions::execution_observer`](crate::CreateOptions::execution_observer) and cloned
/// along with the shell (pipeline stages, subshells, command substitutions).
pub trait ExecutionObserver: Clone + Default + Send + Sync + 'static {
    /// Token identifying one builtin invocation, produced by
    /// [`begin_builtin`](Self::begin_builtin) and consumed by [`run_builtin`](Self::run_builtin).
    type Builtin: Send + 'static;

    /// Guard returned by [`enter_sync`](Self::enter_sync); the synchronous section ends when it
    /// is dropped. The shell never holds it across an `.await`.
    type SyncGuard;

    /// Enters a synchronous section of host work, which lasts until the returned guard is
    /// dropped. Returning `Err` refuses the work; the shell then does not perform it.
    fn enter_sync(&self) -> Result<Self::SyncGuard, error::Error>;

    /// Announces a builtin invocation that is about to run.
    ///
    /// # Arguments
    ///
    /// * `name` - The name under which the builtin was resolved.
    /// * `args` - The builtin's arguments, including the command name itself as the first one.
    /// * `cwd` - The shell's working directory at the time of the invocation.
    fn begin_builtin(&self, name: &str, args: &[CommandArg], cwd: &Path) -> Self::Builtin;

    /// Runs the builtin invocation identified by `token`.
    ///
    /// `make` invokes the builtin's registered execute function and returns the future that
    /// completes the invocation; calling it runs the function's synchronous prefix. To run the
    /// builtin, an implementation calls `make` exactly once and resolves to the output of the
    /// resulting future, unchanged. To refuse it, an implementation resolves to `Err` without
    /// calling `make`, and the builtin does not run.
    fn run_builtin<F, Make>(
        &self,
        token: Self::Builtin,
        make: Make,
    ) -> impl Future<Output = Result<ExecutionResult, error::Error>> + Send + use<Self, F, Make>
    where
        F: Future<Output = Result<ExecutionResult, error::Error>> + Send,
        Make: FnOnce() -> F + Send;

    /// Wraps a future the shell is about to drive on the current task. The returned future
    /// must resolve to the output of `future`, unchanged. Returning `Err` refuses the work; the
    /// shell then drops `future` without polling it.
    fn scope_future<F>(
        &self,
        future: F,
    ) -> Result<impl Future<Output = F::Output> + Send + use<Self, F>, error::Error>
    where
        F: Future + Send;

    /// Schedules `future` to run concurrently as a Tokio task, returning its join handle.
    /// Returning `Err` refuses the work; the shell then drops `future` without polling it.
    ///
    /// The shell may drop the returned handle without awaiting it (a process substitution runs
    /// alongside its consumer and nobody waits for it); the task is then still running, and
    /// an implementation that needs to know when it finishes must track it itself.
    fn spawn_task<F>(&self, future: F) -> Result<tokio::task::JoinHandle<F::Output>, error::Error>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static;

    /// Schedules `operation` to run on a thread where blocking is acceptable, returning its
    /// join handle. Returning `Err` refuses the work; the shell then drops `operation` without
    /// calling it.
    fn spawn_blocking_task<F, T>(
        &self,
        operation: F,
    ) -> Result<tokio::task::JoinHandle<T>, error::Error>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static;

    /// Reports one filesystem access the interpreter just performed on the current thread.
    ///
    /// Called synchronously right after the access, never across an `.await`, on whatever
    /// thread performed it. Accesses made by external commands (after the spawner's
    /// [`spawn`](ExternalCommandSpawner::spawn)) are not reported. The default does nothing.
    fn host_access(&self, _access: HostAccess<'_>) {}
}

/// Default execution observer; runs and schedules everything exactly as the shell requests,
/// with unit tokens and guards and no added state or allocation.
#[derive(Clone, Default)]
pub struct DefaultExecutionObserver;

impl ExecutionObserver for DefaultExecutionObserver {
    type Builtin = ();
    type SyncGuard = ();

    fn enter_sync(&self) -> Result<Self::SyncGuard, error::Error> {
        Ok(())
    }

    fn begin_builtin(&self, name: &str, args: &[CommandArg], cwd: &Path) -> Self::Builtin {
        let _ = (name, args, cwd);
    }

    fn run_builtin<F, Make>(
        &self,
        token: Self::Builtin,
        make: Make,
    ) -> impl Future<Output = Result<ExecutionResult, error::Error>> + Send + use<F, Make>
    where
        F: Future<Output = Result<ExecutionResult, error::Error>> + Send,
        Make: FnOnce() -> F + Send,
    {
        let () = token;
        make()
    }

    fn scope_future<F>(
        &self,
        future: F,
    ) -> Result<impl Future<Output = F::Output> + Send + use<F>, error::Error>
    where
        F: Future + Send,
    {
        Ok(future)
    }

    fn spawn_task<F>(&self, future: F) -> Result<tokio::task::JoinHandle<F::Output>, error::Error>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        Ok(tokio::spawn(future))
    }

    fn spawn_blocking_task<F, T>(
        &self,
        operation: F,
    ) -> Result<tokio::task::JoinHandle<T>, error::Error>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        Ok(tokio::task::spawn_blocking(operation))
    }
}

/// Default shell error behavior implementation.
#[derive(Clone, Default)]
pub struct DefaultErrorFormatter;

impl ErrorFormatter for DefaultErrorFormatter {}

/// Trait for placeholder behavior (stub for future extension).
pub trait PlaceholderBehavior: Clone + Default + Send + Sync + 'static {}

/// Default placeholder implementation.
#[derive(Clone, Default)]
pub struct DefaultPlaceholder;

impl PlaceholderBehavior for DefaultPlaceholder {}
