//! Hooks into builtin command lifecycles, installed from outside brush-core.
//!
//! A builtin runs *inside* the shell process, so an external tracer sees only its syscalls and
//! never the fact that a builtin was invoked at all. An embedder that needs to attribute effects to
//! commands must therefore be told by the shell itself.
//!
//! brush-core has no hook point, and needs none: a [`Registration`]'s `execute_func` is a public
//! field holding a plain function pointer, so [`instrument`] can hand the shell a builtin map whose
//! entries call the originals with a `begin`/`end` pair around them. The shell is stock; only the
//! map it was built with is different.
//!
//! Implementations must be cheap and must not block: [`BuiltinHook::begin`] and
//! [`BuiltinHook::end`] run on the thread executing the builtin, in its critical path.

use std::any::Any;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use brush_core::builtins::{BoxFuture, CommandExecuteFunc, Registration};
use brush_core::extensions::ShellExtensions;
use brush_core::{CommandArg, ExecutionContext, ExecutionExitCode, ExecutionResult};

/// Notified around every builtin execution of an instrumented shell.
///
/// The hook is shared (`Arc`) and the installation is process-global, so builtins executed by a
/// subshell or by an owned-shell pipeline element report to the same installation as the parent's.
#[cfg_attr(test, mockall::automock)]
pub trait BuiltinHook: Send + Sync {
    /// Called immediately before the builtin named `name` executes with `argv` (including
    /// `argv[0]`) and the shell's logical working directory `cwd`.
    ///
    /// The returned id identifies this invocation; the matching [`Self::end`] echoes it.
    fn begin(&self, name: &str, argv: &[String], cwd: &Path) -> u64;

    /// Called after the invocation identified by `id` finished with exit code `exit`.
    ///
    /// Not called when the builtin panics or when the process is replaced (`exec`); an
    /// unterminated invocation is therefore observable, and meaningful, to the embedder.
    fn end(&self, id: u64, exit: u8);

    /// Whether the shell whose logical working directory is `cwd` must stop what it is doing.
    ///
    /// Asked immediately before each builtin runs, so an embedder that has learned the line must
    /// be evaluated again does not pay for the rest of it. `waker` is registered whatever the
    /// answer, so a driver that observed `false` and then suspended is still woken by a decision
    /// that lands right afterwards.
    ///
    /// The default is `false`: a hook that only records has nothing to interrupt.
    fn interrupted(&self, cwd: &Path, waker: &std::task::Waker) -> bool {
        let _ = (cwd, waker);
        false
    }
}

/// The hook and the original implementations one [`instrument`] call put in place.
struct Installed {
    /// Who to notify.
    hook: Arc<dyn BuiltinHook>,
    /// The `execute_func` each wrapped registration had before it was wrapped, as a
    /// `HashMap<String, CommandExecuteFunc<SE>>` for the `SE` the map was instrumented with.
    ///
    /// Erased because a `static` cannot be generic while [`CommandExecuteFunc`] must be reached
    /// from a non-capturing `fn`. A [`CommandExecuteFunc`] is a bare `'static` function pointer and
    /// [`ShellExtensions`] is `'static`, so the map really is `Any + Send + Sync`.
    originals: Box<dyn Any + Send + Sync>,
}

/// The current installation.
///
/// A `static` because [`CommandExecuteFunc`] is a bare `fn` pointer: the wrapper cannot capture the
/// hook, so it has to find it. That makes instrumentation process-global — one instrumented shell
/// per process.
static INSTALLED: RwLock<Option<Installed>> = RwLock::new(None);

