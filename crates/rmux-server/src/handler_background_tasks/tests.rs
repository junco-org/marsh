use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use super::*;

#[cfg(unix)]
use rmux_os::process_tree::ProcessTreeChild;

/// The runtime these tests build their handler inside, and drive their closes on.
///
/// A background task is an OS thread that drives its future with `Handle::block_on`, and the
/// handle it blocks on is the one `RequestHandler::new` captured from the *ambient* runtime —
/// the daemon's, in production. These tests are deliberately `#[test]` rather than
/// `#[tokio::test]`, because each of them blocks its own thread on a channel while a second
/// thread drives a lifecycle close; nothing here would make a runtime ambient on its own, the
/// handler would capture `None`, and every spawn would be refused with "has no runtime to run
/// on" before the behaviour under test ever ran.
///
/// Multi-threaded rather than current-thread because several threads block on this one handle at
/// the same time, and one of them stays blocked on purpose while the test asserts that another
/// makes progress. A current-thread runtime has a single core to hand out, so the thread that
/// did not get it could only be driven by the thread that did.
fn lifecycle_test_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("lifecycle background task runtime")
}

#[test]
fn background_task_limiter_releases_capacity_when_permits_drop() {
    let limiter = BackgroundTaskLimiter::new(2);
    let first = limiter.try_acquire().expect("first permit");
    let second = limiter.try_acquire().expect("second permit");
    let error = limiter
        .try_acquire()
        .expect_err("third permit should exceed capacity");
    assert!(
        error.to_string().contains("too many background tasks"),
        "unexpected error: {error}"
    );

    drop(first);
    let third = limiter
        .try_acquire()
        .expect("dropped permit should restore capacity");
    drop(second);
    drop(third);
}

#[test]
fn shutdown_cancels_and_joins_a_started_background_task() {
    let registry = BackgroundTaskRegistry::new();
    let (started_tx, started_rx) = mpsc::channel();
    let (dropped_tx, dropped_rx) = mpsc::channel();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("background task test runtime");
    registry
        .spawn(
            "rmux-background-registry-test",
            runtime.handle().clone(),
            move || async move {
                let _drop_signal = DropSignal(Some(dropped_tx));
                started_tx.send(()).expect("report task startup");
                std::future::pending::<()>().await;
            },
        )
        .expect("spawn tracked task");
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("background task starts");

    let unfinished = registry.begin_shutdown().join(Duration::from_secs(1));

    assert!(unfinished.is_empty(), "unfinished tasks: {unfinished:?}");
    dropped_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("task future is dropped during shutdown");
    assert!(
        registry
            .spawn(
                "rmux-background-after-close",
                runtime.handle().clone(),
                || async {},
            )
            .is_err(),
        "closed registry must reject new tasks"
    );
}

#[test]
fn lifecycle_worker_pending_is_cancelled_and_releases_its_registration() {
    let runtime = lifecycle_test_runtime();
    // Scoped: `Runtime::block_on` below refuses to start a runtime from inside one, and the
    // guard is only needed for the constructor that captures the handle.
    let handler = {
        let _runtime_guard = runtime.enter();
        RequestHandler::new()
    };
    let (started_tx, started_rx) = mpsc::channel();
    let (dropped_tx, dropped_rx) = mpsc::channel();
    handler
        .spawn_lifecycle_producer_task("rmux-lifecycle-worker-pending-test", move || async move {
            let _drop_signal = DropSignal(Some(dropped_tx));
            started_tx.send(()).expect("report lifecycle worker start");
            std::future::pending::<()>().await;
        })
        .expect("spawn lifecycle worker");
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("lifecycle worker starts");

    let close_handler = handler.clone();
    let close_runtime = runtime.handle().clone();
    let (closed_tx, closed_rx) = mpsc::channel();
    let close = std::thread::spawn(move || {
        close_runtime.block_on(close_handler.close_normal_and_drain_lifecycle_producers());
        closed_tx.send(()).expect("report lifecycle close");
    });

    dropped_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("pending lifecycle worker is cancelled before the lane drains");
    closed_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("normal lifecycle close is bounded");
    close.join().expect("normal lifecycle close joins");
    assert!(
        handler
            .reserve_lifecycle_producer_task("rmux-lifecycle-worker-after-close")
            .is_err(),
        "normal producer registration stays sealed after close"
    );
    handler.shutdown_background_tasks_for_drop();
}

