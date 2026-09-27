#[cfg(all(test, unix))]
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use rmux_core::events::SubscriptionLimits;
use rmux_ipc::LocalEndpoint;

use crate::io::{IoError, IoResult, ShellIo};
use crate::listener;
use crate::listener_options::ServeOptions;
use crate::unix_socket::bind_unix_listener_at;
use crate::unix_socket::real_user_id;
#[cfg(all(test, unix))]
use crate::unix_socket::{
    ensure_parent_directory, indicates_stale_socket, remove_stale_socket_if_needed,
};
use crate::unix_socket_access::UnixSocketAccessController;

#[cfg(all(test, unix))]
const FALLBACK_SOCKET_ROOT: &str = "/tmp";
const DEFAULT_WEB_PORT: u16 = 9777;

/// Computes the default RMUX daemon socket path.
///
/// The path uses an rmux-specific per-user directory so it cannot collide with
/// a real tmux server socket.
pub fn default_socket_path() -> io::Result<PathBuf> {
    rmux_ipc::default_endpoint().map(LocalEndpoint::into_path)
}

#[cfg(all(test, unix))]
fn socket_root_from_env(tmpdir: Option<&std::ffi::OsStr>) -> io::Result<PathBuf> {
    let tmpdir = tmpdir
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .into_iter();
    let candidates = tmpdir.chain(std::iter::once(PathBuf::from(FALLBACK_SOCKET_ROOT)));

    for candidate in candidates {
        if let Ok(resolved) = fs::canonicalize(&candidate) {
            return Ok(resolved);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no suitable rmux socket directory",
    ))
}

/// Daemon configuration for a single RMUX server instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    socket_path: PathBuf,
    config_load: ConfigLoadOptions,
    subscription_limits: SubscriptionLimits,
    web_frontend: Option<String>,
    web_port: u16,
    web_port_explicit: bool,
    web_required: bool,
    startup_ready_fd: Option<i32>,
}

impl DaemonConfig {
    /// Builds a daemon configuration for the given socket path.
    #[must_use]
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            config_load: ConfigLoadOptions::disabled(),
            subscription_limits: SubscriptionLimits::default(),
            web_frontend: None,
            web_port: DEFAULT_WEB_PORT,
            web_port_explicit: false,
            web_required: false,
            startup_ready_fd: None,
        }
    }

    /// Builds a daemon configuration using the default spec socket path.
    pub fn with_default_socket_path() -> io::Result<Self> {
        Ok(Self::new(default_socket_path()?))
    }

    /// Returns the configured local IPC endpoint path.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Returns the startup config loading policy.
    #[must_use]
    pub const fn config_load(&self) -> &ConfigLoadOptions {
        &self.config_load
    }

    /// Returns the pane-output subscription limits.
    #[must_use]
    pub fn subscription_limits(&self) -> SubscriptionLimits {
        self.subscription_limits
    }

    /// Returns the configured web-share listener port.
    #[must_use]
    pub const fn web_port(&self) -> u16 {
        self.web_port
    }

    /// Returns whether the web-share listener port was explicitly configured.
    #[must_use]
    pub const fn web_port_explicit(&self) -> bool {
        self.web_port_explicit
    }

    /// Returns whether this daemon startup requires the web listener to bind.
    #[must_use]
    pub const fn web_required(&self) -> bool {
        self.web_required
    }

    /// Returns the optional external web-share frontend origin.
    #[must_use]
    pub fn web_frontend(&self) -> Option<&str> {
        self.web_frontend.as_deref()
    }

    /// Enables RMUX default startup config loading.
    #[must_use]
    pub fn with_default_config_load(mut self, quiet: bool, cwd: Option<PathBuf>) -> Self {
        self.config_load = ConfigLoadOptions {
            selection: ConfigFileSelection::Default,
            quiet,
            cwd,
        };
        self
    }

    /// Overrides pane-output subscription limits for this daemon.
    #[must_use]
    pub fn with_subscription_limits(mut self, subscription_limits: SubscriptionLimits) -> Self {
        self.subscription_limits = subscription_limits;
        self
    }

    /// Overrides the web-share listener port.
    #[must_use]
    pub const fn with_web_port(mut self, port: u16) -> Self {
        self.web_port = port;
        self.web_port_explicit = true;
        self.web_required = true;
        self
    }

    /// Overrides the frontend origin used in generated web-share URLs.
    #[must_use]
    pub fn with_web_frontend(mut self, frontend: String) -> Self {
        self.web_frontend = Some(frontend);
        self.web_required = true;
        self
    }

    /// Enables explicit `-f` startup config loading.
    #[must_use]
    pub fn with_config_files(
        mut self,
        files: Vec<PathBuf>,
        quiet: bool,
        cwd: Option<PathBuf>,
    ) -> Self {
        self.config_load = ConfigLoadOptions {
            selection: ConfigFileSelection::Files(files),
            quiet,
            cwd,
        };
        self
    }

    /// Signals this inherited eventfd after the daemon listener is bound.
    #[must_use]
    pub const fn with_startup_ready_fd(mut self, ready_fd: i32) -> Self {
        self.startup_ready_fd = Some(ready_fd);
        self
    }

    const fn startup_ready_fd(&self) -> Option<i32> {
        self.startup_ready_fd
    }
}

