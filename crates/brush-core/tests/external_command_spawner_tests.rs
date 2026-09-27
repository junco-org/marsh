//! Integration tests for the shell's injected extension seams.
//!
//! * `ExternalCommandSpawner`: that it receives every external command the shell runs, that it
//!   can substitute the spawned process, and that the error it returns is mapped to the
//!   command's exit status.
//! * `ExecutionObserver`: that builtins run inside `run_builtin` (synchronous prefix and every
//!   poll), that every construct that schedules concurrent work does so through the observer
//!   and runs inside the task it scheduled, that the shell's public code-running entry points
//!   are scoped, and that a refusal fails the construct without running it at all.

#![cfg(test)]
#![allow(clippy::panic_in_result_fn)]

use std::cell::Cell;
use std::collections::HashMap;
use std::io::Write as _;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use anyhow::Result;
use brush_core::builtins::{BoxFuture, ContentOptions, ContentType, Registration};
use brush_core::extensions::{
    DefaultErrorFormatter, DefaultExternalCommandSpawner, ExecutionObserver,
    ExternalCommandSpawner, ShellExtensionsImpl,
};
use brush_core::{CommandArg, ExecutionContext, ExecutionResult, ShellVariable, SourceInfo};

/// Returns the string value of the named shell variable, if it's set.
fn var<SE: brush_core::ShellExtensions>(
    shell: &brush_core::Shell<SE>,
    name: &str,
) -> Option<String> {
    shell
        .env_var(name)
        .map(|v| v.value().to_cow_str(shell).into_owned())
}

/// A spawner that records the program and arguments of every command it is asked to spawn,
/// then spawns it unchanged.
#[derive(Clone, Default)]
struct RecordingSpawner {
    seen: Arc<Mutex<Vec<String>>>,
}

impl ExternalCommandSpawner for RecordingSpawner {
    fn spawn(
        &self,
        command: std::process::Command,
        kill_on_drop: bool,
    ) -> std::io::Result<brush_core::sys::process::Child> {
        let program = std::path::Path::new(command.get_program())
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let argv = std::iter::once(program)
            .chain(command.get_args().map(|a| a.to_string_lossy().into_owned()))
            .collect::<Vec<_>>()
            .join(" ");
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(argv);

        DefaultExternalCommandSpawner.spawn(command, kill_on_drop)
    }
}

#[tokio::test]
async fn custom_spawner_sees_every_external_command() -> Result<()> {
    let spawner = RecordingSpawner::default();

    let mut shell = brush_core::Shell::builder_with_extensions::<
        ShellExtensionsImpl<DefaultErrorFormatter, RecordingSpawner>,
    >()
    .external_command_spawner(spawner.clone())
    .build()
    .await?;

    let params = shell.default_exec_params();
    shell
        .run_string(
            "f() { true inner; }; f; true a b | true c; x=$(true subst)",
            &brush_core::SourceInfo::default(),
            &params,
        )
        .await?;

    let seen = spawner
        .seen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();

    assert_eq!(seen, vec!["true inner", "true a b", "true c", "true subst"]);

    Ok(())
}

/// A spawner that rewrites or refuses commands based on their first argument: `flip` swaps
/// the program for `false`, `missing` reports the program as not found, and anything else is
/// spawned unchanged.
#[derive(Clone, Default)]
struct RewritingSpawner;

impl ExternalCommandSpawner for RewritingSpawner {
    fn spawn(
        &self,
        command: std::process::Command,
        kill_on_drop: bool,
    ) -> std::io::Result<brush_core::sys::process::Child> {
        match command.get_args().next().and_then(|a| a.to_str()) {
            Some("flip") => DefaultExternalCommandSpawner
                .spawn(std::process::Command::new("false"), kill_on_drop),
            Some("missing") => Err(std::io::ErrorKind::NotFound.into()),
            _ => DefaultExternalCommandSpawner.spawn(command, kill_on_drop),
        }
    }
}

