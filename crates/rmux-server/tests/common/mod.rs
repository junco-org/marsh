#![allow(dead_code)]

mod requests;

use std::collections::BTreeSet;
use std::error::Error;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use marsh_core::shellmux::TerminalGeometry;
use rmux_proto::{
    decode_frame, encode_frame, AttachMessage, AttachSessionRequest, AttachSessionResponse,
    CapturePaneRequest, FrameDecoder, KillSessionRequest, KillSessionResponse,
    NewSessionExtRequest, NewSessionRequest, NewSessionResponse, PaneTarget, Request, Response,
    RmuxError, SessionName, SplitWindowTarget, TerminalSize, DEFAULT_MAX_DETACHED_FRAME_LENGTH,
    RMUX_FRAME_MAGIC, RMUX_WIRE_VERSION,
};
use rmux_server::{DaemonConfig, RmuxFrontend};
use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::fs::{flock, FlockOperation};
use rustix::termios::{
    tcgetattr, tcgetwinsize, tcsetattr, OptionalActions, SpecialCodeIndex, Termios,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, MutexGuard};
use tokio::time::Instant;

static UNIQUE_ID: AtomicUsize = AtomicUsize::new(0);
static IN_PROCESS_PTY_TEST_LOCK: Mutex<()> = Mutex::const_new(());
pub(crate) static PTY_TEST_LOCK: PtyTestLock = PtyTestLock;
const SOCKET_REMOVAL_TIMEOUT: Duration = Duration::from_secs(2);
const SOCKET_REMOVAL_POLL_INTERVAL: Duration = Duration::from_millis(5);
/// Default geometry for a test daemon's terminals; any nonzero pair would do.
const SEED_ROWS: u16 = 24;
/// Default width, as above.
const SEED_COLS: u16 = 80;
/// The pane geometry every sized [`Fixture`] starts from.
pub(crate) const DEFAULT_SIZE: TerminalSize = TerminalSize { cols: 80, rows: 24 };
/// How often the `wait_for_*` helpers re-check their condition.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A request payload with its usual test defaults, built from the value(s) a test always varies.
///
/// Mirrors the crate's unit-test fixtures, which these separate test crates cannot see. `Key` is
/// that value, or a tuple of them; every other field takes the most common value the tests spelled
/// out. Override fields with struct-update syntax:
/// `NewWindowRequest { name: Some("logs".to_owned()), ..Fixture::fixture(&alpha) }`.
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

/// Lets each listed target type, owned or borrowed, stand for its [`SplitWindowTarget`] variant.
macro_rules! split_targets {
    ($($source:ty => $variant:path),* $(,)?) => {$(
        impl Owned<SplitWindowTarget> for $source {
            fn owned(self) -> SplitWindowTarget {
                $variant(self)
            }
        }

        impl Owned<SplitWindowTarget> for &$source {
            fn owned(self) -> SplitWindowTarget {
                $variant(self.clone())
            }
        }
    )*};
}

split_targets!(SessionName => SplitWindowTarget::Session, PaneTarget => SplitWindowTarget::Pane);

/// A request payload [`ClientConnection::send_ok`] can send, paired with the response variant that
/// means it succeeded.
pub(crate) trait TestRequest {
    /// The payload of the success response.
    type Success;

    /// Wraps the payload in its [`Request`] variant.
    fn into_request(self) -> Request;

    /// Unwraps the success payload, or hands back any other response unchanged.
    fn success(response: Response) -> Result<Self::Success, Response>;
}

/// What [`create_session`] accepts: a session name (a detached 80x24 `new-session`), a
/// `(name, TerminalSize)` pair, [`Sizeless`], or a complete request.
pub(crate) trait SessionSpec {
    /// The request the spec sends.
    type Request: TestRequest<Success = NewSessionResponse>;

    /// Builds that request.
    fn into_session_request(self) -> Self::Request;
}

impl<N: Owned<SessionName>> SessionSpec for N {
    type Request = NewSessionRequest;

