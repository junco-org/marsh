//! Storage faults are tested privately; consumer Shell APIs expose only their typed verdicts.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::super::ShellErrorKind;
use super::{Fixture, accepted, close, refused, session};
use marsh_btrfs::Subvolumes;
use serial_test::serial;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[tokio::test]
#[serial]
async fn complete_bad_wal_records_reset_startup_without_changing_the_seed() {
    let fixture = Fixture::new();
    let owner = fixture.shell().await;
    accepted(&owner, "printf first > first").await;
    accepted(&owner, "printf second > second").await;
    let log = fixture.log();
    close(owner).await;
    let original = std::fs::read(&log).unwrap();
    let split = original.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    let mut corrupt = original[..split].to_vec();
    corrupt.extend_from_slice(b"{not a complete valid record}\n");
    corrupt.extend_from_slice(&original[split..]);
    std::fs::write(&log, &corrupt).unwrap();
    let reopened = fixture
        .open()
        .await
        .expect("an undecodable startup log is reset");
    assert_eq!(std::fs::read(&log).unwrap(), b"");
    assert_eq!(std::fs::read(fixture.seed.join("first")).unwrap(), b"first");
    assert_eq!(
        std::fs::read(fixture.seed.join("second")).unwrap(),
        b"second"
    );
    close(reopened).await;
}

#[tokio::test]
#[serial]
async fn missing_staging_at_startup_starts_a_fresh_usable_wal() {
    let fixture = Fixture::new();
    let shell = fixture.shell().await;
    accepted(&shell, "printf before > before").await;
    close(shell).await;
    let log = fixture.log();
    let original = std::fs::read(&log).unwrap();
    let split = original.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    let mut begin: serde_json::Value = serde_json::from_slice(&original[..split]).unwrap();
    assert_eq!(begin["op"], "BEGIN");
    begin
        .as_object_mut()
        .unwrap()
        .remove("staging")
        .expect("a current BEGIN names its staging");
    let mut incompatible = serde_json::to_vec(&begin).unwrap();
    incompatible.push(b'\n');
    incompatible.extend_from_slice(&original[split..]);
    std::fs::write(&log, &incompatible).unwrap();

    let reopened = fixture
        .open()
        .await
        .expect("an incompatible startup log is reset");
    assert_eq!(std::fs::read(&log).unwrap(), b"");
    assert_eq!(fixture.read("before"), "before");
    accepted(&reopened, "printf recovered > recovered").await;
    assert_eq!(fixture.read("recovered"), "recovered");
    close(reopened).await;
    let fresh = std::fs::read(&log).unwrap();

    let reopened = fixture.open().await.expect("the fresh log recovers");
    assert_eq!(std::fs::read(&log).unwrap(), fresh);
    refused(&reopened, "printf blind > recovered").await;
    assert_eq!(fixture.read("recovered"), "recovered");
    close(reopened).await;
}

#[tokio::test]
#[serial]
async fn pre_intent_failure_preserves_exit_and_rolls_back_tentative_grants() {
    use super::super::policy::{Action, Event, Resource};
    use rust_validator::PolicyDecision;
    let fixture = Fixture::new();
    let shell = fixture.shell().await;
    // A live sibling keeps the source's in-memory authority across the failing shell's close.
    let sibling = fixture.shell().await;
    let validator = Arc::clone(&session(&sibling).await.validator);
    let blind = Event::new(
        sibling.principal().clone(),
        Action::Edit,
        Resource::from(["candidate"]),
    );
    let log = fixture.log();
    std::fs::create_dir(&log).unwrap();
    let error = shell
        .run("printf candidate > candidate; exit 7")
        .await
        .err()
        .expect("obstructed publication");
    assert!(matches!(error.kind(), ShellErrorKind::Infrastructure));
    assert_eq!(u8::from(error.execution_result().unwrap().exit_code), 7);
    assert!(shell.is_closed());
    assert!(!fixture.seed.join("candidate").exists());
    close(shell).await;
    assert!(
        matches!(validator.decide(&blind).unwrap(), PolicyDecision::Grant),
        "a routing query never sees the refused command's tentative grant"
    );
    std::fs::remove_dir(log).unwrap();
    let reopened = fixture.shell().await;
    accepted(&reopened, "printf accepted > candidate").await;
    assert_eq!(
        std::fs::read(fixture.seed.join("candidate")).unwrap(),
        b"accepted"
    );
    assert!(
        matches!(
            validator.decide(&blind).unwrap(),
            PolicyDecision::Deny { .. }
        ),
        "a committed grant is visible to the same query"
    );
    close(reopened).await;
    close(sibling).await;
}