#[test]
fn lifecycle_worker_close_drains_an_active_mutation() {
    let runtime = lifecycle_test_runtime();
    let handler = {
        let _runtime_guard = runtime.enter();
        RequestHandler::new()
    };
    let registration = handler
        .reserve_lifecycle_producer_task("rmux-lifecycle-worker-mutation-test")
        .expect("reserve lifecycle worker");
    let mut cancellation = registration.cancellation();
    let (started_tx, started_rx) = mpsc::channel();
    let release = Arc::new(Notify::new());
    let published = Arc::new(AtomicBool::new(false));
    handler
        .spawn_registered_lifecycle_producer_task(
            "rmux-lifecycle-worker-mutation-test",
            registration,
            {
                let release = Arc::clone(&release);
                let published = Arc::clone(&published);
                move || async move {
                    let _mutation =
                        super::super::lifecycle_producer_tasks::begin_current_lifecycle_mutation()
                            .expect("worker mutation admitted");
                    started_tx.send(()).expect("report worker mutation");
                    release.notified().await;
                    published.store(true, Ordering::SeqCst);
                }
            },
        )
        .expect("spawn mutating lifecycle worker");
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("worker mutation starts");

    let close_handler = handler.clone();
    let close_runtime = runtime.handle().clone();
    let (closed_tx, closed_rx) = mpsc::channel();
    let close = std::thread::spawn(move || {
        close_runtime.block_on(close_handler.close_normal_and_drain_lifecycle_producers());
        closed_tx.send(()).expect("report lifecycle close");
    });
    runtime.block_on(cancellation.cancelled());
    assert!(
        closed_rx.try_recv().is_err(),
        "normal close must drain the active mutation"
    );

    release.notify_one();
    closed_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("normal close finishes after mutation publication");
    close.join().expect("normal lifecycle close joins");
    assert!(published.load(Ordering::SeqCst));
    handler.shutdown_background_tasks_for_drop();
}

#[test]
fn lifecycle_worker_preserves_hook_lane_until_final_close() {
    let runtime = lifecycle_test_runtime();
    let handler = {
        let _runtime_guard = runtime.enter();
        RequestHandler::new()
    };
    let registration = handler
        .try_begin_lifecycle_hook_producer()
        .expect("hook producer registered");
    let (started_tx, started_rx) = mpsc::channel();
    let (dropped_tx, dropped_rx) = mpsc::channel();
    handler
        .spawn_registered_lifecycle_producer_task(
            "rmux-lifecycle-hook-worker-test",
            registration,
            move || async move {
                let _drop_signal = DropSignal(Some(dropped_tx));
                started_tx.send(()).expect("report hook worker start");
                std::future::pending::<()>().await;
            },
        )
        .expect("spawn hook-lane worker");
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("hook-lane worker starts");

    runtime
        .block_on(async {
            tokio::time::timeout(
                Duration::from_secs(1),
                handler.close_normal_and_drain_lifecycle_producers(),
            )
            .await
        })
        .expect("normal lane close is bounded");
    assert!(
        dropped_rx.try_recv().is_err(),
        "normal close cannot cancel a lifecycle-hook worker"
    );

    runtime
        .block_on(async {
            tokio::time::timeout(
                Duration::from_secs(1),
                handler.close_and_drain_lifecycle_producers(),
            )
            .await
        })
        .expect("final lane close is bounded");
    dropped_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("final close cancels the hook-lane worker");
    handler.shutdown_background_tasks_for_drop();
}

#[test]
fn background_shutdown_cancels_a_pending_opt_in_lifecycle_worker() {
    let runtime = lifecycle_test_runtime();
    let handler = {
        let _runtime_guard = runtime.enter();
        RequestHandler::new()
    };
    let registration = handler
        .try_begin_lifecycle_hook_producer()
        .expect("hook producer registered");
    let (started_tx, started_rx) = mpsc::channel();
    let (dropped_tx, dropped_rx) = mpsc::channel();
    handler
        .spawn_registered_lifecycle_producer_task(
            "rmux-lifecycle-background-pending-test",
            registration,
            move || async move {
                let _drop_signal = DropSignal(Some(dropped_tx));
                started_tx.send(()).expect("report lifecycle worker start");
                std::future::pending::<()>().await;
            },
        )
        .expect("spawn lifecycle worker");
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("lifecycle worker starts");

    let unfinished = handler
        .background_tasks
        .begin_shutdown()
        .join(Duration::from_secs(1));

    assert!(unfinished.is_empty(), "unfinished tasks: {unfinished:?}");
    dropped_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("pending lifecycle worker is cancelled");
    runtime.block_on(handler.close_and_drain_lifecycle_producers());
}

#[test]
fn background_shutdown_drains_an_active_hook_lane_mutation() {
    let runtime = lifecycle_test_runtime();
    let handler = {
        let _runtime_guard = runtime.enter();
        RequestHandler::new()
    };
    let registration = handler
        .try_begin_lifecycle_hook_producer()
        .expect("hook producer registered");
    let (started_tx, started_rx) = mpsc::channel();
    let release = Arc::new(Notify::new());
    let published = Arc::new(AtomicBool::new(false));
    handler
        .spawn_registered_lifecycle_producer_task(
            "rmux-lifecycle-background-mutation-test",
            registration,
            {
                let release = Arc::clone(&release);
                let published = Arc::clone(&published);
                move || async move {
                    let _mutation =
                        super::super::lifecycle_producer_tasks::begin_current_lifecycle_mutation()
                            .expect("hook mutation admitted");
                    started_tx.send(()).expect("report lifecycle mutation");
                    release.notified().await;
                    published.store(true, Ordering::SeqCst);
                }
            },
        )
        .expect("spawn mutating lifecycle worker");
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("lifecycle mutation starts");

    let shutdown = handler.background_tasks.begin_shutdown();
    release.notify_one();
    let unfinished = shutdown.join(Duration::from_secs(1));

    assert!(unfinished.is_empty(), "unfinished tasks: {unfinished:?}");
    assert!(
        published.load(Ordering::SeqCst),
        "background shutdown must drain an admitted mutation"
    );
    runtime.block_on(handler.close_and_drain_lifecycle_producers());
}