    fn into_session_request(self) -> NewSessionRequest {
        NewSessionRequest::fixture(self)
    }
}

impl<N: Owned<SessionName>> SessionSpec for (N, TerminalSize) {
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

/// A session spec for a detached `new-session` of `.0` that sends no size.
pub(crate) struct Sizeless<N>(pub(crate) N);

impl<N: Owned<SessionName>> SessionSpec for Sizeless<N> {
    type Request = NewSessionRequest;

    fn into_session_request(self) -> NewSessionRequest {
        NewSessionRequest {
            size: None,
            ..Fixture::fixture(self.0)
        }
    }
}

pub(crate) struct PtyTestLock;

pub(crate) struct PtyTestGuard {
    _in_process: MutexGuard<'static, ()>,
    file: File,
}

impl PtyTestLock {
    pub(crate) async fn lock(&'static self) -> PtyTestGuard {
        let in_process = IN_PROCESS_PTY_TEST_LOCK.lock().await;
        let path = std::env::temp_dir().join("rmux-server-pty-tests.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap_or_else(|error| {
                panic!("failed to open PTY test lock '{}': {error}", path.display())
            });
        flock(file.as_fd(), FlockOperation::LockExclusive).unwrap_or_else(|error| {
            panic!(
                "failed to acquire PTY test lock '{}': {error}",
                path.display()
            )
        });
        PtyTestGuard {
            _in_process: in_process,
            file,
        }
    }
}

impl Drop for PtyTestGuard {
    fn drop(&mut self) {
        let _ = flock(self.file.as_fd(), FlockOperation::Unlock);
    }
}

/// Opens a daemon for `config` over a private seed under `seed_root`.
///
/// [`RmuxFrontend::open_with`] takes the directory its shells start in by default and the backend
/// seeds are reached through. None of these tests are about storage, so the seed is a plain
/// directory tree behind [`marsh_btrfs::fake::CopyTree`]: the daemon cannot tell the difference,
/// and the tests keep running on hosts with no btrfs.
///
/// `seed_root` must outlive the frontend and everything it hands out — the engine publishes into
/// a tree below it, and a seed deleted underneath a live daemon is not a condition any of these
/// tests mean to exercise. A [`TestHarness`] root satisfies that, since the harness is declared
/// before the frontend it feeds and therefore dropped after it.
///
/// Each call opens its own seed: the seed lease is exclusive, so two daemons sharing one tree
/// would fail on the lease rather than on whatever the test is about.
///
/// Must be called from within a multi-threaded Tokio runtime, which is what the daemon supports.
pub(crate) async fn daemon_over_seed(
    config: DaemonConfig,
    seed_root: &Path,
) -> rmux_server::IoResult<RmuxFrontend> {
    let seed = seed_root.join("seed");
    std::fs::create_dir_all(&seed).map_err(rmux_server::IoError::from)?;
    let seed = seed.canonicalize().map_err(rmux_server::IoError::from)?;
    let filesystem = Arc::new(marsh_btrfs::fake::CopyTree::new());
    filesystem.register(&seed);

    rmux_server::test_support::open_frontend(
        config,
        &seed,
        brush_core::env::ShellEnvironment::new(),
        TerminalGeometry {
            rows: SEED_ROWS,
            cols: SEED_COLS,
        },
        filesystem,
    )
    .await
}

pub(crate) async fn start_server(harness: &TestHarness) -> Result<RmuxFrontend, Box<dyn Error>> {
    let socket_path = harness.socket_path().to_path_buf();
    daemon_over_seed(DaemonConfig::new(socket_path), harness.seed_root())
        .await
        .map_err(Into::into)
}

pub(crate) async fn send_request(
    socket_path: &Path,
    request: &Request,
) -> Result<Response, Box<dyn Error>> {
    let mut client = ClientConnection::connect(socket_path).await?;
    client.send_request(request).await
}