#[tokio::test]
#[serial]
async fn failed_intent_retains_redo_and_poison_is_source_local() {
    let fixture = Fixture::new();
    let shell = fixture.shell().await;
    let sibling = fixture.shell().await;
    let independent = Fixture::new();
    let other = independent.shell().await;
    let log = fixture.log();
    std::os::unix::fs::symlink("/dev/full", &log).unwrap();
    let error = shell
        .run("printf candidate > candidate; false")
        .await
        .err()
        .expect("intent ENOSPC");
    assert!(matches!(error.kind(), ShellErrorKind::Infrastructure));
    assert_eq!(u8::from(error.execution_result().unwrap().exit_code), 1);
    {
        let session = session(&shell).await;
        assert!(session.validator.read().recovery_required);
        let redo = std::fs::read_dir(session.persistence.snap())
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().contains("-redo-"))
            .expect("frozen redo retained");
        assert_eq!(
            std::fs::read(redo.path().join("candidate")).unwrap(),
            b"candidate"
        );
    }
    let error = sibling.run("printf forbidden > later").await.err().unwrap();
    assert!(matches!(error.kind(), ShellErrorKind::Infrastructure));
    assert!(error.execution_result().is_none());
    accepted(&other, "printf healthy > other").await;
    assert_eq!(
        std::fs::read(independent.seed.join("other")).unwrap(),
        b"healthy"
    );
    assert!(!fixture.seed.join("candidate").exists());
    assert!(!fixture.seed.join("later").exists());
    assert!(shell.close(true).await.is_err());
    close(sibling).await;
    close(other).await;
    std::fs::remove_file(log).unwrap();
    let reopened = fixture.shell().await;
    accepted(&reopened, "printf recovered > candidate").await;
    close(reopened).await;
}

struct RefuseCleanup {
    inner: Arc<marsh_btrfs::fake::CopyTree>,
    refuse: AtomicBool,
}
impl Subvolumes for RefuseCleanup {
    fn is_subvolume(&self, path: &Path) -> bool {
        self.inner.is_subvolume(path)
    }
    fn is_mount_root(&self, path: &Path) -> Result<bool, marsh_btrfs::Error> {
        self.inner.is_mount_root(path)
    }
    fn assert_btrfs(&self, path: &Path) -> Result<(), marsh_btrfs::Error> {
        self.inner.assert_btrfs(path)
    }
    fn assert_user_subvol_rm_allowed(&self, path: &Path) -> Result<(), marsh_btrfs::Error> {
        self.inner.assert_user_subvol_rm_allowed(path)
    }
    fn create_subvolume(&self, path: &Path) -> Result<(), marsh_btrfs::Error> {
        self.inner.create_subvolume(path)
    }
    fn snapshot(&self, from: &Path, to: &Path) -> Result<(), marsh_btrfs::Error> {
        self.inner.snapshot(from, to)
    }
    fn snapshot_readonly(&self, from: &Path, to: &Path) -> Result<(), marsh_btrfs::Error> {
        self.inner.snapshot_readonly(from, to)
    }
    fn delete_subvolume(&self, path: &Path) -> Result<(), marsh_btrfs::Error> {
        if self.refuse.load(Ordering::Acquire)
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("-base-"))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "test reclamation obstruction",
            )
            .into());
        }
        self.inner.delete_subvolume(path)
    }
}
#[tokio::test]
#[serial]
async fn cleanup_after_end_cannot_relabel_committed_bytes_as_unpublished() {
    let fixture = Fixture::new();
    let fs = Arc::new(RefuseCleanup {
        inner: fixture.fs.clone(),
        refuse: AtomicBool::new(false),
    });
    let shell = fixture.open_on(fs.clone()).await.unwrap();
    fs.refuse.store(true, Ordering::Release);
    let native = shell.run("printf committed > file; exit 7").await.unwrap();
    assert_eq!(u8::from(native.exit_code), 7);
    assert!(shell.is_closed());
    assert_eq!(
        std::fs::read(fixture.seed.join("file")).unwrap(),
        b"committed"
    );
    assert!(
        shell.close(false).await.is_err(),
        "cleanup failure remains observable after automatic close"
    );
    fs.refuse.store(false, Ordering::Release);
    let reopened = fixture.open_on(fs).await.unwrap();
    refused(&reopened, "printf blind > file").await;
    close(reopened).await;
}

