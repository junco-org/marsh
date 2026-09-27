use std::error::Error;
use std::io;
use std::path::Path;
use std::time::Duration;

use rmux_proto::{encode_attach_message, AttachMessage, Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::sleep;

pub(super) const STEP_TIMEOUT: Duration = Duration::from_secs(12);

pub(super) async fn read_response_exact(
    stream: &mut tokio::net::UnixStream,
) -> Result<Response, Box<dyn Error>> {
    tokio::time::timeout(STEP_TIMEOUT, crate::common::read_response_exact(stream))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timed out reading response frame"))?
}

pub(super) async fn send_attach_command(
    stream: &mut tokio::net::UnixStream,
    command: &str,
) -> Result<(), Box<dyn Error>> {
    let mut bytes = command.as_bytes().to_vec();
    bytes.push(b'\r');
    let frame = encode_attach_message(&AttachMessage::Data(bytes))?;
    stream.write_all(&frame).await?;
    Ok(())
}

pub(super) async fn read_attach_until_eof(
    stream: &mut tokio::net::UnixStream,
    timeout_duration: Duration,
) -> Result<(), Box<dyn Error>> {
    let deadline = std::time::Instant::now() + timeout_duration;
    let mut buffer = [0_u8; 256];

    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let bytes_read = tokio::time::timeout(remaining, stream.read(&mut buffer)).await??;
        if bytes_read == 0 {
            return Ok(());
        }
    }
}

pub(super) async fn retry_request_until(
    socket_path: &Path,
    request: &Request,
    expected: &Response,
) -> Result<Response, Box<dyn Error>> {
    for _ in 0..20 {
        let response = crate::common::send_request(socket_path, request).await?;
        if &response == expected {
            return Ok(response);
        }
        sleep(Duration::from_millis(10)).await;
    }

    crate::common::send_request(socket_path, request).await
}