/// Wraps every registration in `builtins` so `hook` observes each builtin's begin/end.
///
/// Process-global: a second call replaces the first installation, so the shell built from the
/// first call's map reports to the second call's hook. One instrumented shell per process is the
/// supported arrangement. A map instrumented for one `SE` and then superseded by an installation
/// for another reports each of its builtins as uninstrumented rather than running it blind.
///
/// The map that comes back is the map that went in, hasher and all: only each registration's
/// `execute_func` is overwritten, in place. `content_func`, `disabled`, `special_builtin` and
/// `declaration_builtin` are the caller's own values, so help text, `enable` and POSIX
/// special-builtin semantics behave exactly as they did — and a map built around a configured
/// hasher keeps that hasher, which rebuilding the table through `collect` could not do without
/// demanding `S: Default` and constructing a second, differently configured one.
#[must_use]
pub fn instrument<SE: ShellExtensions, S>(
    mut builtins: HashMap<String, Registration<SE>, S>,
    hook: Arc<dyn BuiltinHook>,
) -> HashMap<String, Registration<SE>, S> {
    // Captured before the replacement below, so what is stored is each builtin's own
    // implementation rather than the wrapper.
    let originals: HashMap<String, CommandExecuteFunc<SE>> = builtins
        .iter()
        .map(|(name, registration)| (name.clone(), registration.execute_func))
        .collect();
    // Poisoning is recovered rather than propagated: the guarded code is a move and a replace,
    // neither of which can panic, so a poisoned lock could only come from an unrelated thread
    // dying — and refusing to instrument because of that would lose the whole record log.
    *INSTALLED
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Installed {
        hook,
        originals: Box::new(originals),
    });

    for registration in builtins.values_mut() {
        registration.execute_func = instrumented_execute::<SE>;
    }
    builtins
}

/// The hook and the original implementation registered under `name`, if this process has an
/// installation for `SE` that knows it.
///
/// A separate, synchronous function so the read guard is released before the wrapper's `await`:
/// [`BoxFuture`] is a `Send` future, and an `RwLockReadGuard` held across a suspension point is
/// not.
fn installation<SE: ShellExtensions>(
    name: &str,
) -> Option<(Arc<dyn BuiltinHook>, CommandExecuteFunc<SE>)> {
    let installed = INSTALLED
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    installed.as_ref().and_then(|installed| {
        let original = *installed
            .originals
            .downcast_ref::<HashMap<String, CommandExecuteFunc<SE>>>()?
            .get(name)?;
        Some((installed.hook.clone(), original))
    })
}

/// The `execute_func` every instrumented registration carries.
///
/// A non-capturing `fn`, which is what [`CommandExecuteFunc`] is: the hook and the original
/// implementation are found through [`INSTALLED`], keyed by the name the shell dispatched under.
fn instrumented_execute<SE: ShellExtensions>(
    context: ExecutionContext<'_, SE>,
    args: Vec<CommandArg>,
) -> BoxFuture<'_, Result<ExecutionResult, brush_core::Error>> {
    Box::pin(async move {
        let found = installation::<SE>(&context.command_name);

        // Unreachable in practice: every key of the wrapped map has an original, and `builtin` and
        // `command` re-dispatch under the registered name. Reachable only if the installation was
        // replaced by one that does not know this builtin — or that was made for another `SE` —
        // which is a caller error, not a shell condition, so it is reported rather than silently
        // run uninstrumented.
        let Some((hook, original)) = found else {
            use std::io::Write;
            writeln!(
                context.stderr(),
                "marsh-instrument: builtin {} is not instrumented",
                context.command_name
            )?;
            return Ok(ExecutionResult::general_error());
        };

        // Before the builtin runs, and before its invocation is recorded: an embedder that has
        // already decided this line must be evaluated again gains nothing from the rest of it, and
        // an unwind that recorded a `Begin` with no `End` would look like a builtin that died.
        let unwind = Interruption {
            hook: hook.as_ref(),
            cwd: context.shell.working_dir(),
        }
        .await;
        if unwind {
            return Ok(ExecutionResult {
                exit_code: ExecutionExitCode::Interrupted,
                next_control_flow: brush_core::ExecutionControlFlow::ExitShell,
            });
        }

        let argv: Vec<String> = args.iter().map(ToString::to_string).collect();
        let id = hook.begin(&context.command_name, &argv, context.shell.working_dir());
        let result = original(context, args).await;
        let exit: u8 = match &result {
            Ok(result) => result.exit_code.into(),
            Err(error) => ExecutionExitCode::from(error).into(),
        };
        hook.end(id, exit);
        result
    })
}