#[tokio::test]
#[serial]
async fn a_distinct_process_is_excluded_until_the_last_live_shell_closes() {
    if let Some(source) = std::env::var_os("MARSH_LEASE_SOURCE") {
        lease_child(PathBuf::from(source)).await;
        return;
    }
    let fixture = Fixture::new();
    let shell = fixture.shell().await;
    let execute = |expectation: &str| {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "shell::tests::publication::a_distinct_process_is_excluded_until_the_last_live_shell_closes",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("MARSH_LEASE_SOURCE", &fixture.seed)
            .env("MARSH_LEASE_EXPECT", expectation)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("lease child executed"));
    };
    execute("busy");
    close(shell).await;
    execute("released");
    assert_eq!(fixture.read("child"), "child");
}

/// The re-executed child half of the cross-process lease regression: opens its own shell over
/// `source` and requires the outcome its parent expects of the live lease.
async fn lease_child(source: PathBuf) {
    let expected = std::env::var("MARSH_LEASE_EXPECT").expect("child expectation");
    let fs = Arc::new(marsh_btrfs::fake::CopyTree::new());
    fs.register(&source);
    // Storage is opened by the first managed command, so that is where the lease is refused.
    match (expected.as_str(), super::managed(source, fs).await) {
        ("busy", Err(error)) => assert!(matches!(error.kind(), ShellErrorKind::Infrastructure)),
        ("released", Ok(shell)) => {
            accepted(&shell, "printf child > child").await;
            close(shell).await;
        }
        _ => panic!("lease expectation did not hold"),
    }
    println!("lease child executed");
}

#[tokio::test]
#[serial]
async fn incompatible_ownership_metadata_resets_startup_without_changing_the_seed() {
    const FILES: [&str; 3] = ["first", "second", "third"];
    for violation in [
        "missing principal",
        "missing grants",
        "empty principal",
        "obsolete ownership",
    ] {
        let fixture = Fixture::new();
        let shell = fixture.shell().await;
        for name in FILES {
            accepted(&shell, &format!("printf {name} > {name}")).await;
        }
        let log = fixture.log();
        close(shell).await;
        let original = std::fs::read(&log).unwrap();
        let mut offset = 0;
        let mut frames = 0;
        let (start, end, mut begin) = original
            .split_inclusive(|byte| *byte == b'\n')
            .find_map(|line| {
                let start = offset;
                offset += line.len();
                let record: serde_json::Value = serde_json::from_slice(line).unwrap();
                if record["op"] != "BEGIN" {
                    return None;
                }
                frames += 1;
                (frames == 2).then_some((start, offset, record))
            })
            .expect("second transaction has a valid prefix and suffix");
        let metadata = begin.as_object_mut().unwrap();
        match violation {
            "missing principal" => {
                metadata.remove("principal").unwrap();
            }
            "missing grants" => {
                metadata.remove("granted").unwrap();
            }
            "empty principal" => {
                metadata.insert("principal".into(), "".into());
            }
            "obsolete ownership" => {
                metadata.insert("durable_principal".into(), "another-owner".into());
            }
            _ => unreachable!(),
        }
        let mut invalid = original[..start].to_vec();
        invalid.extend(serde_json::to_vec(&begin).unwrap());
        invalid.push(b'\n');
        invalid.extend_from_slice(&original[end..]);
        std::fs::write(&log, &invalid).unwrap();
        let reopened = fixture
            .open()
            .await
            .unwrap_or_else(|error| panic!("{violation}: {error}"));
        assert_eq!(std::fs::read(&log).unwrap(), b"", "{violation}");
        for name in FILES {
            assert_eq!(fixture.read(name), name);
        }
        close(reopened).await;
    }
}

