//! One shared completion, and the wait that resolves it.
//!
//! The shape a watch-carried result keeps coming back to: a value that is not there yet, a value
//! every holder of the watch shares once it is, and a reason it will never arrive. The two facts
//! that make it worth writing once are easy to get wrong separately — a waiter that attaches after
//! the value was published must not block, and a waiter whose producer disappeared while the state
//! is still [`WaitState::Pending`] must not hang.
//!
//! What the failure *means* stays with the caller. The published failure is a domain error the
//! producer chose, and the lost-producer error is one the waiter's own domain names; this module
//! decides neither.

use std::sync::Arc;

/// What one watch-carried completion holds.
///
/// `Done` is shared, not cloned: every waiter resolves to the same [`Arc`], so a large verdict is
/// published once however many holders read it. `Failed` carries the producer's own reason, which
/// is why it is not collapsed into an absent value.
#[derive(Debug)]
pub enum WaitState<T, E> {
    /// No result yet.
    Pending,
    /// Completed, with the value every waiter shares.
    Done(Arc<T>),
    /// The producer concluded that no value will be delivered, and said why.
    Failed(E),
}

/// Waits until `state` leaves [`WaitState::Pending`].
///
/// Resolves immediately when the state is already terminal, so a waiter attached long after the
/// fact gets the stored answer without blocking, and a terminal value stays observable after the
/// sender is dropped. `on_closed` is consulted only for the one case the state cannot describe:
/// the producer disappearing while the result is still pending, which no amount of further waiting
/// can resolve.
///
/// The bounds are the waiter's, not the body's. A caller awaits this from a task the runtime may
/// move between threads, so the future has to be [`Send`] — and that is a property of what the
/// watch carries: `tokio::sync::watch::Receiver<W>` is `Send` only when `W` is `Send + Sync`, and
/// `W` here holds both the shared [`Arc`] and the published error. Requiring it where the
/// compiler checks it is worth more than asserting it at each call site. `on_closed` is
/// deliberately not bounded: a waiter that never leaves its thread is still served, and one that
/// hands a thread-bound factory to `tokio::spawn` hears about it there, against its own future.
///
/// # Errors
///
/// Fails with the published [`WaitState::Failed`] error, or with whatever `on_closed` returns when
/// the producer was lost with the state still pending.
pub async fn wait_for_completion<T, E>(
    state: &mut tokio::sync::watch::Receiver<WaitState<T, E>>,
    on_closed: impl FnOnce() -> E,
) -> Result<Arc<T>, E>
where
    T: Send + Sync,
    E: Clone + Send + Sync,
{
    loop {
        {
            let current = state.borrow_and_update();
            match &*current {
                WaitState::Done(value) => {
                    let value = Arc::clone(value);
                    drop(current);
                    return Ok(value);
                }
                WaitState::Failed(error) => {
                    let error = error.clone();
                    drop(current);
                    return Err(error);
                }
                WaitState::Pending => {}
            }
        }
        // The sender going away with the state still `Pending` is the producer being lost:
        // nothing will ever resolve this, and a waiter must not hang on it. `on_closed` is
        // consumed on the way out of the loop, never on an iteration that continues.
        if state.changed().await.is_err() {
            return Err(on_closed());
        }
    }
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::watch;

    /// A verdict that cannot be cloned: waiters share it or they do not get it.
    #[derive(Debug, PartialEq, Eq)]
    struct Verdict(u32);

    /// A domain error, independent of anything this crate knows about.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum TestError {
        /// The producer published a refusal.
        Refused,
        /// The producer disappeared.
        Lost(String),
    }

    /// The completion every test publishes on.
    type State = WaitState<Verdict, TestError>;

    /// The single-use factory the loop may consume only on its way out: it owns a value that
    /// cannot be copied, so moving it on a continuing iteration would not compile.
    fn lost_factory() -> impl FnOnce() -> TestError {
        let reason = String::from("producer");
        move || TestError::Lost(reason)
    }

    #[tokio::test]
    async fn a_waiter_attached_before_the_result_resolves_to_it() {
        let (tx, mut waiter) = watch::channel(State::Pending);
        let wait =
            tokio::spawn(async move { wait_for_completion(&mut waiter, lost_factory()).await });
        tokio::task::yield_now().await;

        // Repeated pending notifications wake the waiter and must not resolve anything.
        tx.send_replace(WaitState::Pending);
        tokio::task::yield_now().await;
        tx.send_replace(WaitState::Pending);
        tokio::task::yield_now().await;
        assert!(!wait.is_finished());

        tx.send_replace(WaitState::Done(Arc::new(Verdict(7))));
        let resolved = wait.await.expect("waiter").expect("verdict");
        assert_eq!(*resolved, Verdict(7));
    }

    #[tokio::test]
    async fn late_waiters_share_the_published_value() {
        let (tx, mut first) = watch::channel(State::Pending);
        tx.send_replace(WaitState::Done(Arc::new(Verdict(3))));

        let mut second = first.clone();
        let first = wait_for_completion(&mut first, lost_factory())
            .await
            .expect("first");
        let second = wait_for_completion(&mut second, lost_factory())
            .await
            .expect("second");

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(*first, Verdict(3));
    }

    #[tokio::test]
    async fn a_published_failure_is_the_producer_s_own() {
        let (tx, mut waiter) = watch::channel(State::Pending);
        tx.send_replace(WaitState::Failed(TestError::Refused));

        let error = wait_for_completion(&mut waiter, lost_factory())
            .await
            .expect_err("refusal");
        assert_eq!(error, TestError::Refused);
    }

    #[tokio::test]
    async fn losing_the_producer_while_pending_consults_the_factory() {
        let (tx, mut waiter) = watch::channel(State::Pending);
        let wait =
            tokio::spawn(async move { wait_for_completion(&mut waiter, lost_factory()).await });
        tokio::task::yield_now().await;

        tx.send_replace(WaitState::Pending);
        drop(tx);

        let error = wait.await.expect("waiter").expect_err("producer lost");
        assert_eq!(error, TestError::Lost("producer".to_owned()));
    }

    #[tokio::test]
    async fn a_terminal_state_outlives_its_producer() {
        let (done_tx, mut done) = watch::channel(State::Pending);
        done_tx.send_replace(WaitState::Done(Arc::new(Verdict(11))));
        drop(done_tx);

        let (failed_tx, mut failed) = watch::channel(State::Pending);
        failed_tx.send_replace(WaitState::Failed(TestError::Refused));
        drop(failed_tx);

        assert_eq!(
            *wait_for_completion(&mut done, lost_factory())
                .await
                .expect("stored verdict"),
            Verdict(11)
        );
        assert_eq!(
            wait_for_completion(&mut failed, lost_factory())
                .await
                .expect_err("stored failure"),
            TestError::Refused
        );
    }
}
