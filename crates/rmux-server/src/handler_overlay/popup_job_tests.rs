use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rmux_proto::TerminalSize;
use tokio::sync::watch;

use super::{PopupIoOperation, PopupIoQueue, POPUP_IO_QUEUE_CAPACITY};

/// A gate an executor awaits instead of finishing, so a test can hold one operation in flight.
///
/// The predecessor of this held a `Condvar` inside the executor, because the queue used to run its
/// operations on a blocking worker. It does not any more: a popup write is `ShellIo::write_input`
/// and a popup resize is `ShellIo::resize`, both asynchronous, and the worker awaits them
/// directly. Blocking a runtime thread here would therefore no longer model anything the queue
/// does — and on the current-thread runtime these tests use, it would deadlock the test itself.
struct IoGate {
    release: watch::Sender<bool>,
}

impl IoGate {
    fn new() -> Arc<Self> {
        let (release, _receiver) = watch::channel(false);
        Arc::new(Self { release })
    }

    fn release(&self) {
        let _ = self.release.send(true);
    }

    async fn wait(&self) {
        let mut released = self.release.subscribe();
        loop {
            if *released.borrow_and_update() {
                return;
            }
            if released.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Releases the gate after three seconds so a regression hangs the one test rather than the suite.
fn arm_gate_watchdog(gate: &Arc<IoGate>) {
    let gate = Arc::clone(gate);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        gate.release();
    });
}

/// Signals "the first operation has started" exactly once.
#[derive(Clone)]
struct StartSignal(Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>);

impl StartSignal {
    fn new() -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        (
            Self(Arc::new(std::sync::Mutex::new(Some(sender)))),
            receiver,
        )
    }

    fn fire(&self) {
        if let Some(sender) = self.0.lock().expect("popup I/O start").take() {
            let _ = sender.send(());
        }
    }
}

async fn await_start(started: tokio::sync::oneshot::Receiver<()>) {
    tokio::time::timeout(std::time::Duration::from_secs(2), started)
        .await
        .expect("the first popup I/O operation should start")
        .expect("popup I/O start sender should remain connected");
}

/// A cancellable queue whose executor signals the first start and then parks every operation on
/// a watchdog-armed gate, counting the operations it runs and the cancellations it receives.
struct GatedQueue {
    queue: PopupIoQueue,
    gate: Arc<IoGate>,
    started_rx: tokio::sync::oneshot::Receiver<()>,
    executions: Arc<AtomicUsize>,
    cancellations: Arc<AtomicUsize>,
}

impl GatedQueue {
    fn spawn() -> Self {
        let gate = IoGate::new();
        arm_gate_watchdog(&gate);
        let (start, started_rx) = StartSignal::new();
        let callback_gate = Arc::clone(&gate);
        let executions = Arc::new(AtomicUsize::new(0));
        let execution_count = Arc::clone(&executions);
        let cancellations = Arc::new(AtomicUsize::new(0));
        let cancellation_count = Arc::clone(&cancellations);
        let queue = PopupIoQueue::spawn_with_cancel(
            move |_| {
                execution_count.fetch_add(1, Ordering::AcqRel);
                let gate = Arc::clone(&callback_gate);
                let start = start.clone();
                async move {
                    start.fire();
                    gate.wait().await;
                    Ok(())
                }
            },
            move || {
                cancellation_count.fetch_add(1, Ordering::AcqRel);
            },
        );
        Self {
            queue,
            gate,
            started_rx,
            executions,
            cancellations,
        }
    }
}

#[tokio::test]
async fn popup_io_queue_preserves_write_resize_write_enqueue_order() {
    let observed = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let gate = IoGate::new();
    arm_gate_watchdog(&gate);
    let (start, started_rx) = StartSignal::new();
    let callback_observed = Arc::clone(&observed);
    let callback_gate = Arc::clone(&gate);
    let queue = PopupIoQueue::spawn(move |operation| {
        let label = match operation {
            PopupIoOperation::Write(bytes) => {
                format!("write:{}", String::from_utf8_lossy(&bytes))
            }
            PopupIoOperation::Resize(size) => {
                format!("resize:{}x{}", size.cols, size.rows)
            }
        };
        let observed = Arc::clone(&callback_observed);
        let gate = Arc::clone(&callback_gate);
        let start = start.clone();
        async move {
            let first = {
                let mut observed = observed.lock().expect("observed popup I/O");
                observed.push(label);
                observed.len() == 1
            };
            if first {
                start.fire();
                gate.wait().await;
            }
            Ok(())
        }
    });

    let first = queue
        .enqueue(PopupIoOperation::Write(b"a".to_vec()))
        .expect("enqueue first write");
    await_start(started_rx).await;
    let second = queue
        .enqueue(PopupIoOperation::Resize(TerminalSize {
            cols: 41,
            rows: 17,
        }))
        .expect("enqueue resize");
    let third = queue
        .enqueue(PopupIoOperation::Write(b"b".to_vec()))
        .expect("enqueue second write");
    gate.release();

    let (first, second, third) = tokio::join!(first.wait(), second.wait(), third.wait());
    first.expect("first write completes");
    second.expect("resize completes");
    third.expect("second write completes");
    assert_eq!(
        *observed.lock().expect("observed popup I/O"),
        ["write:a", "resize:41x17", "write:b"]
    );
}

