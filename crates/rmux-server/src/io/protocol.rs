//! The bridge to rmux's own protocol and SDK, and the composition of a workload into a shell line.
//!
//! Two separate jobs live here, and they are separate on purpose.
//!
//! # Composing a workload
//!
//! A [`ProcessCommand`](rmux_proto::ProcessCommand) is either shell text or an argv vector, and
//! the two must reach the managed shell differently.
//!
//! * **Shell text** is passed verbatim to the embedded brush interpreter. Wrapping it in an
//!   implicit `sh -c` would run it in a shell that has none of marsh's builtins and none of its
//!   instrumentation, which is exactly the gap this engine exists to close.
//! * **Argv** becomes `exec -- <quoted…>`, with every argument individually force-quoted. That
//!   preserves empty arguments, embedded quotes, newlines and glob characters, none of which
//!   survive naive joining.
//!
//! # Reaching rmux
//!
//! Sessions, windows, panes, layouts, captures, key input and subscriptions already exist in
//! rmux's protocol and SDK. Re-implementing them natively would produce a second, diverging model
//! of the same thing. So the facade composes: it opens SDK connections **pinned to this host's
//! own socket**, on explicit caller operations only.
//!
//! Never default discovery, and never `connect_or_start`: a facade that discovered an endpoint
//! could address a *different* daemon, and one that started a daemon could start a second one over
//! the same seed — which the seed's exclusive lease would then refuse, after the damage of trying.
//! No connection is held open merely because the host exists.

use marsh_core::shellmux::MuxError;

use crate::io::{IoError, IoResult, ShellHandle, ShellIo};

/// Composes one workload into a line the managed shell runs.
///
/// # Errors
///
/// Fails when an argv workload is empty: there is no program to run, and treating it as an
/// interactive request would silently change what was asked for.
pub(crate) fn workload_line(process: &rmux_proto::ProcessCommand) -> IoResult<String> {
    match process {
        // Verbatim. The embedded interpreter is the point.
        rmux_proto::ProcessCommand::Shell(text) => Ok(text.clone()),
        rmux_proto::ProcessCommand::Argv(argv) => {
            if argv.is_empty() {
                return Err(IoError::Mux(std::sync::Arc::new(MuxError::Task(
                    "an argv workload needs at least a program".to_string(),
                ))));
            }
            Ok(exec_plan(argv, None))
        }
        // `ProcessCommand` is `#[non_exhaustive]`: a variant added upstream is a workload shape
        // this engine has not been taught to compose, and guessing at one would run something
        // other than what was asked for.
        other => Err(IoError::Mux(std::sync::Arc::new(MuxError::Task(format!(
            "unsupported workload shape: {other:?}"
        ))))),
    }
}

/// `exec [-a <quoted argv0>] -- <quoted argv…>`, with every word force-quoted.
///
/// Force-quoted rather than quoted-if-needed: a word that looks safe today is one the shell's
/// grammar may read differently tomorrow, and an empty argument has no unquoted spelling at all.
///
/// An `argv0` override is **two words**. Do not fold it back into the single word
/// `-a=<quoted argv0>`: that form was written first, and it never ran a shell. `exec`'s `args`
/// operand is declared `trailing_var_arg` with `allow_hyphen_values`, so the option parser hands
/// the very first hyphen-leading word it cannot match straight to the operand list rather than
/// rejecting it. `-a=-bash` is exactly such a word: it became the *program*, the configured shell
/// was never executed at all, and the pane printed `exec: -a=-bash: not found` and finished 127.
/// Because a pane's workload line is admitted with `close_on_finish`, that status closed the job,
/// and the pane, its window and its session were torn down a few hundred milliseconds after the
/// session was created. As two words the option matches and `-bash` is consumed as its value.
///
/// An override is composed by pane creation for a deliberately nonempty `default-shell`, whose
/// interactive plan renames the shell's own argv0 to `-sh`, `-bash` and the like so it reads its
/// login files. Every other caller passes `None`.
pub(crate) fn exec_plan(argv: &[String], argv0: Option<&str>) -> String {
    let mut line = String::from("exec");
    if let Some(argv0) = argv0 {
        line.push_str(" -a ");
        line.push_str(&brush_core::escape::force_quote(
            argv0,
            brush_core::escape::QuoteMode::SingleQuote,
        ));
    }
    line.push_str(" --");
    for argument in argv {
        line.push(' ');
        line.push_str(&brush_core::escape::force_quote(
            argument,
            brush_core::escape::QuoteMode::SingleQuote,
        ));
    }
    line
}

