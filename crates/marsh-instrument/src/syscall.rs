//! Filesystem observations absent from upstream lurk's presentation-oriented record.

use lurk_cli::syscall_info::SyscallInfo;
use serde::{Deserialize, Serialize};

/// A file identity resolved through procfs while its tracee was stopped.
///
/// `path` preserves the kernel's bytes, including any deleted-file decoration. It is never
/// interpreted by the observer as a policy resource or converted through a Unicode string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTarget {
    /// Kernel-reported path bytes.
    pub path: Vec<u8>,
    /// Filesystem device number.
    pub device: u64,
    /// Inode number on that device.
    pub inode: u64,
    /// File kind and permission bits.
    pub mode: u32,
    /// Number of links; zero distinguishes an unlinked file from a literal deleted suffix.
    pub links: u64,
    /// Mount identifier reported by procfs fdinfo.
    pub mount_id: u64,
}

/// An upstream syscall record and only the additional stopped-task evidence Marsh needs.
///
/// Paths occur once, in `paths`; their native arguments remain original pointer addresses.
/// Descriptor arguments remain native `Int` values, sign-extended from Linux's `i32` fd ABI.
/// Other pointers (including buffers and argv) are not read. No display rendering participates
/// in this evidence or its round-trip serde encoding.
#[derive(Debug, Serialize, Deserialize)]
pub struct Syscall {
    /// The original lurk vocabulary, encoded with a lossless field adapter rather than its
    /// presentation-only serializer.
    #[serde(with = "crate::wire::Info")]
    pub info: SyscallInfo,
    /// Delivery sequence assigned to this call's entry, before any completion or exec.
    pub entry_order: u64,
    /// Entry-time cwd when a captured path resolves relative to it.
    pub cwd: Option<FileTarget>,
    /// Exit-time identity of a successfully opened or duplicated descriptor.
    pub return_fd: Option<FileTarget>,
    /// Raw bounded NUL-terminated input paths, indexed by native argument position.
    pub paths: Vec<(usize, Vec<u8>)>,
    /// Entry-time descriptor roles and their identities. An absent identity is not an absent role.
    pub descriptors: Vec<(usize, Option<FileTarget>)>,
    /// First native-endian u64 of `open_how` or `clone_args`, when supplied and readable; for
    /// `io_uring_setup`, the setup flags the kernel accepted.
    pub flags: Option<u64>,
    /// For `io_uring_enter`: the opcodes pending in its submission queue at entry, when the
    /// ring's layout is known and its memory was readable.
    #[serde(default)]
    pub submissions: Option<Vec<u8>>,
}

impl Syscall {
    /// Returns the raw path at a native argument position, if it was captured.
    #[must_use]
    pub fn path(&self, index: usize) -> Option<&[u8]> {
        self.paths
            .iter()
            .find(|(position, _)| *position == index)
            .map(|(_, path)| path.as_slice())
    }

    /// Returns a descriptor role's stopped-task identity, if resolution succeeded.
    ///
    /// # Errors
    /// Fails if the argument was not captured as a descriptor role. `Ok(None)` instead means
    /// the role was captured but procfs could not resolve its target (for example, an invalid fd).
    pub fn fd(&self, index: usize) -> Result<Option<&FileTarget>, String> {
        self.descriptors
            .iter()
            .find(|(position, _)| *position == index)
            .map(|(_, target)| target.as_ref())
            .ok_or_else(|| {
                format!(
                    "{} argument {index} lacks a descriptor observation",
                    self.info.syscall
                )
            })
    }
}
