//! The enforcement modes and the seam between the tracer and the shared policy engine.

use std::io;

use marsh_peer_class::{Answers, Event, Network};

use crate::{FileTarget, InvocationId, TraceRun};

/// Where agent-policy socket contacts are enforced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Enforcement {
    /// Judge the ptrace entry-time address; requires no BPF privileges.
    #[default]
    Unprivileged,
    /// Require BPF LSM enforcement; unavailable kernels fail closed.
    Privileged,
    /// Try BPF LSM and explicitly report any fallback to ptrace.
    Auto,
}

/// One root's answers for the kernel program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelPolicy {
    /// Answers for every peer class, under one policy basis.
    pub answers: Answers,
    /// Declared networks with their class IDs.
    pub networks: Vec<Network>,
    /// Whether recording one more contact could change `answers`: the tracer then refreshes them
    /// at every socket call's entry, since the kernel cannot.
    pub contact_sensitive: bool,
}

/// The policy engine shared by ptrace and kernel socket enforcement.
pub trait Enforcer: Send + Sync {
    /// Judges a contact at its ptrace entry stop, against the address the call names.
    fn permits(
        &self,
        run: TraceRun,
        builtin: Option<InvocationId>,
        peer: &[u8],
        source: Option<&FileTarget>,
    ) -> bool;
    /// The root's current answers, including a read of `source`, the file the stopped call copies
    /// into a socket in-kernel.
    fn refresh(&self, run: TraceRun, source: Option<&FileTarget>) -> io::Result<KernelPolicy>;
    /// A kernel contact or taint receipt of `run`. Its `time` orders it among ptrace records.
    fn event(&self, run: TraceRun, builtin: Option<InvocationId>, event: &Event)
    -> io::Result<()>;
}

/// Runs only in the trusted post-fork setup, before the command's handshake and exec.
/// Root commands must not inherit authority to rewrite maps or inspect the loader's descriptors.
pub(crate) fn restrict_child() -> io::Result<()> {
    let control = |option, value: libc::c_ulong| {
        // SAFETY: these prctl operations take integer arguments, not pointers.
        if unsafe {
            libc::prctl(
                option,
                value,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            )
        } < 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    control(libc::PR_SET_NO_NEW_PRIVS, 1)?;
    // SAFETY: geteuid has no arguments or memory effects.
    if unsafe { libc::geteuid() } == 0 {
        // Preserve existing securebits, adding locked NOROOT so exec cannot restore root caps.
        // SAFETY: PR_GET_SECUREBITS takes no pointer arguments.
        let bits = unsafe {
            libc::prctl(
                libc::PR_GET_SECUREBITS,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            )
        };
        if bits < 0 {
            return Err(io::Error::last_os_error());
        }
        control(
            libc::PR_SET_SECUREBITS,
            libc::c_ulong::from(bits.unsigned_abs()) | 3,
        )?;
        for capability in 0..64 {
            if let Err(error) = control(libc::PR_CAPBSET_DROP, capability) {
                if error.raw_os_error() == Some(libc::EINVAL) {
                    break;
                }
                return Err(error);
            }
        }
    }
    control(
        libc::PR_CAP_AMBIENT,
        libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong,
    )?;
    // Linux capability ABI v3: version/pid followed by two effective/permitted/inheritable rows.
    let header = [0x2008_0522_u32, 0];
    let data = [0_u32; 6];
    // SAFETY: capset reads the live fixed-layout header and the two zero capability rows.
    if unsafe { libc::syscall(libc::SYS_capset, header.as_ptr(), data.as_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // The loader is nondumpable; its capless child must still admit its parent's ptrace seize.
    control(libc::PR_SET_DUMPABLE, 1)
}