/// `<quoted argv…>`, for invoking a registered builtin by name.
///
/// Deliberately *not* [`exec_plan`]. Marsh's `exec` resolves its operand with
/// `find_first_executable_in_path` and runs with functions disabled — bash runs the program, never
/// a builtin or a function of that name — so `exec -- __rmux_io read …` exits 127 with
/// `__rmux_io: not found`. A builtin has to be the command word of an ordinary top-level line.
///
/// Same force-quoting as [`exec_plan`], and for the same reason: a word that the grammar reads
/// safely today may not tomorrow, and an empty argument has no unquoted spelling.
pub(crate) fn builtin_plan(argv: &[String]) -> String {
    let mut line = String::new();
    for argument in argv {
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&brush_core::escape::force_quote(
            argument,
            brush_core::escape::QuoteMode::SingleQuote,
        ));
    }
    line
}

impl ShellIo {
    /// An SDK facade pinned to this host's socket.
    ///
    /// A fresh, connect-only facade per operation: nothing is cached, so the host never holds a
    /// hidden self-connection open, and nothing can be addressed except this daemon.
    async fn sdk(&self) -> IoResult<rmux_sdk::Rmux> {
        Ok(rmux_sdk::Rmux::connect(rmux_sdk::RmuxEndpoint::UnixSocket(
            self.socket().to_path_buf(),
        ))
        .await?)
    }

    /// Creates a new detached session on this host.
    ///
    /// `create_only` and `detached`: this is a programmatic creation, so it must not silently
    /// attach to a session that already had that name, and it must not steal the current
    /// selection from whatever a user is looking at.
    ///
    /// # Errors
    ///
    /// Fails when the session cannot be created, and for the reasons a connection can fail.
    pub async fn new_session(
        &self,
        options: rmux_sdk::EnsureSession,
    ) -> IoResult<rmux_sdk::Session> {
        let sdk = self.sdk().await?;
        Ok(sdk
            .ensure_session(options.create_only().detached(true))
            .await?)
    }

    /// An existing session on this host, by name.
    ///
    /// # Errors
    ///
    /// Fails when no such session exists, and for the reasons a connection can fail.
    pub async fn session(
        &self,
        name: rmux_proto::SessionName,
    ) -> IoResult<rmux_sdk::Session> {
        let sdk = self.sdk().await?;
        Ok(sdk.session(name).await?)
    }

    /// A window on this host.
    ///
    /// # Errors
    ///
    /// Fails when the target does not resolve, and for the reasons a connection can fail.
    pub async fn window(&self, target: rmux_sdk::WindowRef) -> IoResult<rmux_sdk::Window> {
        let sdk = self.sdk().await?;
        Ok(sdk.window(target).await?)
    }

    /// A pane on this host.
    ///
    /// # Errors
    ///
    /// Fails when the target does not resolve, and for the reasons a connection can fail.
    pub async fn pane(&self, target: rmux_sdk::PaneRef) -> IoResult<rmux_sdk::Pane> {
        let sdk = self.sdk().await?;
        Ok(sdk.pane(target).await?)
    }

    /// The pane presenting `job`, if it has one.
    ///
    /// Re-resolves the current mapping rather than caching an index: a pane can be moved, linked
    /// into another window or renamed, and a stale index would address something else entirely.
    ///
    /// # Errors
    ///
    /// Fails with [`IoError::NoPresentation`] for a hidden pipe helper or a popup-only surface —
    /// those genuinely have no pane, and returning an invented one would be worse than an error.
    ///
    /// # Examples
    ///
    /// The job's pane, the window it lives in, a layout change and a screen capture — all through
    /// rmux's own SDK, over this host's socket. Needs a live host over a leased seed and a
    /// pane-backed job, so it is compiled rather than executed.
    ///
    /// ```no_run
    /// use rmux_sdk::{LayoutName, WindowRef};
    /// use rmux_server::io::{IoError, IoResult, ShellHandle, ShellIo};
    ///
    /// # async fn present(io: &ShellIo, job: &ShellHandle) -> IoResult<Vec<u8>> {
    /// let pane = match io.pane_for(job).await {
    ///     Ok(pane) => pane,
    ///     // A hidden pipe helper or a popup-only surface genuinely has no pane. An invented
    ///     // one would be an id the caller could act on and nothing would answer to.
    ///     Err(IoError::NoPresentation) => return Ok(Vec::new()),
    ///     Err(error) => return Err(error),
    /// };
    ///
    /// // Layouts belong to the window, and the pane's own target names it. Every call below
    /// // travels this daemon's existing IPC; none of it is a second native implementation of
    /// // rmux geometry, and none of it carries native policy parity to a remote caller.
    /// let target = pane.target().clone();
    /// let window = io
    ///     .window(WindowRef::new(target.session_name, target.window_index))
    ///     .await?;
    /// window.select_layout(LayoutName::EvenHorizontal).await?;
    ///
    /// // A capture is rendered *screen* content: it has already been through the terminal
    /// // emulator. That is a different thing from the job's output stream, which is bytes.
    /// let capture = pane.capture_pane().await?;
    /// Ok(capture.stdout)
    /// # }
    /// ```
    pub async fn pane_for(&self, job: &ShellHandle) -> IoResult<rmux_sdk::Pane> {
        let (session, pane, _generation) = self
            .route_for(job.sandbox())
            .ok_or(IoError::NoPresentation)?;
        let sdk = self.sdk().await?;
        Ok(sdk.pane_by_id(session, pane).await?)
    }

