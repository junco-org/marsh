//! A command-scoped carrier that runs its payload's finalizer on every drop.

use std::ops::Deref;

/// Work a [`Completion`] settles when dropped.
pub(super) trait Finalize {
    /// Settles the payload; `completed` is false when the work was abandoned.
    fn finalize(&mut self, completed: bool);
}

/// Owns a payload and finalizes it on every drop.
pub(super) struct Completion<P: Finalize> {
    pub payload: P,
    pub completed: bool,
}
impl<P: Finalize> Completion<P> {
    pub const fn new(payload: P) -> Self {
        Self {
            payload,
            completed: false,
        }
    }
    /// Consumes the carrier, finalizing it as completed at this call.
    pub fn complete(mut self) {
        self.completed = true;
    }
}
impl<P: Finalize> Deref for Completion<P> {
    type Target = P;
    fn deref(&self) -> &P {
        &self.payload
    }
}
impl<P: Finalize> Drop for Completion<P> {
    fn drop(&mut self) {
        self.payload.finalize(self.completed);
    }
}