#[tokio::test]
#[serial]
async fn real_btrfs_redo_is_readonly_and_recovery_checks_its_fingerprint() {
    use super::super::policy::{Action, Resource};
    use super::super::session::{GrantedCapability, PublishMeta};
    use marsh_wal::{ContentHash, EntryKind, JsonLog, Mode, Seq, SourceUid, Staging, WalRecord};
    use std::os::unix::fs::PermissionsExt;
    const REQUIRED: &str =
        "readonly redo regression requires a writable btrfs HOME with user_subvol_rm_allowed";
    let fs = marsh_btrfs::LibBtrfs;
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .unwrap_or_else(|| panic!("{REQUIRED}: HOME is unset or empty"));
    let home = std::fs::canonicalize(&home)
        .unwrap_or_else(|error| panic!("{REQUIRED}: {}: {error}", Path::new(&home).display()));
    fs.assert_btrfs(&home)
        .and_then(|()| fs.assert_user_subvol_rm_allowed(&home))
        .unwrap_or_else(|error| panic!("{REQUIRED}: {}: {error}", home.display()));
    let root = tempfile::Builder::new()
        .prefix("marsh-readonly-proof.")
        .tempdir_in(&home)
        .unwrap_or_else(|error| panic!("{REQUIRED}: {}: {error}", home.display()));
    let seed = root.path().join("seed");
    fs.create_subvolume(&seed)
        .unwrap_or_else(|error| panic!("{REQUIRED}: {}: {error}", seed.display()));
    std::fs::write(seed.join("file"), b"before").unwrap();
    let shell = super::managed(seed.clone(), Arc::new(marsh_btrfs::LibBtrfs))
        .await
        .unwrap();
    let log = session(&shell).await.log.clone();
    let snapshots = session(&shell).await.persistence.snap();
    let principal = shell.principal().clone();
    std::os::unix::fs::symlink("/dev/full", &log).unwrap();
    assert!(
        shell
            .run("printf after > file; chmod 640 file")
            .await
            .is_err()
    );
    // Command 1 opened the managed view; the refused command is number 2.
    let uid = SourceUid::new(format!("{}-redo-2", shell.principal())).unwrap();
    let redo = snapshots.join(uid.as_str());
    let refusal = std::fs::write(redo.join("file"), b"tamper").unwrap_err();
    assert_eq!(refusal.raw_os_error(), Some(libc::EROFS));
    assert_eq!(std::fs::read(redo.join("file")).unwrap(), b"after");
    assert_eq!(
        std::fs::metadata(redo.join("file"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o640
    );
    assert!(shell.close(true).await.is_err());
    std::fs::remove_file(&log).unwrap();
    let meta = || PublishMeta {
        cmd: "readonly redo proof".into(),
        principal: principal.clone(),
        granted: vec![GrantedCapability {
            action: Action::Edit,
            resource: Resource::from(vec!["file"]),
        }],
    };
    let intent = |uid: SourceUid, seq: Seq, digest: ContentHash| {
        vec![
            WalRecord::Begin {
                seq,
                staging: Staging::of(seq, &uid),
                uid,
                op_count: 1,
                meta: meta(),
            },
            WalRecord::Move {
                from: "file".into(),
                to: "file".into(),
                sha1: digest,
                kind: EntryKind::File,
                mode: Some(Mode::new(0o640)),
            },
        ]
    };
    JsonLog::open(&log)
        .unwrap()
        .append(&intent(uid, Seq::new(1), ContentHash::of(b"after")))
        .unwrap();
    let reopened = super::managed(seed.clone(), Arc::new(marsh_btrfs::LibBtrfs))
        .await
        .unwrap();
    assert_eq!(std::fs::read(seed.join("file")).unwrap(), b"after");
    assert_eq!(
        std::fs::metadata(seed.join("file"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o640
    );
    assert!(!redo.exists());
    refused(&reopened, "printf blind > file").await;
    close(reopened).await;
    let bad_uid = SourceUid::new("bad-redo").unwrap();
    let bad = snapshots.join(bad_uid.as_str());
    fs.snapshot_readonly(&seed, &bad).unwrap();
    JsonLog::open(&log)
        .unwrap()
        .append(&intent(bad_uid, Seq::new(2), ContentHash::of(b"different")))
        .unwrap();
    let before = std::fs::read(&log).unwrap();
    assert!(
        super::managed(seed.clone(), Arc::new(marsh_btrfs::LibBtrfs))
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&log).unwrap(), before);
    assert_eq!(std::fs::read(seed.join("file")).unwrap(), b"after");
    assert!(bad.exists());
    fs.delete_subvolume(&bad).unwrap();
    fs.delete_subvolume(&seed).unwrap();
    root.close().expect("remove readonly proof scratch");
}

#[tokio::test]
#[serial]
async fn native_internal_calls_order_intent_payload_namespace_and_end() {
    use super::super::session::PublishMeta;
    use lurk_cli::syscall_info::RetCode;
    use marsh_wal::{ContentHash, JsonLog, WalRecord};
    use std::os::unix::ffi::OsStrExt;
    use syscalls::Sysno;

    fn target(call: &marsh_instrument::Syscall) -> Option<&Path> {
        call.fd(0)
            .ok()
            .flatten()
            .map(|target| Path::new(std::ffi::OsStr::from_bytes(&target.path)))
    }
    let fixture = Fixture::new();
    let shell = fixture.shell().await;
    let log = fixture.log();
    let tracing = Arc::clone(&session(&shell).await.tracing);
    tracing.begin_internal_capture().unwrap();
    let result = shell.run("printf durable > src/a.txt").await;
    let calls = tracing.end_internal_capture().unwrap();
    result.unwrap();
    assert_eq!(
        std::fs::read(fixture.seed.join("src/a.txt")).unwrap(),
        b"durable"
    );
    let records = JsonLog::<WalRecord<PublishMeta>>::read(&log).unwrap();
    let Some(WalRecord::Begin {
        seq,
        staging,
        op_count,
        meta,
        ..
    }) = records.first()
    else {
        panic!("missing intent");
    };
    assert_eq!(*op_count, records.len() - 2);
    assert_eq!(&meta.principal, shell.principal());
    assert!(matches!(records.last(), Some(WalRecord::End { seq: ended }) if ended == seq));
    assert!(
        records
            .iter()
            .any(|record| matches!(record, WalRecord::Move { to, sha1, .. }
        if to == Path::new("src/a.txt") && sha1 == &ContentHash::of(b"durable")))
    );

    let writes: Vec<_> = calls
        .iter()
        .filter(|call| {
            matches!(call.info.syscall, Sysno::write | Sysno::writev)
                && !matches!(call.info.result, RetCode::Err(_))
                && target(call) == Some(log.as_path())
        })
        .map(|call| call.entry_order)
        .collect();
    let syncs: Vec<_> = calls
        .iter()
        .filter(|call| {
            matches!(call.info.syscall, Sysno::fsync | Sysno::fdatasync)
                && matches!(call.info.result, RetCode::Ok(0))
                && target(call) == Some(log.as_path())
        })
        .map(|call| call.entry_order)
        .collect();
    assert_eq!(
        writes.len(),
        2,
        "one counted intent batch and one END write"
    );
    assert_eq!(syncs.len(), 2, "both WAL writes must be durable");
    assert!(writes[0] < syncs[0] && syncs[0] < writes[1] && writes[1] < syncs[1]);
    let staging = fixture.seed.join(staging.to_string());
    let payload = calls
        .iter()
        .find(|call| {
            call.info.syscall == Sysno::fsync
                && matches!(call.info.result, RetCode::Ok(0))
                && target(call).is_some_and(|path| path.starts_with(&staging) && path != staging)
        })
        .expect("staged payload fsync")
        .entry_order;
    let parent = fixture.seed.join("src");
    let namespace = calls
        .iter()
        .find(|call| {
            call.info.syscall == Sysno::fsync
                && matches!(call.info.result, RetCode::Ok(0))
                && call.entry_order > payload
                && target(call) == Some(parent.as_path())
        })
        .expect("published namespace fsync")
        .entry_order;
    assert!(syncs[0] < payload && payload < namespace && namespace < writes[1]);
    println!(
        "native ordering: intent sync {} < payload fsync {payload} < namespace fsync {namespace} < END sync {}",
        syncs[0], syncs[1]
    );
    close(shell).await;
}