/// Sends `request` on a fresh connection and answers with the daemon's response.
pub(crate) async fn send(
    socket_path: &Path,
    request: impl TestRequest,
) -> Result<Response, Box<dyn Error>> {
    send_request(socket_path, &request.into_request()).await
}

/// [`ClientConnection::send_ok`] on a fresh connection.
pub(crate) async fn send_ok<R: TestRequest>(
    socket_path: &Path,
    request: R,
) -> Result<R::Success, Box<dyn Error>> {
    ClientConnection::connect(socket_path)
        .await?
        .send_ok(request)
        .await
}

/// [`ClientConnection::create_session`] on a fresh connection.
pub(crate) async fn create_session(
    socket_path: &Path,
    spec: impl SessionSpec,
) -> Result<SessionName, Box<dyn Error>> {
    ClientConnection::connect(socket_path)
        .await?
        .create_session(spec)
        .await
}

/// Kills the session `target` and asserts that it existed.
pub(crate) async fn kill_session(
    socket_path: &Path,
    target: impl Owned<SessionName>,
) -> Result<(), Box<dyn Error>> {
    let removed = send(socket_path, KillSessionRequest::fixture(target)).await?;
    assert_eq!(
        removed,
        Response::KillSession(KillSessionResponse { existed: true })
    );
    Ok(())
}

/// The `capture-pane -p` text of `target`.
pub(crate) async fn capture_pane_text(
    socket_path: &Path,
    target: &PaneTarget,
) -> Result<String, Box<dyn Error>> {
    let response = send(socket_path, CapturePaneRequest::fixture(target)).await?;
    let output = response
        .command_output()
        .ok_or_else(|| io::Error::other("capture-pane -p returned no command output"))?;
    Ok(String::from_utf8_lossy(output.stdout()).into_owned())
}

/// Polls [`capture_pane_text`] of `target` for up to `timeout` until it contains `needle`, and
/// answers with that capture.
pub(crate) async fn wait_for_capture(
    socket_path: &Path,
    target: &PaneTarget,
    needle: &str,
    timeout: Duration,
) -> Result<String, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let capture = capture_pane_text(socket_path, target).await?;
        if capture.contains(needle) {
            return Ok(capture);
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }

    Err(io::Error::other(format!(
        "timed out waiting for pane capture containing {needle:?}"
    ))
    .into())
}

/// Polls the file at `path` for up to `timeout`, answering whether it came to hold exactly
/// `expected`. A missing file counts as not there yet; any other read error fails.
pub(crate) async fn poll_file_contents(
    path: &Path,
    expected: &str,
    timeout: Duration,
) -> Result<bool, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        match std::fs::read_to_string(path) {
            Ok(contents) if contents == expected => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }

    Ok(false)
}

/// [`poll_file_contents`], failing when the file never holds `expected`.
pub(crate) async fn wait_for_file_contents(
    path: &Path,
    expected: &str,
    timeout: Duration,
) -> Result<(), Box<dyn Error>> {
    if poll_file_contents(path, expected, timeout).await? {
        return Ok(());
    }

    Err(io::Error::other(format!(
        "file '{}' never reached expected contents '{expected}' within {timeout:?}",
        path.display()
    ))
    .into())
}

