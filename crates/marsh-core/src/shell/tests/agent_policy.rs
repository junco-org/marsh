//! Agent policies judge real traced commands: reads, writes, execs and socket peers.

use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::{AgentPolicy, AgentRule};

/// Connects to a TEST-NET-1 address that never answers; the attempt itself is the evidence.
const EGRESS: &str = "python3 -c 'import socket; s = socket.socket(); s.setblocking(False); s.connect_ex((\"192.0.2.1\", 9))'";

impl Fixture {
    /// A managed shell over the seed judged by `policy`.
    async fn agent(&self, policy: AgentPolicy) -> Shell {
        let mut builder = Shell::builder()
            .working_dir(self.seed.clone())
            .sandbox_policy(SandboxPolicy::allow())
            .agent_policy(Some(Arc::new(policy)));
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
async fn egress_after_a_secret_read_is_refused_with_its_effects() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    let shell = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    accepted(&shell, EGRESS).await;
    accepted(&shell, "cat .env > /dev/null").await;
    refused(&shell, &format!("printf leaked > src/out.txt; {EGRESS}")).await;
    assert!(!fixture.seed.join("src/out.txt").exists());
    accepted(&shell, "printf kept > src/out.txt").await;
    assert_eq!(fixture.read("src/out.txt"), "kept");
    close(shell).await;
}

#[tokio::test]
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
    refused(&shell, EGRESS).await;
    close(shell).await;
}

#[tokio::test]
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
async fn shells_without_an_agent_policy_are_unaffected_but_observed() {
    let fixture = Fixture::new();
    fixture.write(".env", "TOKEN=1\n");
    let agent = fixture.agent(AgentPolicy::recommended().unwrap()).await;
    let plain = fixture.shell().await;
    accepted(&plain, "cat .env > /dev/null").await;
    accepted(&plain, EGRESS).await;
    // The plain shell's read is not the agent's: taint follows the reading principal.
    accepted(&agent, EGRESS).await;
    close(plain).await;
    close(agent).await;
}
