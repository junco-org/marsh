//! Agent policies judge real traced commands: reads, writes, execs and socket peers.
//!
//! Each test traces interpreters and compilers; they run one at a time so their load does not
//! starve the timing-sensitive tests running beside them.

use std::os::unix::fs::PermissionsExt;

use serial_test::serial;

use super::*;
use crate::{AgentPolicy, AgentRule};

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
    assert!(
        reported
            .iter()
            .any(|line| line.starts_with("net/external blocked: ")),
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