/// Startup config loading policy for a daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigLoadOptions {
    selection: ConfigFileSelection,
    quiet: bool,
    cwd: Option<PathBuf>,
}

impl ConfigLoadOptions {
    /// Builds a config policy that performs no startup config loading.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            selection: ConfigFileSelection::Disabled,
            quiet: true,
            cwd: None,
        }
    }

    /// Returns the selected config files mode.
    #[must_use]
    pub const fn selection(&self) -> &ConfigFileSelection {
        &self.selection
    }

    /// Returns whether missing files should be suppressed.
    #[must_use]
    pub const fn quiet(&self) -> bool {
        self.quiet
    }

    /// Returns the startup client's current working directory.
    #[must_use]
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }
}

/// Config file selection mode for daemon startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigFileSelection {
    /// Do not load config files.
    Disabled,
    /// Load RMUX default config files, with a filtered tmux config fallback.
    Default,
    /// Load the explicit `-f` files in order.
    Files(Vec<PathBuf>),
}

#[derive(Debug, Clone)]
pub(crate) struct ShutdownHandle {
    sender: Arc<StdMutex<Option<oneshot::Sender<()>>>>,
}

impl ShutdownHandle {
    pub(crate) fn new() -> (Self, oneshot::Receiver<()>) {
        let (sender, receiver) = oneshot::channel();
        (
            Self {
                sender: Arc::new(StdMutex::new(Some(sender))),
            },
            receiver,
        )
    }

    pub(crate) fn request_shutdown(&self) {
        if let Some(sender) = self.sender.lock().expect("shutdown sender").take() {
            let _ = sender.send(());
        }
    }
}

/// Serves until the listener stops, then releases the shell engine — however it stopped.
///
/// Teardown lives *inside* the server task rather than in [`RmuxFrontend`] because the owner is
/// not guaranteed to be awaited. A caller may drop it, the task may be cancelled, `serve` may
/// panic, and joining it may itself fail. On every one of those paths the listener is gone and the
/// multiplexer is not — and a multiplexer that outlives its daemon keeps the seed's *exclusive*
/// lease, so the next process to open that seed fails for a daemon that is not running.
///
/// The guard is what makes "however it stopped" true: a return value, an early `?`, a cancellation
/// and an unwinding panic all drop it.
async fn serve_and_release(
    io: crate::io::ShellIo,
    serve: impl std::future::Future<Output = io::Result<()>>,
) -> io::Result<()> {
    let guard = CoreRelease {
        io: Some(io),
        runtime: tokio::runtime::Handle::current(),
    };
    let served = serve.await;
    // Awaited rather than left to the guard on the ordinary path, so a caller joining this task
    // observes a daemon whose snapshots are already reclaimed.
    let released = guard.release().await;
    // A termination failure is not successful cleanup. The server's own result wins when it
    // already failed — that is the more specific diagnosis — but a clean serve that could not
    // stop its commands must not be reported as a clean shutdown.
    served.and(released)
}

