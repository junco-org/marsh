//! Shared fixtures for this crate's unit tests.
//!
//! Three small traits carry the request boilerplate tests used to spell out by hand:
//! [`Fixture`] builds a request payload from the one or two values a test always supplies and
//! fills every other field with its usual test default, [`TestRequest`] pairs a payload with the
//! response variant that means it succeeded and sends it expecting that variant, and [`Owned`]
//! lets a fixture argument be a literal, a borrow or an owned value. [`SessionSpec`] composes them
//! into session set-up, and the [`RequestHandler`] methods below into the window, option and hook
//! set-up most tests begin with. Integration tests under `tests/` cannot see this module; they
//! share `tests/common/mod.rs` instead.

#[path = "test_fixtures/requests.rs"]
mod requests;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmux_core::{input::InputParser, Screen, WindowId};
use rmux_proto::{
    DisplayMessageRequest, HookName, MoveWindowTarget, NewSessionExtRequest, NewSessionRequest,
    NewSessionResponse, NewWindowRequest, OptionName, OptionScopeSelector, PaneTarget, Request,
    Response, ScopeSelector, SessionId, SessionName, SetHookMutationRequest,
    SetOptionByNameRequest, SetOptionMode, SetOptionRequest, SplitWindowTarget, Target,
    TerminalSize, WindowTarget,
};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Instant};

use crate::control::ControlServerEvent;
use crate::handler::RequestHandler;
use crate::pane_io::AttachControl;
use crate::test_names::session_name;

/// The window name a pane running the test shell reports.
pub(crate) const DEFAULT_SHELL_WINDOW_NAME: &str = "bash";

/// A request payload with its usual test defaults, built from the value(s) a test always varies.
///
/// `Key` is that value, or a tuple of them; every other field takes the most common value the
/// existing tests spelled out. Override fields with struct-update syntax:
/// `NewWindowRequest { name: Some("w1".to_owned()), ..Fixture::fixture(&alpha) }`.
pub(crate) trait Fixture<Key> {
    /// Builds the payload for `key`.
    fn fixture(key: Key) -> Self;
}

/// A fixture argument that stands for an owned `T`: the value itself, a borrow of it, or a
/// shorthand such as a session-name literal.
pub(crate) trait Owned<T> {
    /// Converts the argument into the value it stands for.
    fn owned(self) -> T;
}

impl<T> Owned<T> for T {
    fn owned(self) -> T {
        self
    }
}

impl<T: Clone> Owned<T> for &T {
    fn owned(self) -> T {
        self.clone()
    }
}

impl Owned<SessionName> for &str {
    fn owned(self) -> SessionName {
        session_name(self)
    }
}

impl Owned<SessionName> for String {
    fn owned(self) -> SessionName {
        session_name(&self)
    }
}

impl Owned<SessionName> for &String {
    fn owned(self) -> SessionName {
        session_name(self)
    }
}

/// Lets each listed target type, owned or borrowed, stand for its `$variant`, wrapped by `$wrap`.
macro_rules! owned_variants {
    ($target:ty = $wrap:path { $($source:ty => $variant:path),* $(,)? }) => {$(
        impl Owned<$target> for $source {
            fn owned(self) -> $target {
                $wrap($variant(self))
            }
        }

        impl Owned<$target> for &$source {
            fn owned(self) -> $target {
                $wrap($variant(self.clone()))
            }
        }
    )*};
}

owned_variants!(SplitWindowTarget = std::convert::identity {
    SessionName => SplitWindowTarget::Session,
    PaneTarget => SplitWindowTarget::Pane,
});
owned_variants!(MoveWindowTarget = std::convert::identity {
    WindowTarget => MoveWindowTarget::Window,
    SessionName => MoveWindowTarget::Session,
});
owned_variants!(Option<Target> = Some {
    SessionName => Target::Session,
    WindowTarget => Target::Window,
    PaneTarget => Target::Pane,
});

/// A session name stands for the detached default window its [`Fixture`] opens.
impl<N: Owned<SessionName>> Owned<NewWindowRequest> for N {
    fn owned(self) -> NewWindowRequest {
        NewWindowRequest::fixture(self)
    }
}

/// A request payload [`TestRequest::send_ok`] can send, paired with the response variant that
/// means it succeeded.
pub(crate) trait TestRequest: Sized + Send {
    /// The payload of the success response.
    type Success;

    /// Wraps the payload in its [`Request`] variant.
    fn into_request(self) -> Request;

    /// Unwraps the success payload, or hands back any other response unchanged.
    fn success(response: Response) -> Result<Self::Success, Response>;