#[tokio::test]
async fn custom_spawner_result_becomes_exit_status() -> Result<()> {
    let mut shell = brush_core::Shell::builder_with_extensions::<
        ShellExtensionsImpl<DefaultErrorFormatter, RewritingSpawner>,
    >()
    .build()
    .await?;

    let params = shell.default_exec_params();
    shell
        .run_string(
            "true flip; flipped=$?; true missing; missing=$?; true; ok=$?",
            &brush_core::SourceInfo::default(),
            &params,
        )
        .await?;

    assert_eq!(var(&shell, "flipped").as_deref(), Some("1"));
    assert_eq!(var(&shell, "missing").as_deref(), Some("127"));
    assert_eq!(var(&shell, "ok").as_deref(), Some("0"));

    Ok(())
}

/// Nesting depth, on the current thread, of each region the [`TestObserver`] brackets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Depths {
    /// Polls of futures wrapped by `scope_future`.
    scope: usize,
    /// Live guards returned by `enter_sync`.
    sync: usize,
    /// `run_builtin`: the execute function's synchronous prefix and every poll of its future.
    builtin: usize,
    /// Polls of futures scheduled by `spawn_task`.
    task: usize,
    /// Operations scheduled by `spawn_blocking_task`.
    blocking: usize,
}

/// One of the regions tracked by [`Depths`].
#[derive(Clone, Copy)]
enum Region {
    Scope,
    Sync,
    Builtin,
    Task,
    Blocking,
}

thread_local! {
    static DEPTHS: Cell<Depths> = const {
        Cell::new(Depths { scope: 0, sync: 0, builtin: 0, task: 0, blocking: 0 })
    };
}

/// Returns the region depths of the current thread.
fn current_depths() -> Depths {
    DEPTHS.with(Cell::get)
}

/// Marks the current thread as inside a region until dropped.
struct Inside(Region);

impl Inside {
    fn new(region: Region) -> Self {
        Self::adjust(region, true);
        Self(region)
    }

    fn adjust(region: Region, entering: bool) {
        DEPTHS.with(|cell| {
            let mut depths = cell.get();
            let depth = match region {
                Region::Scope => &mut depths.scope,
                Region::Sync => &mut depths.sync,
                Region::Builtin => &mut depths.builtin,
                Region::Task => &mut depths.task,
                Region::Blocking => &mut depths.blocking,
            };
            if entering {
                *depth += 1;
            } else {
                *depth -= 1;
            }
            cell.set(depths);
        });
    }
}

impl Drop for Inside {
    fn drop(&mut self) {
        Self::adjust(self.0, false);
    }
}

/// The test observer's sync guard. It is deliberately `!Send`, like a guard bound to the thread
/// that entered it, so that any shell future holding one across an `.await` would fail to
/// satisfy the `Send` bounds of the shell's own scheduling.
struct SyncSection {
    _inside: Inside,
    _thread_bound: PhantomData<*const ()>,
}

/// Future wrapper that marks the polling thread as inside a region for the duration of each
/// poll.
struct Bracketed<F> {
    region: Region,
    inner: Pin<Box<F>>,
}

impl<F> Bracketed<F> {
    fn new(region: Region, inner: F) -> Self {
        Self {
            region,
            inner: Box::pin(inner),
        }
    }
}

impl<F: Future> Future for Bracketed<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let _inside = Inside::new(this.region);
        this.inner.as_mut().poll(cx)
    }
}

/// A builtin invocation as announced to `begin_builtin`.
#[derive(Clone, Debug)]
struct Invocation {
    argv: String,
    cwd: PathBuf,
}

/// What the `mark` builtin observed about where it ran.
#[derive(Clone, Debug)]
struct Mark {
    name: String,
    /// Depths during the execute function's synchronous prefix.
    prefix: Depths,
    /// Depths during a poll after the builtin's future first suspended.
    polled: Depths,
}