/// Releases the shell engine when it is dropped, for the paths that never reach an `await`.
struct CoreRelease {
    /// Taken by whichever of [`Self::release`] or [`Drop`] runs first.
    io: Option<crate::io::ShellIo>,
    /// The runtime to finish the teardown on, since a destructor cannot await.
    runtime: tokio::runtime::Handle,
}

impl CoreRelease {
    /// Tears the engine down and waits for it, reporting what teardown made of it.
    ///
    /// The failure is returned rather than dropped: the plan is explicit that termination failure
    /// must never be reported as successful cleanup, and a caller that joined this task is
    /// entitled to know its commands could not be stopped.
    async fn release(mut self) -> io::Result<()> {
        match self.io.take() {
            Some(io) => io
                .shutdown()
                .await
                .map_err(|error| io::Error::other(error.to_string())),
            None => Ok(()),
        }
    }
}

impl Drop for CoreRelease {
    /// Tears the engine down without waiting, because a destructor cannot.
    ///
    /// Reached only when the task did not finish normally — a cancellation or a panic. The work is
    /// handed to the runtime rather than skipped: releasing late is recoverable, never releasing
    /// leaks the seed lease for the life of the process.
    fn drop(&mut self) {
        if let Some(io) = self.io.take() {
            self.runtime.spawn(async move {
                let _ = io.shutdown().await;
            });
        }
    }
}

fn signal_startup_ready_fd(ready_fd: i32) {
    let _ = rmux_os::daemon::signal_startup_ready_fd(ready_fd);
}

/// One running rmux daemon: the whole owned system, in one value.
///
/// Opening one binds the local IPC endpoint and builds the single multiplexer every pane, popup
/// and workload helper of this daemon is a job on. It leases no seed: a seed is found from the
/// directory each shell is started in, so a listener may be opened outside any subvolume and
/// still serve a shell in a valid one. Nothing else has to be constructed by the caller: no
/// executor, no mux, no validator, no profile, no callback queue, no observation task, and no
/// daemon launcher.
///
/// Neither cloneable nor comparable, deliberately. This is the unique owner — of every seed lease
/// its shells took, of the multiplexer, and of the listener task — and each of those may exist
/// exactly once. The shareable half is [`ShellIo`]: this type [`Deref`](std::ops::Deref)s to the
/// one it holds, so every operation is directly callable on it, and [`Self::io`] hands out an
/// independent cloneable lease for concurrent code. Dropping this requests shutdown without
/// waiting; [`Self::shutdown`] is the form that waits for the socket to be gone and every
/// snapshot reclaimed.
#[derive(Debug)]
pub struct RmuxFrontend {
    shutdown_handle: ShutdownHandle,
    task: Option<JoinHandle<io::Result<()>>>,
    /// The daemon's in-process interface, leased, shared with its handlers.
    io: ShellIo,
}

impl RmuxFrontend {
    /// Binds a daemon on real btrfs, with `initial_dir` as its default starting directory.
    ///
    /// `initial_dir` is only the default for requests that name no directory of their own: it is
    /// captured as an absolute path and nothing is discovered, leased or recovered from it here.
    /// A shell's own starting directory is what selects the seed it publishes into, so a seed
    /// error belongs to the spawn that named it rather than to this constructor — and a listener
    /// started outside btrfs can still open a pane inside a subvolume.
    ///
    /// `environment` seeds every shell this daemon builds, on top of what the process inherited.
    /// `geometry` is the size terminal jobs open at when they ask for none.
    ///
    /// Requires a multi-threaded Tokio runtime and an unused private socket endpoint.
    pub async fn open(
        config: DaemonConfig,
        initial_dir: &Path,
        environment: brush_core::env::ShellEnvironment,
        geometry: marsh_core::shellmux::TerminalGeometry,
    ) -> IoResult<Self> {
        Self::open_configured(
            config,
            initial_dir,
            environment,
            geometry,
            marsh_core::shellmux::ShellMux::new::<crate::shell_frontend::FrontendQueue>,
        )
        .await
    }