    /// Sends `request` through `handler` and answers with its success payload.
    ///
    /// # Panics
    ///
    /// Panics with the response when the request fails or answers with another variant.
    fn send_ok(
        handler: &RequestHandler,
        request: Self,
    ) -> impl Future<Output = Self::Success> + Send {
        async move {
            let request = request.into_request();
            let command = request.command_name();
            Self::success(handler.handle(request).await)
                .unwrap_or_else(|response| panic!("{command} failed: {response:?}"))
        }
    }
}

/// A subscription request [`SubscribeRequest::subscribe_ok`] sends on a connection, paired with
/// the response variant that means it succeeded. Implemented in `handler_test_support.rs`, beside
/// the handler-private entry points it calls.
pub(crate) trait SubscribeRequest: Sized + Send {
    /// The payload of the success response.
    type Success;

    /// Sends the request on `connection_id` and answers with the raw response.
    fn subscribe(
        self,
        handler: &RequestHandler,
        connection_id: u64,
    ) -> impl Future<Output = Response> + Send;

    /// Unwraps the success payload, or hands back any other response unchanged.
    fn success(response: Response) -> Result<Self::Success, Response>;

    /// Subscribes connection `connection_id` through `handler` with `request` and answers with the
    /// success payload.
    ///
    /// # Panics
    ///
    /// Panics with the response when the subscription is refused.
    fn subscribe_ok(
        handler: &RequestHandler,
        connection_id: u64,
        request: Self,
    ) -> impl Future<Output = Self::Success> + Send {
        async move {
            Self::success(request.subscribe(handler, connection_id).await)
                .unwrap_or_else(|response| panic!("subscription failed: {response:?}"))
        }
    }
}

/// What [`SessionSpec::create`] opens: a session name (a detached 80x24 `new-session`), a
/// `(name, TerminalSize)` pair, [`Sizeless`], [`Quiet`], [`Grouped`], or a complete request.
pub(crate) trait SessionSpec: Sized + Send {
    /// The request the spec sends.
    type Request: TestRequest<Success = NewSessionResponse>;

    /// Builds that request.
    fn into_session_request(self) -> Self::Request;

    /// Creates the detached session `spec` describes through `handler` and answers with its name.
    fn create(handler: &RequestHandler, spec: Self) -> impl Future<Output = SessionName> + Send {
        async move {
            TestRequest::send_ok(handler, spec.into_session_request())
                .await
                .session_name
        }
    }

    /// [`create`](Self::create), then waits until the session's first pane has started.
    ///
    /// Unlike its siblings this future promises no `Send`: rustc cannot prove the startup wait's
    /// async-closure poll `Send` for every lifetime of the borrows it captures.
    fn create_started(handler: &RequestHandler, spec: Self) -> impl Future<Output = SessionName> {
        async move {
            let session = Self::create(handler, spec).await;
            handler
                .wait_for_pane_startup_to_finish_for_test(&PaneTarget::new(session.clone(), 0))
                .await;
            session
        }
    }

    /// [`create`](Self::create), then attaches a client with pid `requester_pid` to the new
    /// session and answers with the receiver of its attach controls.
    fn create_attached(
        handler: &RequestHandler,
        requester_pid: u32,
        spec: Self,
    ) -> impl Future<Output = mpsc::UnboundedReceiver<AttachControl>> + Send {
        async move {
            let session = Self::create(handler, spec).await;
            handler.attach_client(requester_pid, session).await
        }
    }
}

impl<N: Owned<SessionName> + Send> SessionSpec for N {
    type Request = NewSessionRequest;

    fn into_session_request(self) -> NewSessionRequest {
        NewSessionRequest::fixture(self)
    }
}

impl<N: Owned<SessionName> + Send> SessionSpec for (N, TerminalSize) {
    type Request = NewSessionRequest;

    fn into_session_request(self) -> NewSessionRequest {
        NewSessionRequest {
            size: Some(self.1),
            ..Fixture::fixture(self.0)
        }
    }
}

impl SessionSpec for NewSessionRequest {
    type Request = Self;

    fn into_session_request(self) -> Self {
        self
    }
}

impl SessionSpec for NewSessionExtRequest {
    type Request = Self;

    fn into_session_request(self) -> Self {
        self
    }
}

/// A session or window spec whose first pane runs [`quiet_command`] instead of the shell.
///
/// Such a pane neither prints nor exits, so activity, silence and title tests observe only what
/// they cause. Pair it with [`SessionSpec::create_started`] or
/// [`RequestHandler::create_started_window`], which wait for the pane to finish starting.
pub(crate) struct Quiet<S>(pub(crate) S);

