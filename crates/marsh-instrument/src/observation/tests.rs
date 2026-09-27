//! Fresh exec tracees, one isolated wait owner, and kernel-observed behavior.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::zombie_processes
)]

use super::*;
use lurk_cli::syscall_info::SyscallArg;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const FILTER: &str = "trace=%file,%desc,%process,%memory,%fstat,%fstatfs";
const TEST: &str = "observation::tests::native_observer_regressions";

struct Delivery {
    sequence: u64,
    tid: Pid,
    status: WaitStatus,
    event: Option<u64>,
    call: Option<Syscall>,
}

struct Trace(Vec<Delivery>);
impl Trace {
    fn calls(&self) -> impl Iterator<Item = &Syscall> {
        self.0.iter().filter_map(|delivery| delivery.call.as_ref())
    }
    fn open(&self, path: &Path) -> &Syscall {
        self.calls()
            .find(|call| {
                matches!(call.info.syscall, Sysno::open | Sysno::openat)
                    && !matches!(call.info.result, RetCode::Err(_))
                    && call
                        .paths
                        .iter()
                        .any(|(_, bytes)| bytes == path.as_os_str().as_bytes())
            })
            .unwrap_or_else(|| panic!("no successful open of {path:?}"))
    }
    fn sequence(&self, call: &Syscall) -> u64 {
        self.0
            .iter()
            .find(|delivery| {
                delivery
                    .call
                    .as_ref()
                    .is_some_and(|other| std::ptr::eq(other, call))
            })
            .unwrap()
            .sequence
    }
}

fn args() -> Args {
    Args {
        follow_forks: true,
        expr: vec![FILTER.into()],
        ..Args::default()
    }
}

fn trace(command: &mut Command, mut react: impl FnMut(&Delivery)) -> Trace {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut observer = Observer::attach(Pid::from_raw(child.id().cast_signed()), &args()).unwrap();
    child.stdin.take().unwrap().write_all(b"go\n").unwrap();
    let mut deliveries = Vec::new();
    observer
        .run(|sequence, tid, status, event, call| {
            let delivery = Delivery {
                sequence,
                tid,
                status: WaitStatus::from_raw(tid, status)?,
                event,
                call,
            };
            react(&delivery);
            deliveries.push(delivery);
            Ok(())
        })
        .unwrap();
    Trace(deliveries)
}

fn shell(script: &str) -> Trace {
    trace(
        Command::new("/bin/sh").args(["-c", &format!("read _; {script}")]),
        |_| {},
    )
}

fn files_and_sequences(directory: &Path, file: &Path) {
    let trace = shell(&format!(
        "cd {} && /bin/cat file >/dev/null; exec 3<file; exec 4<&3",
        directory.display()
    ));
    for (index, delivery) in trace.0.iter().enumerate() {
        assert_eq!(delivery.sequence, index as u64 + 1);
    }
    for call in trace.calls() {
        let entry = &trace.0[usize::try_from(call.entry_order).unwrap() - 1];
        assert_eq!(entry.tid, call.info.pid);
        assert!(matches!(entry.status, WaitStatus::PtraceSyscall(_)));
        assert!(entry.call.is_none());
        assert!(entry.sequence < trace.sequence(call));
    }
    let open = trace.open(Path::new("file"));
    let cwd = open.fd(0).unwrap().unwrap();
    assert_eq!(cwd.path, directory.as_os_str().as_bytes());
    assert_eq!(
        open.return_fd.as_ref().unwrap().inode,
        std::fs::metadata(file).unwrap().ino()
    );
    assert_eq!(
        open.return_fd.as_ref().unwrap().path,
        file.as_os_str().as_bytes()
    );
    let RetCode::Ok(fd) = open.info.result else {
        panic!("{open:?}")
    };
    let used = trace.calls().find(|call| call.info.pid == open.info.pid && call.entry_order > open.entry_order
        && matches!(call.info.syscall, Sysno::read | Sysno::splice | Sysno::copy_file_range)
        && matches!(call.info.args.0.first(), Some(SyscallArg::Int(value)) if *value == i64::from(fd))).unwrap();
    assert_eq!(
        used.fd(0).unwrap().unwrap().path,
        file.as_os_str().as_bytes()
    );
    assert!(trace.calls().any(|call| {
        matches!(
            call.info.syscall,
            Sysno::dup | Sysno::dup2 | Sysno::dup3 | Sysno::fcntl
        ) && call
            .return_fd
            .as_ref()
            .is_some_and(|target| target.path == file.as_os_str().as_bytes())
    }));
    assert!(trace.0.iter().any(
        |delivery| matches!(delivery.status, WaitStatus::PtraceEvent(_, _, code) if creation(code))
            && delivery.event.is_some()
    ));
    let bytes = shell(&format!(
        "dd if={} of=/dev/null bs=65536 count=1 2>/dev/null",
        file.display()
    ));
    assert!(bytes.calls().any(|call| call.info.syscall == Sysno::read
        && matches!(call.info.result, RetCode::Address(65_536))));

    let raw = directory.join(std::ffi::OsStr::from_bytes(b"caf\xe9\"\\<\n>"));
    std::fs::write(&raw, b"raw filename").unwrap();
    // A Unicode shell source cannot express arbitrary bytes; pass the byte path as argv instead.
    let traced = self::trace(
        Command::new("/bin/sh")
            .args(["-c", "read _; /bin/cat -- \"$1\" >/dev/null", "sh"])
            .arg(&raw),
        |_| {},
    );
    assert!(
        traced.calls().any(|call| call
            .paths
            .iter()
            .any(|(_, path)| path == raw.as_os_str().as_bytes())),
        "raw bytes survive both pointer capture and the filesystem"
    );
}