    /// Shared construction; only the feature-gated test factory substitutes configured builders.
    pub(crate) async fn open_configured<F>(
        config: DaemonConfig,
        initial_dir: &Path,
        environment: brush_core::env::ShellEnvironment,
        geometry: marsh_core::shellmux::TerminalGeometry,
        create_mux: F,
    ) -> IoResult<Self>
    where
        F: FnOnce(
                marsh_core::shellmux::MuxProfile,
                Arc<StdMutex<crate::shell_frontend::FrontendQueue>>,
            )
                -> Result<Arc<marsh_core::shellmux::ShellMux>, marsh_core::shellmux::MuxError>
            + Send,
    {
        // The runtime captured here is the daemon's own. Every managed operation and every task
        // the facade creates lands on it, including ones requested from a status thread, a
        // foreign runtime or a detached command queue.
        //
        // Refused before anything is built, and the reason is no longer the one this guard was
        // written for. The synchronous pane-creation bridge it originally protected is gone: pane
        // create, split and respawn are now prepare -> async open with the handler lock released
        // -> identity-checked commit, and nothing in that path blocks a worker.
        //
        // It stays because a current-thread runtime has exactly ONE worker, and this daemon's
        // correctness depends on peers being pollable while another task waits for them. Three
        // separate deadlocks of precisely that shape were found and fixed in this engine, each one
        // a wait whose wakeup was owed by a task that could never be scheduled. Every one of them
        // was invisible on a multi-threaded runtime and fatal on a single-threaded one. Until a
        // current-thread host is actually exercised end to end, accepting one would be promising
        // something no test covers, and the failure mode is a hang: no timeout, no log, nothing to
        // diagnose.
        //
        // An embedder that wants one should build a multi-threaded runtime with one worker
        // thread, which is cheap and has the pollability this relies on.
        let runtime =
            match tokio::runtime::Handle::try_current() {
                Ok(runtime)
                    if runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread =>
                {
                    runtime
                }
                _ => return Err(IoError::from(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "rmux requires a multi-threaded Tokio runtime: a single worker cannot poll a \
                     task while another waits on it",
                ))),
            };

        let (io, events) = ShellIo::new(
            initial_dir,
            environment,
            geometry,
            runtime,
            config.socket_path().to_path_buf(),
            create_mux,
        )?;
        // From here on every early return drops `io`, and with it the only handle to the service:
        // the multiplexer goes, its executors go, and every seed lease its shells took is
        // released. A refused bind must not leave a leased seed behind for a daemon that never
        // started.
        let bound_listener = bind_unix_listener_at(config.socket_path())?;
        let socket_access =
            UnixSocketAccessController::new(config.socket_path(), bound_listener.identity)?;
        let (shutdown_handle, shutdown_receiver) = ShutdownHandle::new();
        let signal_watcher = crate::signals::SignalWatcher::install()?;
        let socket_path = config.socket_path().to_path_buf();
        let owner_uid = real_user_id()?;
        let serve_options = ServeOptions::new(
            config.config_load().clone(),
            config.subscription_limits(),
            owner_uid,
        )
        .with_web_options(
            config.web_port(),
            config.web_frontend().map(str::to_owned),
            config.web_required(),
            config.web_port_explicit(),
        )
        .with_socket_identity(bound_listener.identity)
        .with_socket_access(socket_access)
        .with_server_signals(signal_watcher)
        .with_shell_io(io.unleased(), events);

        // The owner's lease is taken *before* the listener exists. A daemon that decided it
        // was empty between spawning its server task and this constructor returning would
        // shut itself down under a caller that had done nothing wrong.
        let owner = io.leased();
        let task = tokio::spawn(serve_and_release(
            io,
            listener::serve(
                bound_listener.listener,
                socket_path,
                shutdown_handle.clone(),
                shutdown_receiver,
                serve_options,
            ),
        ));
        if let Some(ready_fd) = config.startup_ready_fd() {
            signal_startup_ready_fd(ready_fd);
        }