impl<N: Owned<SessionName> + Send> SessionSpec for Quiet<N> {
    type Request = NewSessionExtRequest;

    fn into_session_request(self) -> NewSessionExtRequest {
        NewSessionExtRequest {
            command: Some(quiet_command()),
            ..Fixture::fixture(self.0)
        }
    }
}

impl<N: Owned<SessionName>> Owned<NewWindowRequest> for Quiet<N> {
    fn owned(self) -> NewWindowRequest {
        NewWindowRequest {
            command: Some(quiet_command()),
            ..Fixture::fixture(self.0)
        }
    }
}

/// A session spec for session `.0` joining the group of session `.1`.
pub(crate) struct Grouped<N, G>(pub(crate) N, pub(crate) G);

impl<N: Owned<SessionName> + Send, G: Owned<SessionName> + Send> SessionSpec for Grouped<N, G> {
    type Request = NewSessionExtRequest;

    fn into_session_request(self) -> NewSessionExtRequest {
        NewSessionExtRequest {
            group_target: Some(self.1.owned()),
            ..Fixture::fixture(self.0)
        }
    }
}

/// A plain `new-session` spec for session `.0` that requests no size, so the server picks one.
pub(crate) struct Sizeless<N>(pub(crate) N);

impl<N: Owned<SessionName> + Send> SessionSpec for Sizeless<N> {
    type Request = NewSessionRequest;

    fn into_session_request(self) -> NewSessionRequest {
        NewSessionRequest {
            size: None,
            ..Fixture::fixture(self.0)
        }
    }
}

impl RequestHandler {
    /// Opens the window `spec` describes (a session name opens a detached default window) and
    /// answers with its target.
    pub(crate) async fn create_window(&self, spec: impl Owned<NewWindowRequest>) -> WindowTarget {
        TestRequest::send_ok(self, spec.owned()).await.target
    }

    /// [`create_window`](Self::create_window), then waits until the window's pane has started.
    pub(crate) async fn create_started_window(
        &self,
        spec: impl Owned<NewWindowRequest>,
    ) -> WindowTarget {
        let window = self.create_window(spec).await;
        self.wait_for_pane_startup_to_finish_for_test(&PaneTarget::with_window(
            window.session_name().clone(),
            window.window_index(),
            0,
        ))
        .await;
        window
    }

    /// Sets `session`'s `status` option to `value`.
    pub(crate) async fn set_session_status(&self, session: &SessionName, value: &str) {
        let scope = ScopeSelector::Session(session.clone());
        self.set_option(scope, OptionName::Status, value).await;
    }

    /// Sets `window_index` of `session` to `window-size` policy `value`.
    pub(crate) async fn set_window_size_policy(
        &self,
        session: &SessionName,
        window_index: u32,
        value: &str,
    ) {
        let window = WindowTarget::with_window(session.clone(), window_index);
        self.set_option(ScopeSelector::Window(window), OptionName::WindowSize, value)
            .await;
    }

    /// Attaches a client with pid `requester_pid` to `session` and answers with the receiver of
    /// its attach controls.
    pub(crate) async fn attach_client(
        &self,
        requester_pid: u32,
        session: impl Owned<SessionName>,
    ) -> mpsc::UnboundedReceiver<AttachControl> {
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        self.register_attach(requester_pid, session.owned(), control_tx)
            .await;
        control_rx
    }

    /// Stores `option` = `value` in `scope` straight into the option store, bypassing the
    /// `set-option` request path and its side effects.
    pub(crate) async fn store_option_for_test(
        &self,
        scope: ScopeSelector,
        option: OptionName,
        value: &str,
    ) {
        self.state_for_test()
            .lock()
            .await
            .options
            .set(scope, option, value.to_owned(), SetOptionMode::Replace)
            .expect("test option value is valid");
    }

    /// Stores `session`'s `detach-on-destroy` policy `value` straight into the option store.
    pub(crate) async fn set_detach_on_destroy_for_test(&self, session: &SessionName, value: &str) {
        let scope = ScopeSelector::Session(session.clone());
        self.store_option_for_test(scope, OptionName::DetachOnDestroy, value)
            .await;
    }

    /// The id of the live session `session`.
    pub(crate) async fn session_id_for_test(&self, session: impl Owned<SessionName>) -> SessionId {
        self.state_for_test()
            .lock()
            .await
            .sessions
            .session(&session.owned())
            .expect("session exists")
            .id()
    }