fn signals_and_descendants(directory: &Path, file: &Path) {
    let after = directory.join("after-signal");
    std::fs::write(&after, b"after").unwrap();
    let trace = shell(&format!(
        "trap '/bin/cat {file} >/dev/null' USR1; kill -USR1 $$; /bin/sh -c 'kill -9 $$'; /bin/cat {after} >/dev/null",
        file = file.display(),
        after = after.display()
    ));
    let signal = trace
        .0
        .iter()
        .find(|delivery| matches!(delivery.status, WaitStatus::Stopped(_, Signal::SIGUSR1)))
        .unwrap();
    let killed = trace
        .0
        .iter()
        .find(|delivery| matches!(delivery.status, WaitStatus::Signaled(_, Signal::SIGKILL, _)))
        .unwrap();
    assert!(trace.sequence(trace.open(file)) > signal.sequence);
    assert!(trace.sequence(trace.open(&after)) > killed.sequence);

    let script = format!(
        "read _; /bin/sh -c 'kill -STOP $$; /bin/cat {} >/dev/null'",
        file.display()
    );
    let stopped = self::trace(Command::new("/bin/sh").args(["-c", &script]), |delivery| {
        if let WaitStatus::PtraceEvent(tid, Signal::SIGSTOP, code) = delivery.status
            && code == Event::PTRACE_EVENT_STOP as i32
        {
            nix::sys::signal::kill(tid, Signal::SIGCONT).unwrap();
        }
    });
    let stop = stopped.0.iter().find(|delivery| matches!(delivery.status, WaitStatus::PtraceEvent(_, Signal::SIGSTOP, code) if code == Event::PTRACE_EVENT_STOP as i32)).unwrap();
    assert!(stopped.sequence(stopped.open(file)) > stop.sequence);

    let script = format!(
        r#"
import os, subprocess, sys, threading
sys.stdin.readline()
pid = os.fork()
if pid == 0:
    os.open({file:?}, os.O_RDONLY)
    os._exit(0)
os.waitpid(pid, 0)
subprocess.run(["/bin/cat", {file:?}], stdout=subprocess.DEVNULL, check=True)
threading.Thread(target=lambda: os.execv("/bin/cat", ["cat", {file:?}])).start()
threading.Event().wait()
"#,
        file = file.display().to_string()
    );
    let threads = self::trace(Command::new("python3").args(["-c", &script]), |_| {});
    let openers: HashSet<_> = threads
        .calls()
        .filter(|call| {
            call.info.syscall == Sysno::openat
                && call
                    .return_fd
                    .as_ref()
                    .is_some_and(|target| target.path == file.as_os_str().as_bytes())
        })
        .map(|call| call.info.pid)
        .collect();
    assert!(
        openers.len() >= 3,
        "fork child, subprocess and nonleader exec all opened the file"
    );
    let exec = threads.0.iter().find(|delivery| matches!(delivery.status, WaitStatus::PtraceEvent(_, _, code) if code == Event::PTRACE_EVENT_EXEC as i32)
        && delivery.event.is_some_and(|former| former != u64::try_from(delivery.tid.as_raw()).unwrap())).unwrap();
    let completed = threads
        .calls()
        .find(|call| {
            call.info.syscall == Sysno::execve
                && call.info.pid == exec.tid
                && !matches!(call.info.result, RetCode::Err(_))
        })
        .unwrap();
    assert_eq!(
        u64::try_from(
            threads.0[usize::try_from(completed.entry_order).unwrap() - 1]
                .tid
                .as_raw()
        )
        .unwrap(),
        exec.event.unwrap()
    );
}

