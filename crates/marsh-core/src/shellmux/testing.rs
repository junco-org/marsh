//! Fixtures the descriptor tests of this module's files share.

use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::task::Poll;

use nix::fcntl::{FcntlArg, OFlag};

/// How long a descriptor test may wait on the kernel before it is a failure rather than a hang.
pub(super) const LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Polls `future` exactly once and leaves it alive, so a caller can establish that it is waiting
/// without consuming it.
pub(super) async fn poll_once<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
}

/// Whether the descriptor itself is in non-blocking mode, as the kernel reports it.
pub(super) fn is_nonblocking(fd: &OwnedFd) -> bool {
    OFlag::from_bits_retain(nix::fcntl::fcntl(fd, FcntlArg::F_GETFL).expect("status flags"))
        .contains(OFlag::O_NONBLOCK)
}