/// Single-quotes `value` as one `sh` word.
pub(crate) fn shell_quote_str(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Single-quotes `path` as one `sh` word.
pub(crate) fn shell_quote(path: &Path) -> String {
    shell_quote_str(&path.display().to_string())
}

/// Reads the next message of an attach stream, or `None` once the stream has ended.
pub(crate) async fn read_attach_message(
    stream: &mut UnixStream,
) -> Result<Option<AttachMessage>, Box<dyn Error>> {
    let mut tag = [0_u8; 1];
    let bytes_read = stream.read(&mut tag).await?;
    if bytes_read == 0 {
        return Ok(None);
    }

    match tag[0] {
        1 => Ok(Some(AttachMessage::Data(
            read_attach_payload(stream).await?,
        ))),
        2 => {
            let mut size = [0_u8; 4];
            stream.read_exact(&mut size).await?;
            Ok(Some(AttachMessage::Resize(TerminalSize {
                cols: u16::from_le_bytes([size[0], size[1]]),
                rows: u16::from_le_bytes([size[2], size[3]]),
            })))
        }
        5 => Ok(Some(AttachMessage::Suspend)),
        13 => Ok(Some(AttachMessage::Render(
            read_attach_payload(stream).await?,
        ))),
        other => {
            Err(RmuxError::Decode(format!("unknown attach-stream message tag {other}")).into())
        }
    }
}

async fn read_attach_payload(stream: &mut UnixStream) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).await?;
    let mut payload = vec![0_u8; u32::from_le_bytes(length) as usize];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

/// Reads attach output for up to `timeout` until it contains `needle`, and answers with all the
/// output read.
pub(crate) async fn read_attach_until_contains(
    stream: &mut UnixStream,
    needle: &str,
    timeout: Duration,
) -> Result<String, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    let mut output = String::new();

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let read = tokio::time::timeout(remaining, read_attach_message(stream)).await;
        let Some(message) = read.map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for attach output containing {needle:?}: {output:?}"),
            )
        })??
        else {
            break;
        };

        if let AttachMessage::Data(bytes) | AttachMessage::Render(bytes) = message {
            output.push_str(&String::from_utf8_lossy(&bytes));
            if output.contains(needle) {
                return Ok(output);
            }
        }
    }

    Err(io::Error::other(format!(
        "timed out waiting for attach output containing {needle:?}: {output:?}"
    ))
    .into())
}

pub(crate) fn session_name(value: &str) -> SessionName {
    SessionName::new(value).expect("valid session name")
}

pub(crate) fn create_stale_socket(socket_path: &Path) -> Result<StdUnixListener, Box<dyn Error>> {
    let parent = socket_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path must include a parent directory",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let listener = StdUnixListener::bind(socket_path)?;
    Ok(listener)
}

pub(crate) async fn wait_for_socket_removal(socket_path: &Path) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + SOCKET_REMOVAL_TIMEOUT;
    loop {
        match std::fs::symlink_metadata(socket_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        if Instant::now() >= deadline {
            break;
        }

        // A timer-backed yield lets the daemon shutdown task make progress even
        // when the executor immediately re-polls a task after `yield_now()`.
        tokio::time::sleep(SOCKET_REMOVAL_POLL_INTERVAL).await;
    }

    Err(io::Error::other(format!(
        "socket '{}' was not removed after drop",
        socket_path.display()
    ))
    .into())
}

pub(crate) fn pane_tty_paths() -> Result<BTreeSet<PathBuf>, Box<dyn Error>> {
    let mut paths = BTreeSet::new();

    for pid in pane_child_pids()? {
        let target = match std::fs::read_link(format!("/proc/{pid}/fd/0")) {
            Ok(target) => target,
            Err(_) => continue,
        };

        if is_pts_device(&target) {
            paths.insert(target);
        }
    }

    Ok(paths)
}

pub(crate) fn pane_child_pids() -> Result<BTreeSet<u32>, Box<dyn Error>> {
    let task_directory = format!("/proc/{}/task", std::process::id());
    let tasks = match std::fs::read_dir(task_directory) {
        Ok(tasks) => tasks,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error.into()),
    };

    let mut pids = BTreeSet::new();

    for task in tasks {
        let task = task?;
        let children = match std::fs::read_to_string(task.path().join("children")) {
            Ok(children) => children,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };

        for pid in children.split_whitespace() {
            pids.insert(pid.parse()?);
        }
    }

    Ok(pids)
}

pub(crate) fn tty_size(path: &Path) -> Result<TerminalSize, Box<dyn Error>> {
    let file = std::fs::File::open(path)?;
    let winsize = tcgetwinsize(&file)?;

    Ok(TerminalSize {
        cols: winsize.ws_col,
        rows: winsize.ws_row,
    })
}

