#![forbid(unsafe_code)]
//! The parametric primitives the owned marsh packages share.
//!
//! Everything here is an algorithm or a container with no domain knowledge: a record log that does
//! not know what a record is, a completion state that does not know what completed or why a wait
//! failed, and a directory walk that does not know which entries matter. Policy — which errors a
//! caller distinguishes, which filesystem metadata it reads, what a record means — stays in the
//! package that owns it, supplied as a type parameter or a closure.
//!
//! This crate is a leaf: it depends on no other package in the workspace, and it must stay that
//! way. An application type reaching back into it would turn a shared primitive into a second copy
//! of the domain it was extracted from.

mod directory;
mod recorder;

pub use directory::walk_directory;
pub use recorder::Recorder;

#[cfg(feature = "watch")]
mod completion;

#[cfg(feature = "watch")]
pub use completion::{WaitState, wait_for_completion};