#[cfg(unix)]
#[test]
fn shutdown_joins_a_task_between_process_spawn_and_registration() {
    assert_shutdown_joins_process_registration_race();
}

#[cfg(unix)]
fn assert_shutdown_joins_process_registration_race() {
    let registry = BackgroundTaskRegistry::new();
    let race_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("process race test runtime");
    let shell_processes = Arc::new(super::super::shell_processes::ShellProcessRegistry::new());
    let task_shell_processes = shell_processes.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let spawn_result = registry.spawn(
        "rmux-background-spawn-race-test",
        race_runtime.handle().clone(),
        move || async move {
            run_process_registration_race(task_shell_processes, started_tx, release_rx);
        },
    );
    spawn_result.expect("spawn tracked race task");
    let (parent_pid, descendant_pid) = started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("process tree starts before registration");

    let shutdown = registry.begin_shutdown();
    shell_processes.close_and_terminate();
    let joiner = std::thread::spawn(move || shutdown.join(Duration::from_secs(1)));
    std::thread::sleep(Duration::from_millis(50));
    let detached_before_registration_resolved = joiner.is_finished();
    release_tx.send(()).expect("release registration");
    let unfinished = joiner.join().expect("join shutdown worker");

    assert!(
        !detached_before_registration_resolved,
        "shutdown detached the task before registration resolved"
    );
    assert!(unfinished.is_empty(), "unfinished tasks: {unfinished:?}");
    // The parent is reaped by `ProcessTreeChild`'s drop, so its death is already ordered before
    // this line and can be asserted outright.
    assert!(!rmux_os::process::is_live(parent_pid));
    wait_until_not_live(descendant_pid);
}

/// Blocks until `pid` leaves the live set, which a delivered `SIGKILL` does not guarantee
/// synchronously.
///
/// `ProcessTreeChild`'s drop signals the whole process group and then reaps only its own child.
/// This pid is not that child: it is the `sleep` the shell backgrounded, which init inherits the
/// instant the shell dies, and no process can `wait(2)` on a pid it does not parent. The signal
/// is therefore delivered but its effect is observed only once the kernel has scheduled the
/// target and moved it to `Z`/`X` — under whole-suite load that can trail the `kill` by enough
/// to lose a race that is not actually about the daemon at all.
///
/// So the wait is on the kernel state itself. This does not grant a settling period: it returns
/// the moment the predicate holds, and fails if it never does, which is what would distinguish a
/// genuinely escaped descendant from one that is merely slow to be torn down.
#[cfg(unix)]
fn wait_until_not_live(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while rmux_os::process::is_live(pid) {
        assert!(
            Instant::now() < deadline,
            "descendant {pid} survived the process group kill"
        );
        std::thread::yield_now();
    }
}

#[cfg(unix)]
fn run_process_registration_race(
    shell_processes: Arc<crate::handler::shell_processes::ShellProcessRegistry>,
    started: mpsc::Sender<(u32, u32)>,
    release: mpsc::Receiver<()>,
) {
    let mut command = std::process::Command::new("/bin/sh");
    command
        .args(["-c", "trap '' HUP TERM; sleep 30 & echo $!; wait"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = ProcessTreeChild::spawn(&mut command).expect("spawn unregistered process tree");
    let parent_pid = child.child_mut().id();
    let stdout = child
        .child_mut()
        .stdout
        .take()
        .expect("capture descendant pid");
    let mut stdout = std::io::BufReader::new(stdout);
    let mut descendant_pid = String::new();
    std::io::BufRead::read_line(&mut stdout, &mut descendant_pid).expect("read descendant pid");
    let descendant_pid = descendant_pid
        .lines()
        .next()
        .expect("descendant pid line")
        .parse::<u32>()
        .expect("numeric descendant pid");
    started
        .send((parent_pid, descendant_pid))
        .expect("report spawned tree");

    // Hold the spawned tree across shutdown to model the production race in which the daemon
    // could otherwise detach a helper that has already forked but not yet been admitted. The
    // ledger admits managed jobs rather than process trees now, so what is held here is the
    // unadmitted tree itself; dropping it after the release is what ends it.
    let _ = shell_processes;
    release.recv().expect("release registration race");
    drop(child);
}

struct DropSignal(Option<mpsc::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}
