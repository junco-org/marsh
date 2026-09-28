//! Checked successor advancement shared by monotonic counters.

/// A `u64` counter whose successor is checked before it is committed.
pub trait CheckedAdvance: Sized {
    /// What a successful advancement yields.
    type Output;
    /// The owner's exhaustion error.
    type Error;
    /// The current value.
    fn value(&self) -> u64;
    /// Commits `value`, an already-checked successor; never called on exhaustion.
    fn advance(self, value: u64) -> Self::Output;
    /// The error for a successor that would overflow.
    fn exhausted() -> Self::Error;
    /// Checks the successor, then commits it through [`advance`](Self::advance).
    fn next(self) -> Result<Self::Output, Self::Error> {
        let value = self.value().checked_add(1).ok_or_else(Self::exhausted)?;
        Ok(self.advance(value))
    }
}
