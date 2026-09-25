//! `OsStr` helpers for top-level argument parsing.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

/// Returns an OS string as bytes for ASCII-only option parsing.
///
/// Those bytes are already the native `OsStr` representation, so this borrows rather than converts.
#[must_use]
pub(crate) fn os_str_bytes(value: &OsStr) -> Vec<u8> {
    value.as_bytes().to_vec()
}