    /// The id of the window in slot `window`.
    pub(crate) async fn window_id_for_test(&self, window: &WindowTarget) -> WindowId {
        self.state_for_test()
            .lock()
            .await
            .sessions
            .session(window.session_name())
            .and_then(|session| session.window_at(window.window_index()))
            .expect("window exists")
            .id()
    }

    /// The numeric id of `session`'s active window.
    pub(crate) async fn active_window_id_for_test(&self, session: impl Owned<SessionName>) -> u32 {
        self.state_for_test()
            .lock()
            .await
            .sessions
            .session(&session.owned())
            .expect("session exists")
            .window()
            .id()
            .as_u32()
    }

    /// The size of `session`'s active window.
    pub(crate) async fn active_window_size_for_test(
        &self,
        session: impl Owned<SessionName>,
    ) -> TerminalSize {
        self.state_for_test()
            .lock()
            .await
            .sessions
            .session(&session.owned())
            .expect("session exists")
            .window()
            .size()
    }

    /// The terminal size of `target`'s pane.
    pub(crate) async fn pane_terminal_size_for_test(&self, target: &PaneTarget) -> TerminalSize {
        self.state_for_test()
            .lock()
            .await
            .pane_terminal_size(
                target.session_name(),
                target.window_index(),
                target.pane_index(),
            )
            .expect("pane terminal size available")
    }

    /// Replaces `target`'s screen with a `size` screen that has parsed `content`, keeping the
    /// transcript's history limit.
    pub(crate) async fn replace_transcript_for_test(
        &self,
        target: &PaneTarget,
        size: TerminalSize,
        content: &[u8],
    ) {
        let transcript = self
            .state_for_test()
            .lock()
            .await
            .transcript_handle(target)
            .expect("session transcript must exist");
        let history_limit = transcript
            .lock()
            .expect("pane transcript mutex must not be poisoned")
            .history_limit();
        let mut screen = Screen::new(size, history_limit);
        InputParser::new().parse(content, &mut screen);
        transcript
            .lock()
            .expect("pane transcript mutex must not be poisoned")
            .set_screen_for_test(screen);
    }

    /// Sets `option` to `value` in `scope`, replacing any earlier value.
    pub(crate) async fn set_option(&self, scope: ScopeSelector, option: OptionName, value: &str) {
        TestRequest::send_ok(self, SetOptionRequest::fixture((scope, option, value))).await;
    }

    /// Sets the option spelled `name` to `value` in `scope`, replacing any earlier value.
    pub(crate) async fn set_option_by_name(
        &self,
        scope: OptionScopeSelector,
        name: &str,
        value: &str,
    ) {
        TestRequest::send_ok(self, SetOptionByNameRequest::fixture((scope, name, value))).await;
    }

    /// Installs `command` as the persistent global `hook`.
    pub(crate) async fn set_global_hook(&self, hook: HookName, command: &str) {
        TestRequest::send_ok(
            self,
            SetHookMutationRequest::fixture((ScopeSelector::Global, hook, command)),
        )
        .await;
    }

    /// Runs `display-message -p message` against `target` (`None` for no explicit target) and
    /// answers with the printed bytes.
    pub(crate) async fn display_print(
        &self,
        target: impl Owned<Option<Target>>,
        message: impl Into<String>,
    ) -> Vec<u8> {
        TestRequest::send_ok(
            self,
            DisplayMessageRequest {
                target: target.owned(),
                ..Fixture::fixture(message)
            },
        )
        .await
        .output
        .expect("display-message -p returns output")
        .stdout
    }

    /// Waits until paste buffer `name` holds exactly `expected`.
    pub(crate) async fn wait_for_buffer(&self, name: &str, expected: &str) {
        wait_until(
            Duration::from_secs(5),
            Duration::from_millis(10),
            async || {
                let state = self.state_for_test().lock().await;
                match state.buffers.show(Some(name)) {
                    Ok((_, content)) if String::from_utf8_lossy(content) == expected => Ok(()),
                    Ok((_, content)) => Err(Some(String::from_utf8_lossy(content).into_owned())),
                    Err(_) => Err(None),
                }
            },
        )
        .await
        .unwrap_or_else(|last| panic!("buffer {name} did not reach {expected:?}; last={last:?}"));
    }
}

/// The argv of a pane that neither prints nor exits for the length of a test.
pub(crate) fn quiet_command() -> Vec<String> {
    ["/bin/sh", "-c", "sleep 60"].map(str::to_owned).into()
}

