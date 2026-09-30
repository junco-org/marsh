//! Submission-queue inspection for the `io_uring` rings traced processes set up.
//!
//! A ring's operations reach the kernel without a syscall of their own, so the tracer reads the
//! opcodes pending in the submission queue at every `io_uring_enter` stop. Other threads of the
//! tracee share that memory and can change it before the kernel consumes it; like every argument
//! read, this is observation, not confinement.

use std::collections::HashMap;
use std::io;

use lurk_cli::syscall_info::SyscallArg;
use nix::unistd::Pid;

use crate::capture::read_bytes;
use crate::{FileTarget, Syscall};

const SETUP_SQE128: u32 = 1 << 10;
const SETUP_NO_SQARRAY: u32 = 1 << 16;
/// `IORING_OFF_SQ_RING` and `IORING_OFF_SQES`, the ring fd's mmap offsets.
const SQ_RING: u64 = 0;
const SQES: u64 = 0x1000_0000;

/// One ring's submission-queue layout, from the parameters its setup returned.
struct Layout {
    head: u64,
    tail: u64,
    mask: u64,
    array: Option<u64>,
    entry_size: u64,
}

/// Rings created in one traced process tree, by their file identity.
#[derive(Default)]
pub(crate) struct Rings(HashMap<(u64, u64), Layout>);

impl Rings {
    /// Learns a ring a successful `io_uring_setup` created, and states its setup flags on `call`.
    /// A ring whose parameters cannot be read stays unknown, so its submissions are unobserved.
    pub(crate) fn created(&mut self, tid: Pid, call: &mut Syscall) {
        let Some(ring) = call.return_fd.as_ref() else {
            return;
        };
        let Some(SyscallArg::Addr(params)) = call.info.args.0.get(1) else {
            return;
        };
        let mut bytes = [0_u8; 80];
        let Ok(params) = u64::try_from(*params) else {
            return;
        };
        if read_into(tid, params, &mut bytes).is_err() {
            return;
        }
        let field = |offset: usize| {
            u32::from_ne_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ])
        };
        let flags = field(8);
        call.flags = Some(u64::from(flags));
        self.0.insert(
            (ring.device, ring.inode),
            Layout {
                head: u64::from(field(40)),
                tail: u64::from(field(44)),
                mask: u64::from(field(48)),
                array: (flags & SETUP_NO_SQARRAY == 0).then(|| u64::from(field(64))),
                entry_size: if flags & SETUP_SQE128 == 0 { 64 } else { 128 },
            },
        );
    }

    /// The opcodes pending in `ring` while its `io_uring_enter` is stopped at entry, or `None`
    /// when the ring is unknown or its queue unreadable.
    pub(crate) fn pending(&self, tid: Pid, ring: Option<&FileTarget>) -> Option<Vec<u8>> {
        let ring = ring?;
        let layout = self.0.get(&(ring.device, ring.inode))?;
        let (queue, entries) = mappings(tid, ring.inode)?;
        let head = read_u32(tid, queue + layout.head)?;
        let tail = read_u32(tid, queue + layout.tail)?;
        let mask = read_u32(tid, queue + layout.mask)?;
        let count = tail.wrapping_sub(head);
        if count > mask.checked_add(1)? {
            return None;
        }
        (0..count)
            .map(|offset| {
                let slot = head.wrapping_add(offset) & mask;
                let index = match layout.array {
                    Some(array) => {
                        let index = read_u32(tid, queue + array + 4 * u64::from(slot))?;
                        (index <= mask).then_some(index)?
                    }
                    None => slot,
                };
                let mut opcode = [0];
                read_into(tid, entries + u64::from(index) * layout.entry_size, &mut opcode).ok()?;
                Some(opcode[0])
            })
            .collect()
    }
}

/// The submission-queue ring and entry-array addresses `tid` has mapped for the ring `inode`.
fn mappings(tid: Pid, inode: u64) -> Option<(u64, u64)> {
    let maps = std::fs::read_to_string(format!("/proc/{tid}/maps")).ok()?;
    let mut queue = None;
    let mut entries = None;
    for line in maps.lines() {
        let mut fields = line.split_ascii_whitespace();
        let (Some(range), Some(_), Some(offset), Some(_), Some(node), Some(path)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        if path != "anon_inode:[io_uring]" || node.parse::<u64>().ok() != Some(inode) {
            continue;
        }
        let start = u64::from_str_radix(range.split('-').next()?, 16).ok()?;
        match u64::from_str_radix(offset, 16).ok()? {
            SQ_RING => queue = queue.or(Some(start)),
            SQES => entries = entries.or(Some(start)),
            _ => {}
        }
    }
    queue.zip(entries)
}

fn read_u32(tid: Pid, address: u64) -> Option<u32> {
    let mut bytes = [0; 4];
    read_into(tid, address, &mut bytes).ok()?;
    Some(u32::from_ne_bytes(bytes))
}

fn read_into(tid: Pid, address: u64, buffer: &mut [u8]) -> io::Result<()> {
    let mut copied = 0;
    read_bytes(tid, address, "ring memory wraps address space", |byte| {
        buffer[copied] = byte;
        copied += 1;
        Ok(copied < buffer.len())
    })
}
