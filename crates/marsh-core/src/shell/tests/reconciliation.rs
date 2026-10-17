//! Startup reconciliation of the write-ahead log with a seed changed outside marsh: what a real
//! reopen leaves in the log, and what the next shell is then allowed to do.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use marsh_wal::{ContentHash, EntryKind, JsonLog, Mode, Seq, SourceUid, Staging, WalRecord};
use serial_test::serial;

use super::super::Principal;
use super::super::policy::{Action, Resource};
use super::super::session::{GrantedCapability, PublishMeta};

use super::{Fixture, accepted, close, refused};

impl Fixture {
    /// Each logged transaction: its declared operation count, what its operations name, and the
    /// grants its `BEGIN` records — read from disk, not from what recovery returned.
    fn transactions(&self) -> Vec<(usize, Vec<String>, Vec<String>)> {
        let mut transactions: Vec<(usize, Vec<String>, Vec<String>)> = Vec::new();
        for record in JsonLog::<WalRecord<PublishMeta>>::read(&self.log()).expect("read the log") {
            let operation = match record {
                WalRecord::Begin { op_count, meta, .. } => {
                    let grants = meta
                        .granted
                        .iter()
                        .map(|grant| {
                            format!("{:?} {}", grant.action, grant.resource.segments().join("/"))
                        })
                        .collect();
                    transactions.push((op_count, Vec::new(), grants));
                    continue;
                }
                WalRecord::End { .. } => continue,
                WalRecord::Move { to, .. } => format!("MOVE {}", to.display()),
                WalRecord::Delete { path } => format!("DELETE {}", path.display()),
                WalRecord::Mkdir { path, .. } => format!("MKDIR {}", path.display()),
                WalRecord::Chmoddir { path, .. } => format!("CHMODDIR {}", path.display()),
                WalRecord::Rmdir { path } => format!("RMDIR {}", path.display()),
            };
            transactions
                .last_mut()
                .expect("an operation inside a transaction")
                .1
                .push(operation);
        }
        transactions
    }
    /// Writes a finished transaction of `old-owner` that moves `marker` and grants Edit over
    /// `granted`.
    fn old_owner_log(&self, granted: &[&str]) {
        let uid = SourceUid::new("old-owner").expect("a uid");
        let meta = PublishMeta {
            cmd: String::new(),
            principal: Principal::from("old-owner"),
            granted: granted
                .iter()
                .map(|path| GrantedCapability {
                    action: Action::Edit,
                    resource: Resource::from(path.split('/').collect::<Vec<_>>()),
                })
                .collect(),
        };
        JsonLog::<WalRecord<PublishMeta>>::open(&self.log())
            .expect("open the log")
            .append(&[
                WalRecord::Begin {
                    seq: Seq::new(1),
                    staging: Staging::of(Seq::new(1), &uid),
                    uid,
                    op_count: 1,
                    meta,
                },
                WalRecord::Move {
                    from: PathBuf::from("marker"),
                    to: PathBuf::from("marker"),
                    sha1: ContentHash::of(b"marker\n"),
                    kind: EntryKind::File,
                    mode: Some(Mode::new(0o644)),
                },
                WalRecord::End { seq: Seq::new(1) },
            ])
            .expect("append a finished transaction");
    }
}

/// A file deleted from the seed outside marsh belongs to nobody after the next startup: its
/// transaction leaves the log, so another shell can write the path — and that write is then
/// owned, durably, like any other.
#[tokio::test]
#[serial]
async fn startup_reconciliation_releases_externally_deleted_files() {
    let fixture = Fixture::new();
    let owner = fixture.shell().await;
    accepted(&owner, "printf owner > test.txt").await;
    close(owner).await;
    std::fs::remove_file(fixture.seed.join("test.txt")).expect("delete outside marsh");

    let next = fixture.shell().await;
    assert_eq!(
        fixture.transactions(),
        Vec::new(),
        "the only transaction wrote only the deleted file"
    );
    assert_eq!(
        std::fs::metadata(fixture.log()).expect("the log").len(),
        0,
        "an empty log, not a missing one"
    );
    accepted(&next, "printf foo2 > test.txt").await;
    assert_eq!(fixture.read("test.txt"), "foo2");
    close(next).await;

    let third = fixture.shell().await;
    refused(&third, "printf other > test.txt").await;
    assert_eq!(fixture.read("test.txt"), "foo2");
    close(third).await;
}