/// Shared state of a [`TestObserver`] and its clones.
#[derive(Default)]
struct ObserverState {
    refuse_scopes: AtomicBool,
    refuse_sync: AtomicBool,
    refuse_builtins: AtomicBool,
    refuse_spawns: AtomicBool,
    syncs: AtomicUsize,
    tasks: AtomicUsize,
    blocking: AtomicUsize,
    finished: AtomicUsize,
    /// Scope depth of the scheduling thread at each accepted `spawn_task`/`spawn_blocking_task`.
    scheduled_from_scope: Mutex<Vec<usize>>,
    invocations: Mutex<Vec<Invocation>>,
    /// Names passed to `mark` whose execute function was called at all.
    prefixes: Mutex<Vec<String>>,
    marks: Mutex<Vec<Mark>>,
}

/// Locks `mutex`, ignoring poisoning from an earlier failed assertion.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An observer that brackets every region it is handed (see [`Depths`]), records what it is
/// asked to do, tracks the completion of the work it schedules, and refuses on request.
#[derive(Clone, Default)]
struct TestObserver {
    state: Arc<ObserverState>,
}

impl TestObserver {
    fn refusal(hook: &str) -> brush_core::Error {
        brush_core::ErrorKind::InternalError(format!("observer refused {hook}")).into()
    }

    fn refuse(&self, flag: fn(&ObserverState) -> &AtomicBool, refuse: bool) {
        flag(&self.state).store(refuse, Ordering::SeqCst);
    }

    fn refuses(flag: &AtomicBool) -> bool {
        flag.load(Ordering::SeqCst)
    }

    fn syncs(&self) -> usize {
        self.state.syncs.load(Ordering::SeqCst)
    }

    /// Numbers of accepted `spawn_task` and `spawn_blocking_task` requests.
    fn scheduled(&self) -> (usize, usize) {
        (
            self.state.tasks.load(Ordering::SeqCst),
            self.state.blocking.load(Ordering::SeqCst),
        )
    }

    fn scheduled_from_scope(&self) -> Vec<usize> {
        lock(&self.state.scheduled_from_scope).clone()
    }

    fn invocations(&self) -> Vec<Invocation> {
        lock(&self.state.invocations).clone()
    }

    fn prefixes(&self) -> Vec<String> {
        lock(&self.state.prefixes).clone()
    }

    fn mark(&self, name: &str) -> Result<Mark> {
        lock(&self.state.marks)
            .iter()
            .find(|mark| mark.name == name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("`mark {name}` never ran"))
    }

    /// Yields to the runtime until all scheduled work has finished, up to a fixed number of
    /// yields; returns whether it did.
    async fn settle(&self) -> bool {
        for _ in 0..100_000 {
            let (tasks, blocking) = self.scheduled();
            if self.state.finished.load(Ordering::SeqCst) == tasks + blocking {
                return true;
            }
            tokio::task::yield_now().await;
        }
        false
    }

    fn record_scheduling(&self, counter: &AtomicUsize) {
        counter.fetch_add(1, Ordering::SeqCst);
        lock(&self.state.scheduled_from_scope).push(current_depths().scope);
    }
}

impl ExecutionObserver for TestObserver {
    type Builtin = Invocation;
    type SyncGuard = SyncSection;

    fn enter_sync(&self) -> Result<Self::SyncGuard, brush_core::Error> {
        if Self::refuses(&self.state.refuse_sync) {
            return Err(Self::refusal("enter_sync"));
        }
        self.state.syncs.fetch_add(1, Ordering::SeqCst);
        Ok(SyncSection {
            _inside: Inside::new(Region::Sync),
            _thread_bound: PhantomData,
        })
    }

    fn begin_builtin(&self, name: &str, args: &[CommandArg], cwd: &Path) -> Self::Builtin {
        let argv = args.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(argv.first().map(String::as_str), Some(name));
        Invocation {
            argv: argv.join(" "),
            cwd: cwd.to_owned(),
        }
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
        let state = Arc::clone(&self.state);
        async move {
            if Self::refuses(&state.refuse_builtins) {
                return Err(Self::refusal("run_builtin"));
            }
            lock(&state.invocations).push(token);
            let future = {
                let _inside = Inside::new(Region::Builtin);
                make()
            };
            Bracketed::new(Region::Builtin, future).await
        }
    }

