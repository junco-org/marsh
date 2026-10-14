#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::{BPF_F_NO_PREALLOC, bpf_pidns_info},
    helpers::bpf_get_ns_current_pid_tgid,
    macros::{lsm, map},
    maps::{Array, HashMap, LpmTrie, RingBuf, lpm_trie::Key},
    programs::LsmContext,
};
use core::ffi::c_void;
use marsh_peer_class::{
    AF_INET, AF_INET6, Answers, Contact, EXTERNAL, Endpoint, LOOPBACK, LocalKey, MAX_CLASSES,
    UNKNOWN, builtin_class,
};

const EPERM: i32 = -1;
const _: () = assert!(core::mem::size_of::<Endpoint>() == 136);
const _: () = assert!(core::mem::size_of::<Contact>() == 168);

#[map]
static TASK_ROOT: HashMap<u32, u64> = HashMap::with_max_entries(65_536, BPF_F_NO_PREALLOC);
#[map]
static ANSWERS: HashMap<u64, Answers> = HashMap::with_max_entries(4_096, BPF_F_NO_PREALLOC);
#[map]
static CLASS_NET4: LpmTrie<[u8; 12], u32> = LpmTrie::with_max_entries(65_536, 0);
#[map]
static CLASS_NET6: LpmTrie<[u8; 24], u32> = LpmTrie::with_max_entries(65_536, 0);
#[map]
static LOCAL_ALLOWED: HashMap<LocalKey, u32> = HashMap::with_max_entries(65_536, BPF_F_NO_PREALLOC);
#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(1 << 22, 0);
// Sticky: loss, malformed state, or userspace map failure disables ALL governed
// contacts. Ungoverned processes are checked before this switch (machine safety).
#[map]
static HEALTH: Array<u64> = Array::with_max_entries(1, 0);
#[map]
static PID_NAMESPACE: Array<[u64; 2]> = Array::with_max_entries(1, 0);

unsafe extern "C" {
    fn marsh_sockaddr(name: *const c_void, len: i32, out: *mut Endpoint) -> i32;
    fn marsh_message_name(message: *const c_void, name: *mut *const c_void, len: *mut i32) -> i32;
    fn marsh_socket_peer(socket: *const c_void, out: *mut Endpoint) -> i32;
    fn marsh_disconnect(name: *const c_void, len: i32) -> i32;
}

#[lsm(hook = "socket_connect")]
pub fn socket_connect(ctx: LsmContext) -> i32 {
    let previous: i32 = unsafe { ctx.arg(3) };
    if previous != 0 {
        return previous;
    }
    let name: *const c_void = unsafe { ctx.arg(1) };
    let len: i32 = unsafe { ctx.arg(2) };
    if unsafe { marsh_disconnect(name, len) } != 0 {
        // AF_UNSPEC disconnects a socket; no peer contact or observation exists.
        return 0;
    }
    contact(&ctx, false)
}

#[lsm(hook = "socket_sendmsg")]
pub fn socket_sendmsg(ctx: LsmContext) -> i32 {
    let previous: i32 = unsafe { ctx.arg(3) };
    if previous != 0 {
        return previous;
    }
    contact(&ctx, true)
}

#[lsm(hook = "uring_sqpoll")]
pub fn uring_sqpoll(ctx: LsmContext) -> i32 {
    // The kernel hook is int uring_sqpoll(void); only the prior LSM retval exists.
    let previous: i32 = unsafe { ctx.arg(0) };
    if previous != 0 {
        return previous;
    }
    let Some(ids) = current_ids() else {
        return 0;
    };
    if unsafe { TASK_ROOT.get(&ids.tgid) }.is_some() {
        // SQPOLL creates a separate worker TGID without entry-stop refreshes.
        // Refuse before worker creation, even for otherwise permissive/audit roots.
        EPERM
    } else {
        0
    }
}

#[inline(always)]
fn current_ids() -> Option<bpf_pidns_info> {
    let namespace = PID_NAMESPACE.get(0)?;
    let mut ids = bpf_pidns_info { pid: 0, tgid: 0 };
    if unsafe {
        bpf_get_ns_current_pid_tgid(
            namespace[0],
            namespace[1],
            &mut ids,
            core::mem::size_of::<bpf_pidns_info>() as u32,
        )
    } != 0 {
        // Registration rejects namespace-changing children before resume.
        None
    } else {
        Some(ids)
    }
}

