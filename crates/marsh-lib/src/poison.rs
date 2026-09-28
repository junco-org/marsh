//! Explicit recovery of values carried by poisoned synchronization results.

use std::sync::{LockResult, PoisonError};

/// Recovers the value from either a successful or poisoned synchronization result.
///
/// This does not repair the protected data or clear the lock's poison flag. Callers
/// must already permit continued use of the guarded state after a panic.
pub trait RecoverPoison<T>: Into<LockResult<T>> {
    /// Returns the original value, retaining any guard and timeout outcome it carries.
    fn recover(self) -> T {
        self.into().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T> RecoverPoison<T> for LockResult<T> {}