fn missing_upstream_roles(directory: &Path) {
    let before = directory.join("before");
    let after = directory.join("after");
    std::fs::write(&before, b"roles").unwrap();
    let script = format!(
        r#"
import ctypes, os, sys
sys.stdin.readline()
libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long
before = os.fsencode({before:?})
after = os.fsencode({after:?})
class How(ctypes.Structure):
    _fields_ = [("flags", ctypes.c_uint64), ("mode", ctypes.c_uint64), ("resolve", ctypes.c_uint64)]
how = How(os.O_RDONLY | os.O_CLOEXEC, 0, 0)
fd = libc.syscall({openat2}, -100, ctypes.c_char_p(before), ctypes.byref(how), ctypes.sizeof(how))
assert fd >= 0, ctypes.get_errno()
assert os.read(fd, 5) == b"roles"
assert libc.syscall({utimensat}, fd, ctypes.c_void_p(), ctypes.c_void_p(), 0) == 0, ctypes.get_errno()
os.close(fd)
assert libc.syscall({openat2}, -100, ctypes.c_void_p(1), ctypes.byref(how), ctypes.sizeof(how)) == -1
assert libc.syscall({renameat2}, -100, ctypes.c_char_p(before), -100, ctypes.c_char_p(after), 1) == 0, ctypes.get_errno()
assert libc.syscall({fchmodat2}, -100, ctypes.c_char_p(after), 0o640, 0) == 0, ctypes.get_errno()
"#,
        before = before.display().to_string(),
        after = after.display().to_string(),
        openat2 = Sysno::openat2.id(),
        utimensat = Sysno::utimensat.id(),
        renameat2 = Sysno::renameat2.id(),
        fchmodat2 = Sysno::fchmodat2.id()
    );
    let trace = trace(Command::new("python3").args(["-c", &script]), |_| {});
    let opened = trace
        .calls()
        .find(|call| {
            call.info.syscall == Sysno::openat2 && !matches!(call.info.result, RetCode::Err(_))
        })
        .unwrap();
    assert_eq!(
        opened.flags,
        Some((libc::O_RDONLY | libc::O_CLOEXEC) as u64)
    );
    assert_eq!(
        opened.return_fd.as_ref().unwrap().path,
        before.as_os_str().as_bytes()
    );
    let invalid = trace
        .calls()
        .find(|call| {
            call.info.syscall == Sysno::openat2 && matches!(call.info.result, RetCode::Err(_))
        })
        .unwrap();
    assert!(matches!(
        invalid.info.args.0.get(1),
        Some(SyscallArg::Addr(1))
    ));
    assert!(invalid.path(1).is_none());
    let renamed = trace
        .calls()
        .find(|call| call.info.syscall == Sysno::renameat2)
        .unwrap();
    assert!(matches!(
        renamed.info.args.0.get(4),
        Some(SyscallArg::Int(1))
    ));
    assert_eq!(renamed.path(1), Some(before.as_os_str().as_bytes()));
    assert_eq!(renamed.path(3), Some(after.as_os_str().as_bytes()));
    assert!(
        trace
            .calls()
            .any(|call| call.info.syscall == Sysno::utimensat
                && call.path(1) == Some(&[])
                && call.fd(0).unwrap().is_some())
    );
    assert!(
        trace
            .calls()
            .any(|call| call.info.syscall == Sysno::fchmodat2
                && matches!(call.info.args.0.get(3), Some(SyscallArg::Int(0))))
    );
    assert_eq!(std::fs::read(&after).unwrap(), b"roles");
    assert_eq!(std::fs::metadata(&after).unwrap().mode() & 0o7777, 0o640);
    assert!(!before.exists());
}

fn memory_fixture() {
    // SAFETY: sysconf reads a scalar setting and requires no pointers.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap();
    // SAFETY: this creates a fresh private mapping and aliases no existing object.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            page * 3,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(base, libc::MAP_FAILED);
    let base = base.cast::<u8>();
    // SAFETY: the final page's start is inside the three-page mapping.
    let last = unsafe { base.add(page * 2) };
    // SAFETY: the final page is owned by this new mapping and has no references.
    assert_eq!(unsafe { libc::munmap(last.cast(), page) }, 0);
    let edge = page * 2;
    let bytes = b"ab\xffcd\0";
    // SAFETY: the offset stays inside the first two, still mapped, pages.
    let destination = unsafe { base.add(edge - bytes.len()) };
    // SAFETY: the destination is inside the mapped region, ending at its final byte.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination, bytes.len());
    }
    // SAFETY: the first readable page is owned writable memory.
    unsafe {
        std::ptr::write_bytes(base, b'x', page);
    }
    println!("MEMORY {} {} {}", base as usize, edge, bytes.len());
    std::io::stdout().flush().unwrap();
    let mut release = [0];
    std::io::stdin().read_exact(&mut release).unwrap();
}