    fn scope_future<F>(
        &self,
        future: F,
    ) -> Result<impl Future<Output = F::Output> + Send + use<F>, brush_core::Error>
    where
        F: Future + Send,
    {
        if Self::refuses(&self.state.refuse_scopes) {
            return Err(Self::refusal("scope_future"));
        }
        Ok(Bracketed::new(Region::Scope, future))
    }

    fn spawn_task<F>(
        &self,
        future: F,
    ) -> Result<tokio::task::JoinHandle<F::Output>, brush_core::Error>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if Self::refuses(&self.state.refuse_spawns) {
            return Err(Self::refusal("spawn_task"));
        }
        self.record_scheduling(&self.state.tasks);
        let state = Arc::clone(&self.state);
        Ok(tokio::spawn(async move {
            let output = Bracketed::new(Region::Task, future).await;
            state.finished.fetch_add(1, Ordering::SeqCst);
            output
        }))
    }

    fn spawn_blocking_task<F, T>(
        &self,
        operation: F,
    ) -> Result<tokio::task::JoinHandle<T>, brush_core::Error>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        if Self::refuses(&self.state.refuse_spawns) {
            return Err(Self::refusal("spawn_blocking_task"));
        }
        self.record_scheduling(&self.state.blocking);
        let state = Arc::clone(&self.state);
        Ok(tokio::task::spawn_blocking(move || {
            let output = {
                let _inside = Inside::new(Region::Blocking);
                operation()
            };
            state.finished.fetch_add(1, Ordering::SeqCst);
            output
        }))
    }
}

type ObservedExtensions =
    ShellExtensionsImpl<DefaultErrorFormatter, DefaultExternalCommandSpawner, TestObserver>;
type ObservedShell = brush_core::Shell<ObservedExtensions>;

/// Execute function of `mark NAME`: records the regions its synchronous prefix and its
/// resumed future run in, then prints NAME.
fn exec_mark(
    context: ExecutionContext<'_, ObservedExtensions>,
    args: Vec<CommandArg>,
) -> BoxFuture<'_, Result<ExecutionResult, brush_core::Error>> {
    let prefix = current_depths();
    let name = args
        .into_iter()
        .nth(1)
        .map(|arg| arg.to_string())
        .unwrap_or_default();
    lock(&context.shell.execution_observer().state.prefixes).push(name.clone());

    Box::pin(async move {
        // Suspend once, so that the rest of the body runs in a later poll.
        tokio::task::yield_now().await;

        let polled = current_depths();
        lock(&context.shell.execution_observer().state.marks).push(Mark {
            name: name.clone(),
            prefix,
            polled,
        });
        writeln!(context.stdout(), "{name}")?;
        Ok(ExecutionResult::success())
    })
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "the signature of a builtin registration's content function"
)]
fn mark_content(
    name: &str,
    _content_type: ContentType,
    _options: &ContentOptions,
) -> Result<String, brush_core::Error> {
    Ok(format!("{name}: records where it runs\n"))
}

/// The registration of the `mark` builtin.
const fn mark_registration() -> Registration<ObservedExtensions> {
    Registration {
        execute_func: exec_mark,
        content_func: mark_content,
        disabled: false,
        special_builtin: false,
        declaration_builtin: false,
    }
}

/// Descriptors of an observed shell: standard output discarded, standard error written to
/// `stderr`.
fn observed_fds(
    stderr: &tempfile::NamedTempFile,
) -> Result<HashMap<brush_core::ShellFd, brush_core::openfiles::OpenFile>> {
    Ok(HashMap::from([
        (1, brush_core::openfiles::null()?),
        (2, stderr.reopen()?.into()),
    ]))
}