pub(crate) struct RawTty {
    file: std::fs::File,
    original_termios: Termios,
}

impl RawTty {
    pub(crate) fn open(path: &Path) -> Result<Self, Box<dyn Error>> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let original_termios = tcgetattr(&file)?;
        let mut raw_termios = original_termios.clone();
        raw_termios.make_raw();
        raw_termios.special_codes[SpecialCodeIndex::VMIN] = 1;
        raw_termios.special_codes[SpecialCodeIndex::VTIME] = 0;
        tcsetattr(&file, OptionalActions::Now, &raw_termios)?;

        Ok(Self {
            file,
            original_termios,
        })
    }

    pub(crate) fn read_exact(&mut self, len: usize) -> Result<Vec<u8>, Box<dyn Error>> {
        let mut buffer = vec![0; len];
        self.file.read_exact(&mut buffer)?;
        Ok(buffer)
    }

    pub(crate) fn read_exact_with_timeout(
        &mut self,
        len: usize,
        timeout: Duration,
    ) -> Result<Vec<u8>, Box<dyn Error>> {
        let mut fds = [PollFd::new(
            &self.file,
            PollFlags::IN | PollFlags::ERR | PollFlags::HUP,
        )];
        let timeout = Timespec {
            tv_sec: timeout.as_secs() as i64,
            tv_nsec: timeout.subsec_nanos() as i64,
        };

        let ready = poll(&mut fds, Some(&timeout))?;
        if ready == 0 || fds[0].revents().is_empty() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "tty read timed out").into());
        }

        self.read_exact(len)
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
        use std::io::Write;

        self.file.write_all(bytes)?;
        self.file.flush()?;
        Ok(())
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        let _ = tcsetattr(&self.file, OptionalActions::Now, &self.original_termios);
    }
}

fn is_pts_device(path: &Path) -> bool {
    path.parent() == Some(Path::new("/dev/pts"))
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.chars().all(|character| character.is_ascii_digit()))
            .unwrap_or(false)
}

pub(crate) struct ClientConnection {
    stream: UnixStream,
    decoder: FrameDecoder,
    read_buffer: [u8; 4096],
}

impl ClientConnection {
    pub(crate) async fn connect(socket_path: &Path) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            stream: UnixStream::connect(socket_path).await?,
            decoder: FrameDecoder::new(),
            read_buffer: [0; 4096],
        })
    }

    pub(crate) async fn send_request(
        &mut self,
        request: &Request,
    ) -> Result<Response, Box<dyn Error>> {
        let frame = encode_frame(request)?;
        self.stream.write_all(&frame).await?;
        self.read_response().await
    }

    /// Sends `request` and answers with the daemon's response.
    pub(crate) async fn send(
        &mut self,
        request: impl TestRequest,
    ) -> Result<Response, Box<dyn Error>> {
        self.send_request(&request.into_request()).await
    }

    /// Sends `request` and answers with its success payload.
    ///
    /// # Panics
    ///
    /// Panics with the response when the request fails or answers with another variant.
    pub(crate) async fn send_ok<R: TestRequest>(
        &mut self,
        request: R,
    ) -> Result<R::Success, Box<dyn Error>> {
        let request = request.into_request();
        let command = request.command_name();
        let response = self.send_request(&request).await?;
        Ok(
            R::success(response)
                .unwrap_or_else(|response| panic!("{command} failed: {response:?}")),
        )
    }

    /// Creates the detached session `spec` describes and answers with its name.
    pub(crate) async fn create_session(
        &mut self,
        spec: impl SessionSpec,
    ) -> Result<SessionName, Box<dyn Error>> {
        Ok(self
            .send_ok(spec.into_session_request())
            .await?
            .session_name)
    }

    async fn read_response(&mut self) -> Result<Response, Box<dyn Error>> {
        loop {
            match self.decoder.next_frame::<Response>() {
                Ok(Some(response)) => return Ok(response),
                Ok(None) => {}
                Err(error) => return Err(Box::new(error)),
            }

            let bytes_read = self.stream.read(&mut self.read_buffer).await?;
            if bytes_read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed before a response frame arrived",
                )
                .into());
            }

            self.decoder.push_bytes(&self.read_buffer[..bytes_read]);
        }
    }

    pub(crate) async fn begin_attach(
        mut self,
        request: AttachSessionRequest,
    ) -> Result<(AttachSessionResponse, UnixStream), Box<dyn Error>> {
        let frame = encode_frame(&Request::AttachSession(request))?;
        self.stream.write_all(&frame).await?;

        match read_response_exact(&mut self.stream).await? {
            Response::AttachSession(response) => Ok((response, self.stream)),
            other => Err(io::Error::other(format!("unexpected attach response: {other:?}")).into()),
        }
    }
}

