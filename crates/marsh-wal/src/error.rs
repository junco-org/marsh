//! Infrastructure failures.
//!
//! Every variant names a condition the publication layer could not proceed through: a log that
//! cannot be written or replayed, or an I/O failure underneath one. A command that merely failed is
//! not represented here — this crate never runs one.

use std::io::ErrorKind;

/// Failure raised while logging or publishing a transaction.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The write-ahead log is unusable or its recovery failed.
    #[error("write-ahead log failure: {0}")]
    Wal(String),
    /// Filesystem I/O failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<rustix::io::Errno> for Error {
    fn from(error: rustix::io::Errno) -> Self {
        Self::Io(error.into())
    }
}

/// The lookup failures that mean a path names nothing: it is absent, or an ancestor is no
/// directory.
pub(crate) const ABSENT: &[ErrorKind] = &[ErrorKind::NotFound, ErrorKind::NotADirectory];

/// A fallible result some of whose I/O failures are expected outcomes rather than errors.
pub(crate) trait Tolerate<T> {
    /// The value on success, `None` for an I/O failure of one of `kinds`, and any other failure
    /// as the [`Error`] it is.
    fn tolerate(self, kinds: &[ErrorKind]) -> Result<Option<T>, Error>;
}

impl<T, E: Into<Error>> Tolerate<T> for Result<T, E> {
    fn tolerate(self, kinds: &[ErrorKind]) -> Result<Option<T>, Error> {
        match self.map_err(Into::<Error>::into) {
            Ok(value) => Ok(Some(value)),
            Err(Error::Io(error)) if kinds.contains(&error.kind()) => Ok(None),
            Err(error) => Err(error),
        }
    }
}
