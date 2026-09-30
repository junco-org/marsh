//! Shared harness for tests that drive a real client connection through the
//! production listener loop.
//!
//! Everything a listener-level regression needs is here: a connected pair of
//! streams on both platforms, a chosen peer identity, and
//! [`run_connection_with_cleanup`] doing the real read/dispatch/upgrade work.
//! Tests that call handler methods directly cannot observe the connection loop
//! at all, so anything the loop itself owes is proven from here.

use super::*;

use rmux_proto::{NewSessionExtRequest, PaneTarget, TerminalSize};

use crate::test_fixtures::{quiet_command, Fixture, SessionSpec};

/// Async client end of the connection under test.
pub(super) type TestClientStream = LocalStream;

pub(super) async fn connected_streams(_label: &str) -> io::Result<(LocalStream, TestClientStream)> {
    LocalStream::pair()
}

pub(super) fn spawn_connection(
    handler: &Arc<RequestHandler>,
    peer: PeerIdentity,
    server: LocalStream,
) -> (watch::Sender<()>, tokio::task::JoinHandle<io::Result<()>>) {
    let handler = Arc::clone(handler);
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let (shutdown_handle, _shutdown_request_rx) = ShutdownHandle::new();
    let connection_id = handler.allocate_connection_id();
    let task = tokio::spawn(async move {
        run_connection_with_cleanup(
            server,
            peer,
            handler,
            connection_id,
            shutdown_rx,
            shutdown_handle,
        )
        .await
    });
    (shutdown_tx, task)
}

pub(super) async fn finish_connection(
    task: tokio::task::JoinHandle<io::Result<()>>,
) -> io::Result<()> {
    match task.await.expect("connection task") {
        Ok(()) => Ok(()),
        // A dropped named-pipe client surfaces as a peer disconnect rather
        // than a clean end-of-stream.
        Err(error) if rmux_ipc::is_peer_disconnect(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

pub(super) async fn write_test_request<S>(stream: &mut S, request: Request) -> io::Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let frame = encode_frame(&request).map_err(io::Error::other)?;
    stream.write_all(&frame).await
}

pub(super) async fn read_test_response<S>(stream: &mut S) -> io::Result<Response>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut decoder = FrameDecoder::new();
    let mut buffer = [0_u8; 512];

    loop {
        if let Some(response) = decoder.next_frame::<Response>().map_err(io::Error::other)? {
            return Ok(response);
        }

        let bytes_read = stream.read(&mut buffer).await?;
        if bytes_read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed before response frame",
            ));
        }
        decoder.push_bytes(&buffer[..bytes_read]);
    }
}

/// Reads one response frame without swallowing whatever follows it.
///
/// An attach response is immediately followed by the upgraded raw byte stream
/// on the same connection, and a buffering reader would consume the opening
/// frame of that stream into its own decoder. Feeding the decoder one byte at
/// a time leaves the transport positioned exactly after the response.
pub(super) async fn read_response_leaving_raw_bytes<S>(stream: &mut S) -> io::Result<Response>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut decoder = FrameDecoder::new();
    let mut byte = [0_u8; 1];

    loop {
        if let Some(response) = decoder.next_frame::<Response>().map_err(io::Error::other)? {
            return Ok(response);
        }
        if stream.read(&mut byte).await? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed before response frame",
            ));
        }
        decoder.push_bytes(&byte);
    }
}

pub(super) async fn start_quiet_pane(handler: &Arc<RequestHandler>, name: &str) -> PaneTarget {
    start_quiet_pane_sized(handler, name, TerminalSize { cols: 12, rows: 4 }).await
}

pub(super) async fn start_quiet_pane_sized(
    handler: &Arc<RequestHandler>,
    name: &str,
    size: TerminalSize,
) -> PaneTarget {
    let session = SessionSpec::create_started(
        handler,
        NewSessionExtRequest {
            size: Some(size),
            command: Some(quiet_command()),
            ..Fixture::fixture(name)
        },
    )
    .await;
    PaneTarget::with_window(session, 0, 0)
}