    /// The capabilities this daemon advertises.
    ///
    /// # Errors
    ///
    /// Fails for the reasons a connection can fail.
    pub async fn capabilities(&self) -> IoResult<Vec<String>> {
        let sdk = self.sdk().await?;
        Ok(sdk.capabilities().await?)
    }

    /// A connect-only protocol connection to this host.
    ///
    /// Every existing wire request, plus the stateful upgrades — attach, control mode — that a
    /// one-shot request helper would silently drop. Establishing it blocks, so it is done on a
    /// blocking worker of this host's own runtime: a caller with no runtime at all still gets a
    /// connection rather than the panic an ambient `spawn_blocking` would raise. The returned
    /// connection's own I/O is blocking too, and a caller driving it from async code must keep it
    /// on a blocking worker or use the client's attach APIs.
    ///
    /// # Errors
    ///
    /// Fails for the reasons a connection can fail, and when the blocking worker is lost.
    ///
    /// # Examples
    ///
    /// A wire request this facade has no native method for, answered by this same host. Needs a
    /// live host over a leased seed and its bound socket, so it is compiled rather than executed.
    ///
    /// ```no_run
    /// use rmux_proto::{ListSessionsRequest, Request, Response};
    /// use rmux_server::io::{IoError, IoResult, ShellIo};
    ///
    /// # async fn list_sessions(io: &ShellIo) -> IoResult<Vec<u8>> {
    /// // Connecting blocks, which is why this is awaited rather than simply called: the connect
    /// // itself runs on a blocking worker of this host's own runtime.
    /// let connection = io.open_protocol().await?;
    ///
    /// // The connection's subsequent I/O is blocking too, so driving it from async code means
    /// // keeping it on a blocking worker. That is also why it is moved in rather than borrowed.
    /// let answered = tokio::task::spawn_blocking(
    ///     move || -> Result<Vec<u8>, rmux_client::ClientError> {
    ///         let mut connection = connection;
    ///         if !connection.supports_capability("list-sessions")? {
    ///             return Ok(Vec::new());
    ///         }
    ///         let request = Request::ListSessions(ListSessionsRequest {
    ///             format: None,
    ///             filter: None,
    ///             sort_order: None,
    ///             reversed: false,
    ///         });
    ///         match connection.roundtrip(&request)? {
    ///             Response::ListSessions(response) => Ok(response.output.stdout),
    ///             // The connection stays usable: an unexpected response is this exchange's
    ///             // problem, not the transport's.
    ///             _ => Ok(Vec::new()),
    ///         }
    ///     },
    /// )
    /// .await
    /// .map_err(|error| std::io::Error::other(error.to_string()))?;
    ///
    /// // Not a subprocess runner, and not a second protocol: this is the wire every rmux client
    /// // already speaks, including the stateful upgrades — attach, control mode — that a
    /// // one-shot request helper would silently drop after sending.
    /// answered.map_err(IoError::from)
    /// # }
    /// ```
    pub async fn open_protocol(&self) -> IoResult<rmux_client::connection::Connection> {
        let socket = self.socket().to_path_buf();
        self.runtime()
            .spawn_blocking(move || rmux_client::connection::connect(&socket))
            .await
            .map_err(|error| {
                IoError::Mux(std::sync::Arc::new(MuxError::Task(format!(
                    "protocol connect task: {error}"
                ))))
            })?
            .map_err(IoError::from)
    }
}