/// Creates a shell observed by `observer`, with the `mark` builtin as its only builtin and the
/// descriptors of [`observed_fds`].
async fn observed_shell(
    observer: &TestObserver,
    stderr: &tempfile::NamedTempFile,
) -> Result<ObservedShell> {
    Ok(
        brush_core::Shell::builder_with_extensions::<ObservedExtensions>()
            .execution_observer(observer.clone())
            .builtin("mark", mark_registration())
            .fds(observed_fds(stderr)?)
            .build()
            .await?,
    )
}

/// Runs `line` as a program string, returning its exit status.
async fn run(shell: &mut ObservedShell, line: &str) -> Result<u8> {
    let params = shell.default_exec_params();
    let result = shell
        .run_string(line, &SourceInfo::default(), &params)
        .await?;
    Ok(result.exit_code.into())
}

/// Passes `future` through, requiring it to be `Send` even though the observer's sync guard is
/// not.
const fn send<F: Future + Send>(future: F) -> F {
    future
}

#[tokio::test]
async fn builtins_run_inside_run_builtin() -> Result<()> {
    let observer = TestObserver::default();
    let stderr = tempfile::NamedTempFile::new()?;
    let mut shell = observed_shell(&observer, &stderr).await?;

    assert_eq!(run(&mut shell, "mark top 'second arg'").await?, 0);

    let invocations = observer.invocations();
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0].argv, "mark top second arg");
    assert_eq!(invocations[0].cwd, shell.working_dir());

    // Both the execute function's synchronous prefix and the poll that resumed its future ran
    // inside `run_builtin`, within the single scope of `run_string`.
    let inside = Depths {
        scope: 1,
        builtin: 1,
        ..Depths::default()
    };
    let mark = observer.mark("top")?;
    assert_eq!((mark.prefix, mark.polled), (inside, inside));

    Ok(())
}

/// A line, the (tasks, blocking tasks) it schedules, and the `mark`s (with the depths they
/// observe) that must have run inside that scheduled work.
type ScheduledCase<'a> = (&'a str, (usize, usize), &'a [(&'a str, Depths)]);

#[tokio::test]
async fn concurrent_work_runs_inside_observer_scheduled_tasks() -> Result<()> {
    let observer = TestObserver::default();
    let stderr = tempfile::NamedTempFile::new()?;
    let mut shell = observed_shell(&observer, &stderr).await?;

    let in_task = Depths {
        task: 1,
        builtin: 1,
        ..Depths::default()
    };
    let in_blocking = Depths {
        blocking: 1,
        builtin: 1,
        ..Depths::default()
    };
    let cases: [ScheduledCase<'_>; 5] = [
        ("subst=$(mark subst)", (1, 0), &[("subst", in_task)]),
        ("mark bg &", (1, 0), &[("bg", in_task)]),
        ("mark consumer <(mark ps)", (1, 0), &[("ps", in_task)]),
        (
            "mark p1 | mark p2",
            (0, 2),
            &[("p1", in_blocking), ("p2", in_blocking)],
        ),
        ("coproc mark co", (1, 0), &[("co", in_task)]),
    ];

    for (line, expected, marks) in cases {
        let before = observer.scheduled();
        assert_eq!(run(&mut shell, line).await?, 0, "{line}");
        assert!(
            observer.settle().await,
            "{line}: scheduled work never finished"
        );

        let after = observer.scheduled();
        assert_eq!((after.0 - before.0, after.1 - before.1), expected, "{line}");
        for &(name, depths) in marks {
            assert_eq!(observer.mark(name)?.polled, depths, "{line}: {name}");
        }
    }

    assert_eq!(var(&shell, "subst").as_deref(), Some("subst"));
    // All of it was scheduled by code running within `run_string`'s scope.
    assert_eq!(observer.scheduled_from_scope(), [1; 6]);

    Ok(())
}