fn memory_at_page_edges() {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env("MARSH_OBSERVER_SCENARIO", "memory-fixture")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(output.read_line(&mut line).unwrap(), 0);
        if line.starts_with("MEMORY ") {
            break;
        }
    }
    let locations: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(|value| value.parse().unwrap())
        .collect();
    let tid = Pid::from_raw(child.id().cast_signed());
    let mut observer = Observer::attach(tid, &args()).unwrap();
    let address = locations[0] + locations[1] - locations[2];
    assert_eq!(
        crate::capture::read_path(tid, address).unwrap(),
        b"ab\xffcd"
    );
    assert!(
        crate::capture::read_path(tid, locations[0])
            .unwrap_err()
            .to_string()
            .contains("PATH_MAX")
    );
    assert_eq!(
        crate::capture::read_path(tid, locations[0] + 1).unwrap(),
        vec![b'x'; crate::capture::PATH_LIMIT - 1]
    );
    assert!(crate::capture::read_path(tid, locations[0] + locations[1]).is_err());
    assert!(crate::capture::read_path(tid, 0).is_err());
    assert_eq!(
        crate::capture::read_flags(tid, locations[0] + 1).unwrap(),
        u64::from_ne_bytes([b'x'; 8])
    );
    assert!(crate::capture::read_flags(tid, locations[0] + locations[1] - 7).is_err());
    // Release the fresh executable, then let the same observer own every exit notification.
    child.stdin.take().unwrap().write_all(b"x").unwrap();
    observer.run(|_, _, _, _, _| Ok(())).unwrap();
}

/// A task listed under the host can exit before the observer reaches it, and a busy host does so
/// constantly. Seizing it then finds nothing, which is no failure to attach: the task is simply
/// no longer part of the host, so it is skipped rather than ending the whole observation.
#[test]
fn a_task_gone_before_its_seizure_is_skipped() {
    let mut child = Command::new("/bin/true").spawn().unwrap();
    let gone = Pid::from_raw(child.id().cast_signed());
    child.wait().unwrap();
    let mut observer = Observer {
        selected: SysnoSet::empty(),
        tasks: HashMap::new(),
        initial: Vec::new(),
        sequence: 0,
    };
    assert!(
        !observer.seize(gone, Options::empty()).unwrap(),
        "a vanished task owes no initial stop"
    );
    assert!(observer.tasks.is_empty(), "and is not tracked");
}

#[test]
fn native_observer_regressions() {
    match std::env::var("MARSH_OBSERVER_SCENARIO").as_deref() {
        Ok("memory-fixture") => {
            memory_fixture();
            return;
        }
        Ok("isolated") => {
            let directory = tempfile::tempdir().unwrap();
            let file = directory.path().join("file");
            std::fs::write(&file, vec![b'x'; 70_000]).unwrap();
            files_and_sequences(directory.path(), &file);
            signals_and_descendants(directory.path(), &file);
            missing_upstream_roles(directory.path());
            memory_at_page_edges();
            println!("native observer scenarios completed");
            return;
        }
        Ok(other) => panic!("unknown scenario {other}"),
        Err(_) => {}
    }
    isolated(
        TEST,
        "MARSH_OBSERVER_SCENARIO",
        "isolated",
        Duration::from_secs(90),
        "native observer scenarios completed",
    );
}

/// Reruns `test` alone with `variable=scenario` in a fresh process group, killed after `patience`.
/// The outer test never waits on tracees. The fresh inner test is the sole __WALL owner,
/// avoiding concurrent test-harness waits or a post-fork Rust runtime.
pub(crate) fn isolated(
    test: &str,
    variable: &str,
    scenario: &str,
    patience: Duration,
    completed: &str,
) {
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(variable, scenario)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let group = Pid::from_raw(i32::try_from(child.id()).unwrap());
    let (sent, received) = mpsc::channel();
    let waiter = std::thread::spawn(move || sent.send(child.wait_with_output()).unwrap());
    let result = received.recv_timeout(patience);
    if result.is_err() {
        nix::sys::signal::killpg(group, Signal::SIGKILL).unwrap();
    }
    waiter.join().unwrap();
    let result = result
        .unwrap_or_else(|error| panic!("native scenario {scenario} timed out: {error}"))
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        result.status.success(),
        "{scenario}: {stdout} {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(stdout.contains(completed));
}