pub(crate) async fn read_response_exact(
    stream: &mut UnixStream,
) -> Result<Response, Box<dyn Error>> {
    let frame = read_detached_frame_exact(stream).await?;
    decode_frame(&frame).map_err(Into::into)
}

async fn read_detached_frame_exact(stream: &mut UnixStream) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut frame = Vec::new();
    let mut magic = [0_u8; 1];
    stream.read_exact(&mut magic).await?;
    if magic[0] != RMUX_FRAME_MAGIC {
        return Err(RmuxError::BadFrameMagic(magic[0]).into());
    }
    frame.push(magic[0]);

    let version = read_varint_u32_exact(stream, &mut frame).await?;
    if version != RMUX_WIRE_VERSION {
        return Err(RmuxError::UnsupportedWireVersion {
            got: version,
            minimum: RMUX_WIRE_VERSION,
            maximum: RMUX_WIRE_VERSION,
        }
        .into());
    }

    let mut length_bytes = [0_u8; 4];
    stream.read_exact(&mut length_bytes).await?;
    frame.extend_from_slice(&length_bytes);
    let length = u32::from_le_bytes(length_bytes) as usize;
    if length == 0 {
        return Err(RmuxError::EmptyFrame.into());
    }
    if length > DEFAULT_MAX_DETACHED_FRAME_LENGTH {
        return Err(RmuxError::FrameTooLarge {
            length,
            maximum: DEFAULT_MAX_DETACHED_FRAME_LENGTH,
        }
        .into());
    }

    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).await?;
    frame.extend_from_slice(&payload);
    Ok(frame)
}

async fn read_varint_u32_exact(
    stream: &mut UnixStream,
    frame: &mut Vec<u8>,
) -> Result<u32, Box<dyn Error>> {
    let mut value = 0_u32;
    for index in 0..5 {
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte).await?;
        let byte = byte[0];
        frame.push(byte);
        value |= u32::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }

    Err(RmuxError::Decode("wire-version varint exceeds u32 length".to_owned()).into())
}

pub(crate) struct TestHarness {
    root: PathBuf,
    socket_path: PathBuf,
}

impl TestHarness {
    pub(crate) fn new(label: &str) -> Self {
        let unique_id = UNIQUE_ID.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from("/tmp").join(format!(
            "rxs-{}-{}-{unique_id}",
            compact_label(label),
            std::process::id()
        ));
        let socket_path = root.join("s.sock");
        std::fs::create_dir_all(&root).expect("test harness root");

        Self { root, socket_path }
    }

    pub(crate) fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Where this harness's daemon builds its seed.
    ///
    /// Inside the harness root, so the seed, its snapshots and its log are removed with
    /// everything else this harness owns — and only when the harness itself drops, which is
    /// after the daemon that was publishing into it.
    pub(crate) fn seed_root(&self) -> &Path {
        &self.root
    }
}

fn compact_label(label: &str) -> String {
    let compact = label
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(16)
        .collect::<String>();
    if compact.is_empty() {
        "x".to_owned()
    } else {
        compact
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