#[tokio::test]
async fn refused_scheduling_fails_the_construct_without_running_it() -> Result<()> {
    let observer = TestObserver::default();
    let stderr = tempfile::NamedTempFile::new()?;
    let mut shell = observed_shell(&observer, &stderr).await?;
    observer.refuse(|state| &state.refuse_spawns, true);

    for line in [
        "subst=$(mark subst)",
        "mark bg &",
        "mark consumer <(mark ps)",
        "mark p1 | mark p2",
        "coproc mark co",
    ] {
        let open_fds = shell.open_files().iter_fds().count();
        assert_ne!(run(&mut shell, line).await?, 0, "{line}");
        assert_eq!(
            shell.open_files().iter_fds().count(),
            open_fds,
            "{line}: descriptors of the refused construct were left open"
        );
    }

    // No part of any refused construct ran, observed or otherwise.
    assert_eq!(observer.scheduled(), (0, 0));
    assert!(observer.prefixes().is_empty(), "{:?}", observer.prefixes());
    assert_eq!(var(&shell, "subst"), None);
    assert_eq!(var(&shell, "COPROC"), None);

    // Each refusal was reported as the error of the construct that needed the work; like any
    // failing pipeline stage, each stage of `mark p1 | mark p2` fails (and is reported) alone.
    let errors = std::fs::read_to_string(stderr.path())?;
    assert_eq!(
        errors.matches("observer refused spawn_task").count(),
        4,
        "{errors}"
    );
    assert_eq!(
        errors
            .matches("observer refused spawn_blocking_task")
            .count(),
        2,
        "{errors}"
    );

    Ok(())
}

#[tokio::test]
async fn refused_builtin_never_calls_its_execute_function() -> Result<()> {
    let observer = TestObserver::default();
    let stderr = tempfile::NamedTempFile::new()?;
    let mut shell = observed_shell(&observer, &stderr).await?;
    observer.refuse(|state| &state.refuse_builtins, true);

    assert_ne!(run(&mut shell, "mark refused").await?, 0);

    assert!(observer.prefixes().is_empty(), "{:?}", observer.prefixes());
    let errors = std::fs::read_to_string(stderr.path())?;
    assert!(errors.contains("observer refused run_builtin"), "{errors}");

    Ok(())
}

#[tokio::test]
async fn public_entry_points_run_in_observer_scopes() -> Result<()> {
    let observer = TestObserver::default();
    let stderr = tempfile::NamedTempFile::new()?;
    let mut shell = observed_shell(&observer, &stderr).await?;
    let params = shell.default_exec_params();
    let in_scope = Depths {
        scope: 1,
        builtin: 1,
        ..Depths::default()
    };

    run(
        &mut shell,
        "f() { mark function; }; compfn() { mark completion; }; PS1='$(mark prompt)'",
    )
    .await?;

    let status = send(shell.invoke_function("f", std::iter::empty::<&str>(), &params)).await?;
    assert_eq!(status, 0);
    assert_eq!(observer.mark("function")?.polled, in_scope);

    // A prompt's command substitution is scheduled from within the prompt's scope.
    let scheduled = observer.scheduled_from_scope().len();
    assert_eq!(send(shell.compose_prompt()).await?, "prompt");
    assert_eq!(observer.scheduled_from_scope()[scheduled..], [1]);

    shell.traps_mut().register_handler(
        brush_core::traps::TrapSignal::Exit,
        "mark exit".into(),
        SourceInfo::default(),
    );
    send(shell.on_exit()).await?;
    assert_eq!(observer.mark("exit")?.polled, in_scope);

    shell.completion_config_mut().set(
        "cmd",
        brush_core::completion::Spec {
            function_name: Some("compfn".into()),
            ..brush_core::completion::Spec::default()
        },
    );
    send(shell.complete("cmd ", 4)).await?;
    let completion = observer.mark("completion")?.polled;
    assert!(
        completion.scope >= 1 && completion.builtin == 1,
        "{completion:?}"
    );

    // Opening and parsing the script is one sync section; its program runs in a scope.
    let script = tempfile::NamedTempFile::new()?;
    std::fs::write(script.path(), "mark sourced\n")?;
    let syncs = observer.syncs();
    send(shell.source_script(script.path(), std::iter::empty::<String>(), &params)).await?;
    assert_eq!(observer.syncs(), syncs + 1);
    assert_eq!(observer.mark("sourced")?.polled, in_scope);

    Ok(())
}