/// Asks the hook whether to unwind, handing it the polling task's waker.
///
/// A future rather than a plain call, because the waker is only reachable from a poll: an embedder
/// that answers `false` has to be able to wake this task later without it having to spin.
struct Interruption<'hook> {
    /// The installed hook.
    hook: &'hook dyn BuiltinHook,
    /// The shell's logical working directory, which is what identifies it to the hook.
    cwd: &'hook Path,
}

impl Future for Interruption<'_> {
    type Output = bool;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<bool> {
        std::task::Poll::Ready(self.hook.interrupted(self.cwd, context.waker()))
    }
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::{Mutex, PoisonError};

    use brush_builtins::BuiltinSet;
    use brush_core::extensions::{
        DefaultErrorFormatter, DefaultExternalCommandSpawner, DefaultShellExtensions,
        ExternalCommandSpawner, ShellExtensionsImpl,
    };
    use brush_core::{ProfileLoadBehavior, RcLoadBehavior, Shell, SourceInfo};
    use serial_test::serial;

    /// An [`ExternalCommandSpawner`] that only delegates: what makes [`PassThroughExtensions`] a
    /// second, distinct `SE` for the generic and downcast-miss tests.
    #[derive(Clone, Default)]
    struct PassThroughSpawner;

    impl ExternalCommandSpawner for PassThroughSpawner {
        fn spawn(
            &self,
            command: std::process::Command,
            kill_on_drop: bool,
        ) -> std::io::Result<brush_core::sys::process::Child> {
            DefaultExternalCommandSpawner.spawn(command, kill_on_drop)
        }
    }

    /// Shell extensions that are not [`DefaultShellExtensions`].
    type PassThroughExtensions = ShellExtensionsImpl<DefaultErrorFormatter, PassThroughSpawner>;

    /// A builtin whose execution is an `Err`, so the wrapper's error arm is reached on purpose.
    struct Failing;

    impl brush_core::builtins::SimpleCommand for Failing {
        fn get_content(
            _: &str,
            _: brush_core::builtins::ContentType,
            _: &brush_core::builtins::ContentOptions,
        ) -> Result<String, brush_core::Error> {
            Ok(String::new())
        }

        fn execute<SE: ShellExtensions, I: Iterator<Item = S>, S: AsRef<str>>(
            _: ExecutionContext<'_, SE>,
            _: I,
        ) -> Result<ExecutionResult, brush_core::Error> {
            brush_core::error::unimp("fail")
        }
    }

    /// One observed builtin invocation: what the hook was told, and how it ended.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Call {
        /// The name the shell dispatched under.
        name: String,
        /// The full argument vector.
        argv: Vec<String>,
        /// The shell's logical working directory.
        cwd: PathBuf,
        /// The exit code the matching `end` reported, once it arrived.
        exit: Option<u8>,
    }

    /// A [`BuiltinHook`] that appends every call to a shared log.
    #[derive(Default)]
    struct LogHook {
        /// The calls, in invocation order.
        calls: Mutex<Vec<Call>>,
    }

    impl LogHook {
        /// The calls observed so far.
        fn calls(&self) -> Vec<Call> {
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl BuiltinHook for LogHook {
        fn begin(&self, name: &str, argv: &[String], cwd: &Path) -> u64 {
            let mut calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
            calls.push(Call {
                name: name.to_string(),
                argv: argv.to_vec(),
                cwd: cwd.to_path_buf(),
                exit: None,
            });
            (calls.len() - 1) as u64
        }

        fn end(&self, id: u64, exit: u8) {
            let mut calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
            calls[usize::try_from(id).expect("an id fits a usize")].exit = Some(exit);
        }
    }

    /// A scratch working tree.
    fn scratch() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("scratch directory");
        let work = dir.path().canonicalize().expect("canonical work tree");
        (dir, work)
    }

    /// Builds a non-interactive shell rooted at `work` with `builtins`.
    async fn shell_with<SE: ShellExtensions>(
        work: &Path,
        builtins: HashMap<String, Registration<SE>>,
    ) -> Shell<SE> {
        Shell::builder_with_extensions::<SE>()
            .interactive(false)
            .no_editing(true)
            .profile(ProfileLoadBehavior::Skip)
            .rc(RcLoadBehavior::Skip)
            .working_dir(work.to_path_buf())
            .builtins(builtins)
            .build()
            .await
            .expect("build the shell")
    }

    /// Runs one line, returning its exit code.
    async fn run<SE: ShellExtensions>(shell: &mut Shell<SE>, line: &str) -> u8 {
        let params = shell.default_exec_params();
        shell
            .run_string(line, &SourceInfo::from("test"), &params)
            .await
            .expect("run the line")
            .exit_code
            .into()
    }

    #[tokio::test]
    #[serial]
    async fn a_builtin_reports_its_name_argv_and_working_directory() {
        let (_dir, work) = scratch();

        let mut mock = MockBuiltinHook::new();
        let expected = work.clone();
        mock.expect_begin()
            .withf(move |name, argv, cwd| {
                name == "cd" && argv == ["cd".to_string(), ".".to_string()] && cwd == expected
            })
            .times(1)
            .return_const(7_u64);
        let ends = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&ends);
        mock.expect_end().times(1).returning(move |id, exit| {
            observed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((id, exit));
        });
        // The wrapper asks before every builtin; a recorder with nothing to interrupt says no.
        mock.expect_interrupted().returning(|_, _| false);

        let builtins = instrument(
            brush_builtins::default_builtins::<DefaultShellExtensions>(BuiltinSet::BashMode),
            Arc::new(mock),
        );
        let mut shell = shell_with(&work, builtins).await;

        assert_eq!(run(&mut shell, "cd .").await, 0);
        assert_eq!(
            *ends.lock().unwrap_or_else(PoisonError::into_inner),
            vec![(7, 0)],
            "the end echoes the id the begin returned, with the builtin's exit code"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_failing_builtin_reports_its_exit_code() {
        let (_dir, work) = scratch();

        let mut mock = MockBuiltinHook::new();
        mock.expect_begin()
            .withf(|name, argv, _| name == "false" && argv == ["false".to_string()])
            .times(1)
            .return_const(5_u64);
        let ends = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&ends);
        mock.expect_end().times(1).returning(move |id, exit| {
            observed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((id, exit));
        });
        mock.expect_interrupted().returning(|_, _| false);

        let builtins = instrument(
            brush_builtins::default_builtins::<DefaultShellExtensions>(BuiltinSet::BashMode),
            Arc::new(mock),
        );
        let mut shell = shell_with(&work, builtins).await;

        assert_eq!(run(&mut shell, "false").await, 1);
        assert_eq!(
            *ends.lock().unwrap_or_else(PoisonError::into_inner),
            vec![(5, 1)],
            "a non-zero exit reaches the hook"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_builtin_that_errors_reports_the_error_as_an_exit_code() {
        let (_dir, work) = scratch();

        let hook = Arc::new(LogHook::default());
        let mut registrations =
            brush_builtins::default_builtins::<DefaultShellExtensions>(BuiltinSet::BashMode);
        registrations.insert(
            "fail".to_string(),
            brush_core::builtins::simple_builtin::<Failing, DefaultShellExtensions>(),
        );
        let builtins = instrument(registrations, hook.clone());
        let mut shell = shell_with(&work, builtins).await;

        // `fail` returns `Err`, which the wrapper still has to terminate — an unterminated
        // invocation would claim the builtin never returned. The brace group's redirection keeps
        // the interpreter's rendering of the error out of the test output.
        let exit = run(&mut shell, "{ fail; } 2>/dev/null").await;
        let calls = hook.calls();
        assert_eq!(calls.len(), 1, "one invocation: {calls:?}");
        assert_eq!(calls[0].name, "fail");
        assert_ne!(exit, 0, "an erroring builtin does not report success");
        assert_eq!(
            calls[0].exit,
            Some(exit),
            "an erroring builtin ends with the exit code the shell reports"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_shell_with_custom_extensions_reports_its_builtins_identically() {
        let (_default_dir, default_work) = scratch();
        let default_hook = Arc::new(LogHook::default());
        let mut default_shell = shell_with(
            &default_work,
            instrument(
                brush_builtins::default_builtins::<DefaultShellExtensions>(BuiltinSet::BashMode),
                default_hook.clone(),
            ),
        )
        .await;
        assert_eq!(run(&mut default_shell, "cd .").await, 0);
        assert_eq!(run(&mut default_shell, "false").await, 1);

        let (_custom_dir, custom_work) = scratch();
        let custom_hook = Arc::new(LogHook::default());
        let mut custom_shell = shell_with(
            &custom_work,
            instrument(
                brush_builtins::default_builtins::<PassThroughExtensions>(BuiltinSet::BashMode),
                custom_hook.clone(),
            ),
        )
        .await;
        assert_eq!(run(&mut custom_shell, "cd .").await, 0);
        assert_eq!(run(&mut custom_shell, "false").await, 1);

        let erase = |calls: Vec<Call>| -> Vec<Call> {
            calls
                .into_iter()
                .map(|call| Call {
                    cwd: PathBuf::new(),
                    ..call
                })
                .collect()
        };
        assert_eq!(
            erase(custom_hook.calls()),
            erase(default_hook.calls()),
            "instrumentation does not depend on which extensions the shell was built with"
        );
        assert_eq!(
            custom_hook.calls()[0].cwd,
            custom_work,
            "each shell reports its own working directory"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_superseded_installation_refuses_to_run_a_builtin_blind() {
        let (_dir, work) = scratch();

        let hook = Arc::new(LogHook::default());
        let builtins = instrument(
            brush_builtins::default_builtins::<DefaultShellExtensions>(BuiltinSet::BashMode),
            hook.clone(),
        );
        let mut shell = shell_with(&work, builtins).await;

        // A second installation, for another `SE`: the first map's originals can no longer be
        // found, because the erased map they live in is keyed by the `SE` it was made for.
        let superseding = instrument(
            brush_builtins::default_builtins::<PassThroughExtensions>(BuiltinSet::BashMode),
            Arc::new(LogHook::default()),
        );
        assert!(superseding.contains_key("true"));

        assert_eq!(
            run(&mut shell, "true 2> err.txt").await,
            1,
            "an uninstrumented builtin is an error, not a silent success"
        );
        assert_eq!(
            std::fs::read_to_string(work.join("err.txt")).expect("the redirected stderr"),
            "marsh-instrument: builtin true is not instrumented\n"
        );
        assert!(
            hook.calls().is_empty(),
            "the superseded hook observes nothing: {:?}",
            hook.calls()
        );
    }

    /// A [`std::hash::BuildHasher`] carrying a configured seed, and implementing neither `Default`
    /// nor `Clone`: a map built with it cannot be rebuilt by `collect`, so a map of this type
    /// reaching the shell at all is what proves the caller's hasher was carried over instead of
    /// reconstructed.
    struct KeyedHasher {
        /// Written into every hasher ahead of the key's own bytes, so the configuration really
        /// reaches the hashes the map computes rather than sitting unused beside them.
        seed: u64,
    }

    impl std::hash::BuildHasher for KeyedHasher {
        type Hasher = std::collections::hash_map::DefaultHasher;

        fn build_hasher(&self) -> Self::Hasher {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hasher::write_u64(&mut hasher, self.seed);
            hasher
        }
    }

    #[tokio::test]
    #[serial]
    async fn a_map_with_a_configured_hasher_is_instrumented_in_place() {
        let (_dir, work) = scratch();

        let mut builtins: HashMap<String, Registration<DefaultShellExtensions>, KeyedHasher> =
            HashMap::with_hasher(KeyedHasher {
                seed: 0x5eed_0f15_c0ff_ee01,
            });
        builtins.extend(brush_builtins::default_builtins::<DefaultShellExtensions>(
            BuiltinSet::BashMode,
        ));

        let hook = Arc::new(LogHook::default());
        let instrumented = instrument(builtins, hook.clone());
        assert!(
            instrumented.contains_key("false"),
            "the returned map still looks its own keys up through the hasher it was built with"
        );

        let mut shell =
            shell_with(&work, instrumented.into_iter().collect::<HashMap<_, _>>()).await;
        assert_eq!(run(&mut shell, "false").await, 1);

        let calls = hook.calls();
        assert_eq!(calls.len(), 1, "one invocation: {calls:?}");
        assert_eq!(calls[0].name, "false");
        assert_eq!(
            calls[0].exit,
            Some(1),
            "the end reached the begin's own invocation"
        );
    }
}