/// Polls `probe` every `interval` until it answers `Ok`, and answers with that value.
///
/// The probe runs at least once and once more after `timeout` has passed, so a condition that
/// became true during the final sleep is still seen. On timeout this answers with the probe's last
/// `Err`, which the caller turns into its own failure message.
pub(crate) async fn wait_until<T, E>(
    timeout: Duration,
    interval: Duration,
    mut probe: impl AsyncFnMut() -> Result<T, E>,
) -> Result<T, E> {
    let deadline = Instant::now() + timeout;
    loop {
        match probe().await {
            Ok(value) => return Ok(value),
            Err(last) if Instant::now() >= deadline => return Err(last),
            Err(_) => sleep(interval).await,
        }
    }
}

/// Waits up to five seconds for the file at `path` to hold exactly `expected`.
pub(crate) async fn wait_for_file_contents(path: &Path, expected: &str) {
    let last = wait_until(
        Duration::from_secs(5),
        Duration::from_millis(25),
        async || match std::fs::read_to_string(path) {
            Ok(contents) if contents == expected => Ok(()),
            other => Err(other),
        },
    )
    .await;
    match last {
        Ok(()) => {}
        Err(Ok(contents)) => panic!(
            "timed out waiting for {} to contain {expected:?}, got {contents:?}",
            path.display()
        ),
        Err(Err(error)) => panic!(
            "timed out waiting for {} to exist with {expected:?}: {error}",
            path.display()
        ),
    }
}

/// A fresh path under the system temp directory, unique to this process and moment.
pub(crate) fn unique_temp_path(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("rmux-{label}-{}-{unique}", std::process::id()))
}

/// A fresh option store with each `(scope, option, value, mode)` entry applied in order.
///
/// # Panics
///
/// Panics when an entry is rejected.
pub(crate) fn option_store<'a>(
    entries: impl IntoIterator<Item = (ScopeSelector, OptionName, &'a str, SetOptionMode)>,
) -> rmux_core::OptionStore {
    let mut options = rmux_core::OptionStore::new();
    for (scope, option, value, mode) in entries {
        options
            .set(scope, option, value.to_owned(), mode)
            .expect("option set succeeds");
    }
    options
}

/// The `t` access token carried in a web-share URL's `#` fragment.
///
/// # Panics
///
/// Panics when the fragment carries no `t` parameter.
#[cfg(all(unix, feature = "web"))]
pub(crate) fn token_from_url(url: &str) -> String {
    url.split_once('#')
        .and_then(|(_, fragment)| {
            fragment.split('&').find_map(|param| {
                let (key, value) = param.split_once('=')?;
                (key == "t").then_some(value.to_owned())
            })
        })
        .expect("URL contains access token")
}

/// The access token in `share`'s spectator URL.
#[cfg(all(unix, feature = "web"))]
pub(crate) fn spectator_token(share: &rmux_proto::WebShareCreatedResponse) -> String {
    token_from_url(share.spectator_url.as_deref().expect("spectator URL"))
}

/// The access token in `share`'s operator URL.
#[cfg(all(unix, feature = "web"))]
pub(crate) fn operator_token(share: &rmux_proto::WebShareCreatedResponse) -> String {
    token_from_url(share.operator_url.as_deref().expect("operator URL"))
}

/// Collects control notifications for up to five seconds, through the first one starting with
/// `prefix`, and answers with every notification seen.
pub(crate) async fn collect_control_notifications_through(
    events: &mut mpsc::Receiver<ControlServerEvent>,
    prefix: &str,
) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut lines = Vec::new();
    while Instant::now() < deadline {
        match timeout(Duration::from_millis(25), events.recv()).await {
            Ok(Some(ControlServerEvent::Notification(line))) => {
                let matched = line.starts_with(prefix);
                lines.push(line);
                if matched {
                    return lines;
                }
            }
            Ok(Some(_)) | Err(_) => {}
            Ok(None) => break,
        }
    }
    lines
}

/// Collects control notifications until none arrive for 250 ms, and answers with them.
pub(crate) async fn settle_control_notifications(
    events: &mut mpsc::Receiver<ControlServerEvent>,
) -> Vec<String> {
    let quiet = Duration::from_millis(250);
    let mut deadline = Instant::now() + quiet;
    let mut lines = Vec::new();
    while Instant::now() < deadline {
        match timeout(Duration::from_millis(25), events.recv()).await {
            Ok(Some(event)) => {
                deadline = Instant::now() + quiet;
                if let ControlServerEvent::Notification(line) = event {
                    lines.push(line);
                }
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }
    lines
}