#[tokio::test]
async fn refused_scopes_and_sync_sections_run_nothing() -> Result<()> {
    let observer = TestObserver::default();
    let stderr = tempfile::NamedTempFile::new()?;
    let mut shell = observed_shell(&observer, &stderr).await?;
    let params = shell.default_exec_params();
    run(&mut shell, "f() { mark function; }").await?;

    observer.refuse(|state| &state.refuse_scopes, true);
    assert!(
        shell
            .run_string("mark top", &SourceInfo::default(), &params)
            .await
            .is_err()
    );
    assert!(
        shell
            .invoke_function("f", std::iter::empty::<&str>(), &params)
            .await
            .is_err()
    );
    assert!(shell.compose_prompt().await.is_err());
    assert!(shell.complete("mark ", 5).await.is_err());
    observer.refuse(|state| &state.refuse_scopes, false);

    observer.refuse(|state| &state.refuse_sync, true);
    let script = tempfile::NamedTempFile::new()?;
    std::fs::write(script.path(), "mark sourced\n")?;
    assert!(
        shell
            .source_script(script.path(), std::iter::empty::<String>(), &params)
            .await
            .is_err()
    );

    assert!(observer.prefixes().is_empty(), "{:?}", observer.prefixes());

    Ok(())
}

#[tokio::test]
async fn history_file_io_runs_in_sync_sections() -> Result<()> {
    let histfile = tempfile::NamedTempFile::new()?;
    std::fs::write(histfile.path(), "mark loaded\n")?;
    let observer = TestObserver::default();

    let mut shell = brush_core::Shell::builder_with_extensions::<ObservedExtensions>()
        .execution_observer(observer.clone())
        .enable_option("history")
        .var(
            "HISTFILE",
            ShellVariable::new(histfile.path().to_string_lossy().into_owned()),
        )
        .build()
        .await?;

    // Loading the history file while constructing the shell was a sync section.
    assert_eq!(observer.syncs(), 1);
    let loaded = shell
        .history()
        .map(|history| {
            history
                .iter()
                .map(|item| item.command_line.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert_eq!(loaded, ["mark loaded"]);

    shell.add_to_history("mark saved")?;
    shell.save_history()?;
    assert_eq!(observer.syncs(), 2);
    assert!(std::fs::read_to_string(histfile.path())?.contains("mark saved"));

    // A refused sync section leaves the history file untouched.
    observer.refuse(|state| &state.refuse_sync, true);
    shell.add_to_history("mark refused")?;
    assert!(shell.save_history().is_err());
    assert!(!std::fs::read_to_string(histfile.path())?.contains("mark refused"));

    Ok(())
}

#[tokio::test]
async fn startup_files_loaded_by_the_builder_run_in_observer_scopes() -> Result<()> {
    let observer = TestObserver::default();
    let stderr = tempfile::NamedTempFile::new()?;
    let histfile = tempfile::NamedTempFile::new()?;
    let rc = tempfile::NamedTempFile::new()?;
    std::fs::write(rc.path(), "mark rc\n")?;

    brush_core::Shell::builder_with_extensions::<ObservedExtensions>()
        .execution_observer(observer.clone())
        .builtin("mark", mark_registration())
        .fds(observed_fds(&stderr)?)
        .interactive(true)
        .var(
            "HISTFILE",
            ShellVariable::new(histfile.path().to_string_lossy().into_owned()),
        )
        .profile(brush_core::ProfileLoadBehavior::Skip)
        .rc(brush_core::RcLoadBehavior::LoadCustom(rc.path().to_owned()))
        .build()
        .await?;

    // The rc file was opened and parsed in a sync section and ran within a scope.
    let rc_mark = observer.mark("rc")?.polled;
    assert!(rc_mark.scope >= 1 && rc_mark.builtin == 1, "{rc_mark:?}");
    assert_eq!(observer.syncs(), 2, "history load and rc file parse");

    Ok(())
}
