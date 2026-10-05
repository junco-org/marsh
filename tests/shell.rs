//! Consumer-visible Shell contracts through the real native helper and the test-only copy backend.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module
)]

mod common;

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use common::{Seed, TIMEOUT, controlled, denied, join, launch, run, status};
use marsh::{ExecutionResult, ShellErrorKind};
use serial_test::serial;

/// A source holding `src/a.txt`.
fn seed() -> Seed {
    Seed::new("src/a.txt", "seed\n")
}

#[derive(clap::Parser)]
struct NativeRead;
impl marsh::builtins::Command for NativeRead {
    type Error = brush_core::Error;
    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        _: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let context = marsh::builtins::current_context().expect("owning context");
        let worker = context.clone();
        let input = context
            .spawn_blocking(move || {
                let mut file = worker
                    .open(
                        Path::new("src/a.txt"),
                        std::fs::OpenOptions::new().read(true),
                    )
                    .unwrap();
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).unwrap();
                bytes
            })
            .unwrap();
        assert_eq!(input.await.unwrap(), b"seed\n");
        Ok(ExecutionResult::success())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn read_claims_cover_external_builtin_and_native_io() {
    for command in [
        "/bin/cat src/a.txt >/dev/null",
        "read -r value < src/a.txt",
        "native-read",
    ] {
        let fixture = seed();
        // Exercise a blocking pool that predates attachment, not only threads created by the run.
        tokio::task::spawn_blocking(|| {}).await.unwrap();
        let a = fixture
            .managed_builder()
            .builtin("native-read", marsh::builtins::builtin::<NativeRead>())
            .build()
            .await
            .unwrap();
        let b = fixture.shell().await;
        run(&a, command).await;
        let error = denied(&b, "printf B > src/a.txt").await;
        let ShellErrorKind::Denied { denials } = error.kind() else {
            unreachable!()
        };
        assert!(
            denials
                .iter()
                .any(|denial| denial.event.resource.segments() == ["src", "a.txt"])
        );
        assert_eq!(fixture.bytes("src/a.txt"), b"seed\n");
        run(&b, "/bin/cat src/a.txt >/dev/null; printf B > src/a.txt").await;
        assert_eq!(
            fixture.bytes("src/a.txt"),
            b"B",
            "the claim was Read, not a fabricated Edit"
        );
        a.close(false).await.unwrap();
        b.close(false).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn persistent_descriptors_reassert_accesses() {
    let fixture = seed();
    let a = fixture.shell().await;
    let b = fixture.shell().await;
    run(&a, "exec 3<src/a.txt").await;
    run(&b, "/bin/cat src/a.txt >/dev/null").await;
    run(&a, "read -r value <&3").await;
    denied(&b, "printf B > src/a.txt").await;
    assert_eq!(fixture.bytes("src/a.txt"), b"seed\n");
    run(&a, "exec 3<&-").await;
    a.close(false).await.unwrap();
    b.close(false).await.unwrap();

    let fixture = seed();
    fixture.git();
    let a = fixture.shell().await;
    let b = fixture.shell().await;
    run(&a, "exec 4>>src/a.txt; git add -- src/a.txt").await;
    run(&b, "/bin/cat src/a.txt >/dev/null").await;
    denied(&a, "printf x >&4").await;
    assert_eq!(fixture.bytes("src/a.txt"), b"seed\n");
    run(&a, "exec 4>&-").await;
    a.close(false).await.unwrap();
    b.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn old_generation_descriptors_fail_without_rebinding_or_harming_a_sibling() {
    let fixture = seed();
    let a = fixture.shell().await;
    let b = fixture.shell().await;
    run(&a, "exec 3<src/a.txt").await;
    run(&b, "exec 4<src/a.txt; printf disjoint > other").await;
    let error = a
        .run("read -r value <&3")
        .await
        .err()
        .expect("retired descriptor refused");
    assert!(
        matches!(error.kind(), ShellErrorKind::Unsupported),
        "{error}"
    );
    run(&a, "exec 3<&-").await;
    run(&b, "read -r value <&4; exec 4<&-").await;
    assert!(
        matches!(b.env_var("value").await.unwrap().value(), brush_core::ShellValue::String(value) if value == "seed")
    );
    a.close(false).await.unwrap();
    b.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn unchanged_bytes_still_require_edit_capabilities() {
    for command in [
        "printf 'seed\\n' > src/a.txt",
        "printf changed > src/a.txt; printf 'seed\\n' > src/a.txt",
    ] {
        let fixture = seed();
        let a = fixture.shell().await;
        let b = fixture.shell().await;
        run(&a, command).await;
        assert_eq!(fixture.bytes("src/a.txt"), b"seed\n");
        denied(&b, "printf B > src/a.txt").await;
        a.close(false).await.unwrap();
        b.close(false).await.unwrap();
        let reopened = fixture.shell().await;
        denied(&reopened, "printf reopened > src/a.txt").await;
        reopened.close(false).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn git_effects_keep_causal_order() {
    for line in [
        "git checkout HEAD -- src/a.txt; printf after > src/a.txt",
        "printf one > src/a.txt; git add -- src/a.txt; printf two > src/a.txt",
        "printf one > src/a.txt; builtin git add -- src/a.txt; printf two > src/a.txt; touch -t 200001010000 src/a.txt",
    ] {
        let fixture = seed();
        fixture.git();
        let a = fixture.shell().await;
        let b = fixture.shell().await;
        run(&a, line).await;
        let expected = if line.starts_with("git checkout") {
            b"after".as_slice()
        } else {
            b"two".as_slice()
        };
        assert_eq!(fixture.bytes("src/a.txt"), expected);
        denied(&b, "printf lost > src/a.txt").await;
        a.close(false).await.unwrap();
        b.close(false).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn external_git_publishes_through_managed_authorization() {
    for line in [
        "printf staged > src/a.txt; /bin/git add -- src/a.txt; printf unrelated > other",
        "printf staged > src/a.txt; /bin/sh -c 'git add -- src/a.txt'; printf unrelated > other",
    ] {
        let fixture = seed();
        fixture.git();
        let index = fixture.bytes(".git/index");
        let a = fixture.shell().await;
        run(&a, line).await;
        assert_ne!(fixture.bytes(".git/index"), index, "{line}");
        assert_eq!(fixture.bytes("src/a.txt"), b"staged");
        assert_eq!(fixture.bytes("other"), b"unrelated");
        a.close(false).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn unmanaged_git_metadata_cannot_publish() {
    for line in [
        "/bin/sh -c 'printf injected > .git/injected'; printf unrelated > other",
        "/bin/mkdir .git/spoof; printf unrelated > other",
    ] {
        let fixture = seed();
        fixture.git();
        let index = fixture.bytes(".git/index");
        let head = fixture.bytes(".git/HEAD");
        let a = fixture.shell().await;
        let error = a.run(line).await.err().expect("unmanaged Git refused");
        assert!(
            matches!(error.kind(), ShellErrorKind::Unsupported),
            "{error}"
        );
        assert_eq!(fixture.bytes(".git/index"), index);
        assert_eq!(fixture.bytes(".git/HEAD"), head);
        assert!(!fixture.source.join("other").exists());
        assert!(!fixture.source.join(".git/injected").exists());
        assert!(!fixture.source.join(".git/spoof").exists());
        run(&a, "printf managed > src/a.txt; git add -- src/a.txt").await;
        assert_eq!(fixture.bytes("src/a.txt"), b"managed");
        a.close(false).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_conflict_never_reexecutes_the_line() {
    let fixture = seed();
    let mut a = controlled(fixture.managed_builder()).await;
    let b = fixture.shell().await;
    let counter = fixture.outside("counter");
    let task = launch(
        &a.shell,
        format!(
            "printf x >> {}; COUNT=$((COUNT+1)); export COUNT; /bin/cat src/a.txt; printf 'READY\\n'; /bin/sh -c 'read value'; printf local > candidate",
            counter.display()
        ),
    );
    assert_eq!(a.ready().await, b"seed\nREADY\n");
    let busy = a
        .shell
        .run("printf should-not-run > busy")
        .await
        .err()
        .expect("second command is busy");
    assert!(matches!(busy.kind(), ShellErrorKind::Busy));
    run(&b, "/bin/cat src/a.txt >/dev/null; printf B > src/a.txt").await;
    a.release();
    let error = join(task).await.err().expect("stale, not replayed");
    assert!(
        matches!(error.kind(), ShellErrorKind::Stale { .. }),
        "{error}"
    );
    assert_eq!(u8::from(error.execution_result().unwrap().exit_code), 0);
    assert_eq!(std::fs::read(counter).unwrap(), b"x");
    assert!(
        matches!(a.shell.env_var("COUNT").await.unwrap().value(), brush_core::ShellValue::String(value) if value == "1")
    );
    assert!(!fixture.source.join("candidate").exists());
    assert!(!fixture.source.join("busy").exists());
    assert_eq!(fixture.bytes("src/a.txt"), b"B");
    a.shell.close(false).await.unwrap();
    b.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn directory_membership_and_git_dependencies_become_stale() {
    for git in [false, true] {
        let fixture = seed();
        if git {
            fixture.git();
        }
        let mut a = controlled(fixture.managed_builder()).await;
        let b = fixture.shell().await;
        let inspect = if git {
            "git status >/dev/null"
        } else {
            "printf '%s\\n' src/* >/dev/null"
        };
        let task = launch(
            &a.shell,
            format!(
                "{inspect}; printf 'READY\\n'; /bin/sh -c 'read value'; printf local > candidate"
            ),
        );
        a.ready().await;
        if git {
            run(
                &b,
                "printf B > src/a.txt; git add -- src/a.txt; git commit -qm newer",
            )
            .await;
        } else {
            run(&b, "printf member > src/new").await;
        }
        a.release();
        let error = join(task).await.err().expect("dependency invalidation");
        assert!(
            matches!(error.kind(), ShellErrorKind::Stale { .. }),
            "{error}"
        );
        assert!(!fixture.source.join("candidate").exists());
        a.shell.close(false).await.unwrap();
        b.close(false).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn disjoint_concurrent_writes_both_publish() {
    let fixture = seed();
    let mut a = controlled(fixture.managed_builder()).await;
    let b = fixture.shell().await;
    let task = launch(
        &a.shell,
        "printf A > a.txt; printf 'READY\n'; /bin/sh -c 'read value'".into(),
    );
    a.ready().await;
    run(&b, "printf B > b.txt").await;
    a.release();
    join(task).await.unwrap();
    assert_eq!(fixture.bytes("a.txt"), b"A");
    assert_eq!(fixture.bytes("b.txt"), b"B");
    run(&*a.shell, "test -f a.txt && test -f b.txt").await;
    a.shell.close(false).await.unwrap();
    b.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn background_producers_finish_before_the_boundary() {
    for line in [
        "( /bin/sh -c 'read value'; printf background > late ) & printf 'READY\n'",
        "( ( /bin/sh -c 'read value'; printf nested > late ) & ) & printf 'READY\n'",
        "printf 'READY\n'; /bin/cat <(/bin/sh -c 'read value'; printf substitution > late)",
    ] {
        let fixture = seed();
        let mut controlled = controlled(fixture.managed_builder()).await;
        let task = launch(&controlled.shell, line.into());
        controlled.ready().await;
        assert!(!task.is_finished());
        assert!(!fixture.source.join("late").exists());
        controlled.release();
        join(task).await.unwrap();
        let expected = if line.contains("nested") {
            b"nested".as_slice()
        } else if line.contains("substitution") {
            b"substitution".as_slice()
        } else {
            b"background".as_slice()
        };
        assert_eq!(fixture.bytes("late"), expected);
        controlled.shell.close(false).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn forced_close_joins_detached_descendants_and_a_sibling_still_publishes() {
    let fixture = seed();
    let mut a = controlled(fixture.managed_builder()).await;
    let b = fixture.shell().await;
    let script = fixture.outside("detached.sh");
    std::fs::write(
        &script,
        b"printf 'READY\\n'\nread value\nprintf leaked > late\n",
    )
    .unwrap();
    let child = format!(
        "exec 3<&0; setsid /bin/sh {} <&3 &",
        brush_core::escape::quote_if_needed(
            &script.to_string_lossy(),
            brush_core::escape::QuoteMode::SingleQuote
        )
    );
    let command = format!(
        "/bin/sh -c {}",
        brush_core::escape::quote_if_needed(&child, brush_core::escape::QuoteMode::SingleQuote)
    );
    let task = launch(&a.shell, command);
    a.ready().await;
    assert!(!task.is_finished());
    tokio::time::timeout(TIMEOUT, a.shell.close(true))
        .await
        .unwrap()
        .unwrap();
    let error = join(task)
        .await
        .err()
        .expect("forced close refuses publication");
    assert!(
        matches!(error.kind(), ShellErrorKind::Interrupted),
        "{error}"
    );
    assert!(a.shell.is_closed());
    assert!(!fixture.source.join("late").exists());
    run(&b, "printf sibling > healthy").await;
    assert_eq!(fixture.bytes("healthy"), b"sibling");
    b.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn abandoning_the_caller_does_not_abandon_finalization() {
    let fixture = seed();
    let mut a = controlled(fixture.managed_builder()).await;
    let task = launch(
        &a.shell,
        "printf 'READY\n'; /bin/sh -c 'read value'; printf completed > owned".into(),
    );
    a.ready().await;
    task.abort();
    let shell = Arc::clone(&a.shell);
    let close = tokio::spawn(async move { shell.close(false).await });
    a.release();
    tokio::time::timeout(TIMEOUT, close)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(fixture.bytes("owned"), b"completed");
    assert!(a.shell.is_closed());
    let reopened = fixture.shell().await;
    denied(&reopened, "printf blind > owned").await;
    reopened.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn directory_and_special_entry_publication() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = seed();
    let shell = fixture.shell().await;
    run(&shell, "mkdir -p empty/nested; chmod 711 empty; printf leaf > empty/nested/file; printf keep > suffix.tmp-wal").await;
    run(&shell, "rm empty/nested/file").await;
    assert!(fixture.source.join("empty/nested").is_dir());
    run(&shell, "rmdir empty/nested; printf file > replacement; rm replacement; mkdir replacement; rmdir replacement; ln -s empty replacement").await;
    assert_eq!(
        std::fs::read_link(fixture.source.join("replacement")).unwrap(),
        Path::new("empty")
    );
    run(&shell, "chmod 700 replacement").await;
    assert_eq!(
        std::fs::metadata(fixture.source.join("empty"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o700
    );
    run(&shell, "chmod 711 replacement").await;
    let error = tokio::time::timeout(TIMEOUT, shell.run("mkfifo fifo"))
        .await
        .unwrap()
        .err()
        .expect("FIFO is refused without a writer");
    assert!(
        matches!(error.kind(), ShellErrorKind::Unsupported),
        "{error}"
    );
    assert!(!fixture.source.join("fifo").exists());
    run(&shell, "printf healthy > healthy").await;
    shell.close(false).await.unwrap();
    let reopened = fixture.shell().await;
    assert_eq!(
        std::fs::metadata(fixture.source.join("empty"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o711
    );
    assert_eq!(fixture.bytes("suffix.tmp-wal"), b"keep");
    assert_eq!(fixture.bytes("healthy"), b"healthy");
    reopened.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn normal_state_scripts_functions_and_status_are_preserved() {
    let fixture = seed();
    let shell = fixture.shell().await;
    run(
        &shell,
        "export KEPT=value; f() { printf function > function.txt; }",
    )
    .await;
    assert!(
        matches!(shell.env_var("KEPT").await.unwrap().value(), brush_core::ShellValue::String(value) if value == "value")
    );
    let parameters = shell.default_exec_params().await;
    assert_eq!(
        shell.invoke_function("f", &[], &parameters).await.unwrap(),
        0
    );
    run(
        &shell,
        "printf 'export SOURCED=yes; printf sourced > sourced.txt\\n' > script.sh",
    )
    .await;
    shell
        .source_script(&fixture.source.join("script.sh"), &[], &parameters)
        .await
        .unwrap();
    assert!(
        matches!(shell.env_var("SOURCED").await.unwrap().value(), brush_core::ShellValue::String(value) if value == "yes")
    );
    shell.set_working_dir(Path::new("src")).await.unwrap();
    assert_eq!(shell.working_dir().await, fixture.source.join("src"));
    assert!(
        matches!(shell.env_var("PWD").await.unwrap().value(), brush_core::ShellValue::String(value) if value.as_str() == fixture.source.join("src").to_string_lossy())
    );
    run(&shell, "readonly PWD").await;
    let pwd = shell.env_var("PWD").await.unwrap();
    assert!(pwd.is_readonly());
    assert!(
        matches!(pwd.value(), brush_core::ShellValue::String(value) if value.as_str() == fixture.source.join("src").to_string_lossy())
    );
    let environment = shell.env().await;
    assert!(
        environment
            .get_using_policy("PWD", brush_core::env::EnvironmentLookup::Anywhere)
            .unwrap()
            .is_readonly()
    );
    assert_eq!(status(&shell, "false").await, 1);
    assert_eq!(status(&shell, "exit 7").await, 7);
    assert!(shell.is_closed());
    assert!(matches!(
        shell.run("true").await.err().unwrap().kind(),
        ShellErrorKind::Closed
    ));
    shell.close(false).await.unwrap();
    assert_eq!(fixture.bytes("function.txt"), b"function");
    assert_eq!(fixture.bytes("sourced.txt"), b"sourced");
}

#[tokio::test]
#[serial]
async fn current_thread_commands_work_but_the_blocking_editor_refuses() {
    let fixture = seed();
    let shell = fixture.shell().await;
    run(&shell, "printf current > current").await;
    let error = shell
        .run_interactively(marsh::UIOptions::default())
        .await
        .err()
        .expect("unsupported editor runtime");
    assert!(matches!(error.kind(), ShellErrorKind::Unsupported));
    shell.close(false).await.unwrap();
    assert_eq!(fixture.bytes("current"), b"current");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn startup_releases_an_externally_deleted_paths_claims_durably() {
    let fixture = seed();
    let owner = fixture.shell().await;
    run(&owner, "printf original > owned").await;
    owner.close(false).await.unwrap();
    std::fs::remove_file(fixture.source.join("owned")).unwrap();
    let replacement = fixture.shell().await;
    run(&replacement, "printf replacement > owned").await;
    replacement.close(false).await.unwrap();
    let reopened = fixture.shell().await;
    denied(&reopened, "printf blind > owned").await;
    assert_eq!(fixture.bytes("owned"), b"replacement");
    reopened.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn startup_preserves_claims_for_other_paths_in_a_partly_reconciled_frame() {
    let fixture = seed();
    let owner = fixture.shell().await;
    run(
        &owner,
        "printf owned > removed; /bin/cat src/a.txt >/dev/null",
    )
    .await;
    owner.close(false).await.unwrap();
    std::fs::remove_file(fixture.source.join("removed")).unwrap();
    let reopened = fixture.shell().await;
    run(&reopened, "printf new > removed").await;
    denied(&reopened, "printf blind > src/a.txt").await;
    assert_eq!(fixture.bytes("src/a.txt"), b"seed\n");
    reopened.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn startup_preserves_claims_over_a_logged_deletion() {
    let fixture = seed();
    let owner = fixture.shell().await;
    run(&owner, "printf owned > deleted").await;
    run(&owner, "rm deleted").await;
    owner.close(false).await.unwrap();
    let reopened = fixture.shell().await;
    denied(&reopened, "printf blind > deleted").await;
    assert!(!fixture.source.join("deleted").exists());
    reopened.close(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn startup_reconciles_a_grant_only_path_when_it_is_deleted_externally() {
    let fixture = seed();
    let owner = fixture.shell().await;
    run(&owner, "/bin/cat src/a.txt >/dev/null").await;
    owner.close(false).await.unwrap();
    std::fs::remove_file(fixture.source.join("src/a.txt")).unwrap();
    let reopened = fixture.shell().await;
    run(&reopened, "printf new > src/a.txt").await;
    reopened.close(false).await.unwrap();
    let next = fixture.shell().await;
    denied(&next, "printf blind > src/a.txt").await;
    next.close(false).await.unwrap();
}