#[inline(always)]
fn contact(ctx: &LsmContext, send: bool) -> i32 {
    let Some(ids) = current_ids() else {
        return 0;
    };
    let tgid = ids.tgid;
    // NO_PREALLOC keeps looked-up values RCU alive across userspace replacements.
    let root = match unsafe { TASK_ROOT.get(&tgid) } {
        Some(root) => *root,
        None => return 0,
    };
    // Kernel-shared aligned scalar; all writers only set the sticky value to 1.
    // Volatile keeps both checks visible without unsupported BPF atomic-load IR.
    let healthy = HEALTH.get_ptr_mut(0);
    if healthy.is_none_or(|state| unsafe { state.read_volatile() } != 0) {
        return EPERM;
    }
    let Some(mut event) = EVENTS.reserve::<Contact>(0) else {
        if let Some(state) = healthy {
            unsafe { state.write_volatile(1) };
        }
        return EPERM;
    };
    // Write directly into the reservation rather than putting Contact on BPF's
    // 512-byte stack. Zeroing also makes every ABI byte deterministic.
    let out = event.as_mut_ptr();
    unsafe { core::ptr::write_bytes(out, 0, 1) };
    let record = unsafe { &mut *out };
    record.tgid = tgid;
    record.tid = ids.pid;
    record.root = root;
    let read = if send {
        let socket: *const c_void = unsafe { ctx.arg(0) };
        let message: *const c_void = unsafe { ctx.arg(1) };
        let mut name = core::ptr::null();
        let mut len = 0;
        if unsafe { marsh_message_name(message, &mut name, &mut len) } != 0 {
            -1
        } else if !name.is_null() {
            unsafe { marsh_sockaddr(name, len, &mut record.endpoint) }
        } else {
            unsafe { marsh_socket_peer(socket, &mut record.endpoint) }
        }
    } else {
        let name: *const c_void = unsafe { ctx.arg(1) };
        let len: i32 = unsafe { ctx.arg(2) };
        unsafe { marsh_sockaddr(name, len, &mut record.endpoint) }
    };
    if read != 0 {
        record.endpoint = unsafe { core::mem::zeroed() };
    }
    record.endpoint.normalize();
    record.class = classify(root, &record.endpoint);
    let mut result = decide(record, read != 0);
    if healthy.is_none_or(|state| unsafe { state.read_volatile() } != 0) {
        record.decision = 1;
        result = EPERM;
    }
    event.submit(0);
    result
}

#[inline(always)]
fn classify(root: u64, endpoint: &Endpoint) -> u32 {
    if let Some(class) = builtin_class(endpoint) {
        return class;
    }
    match endpoint.family {
        AF_INET => {
            let mut data = [0; 12];
            data[..8].copy_from_slice(&root.to_be_bytes());
            data[8..].copy_from_slice(&endpoint.address[..4]);
            CLASS_NET4
                .get(&Key::new(96, data))
                .copied()
                .unwrap_or(EXTERNAL)
        }
        AF_INET6 => {
            let mut data = [0; 24];
            data[..8].copy_from_slice(&root.to_be_bytes());
            data[8..].copy_from_slice(&endpoint.address);
            CLASS_NET6
                .get(&Key::new(192, data))
                .copied()
                .unwrap_or(EXTERNAL)
        }
        _ => UNKNOWN,
    }
}

#[inline(always)]
fn decide(record: &mut Contact, unreadable: bool) -> i32 {
    if record.class as usize >= MAX_CLASSES {
        record.decision = 1;
        return EPERM;
    }
    let Some(answers) = (unsafe { ANSWERS.get(&record.root) }).copied() else {
        record.decision = 1;
        return EPERM;
    };
    record.generation = answers.generation;
    let bit = 1u64 << record.class;
    // A failed read could hide ANY class: permit only when all possibilities
    // permit. A valid unsupported family is the ordinary UNKNOWN policy class.
    let stale = answers.stale != 0
        && if unreadable {
            answers.guarded != 0
        } else {
            answers.guarded & bit != 0
        };
    let local = if unreadable {
        answers.local != 0
    } else {
        answers.local & bit != 0 && !local_allowed(record)
    };
    let rule = if unreadable {
        answers.deny != 0
    } else {
        answers.deny & bit != 0
    };
    let denied = stale || rule || local;
    record.decision = match (denied, stale, answers.audit != 0) {
        (false, _, _) => 0,
        (true, false, false) => 1,
        (true, false, true) => 2,
        (true, true, false) => 3,
        (true, true, true) => 4,
    };
    if denied && answers.audit == 0 {
        EPERM
    } else {
        0
    }
}

#[inline(always)]
fn local_allowed(record: &Contact) -> bool {
    if record.endpoint.family == marsh_peer_class::AF_UNIX && record.endpoint.unix_len == 0 {
        // An unnamed socketpair is UNIX, but its name cannot identify a receiver.
        return false;
    }
    let mut key = LocalKey {
        root: record.root,
        endpoint: record.endpoint,
    };
    if unsafe { LOCAL_ALLOWED.get(&key) }.is_some() {
        return true;
    }
    if record.class != LOOPBACK {
        return false;
    }
    // Wildcard listeners bind every local address of this family, at this port.
    // Never cross families: an IPv6 wildcard may have IPV6_V6ONLY enabled.
    key.endpoint.address = [0; 16];
    key.endpoint.scope = 0;
    unsafe { LOCAL_ALLOWED.get(&key) }.is_some()
}

#[unsafe(link_section = "license")]
#[unsafe(no_mangle)]
static LICENSE: [u8; 4] = *b"GPL\0";

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}
