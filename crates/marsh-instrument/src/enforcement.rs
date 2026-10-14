//! Process-wide socket enforcement selection and the shared policy/kernel boundary.

use std::io;
use std::sync::Mutex;

use marsh_peer_class::{Answers, Contact, Endpoint, Network};

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

/// A root's eagerly computed policy table and local exceptions.
pub struct KernelPolicy {
    /// Answers for every peer class under one policy basis.
    pub answers: Answers,
    /// Declared networks with stable class IDs.
    pub networks: Vec<Network>,
    /// Local endpoints held by commands belonging to the same source.
    pub local: Vec<Endpoint>,
}

/// The policy engine shared by ptrace and kernel socket enforcement.
pub trait Enforcer: Send + Sync {
    /// Judge a ptrace-mode contact using the lazy policy cache.
    fn permits(
        &self,
        run: TraceRun,
        builtin: Option<InvocationId>,
        peer: &[u8],
        source: Option<&FileTarget>,
    ) -> bool;
    /// Compute every class's answer before a kernel-mode contact can run.
    fn refresh(
        &self,
        run: TraceRun,
        builtin: Option<InvocationId>,
        source: Option<&FileTarget>,
    ) -> io::Result<KernelPolicy>;
    /// Preserve the early local taint handoff without using it as kernel evidence.
    fn handoff(
        &self,
        _run: TraceRun,
        _peer: &[u8],
        _source: Option<&FileTarget>,
    ) -> io::Result<()> {
        Ok(())
    }
    /// Consume a kernel-observed contact before the corresponding syscall's effects.
    fn contact(
        &self,
        run: TraceRun,
        builtin: Option<InvocationId>,
        event: Contact,
    ) -> io::Result<()>;
    /// Record an asynchronous kernel contact with no ptrace syscall completion of its own.
    fn asynchronous_contact(
        &self,
        run: TraceRun,
        builtin: Option<InvocationId>,
        order: u64,
        event: Contact,
    ) -> io::Result<()>;
}

pub(crate) static SELECTION: Mutex<Option<(Enforcement, Option<String>)>> = Mutex::new(None);

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

#[cfg(feature = "privileged")]
pub(crate) struct Kernel {
    pub controller: marsh_bpf::Controller,
    pub epochs: std::collections::HashMap<u64, u64>,
    pub networks: std::collections::HashMap<u64, Vec<Network>>,
    pub tasks: std::collections::HashMap<u32, u32>,
    pub local: std::collections::HashMap<u64, Vec<Endpoint>>,
    pub published: std::collections::HashMap<u64, Answers>,
}

#[cfg(feature = "privileged")]
impl Kernel {
    pub fn load() -> io::Result<Self> {
        Ok(Self {
            controller: marsh_bpf::Controller::load()?,
            epochs: std::collections::HashMap::new(),
            networks: std::collections::HashMap::new(),
            tasks: std::collections::HashMap::new(),
            local: std::collections::HashMap::new(),
            published: std::collections::HashMap::new(),
        })
    }
}
