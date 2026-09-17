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
/// The returned map is otherwise the one that was passed in — `content_func`, `disabled`,
/// `special_builtin` and `declaration_builtin` are carried over untouched — so help text, `enable`
/// and POSIX special-builtin semantics behave exactly as they did.
#[must_use]
pub fn instrument<SE: ShellExtensions, S: std::hash::BuildHasher + Default>(
    builtins: HashMap<String, Registration<SE>, S>,
    hook: Arc<dyn BuiltinHook>,
) -> HashMap<String, Registration<SE>, S> {
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

    builtins
        .into_iter()
        .map(|(name, registration)| {
            (
                name,
                Registration {
                    execute_func: instrumented_execute::<SE>,
                    ..registration
                },
            )
        })
        .collect()
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
                "brush-instrument: builtin {} is not instrumented",
                context.command_name
            )?;
            return Ok(ExecutionResult::general_error());
        };

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
