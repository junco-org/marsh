//! The provider's two output streams, turned into lines.
//!
//! Upstream read the child's pipes on two OS threads with a blocking `BufReader`. The streams are
//! managed ones now and the readers are tasks on the daemon runtime, but everything the readiness
//! scan depends on is unchanged: the 16KiB line bound and its resume-at-the-next-newline
//! behaviour, the `\r\n` stripping, the final unterminated line, and the 64-line bounded channel
//! that lets a chatty provider apply backpressure instead of growing an unbounded queue.
//!
//! # These bytes are provisional
//!
//! They come out of a job whose publication gate has not run. Matching a URL in them is a
//! statement about what the provider *printed*, which is real and is all readiness needs. It is
//! not a claim that anything the provider wrote to the filesystem was approved; that is the
//! command's verdict, and it is a separate answer.

use rmux_core::events::OutputCursorItem;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::debug;

use super::preset::ProcessOutput;
use crate::io::OutputStream;

const LINE_CHANNEL_CAPACITY: usize = 64;
const MAX_PROVIDER_LINE_BYTES: usize = 16 * 1024;

type ProviderLine = (ProcessOutput, String);

pub(super) fn channel() -> (mpsc::Sender<ProviderLine>, mpsc::Receiver<ProviderLine>) {
    mpsc::channel(LINE_CHANNEL_CAPACITY)
}

/// Starts the reader for one of the provider's streams.
///
/// On the daemon's runtime, like every other managed operation: a reader on a private runtime
/// would be a second scheduler owning bytes that belong to this one's jobs. The returned handle
/// is the caller's claim on the task, which ends by itself when the stream reaches end of file.
pub(super) fn spawn_reader(
    runtime: &Handle,
    stream: OutputStream,
    tx: mpsc::Sender<ProviderLine>,
    source: ProcessOutput,
) -> JoinHandle<()> {
    runtime.spawn(read_lines(source, stream, tx))
}

async fn read_lines(
    source: ProcessOutput,
    mut stream: OutputStream,
    tx: mpsc::Sender<ProviderLine>,
) {
    let mut line = Vec::with_capacity(MAX_PROVIDER_LINE_BYTES);
    let mut discarding_overflow = false;

    loop {
        let item = match stream.recv().await {
            Ok(Some(item)) => item,
            Ok(None) => {
                if !discarding_overflow && !line.is_empty() {
                    let _ = send_line(source, &mut line, false, &tx).await;
                }
                return;
            }
            Err(error) => {
                debug!("web-share tunnel output read failed: {error}");
                return;
            }
        };
        let event = match item {
            OutputCursorItem::Event(event) => event,
            OutputCursorItem::Gap(gap) => {
                // Bytes were lost, so the partial line goes with them and the rest of the
                // interrupted line is discarded up to the next newline. Joining what arrived
                // before the gap onto what arrived after would manufacture a line the provider
                // never printed — and a manufactured line is exactly what the URL scan reads.
                debug!(
                    missed_events = gap.missed_events(),
                    expected_sequence = gap.expected_sequence(),
                    resume_sequence = gap.resume_sequence(),
                    "web-share tunnel output gap"
                );
                line.clear();
                discarding_overflow = true;
                continue;
            }
        };

        for &byte in event.bytes() {
            if byte == b'\n' {
                if !discarding_overflow && !send_line(source, &mut line, true, &tx).await {
                    return;
                }
                line.clear();
                discarding_overflow = false;
                continue;
            }
            if discarding_overflow {
                continue;
            }
            if line.len() == MAX_PROVIDER_LINE_BYTES {
                debug!(
                    limit = MAX_PROVIDER_LINE_BYTES,
                    "web-share tunnel output line truncated"
                );
                if !send_line(source, &mut line, false, &tx).await {
                    return;
                }
                line.clear();
                discarding_overflow = true;
                continue;
            }
            line.push(byte);
        }
    }
}

async fn send_line(
    source: ProcessOutput,
    line: &mut Vec<u8>,
    strip_carriage_return: bool,
    tx: &mpsc::Sender<ProviderLine>,
) -> bool {
    if strip_carriage_return && line.last() == Some(&b'\r') {
        line.pop();
    }
    let line = String::from_utf8_lossy(line).into_owned();
    tx.send((source, line)).await.is_ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rmux_core::events::{OutputCursorItem, OutputEvent};
    use tokio::sync::mpsc;

    use super::{read_lines, MAX_PROVIDER_LINE_BYTES};
    use crate::io::OutputStream;
    use crate::web::tunnel::preset::ProcessOutput;

    /// An ended owner stream carrying `chunks`, the shape a managed execution hands the reader.
    fn stream(chunks: impl IntoIterator<Item = Vec<u8>>) -> OutputStream {
        let (tx, rx) = mpsc::channel(16);
        for (sequence, bytes) in chunks.into_iter().enumerate() {
            let sequence = u64::try_from(sequence).expect("test chunk counts fit a u64");
            tx.try_send(OutputCursorItem::Event(OutputEvent::from_shared(
                sequence,
                Arc::from(bytes),
                Vec::new(),
            )))
            .expect("test chunks fit the stream channel");
        }
        drop(tx);
        OutputStream::owner(rx)
    }

    #[tokio::test]
    async fn reader_bounds_overlong_lines_and_resumes() {
        let mut output = vec![b'a'; MAX_PROVIDER_LINE_BYTES + 1];
        output.extend_from_slice(b"\nnext\r\n");
        let (tx, mut rx) = mpsc::channel(4);

        read_lines(ProcessOutput::Stdout, stream([output]), tx).await;

        let (_, bounded) = rx.recv().await.expect("bounded prefix is forwarded");
        let (_, next) = rx.recv().await.expect("next line is forwarded");
        assert_eq!(bounded.len(), MAX_PROVIDER_LINE_BYTES);
        assert!(bounded.bytes().all(|byte| byte == b'a'));
        assert_eq!(next, "next");
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn reader_forwards_final_unterminated_line() {
        let (tx, mut rx) = mpsc::channel(2);

        read_lines(ProcessOutput::Stderr, stream([b"last line".to_vec()]), tx).await;

        let (_, line) = rx.recv().await.expect("unterminated line is forwarded");
        assert_eq!(line, "last line");
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn reader_joins_lines_split_across_chunks() {
        let (tx, mut rx) = mpsc::channel(2);

        read_lines(
            ProcessOutput::Stdout,
            stream([b"https://exa".to_vec(), b"mple.test\r\n".to_vec()]),
            tx,
        )
        .await;

        let (_, line) = rx.recv().await.expect("split line is reassembled");
        assert_eq!(line, "https://example.test");
        assert!(rx.recv().await.is_none());
    }
}