        Ok(Self {
            shutdown_handle,
            task: Some(task),
            io: owner,
        })
    }

    /// The bound local IPC endpoint path for the running daemon.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        self.io.socket()
    }

    /// An independently shareable handle to this daemon's shells, processes and terminals.
    ///
    /// The borrowed operations are already here through [`Deref`](std::ops::Deref); this is for
    /// the code that needs an *owned* handle — a spawned task, a second thread, a struct field.
    /// The returned handle is a native-client lease of its own: while one is alive this daemon
    /// does not decide it is idle merely because no UI session is attached, which is what keeps
    /// `exit-empty` from racing a headless API consumer. An explicit shutdown always overrides
    /// every lease.
    ///
    /// There is no `mux()` beside this, and deliberately so: the raw multiplexer is an ungated
    /// spawner and a publication capability, and this facade is the whole supported surface.
    #[must_use]
    pub fn io(&self) -> ShellIo {
        self.io.leased()
    }

    /// Runs until something else stops the daemon, then tears the engine down.
    ///
    /// `kill-server`, a signal, an idle `exit-empty` exit, or another holder's [`Self::shutdown`].
    /// This owner's own native-client lease is dropped first, so a caller that merely waits is not
    /// itself the reason the daemon stays up.
    ///
    /// Every way this daemon can stop converges on the release below. Each of them ends the
    /// *listener*, and none of them on its own ends the multiplexer — so without it the seed's
    /// exclusive lease would outlive the daemon for as long as any application clone of the facade
    /// was still held, and a caller would find a "closed" handle that was still quietly holding
    /// the seed. Afterwards, remaining [`ShellIo`] clones answer frozen read-only state, report
    /// empty live state, and refuse work; a snapshot whose *approved* publication failed is left
    /// on disk, because it is what the next open replays from.
    ///
    /// # Errors
    ///
    /// Fails with whatever the daemon reported on its way out, and with a teardown that could not
    /// stop a job's processes. The core is released either way.
    pub async fn wait(mut self) -> IoResult<()> {
        // The lease goes first, and the wakeup it fires on the way out is what lets an idle daemon
        // notice it has nothing left to do. Replaced rather than dropped, because the release
        // below still needs a handle.
        self.io = self.io.unleased();
        let served = match self.task.take() {
            // Deliberately not `?`: a join failure must not skip the release below.
            Some(task) => task
                .await
                .map_err(|error| IoError::from(io::Error::other(error)))
                .and_then(|served| served.map_err(IoError::from)),
            None => Ok(()),
        };
        // Reported, not swallowed, and performed even when the join or the serve failed: the core
        // is released either way — that is what this call is for — but a teardown that could not
        // stop a job's processes is a failure the caller has to see.
        let released = self.io.shutdown().await;
        // A termination failure is not successful cleanup. The server's own result wins when it
        // already failed — that is the more specific diagnosis — but a clean serve that could not
        // stop its commands must not be reported as a clean shutdown.
        served.and(released)
    }

    /// Stops the daemon and waits for it to be gone.
    ///
    /// Admission closes, helper producers stop, owned operations and native workers are joined or
    /// cancelled, unfinished work is discarded, the observer bus closes and the socket is removed
    /// — regardless of how many [`ShellIo`] handles are still held elsewhere.
    ///
    /// # Errors
    ///
    /// Fails for the reasons [`Self::wait`] does.
    pub async fn shutdown(self) -> IoResult<()> {
        self.shutdown_handle.request_shutdown();
        self.wait().await
    }
}

impl std::ops::Deref for RmuxFrontend {
    type Target = ShellIo;

    /// The owner's own handle, so every facade operation is callable directly on it.
    ///
    /// Borrowed on purpose, and with no `DerefMut` beside it: an owned handle is a *lease*, and
    /// handing one out silently through a coercion would make the daemon's idle decision depend on
    /// where a temporary happened to be dropped. [`Self::io`] is the explicit way to take one.
    fn deref(&self) -> &ShellIo {
        &self.io
    }
}

impl Drop for RmuxFrontend {
    /// Requests shutdown without waiting for it, because a destructor cannot await.
    ///
    /// The listener task releases the core itself on every path it can end by, so a dropped owner
    /// still gives the seed back; what it cannot do is tell the caller when, or whether stopping
    /// the jobs succeeded. [`Self::shutdown`] is the form that answers both.
    fn drop(&mut self) {
        self.shutdown_handle.request_shutdown();
    }
}

#[cfg(all(test, unix))]
#[path = "daemon_tests/unix.rs"]
mod tests;