/// Only the deleted path is forgotten: every transaction's history of it goes, a transaction that
/// wrote it beside another path keeps that path, counted, and its grant, and the rewritten log is
/// stable across a further reopen.
#[tokio::test]
#[serial]
async fn startup_reconciliation_preserves_other_paths() {
    let fixture = Fixture::new();
    let owner = fixture.shell().await;
    accepted(&owner, "printf gone > gone; printf kept > kept").await;
    accepted(&owner, "printf again > gone").await;
    close(owner).await;
    std::fs::remove_file(fixture.seed.join("gone")).expect("delete outside marsh");

    let reopened = fixture.shell().await;
    assert_eq!(
        fixture.transactions(),
        [(
            1,
            vec!["MOVE kept".to_owned()],
            vec!["Edit kept".to_owned()]
        )],
    );
    let compacted = std::fs::read(fixture.log()).expect("the log");
    close(reopened).await;

    let next = fixture.shell().await;
    assert_eq!(
        std::fs::read(fixture.log()).expect("the log"),
        compacted,
        "nothing is left to reconcile"
    );
    refused(&next, "printf mine > kept").await;
    assert_eq!(fixture.read("kept"), "kept");
    accepted(&next, "printf back > gone").await;
    assert_eq!(fixture.read("gone"), "back");
    close(next).await;
}

/// A logged removal releases the removed name: after reopen another shell recreates it without a
/// read of what no longer exists, even where create-then-remove left no diff at all. A name
/// recreated after its removal stays owned, and a challenger cannot remove a foreign-owned file
/// to clear its claims.
#[tokio::test]
#[serial]
async fn startup_reconciliation_releases_logged_deletions() {
    let fixture = Fixture::new();
    let owner = fixture.shell().await;
    accepted(
        &owner,
        "rm src/a.txt; printf x > tmp; rm tmp; mkdir d; rmdir d; printf one > kept; rm kept; printf two > kept",
    )
    .await;
    let challenger = fixture.shell().await;
    refused(&challenger, "rm kept").await;
    assert_eq!(fixture.read("kept"), "two");
    close(challenger).await;
    close(owner).await;

    let next = fixture.shell().await;
    accepted(
        &next,
        "mkdir -p src; printf other > src/a.txt; printf other > tmp; mkdir d",
    )
    .await;
    assert_eq!(fixture.read("src/a.txt"), "other");
    refused(&next, "printf other > kept").await;
    assert_eq!(fixture.read("kept"), "two");
    close(next).await;
}

/// An explicit release is durable across reopen, changes no bytes and keeps the last reader's
/// claim: a third shell must read before it may edit.
#[tokio::test]
#[serial]
async fn startup_keeps_explicit_releases() {
    let fixture = Fixture::new();
    let owner = fixture.shell().await;
    let reader = fixture.shell().await;
    accepted(&owner, "printf mine > kept").await;
    accepted(&reader, "cat kept >/dev/null").await;
    accepted(&owner, "release -- kept").await;
    assert_eq!(fixture.read("kept"), "mine");
    close(reader).await;
    close(owner).await;

    let next = fixture.shell().await;
    refused(&next, "printf other > kept").await;
    accepted(&next, "cat kept >/dev/null; printf other > kept").await;
    assert_eq!(fixture.read("kept"), "other");
    close(next).await;
}

/// A grant names a path no operation of its transaction does — an explicit release writes
/// nothing — and it is reconciled all the same: a grant over a deleted path is dropped, a grant
/// over a present one stays in force, and a transaction whose operations all went keeps its
/// surviving grants in a frame of no operations.
#[tokio::test]
#[serial]
async fn startup_reconciliation_includes_grant_only_paths() {
    let fixture = Fixture::new();
    std::fs::write(fixture.seed.join("marker"), b"marker\n").expect("the moved file");
    fixture.old_owner_log(&["gone", "src/a.txt"]);

    let next = fixture.shell().await;
    assert_eq!(
        fixture.transactions(),
        [(
            1,
            vec!["MOVE marker".to_owned()],
            vec!["Edit src/a.txt".to_owned()]
        )],
    );
    accepted(&next, "printf new > gone").await;
    assert_eq!(fixture.read("gone"), "new");
    refused(&next, "printf other > src/a.txt").await;
    assert_eq!(fixture.read("src/a.txt"), "seed\n");
    close(next).await;

    let fixture = Fixture::new();
    fixture.old_owner_log(&["src/a.txt"]);
    for _ in 0..2 {
        let next = fixture.shell().await;
        let records =
            JsonLog::<WalRecord<PublishMeta>>::read(&fixture.log()).expect("read the log");
        assert!(
            matches!(
                records.as_slice(),
                [WalRecord::Begin { seq: begun, op_count: 0, .. }, WalRecord::End { seq: ended }]
                    if *begun == Seq::new(1) && *ended == Seq::new(1)
            ),
            "the surviving grant keeps an empty frame: {records:?}"
        );
        assert_eq!(
            fixture.transactions(),
            [(0, Vec::new(), vec!["Edit src/a.txt".to_owned()])]
        );
        refused(&next, "printf other > src/a.txt").await;
        assert_eq!(fixture.read("src/a.txt"), "seed\n");
        close(next).await;
    }
}