#[tokio::test]
async fn popup_io_receipt_times_out_when_blocking_write_never_acknowledges() {
    let GatedQueue {
        queue,
        gate,
        started_rx,
        cancellations,
        ..
    } = GatedQueue::spawn();
    let active = queue
        .enqueue(PopupIoOperation::Write(b"blocked".to_vec()))
        .expect("enqueue blocked write");
    await_start(started_rx).await;
    let pending = queue
        .enqueue(PopupIoOperation::Write(b"pending".to_vec()))
        .expect("enqueue pending write");

    let error = active
        .wait()
        .await
        .expect_err("blocked popup I/O must have a deadline");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(cancellations.load(Ordering::Acquire), 1);
    let pending_error = pending
        .wait()
        .await
        .expect_err("timeout must drain queued popup I/O");
    assert_eq!(pending_error.kind(), std::io::ErrorKind::Interrupted);
    assert_eq!(cancellations.load(Ordering::Acquire), 1);

    gate.release();
}

#[tokio::test]
async fn popup_io_queue_saturation_cancels_active_and_pending_work() {
    let GatedQueue {
        queue,
        gate,
        started_rx,
        executions,
        cancellations,
    } = GatedQueue::spawn();

    let active = queue
        .enqueue(PopupIoOperation::Write(b"active".to_vec()))
        .expect("enqueue active write");
    await_start(started_rx).await;
    let pending = (0..POPUP_IO_QUEUE_CAPACITY)
        .map(|index| {
            queue
                .enqueue(PopupIoOperation::Write(vec![index as u8]))
                .expect("enqueue bounded pending write")
        })
        .collect::<Vec<_>>();
    let saturated = queue
        .enqueue(PopupIoOperation::Write(b"overflow".to_vec()))
        .expect("saturation is reported through the receipt");

    let saturated_error = saturated
        .wait()
        .await
        .expect_err("a saturated popup queue must fail closed");
    assert_eq!(saturated_error.kind(), std::io::ErrorKind::WouldBlock);
    let active_error = active
        .wait()
        .await
        .expect_err("saturation must cancel the active popup write");
    assert_eq!(active_error.kind(), std::io::ErrorKind::Interrupted);
    for receipt in pending {
        let error = receipt
            .wait()
            .await
            .expect_err("saturation must release every queued receipt");
        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
    }
    assert_eq!(cancellations.load(Ordering::Acquire), 1);
    assert_eq!(executions.load(Ordering::Acquire), 1);
    assert_eq!(
        queue
            .enqueue(PopupIoOperation::Resize(TerminalSize { cols: 2, rows: 2 }))
            .expect_err("cancelled queue must reject new work")
            .kind(),
        std::io::ErrorKind::BrokenPipe
    );

    gate.release();
}

#[tokio::test]
async fn dropping_last_popup_io_queue_cancels_worker_and_releases_receipts() {
    let GatedQueue {
        queue,
        gate,
        started_rx,
        cancellations,
        ..
    } = GatedQueue::spawn();
    let active = queue
        .enqueue(PopupIoOperation::Write(b"active".to_vec()))
        .expect("enqueue active write");
    await_start(started_rx).await;
    let pending = queue
        .enqueue(PopupIoOperation::Write(b"pending".to_vec()))
        .expect("enqueue pending write");

    drop(queue);

    assert_eq!(
        cancellations.load(Ordering::Acquire),
        0,
        "dropping a superseded queue stops its worker without killing a shared shell"
    );
    let active_error = active
        .wait()
        .await
        .expect_err("queue drop must release active receipt");
    assert_eq!(active_error.kind(), std::io::ErrorKind::Interrupted);
    let pending_error = pending
        .wait()
        .await
        .expect_err("queue drop must release pending receipt");
    assert_eq!(pending_error.kind(), std::io::ErrorKind::Interrupted);
    assert_eq!(cancellations.load(Ordering::Acquire), 1);

    gate.release();
}

#[tokio::test]
async fn dropping_unacknowledged_popup_io_receipt_cancels_worker() {
    let GatedQueue {
        queue,
        gate,
        started_rx,
        cancellations,
        ..
    } = GatedQueue::spawn();
    let receipt = queue
        .enqueue(PopupIoOperation::Write(b"active".to_vec()))
        .expect("enqueue active write");
    await_start(started_rx).await;

    drop(receipt);

    assert_eq!(cancellations.load(Ordering::Acquire), 1);
    assert_eq!(
        queue
            .enqueue(PopupIoOperation::Write(b"late".to_vec()))
            .expect_err("receipt drop must stop future popup I/O")
            .kind(),
        std::io::ErrorKind::BrokenPipe
    );
    gate.release();
}

#[test]
fn a_popup_exit_of_zero_whose_publication_was_refused_is_a_failure() {
    // The one decision `display-popup -E` turns on. A refused publication means the command
    // changed nothing, so closing the popup as though it had succeeded would hide the refusal
    // behind the exit status the program happened to return.
    assert_eq!(super::gated_status(Some(0), true, false), 0);
    assert_eq!(super::gated_status(Some(0), false, false), 1);
    // A real nonzero status is the program's own answer and survives either verdict.
    assert_eq!(super::gated_status(Some(3), true, false), 3);
    assert_eq!(super::gated_status(Some(3), false, false), 3);
    // No verdict at all, and a popup that closed without ever running a line.
    assert_eq!(super::gated_status(None, false, true), 1);
    assert_eq!(super::gated_status(None, true, false), 0);
}
