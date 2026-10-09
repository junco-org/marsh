#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;
use lurk_cli::syscall_info::SyscallArgs;

fn target(path: &[u8]) -> FileTarget {
    FileTarget {
        path: path.to_vec(),
        device: 1,
        inode: 2,
        mode: libc::S_IFREG | 0o644,
        links: 1,
        mount_id: 3,
    }
}
fn info(syscall: Sysno, args: Vec<SyscallArg>, result: RetCode) -> Syscall {
    Syscall {
        info: lurk_cli::syscall_info::SyscallInfo {
            typ: "SYSCALL",
            pid: nix_observer::unistd::Pid::from_raw(1),
            syscall,
            args: SyscallArgs(args),
            result,
            duration: std::time::Duration::ZERO,
        },
        entry_order: 1,
        cwd: Some(target(b"/work")),
        return_fd: None,
        paths: Vec::new(),
        descriptors: Vec::new(),
        flags: None,
        submissions: None,
    }
}
fn with_descriptor(mut info: Syscall, index: usize, path: &[u8]) -> Syscall {
    info.descriptors.push((index, Some(target(path))));
    info
}
fn open(path: &[u8], flags: i32) -> Syscall {
    let mut info = info(
        Sysno::openat,
        vec![
            SyscallArg::Int(i64::from(libc::AT_FDCWD)),
            SyscallArg::Addr(0x1000),
            SyscallArg::Int(i64::from(flags)),
            SyscallArg::Int(0o644),
        ],
        RetCode::Ok(3),
    );
    info.paths.push((1, path.to_vec()));
    info.descriptors.push((0, Some(target(b"/work"))));
    info.return_fd = Some(target(path));
    info
}
/// A successful two-byte `read` of descriptor 3, open on `path`.
fn read_fd(path: &[u8]) -> Syscall {
    with_descriptor(
        info(
            Sysno::read,
            vec![SyscallArg::Int(3), SyscallArg::Addr(42), SyscallArg::Int(2)],
            RetCode::Ok(2),
        ),
        0,
        path,
    )
}

#[test]
fn io_uring_is_accepted_only_while_every_submission_is_effectless_and_visible() {
    let mut access = Access::default();
    let root = Path::new("/work");
    let setup = |flags: u64| {
        let mut setup = info(
            Sysno::io_uring_setup,
            vec![SyscallArg::Int(256), SyscallArg::Addr(0x1000)],
            RetCode::Ok(4),
        );
        setup.flags = Some(flags);
        setup
    };
    let enter = |to_submit: i64, submissions: Option<Vec<u8>>| {
        let mut enter = info(
            Sysno::io_uring_enter,
            vec![
                SyscallArg::Int(4),
                SyscallArg::Int(to_submit),
                SyscallArg::Int(0),
                SyscallArg::Int(0),
            ],
            RetCode::Ok(0),
        );
        enter.submissions = submissions;
        enter
    };
    // libuv's epoll batching ring: no submission thread, only EPOLL_CTL.
    assert!(access.observe(&setup(1 << 16), root).is_ok());
    assert!(access.observe(&enter(2, Some(vec![29, 29])), root).is_ok());
    // Waiting for completions submits nothing.
    assert!(access.observe(&enter(0, None), root).is_ok());
    // A kernel submission thread consumes entries no stop ever shows.
    assert!(access.observe(&setup(1 << 1), root).is_err());
    // OPENAT, or entries that could not be read, carry effects nothing observed.
    assert!(access.observe(&enter(2, Some(vec![29, 18])), root).is_err());
    assert!(access.observe(&enter(1, None), root).is_err());
    // Registration stays unsupported.
    assert!(
        access
            .observe(
                &info(Sysno::io_uring_register, Vec::new(), RetCode::Ok(0)),
                root
            )
            .is_err()
    );
}

#[test]
fn metadata_is_a_dependency_not_a_content_claim() {
    let mut access = Access::default();
    let root = Path::new("/work");
    let mut probe = info(
        Sysno::newfstatat,
        vec![
            SyscallArg::Int(i64::from(libc::AT_FDCWD)),
            SyscallArg::Addr(0x1000),
            SyscallArg::Addr(42),
            SyscallArg::Int(0),
        ],
        RetCode::Err(-libc::ENOENT),
    );
    probe.paths.push((1, b"missing".to_vec()));
    probe.descriptors.push((0, Some(target(b"/work"))));
    let effect = access.observe(&probe, root).unwrap();
    assert_eq!(effect.dependencies, [PathBuf::from("missing")]);
    assert!(effect.reads.is_empty());
    assert!(effect.writes.is_empty());
}

#[test]
fn read_only_open_and_large_descriptor_read_have_distinct_claims() {
    let mut access = Access::default();
    let root = Path::new("/work");
    assert!(
        access
            .observe(&open(b"/work/file", libc::O_RDONLY), root)
            .unwrap()
            .reads
            .is_empty()
    );
    let read = with_descriptor(
        info(
            Sysno::pread64,
            vec![
                SyscallArg::Int(3),
                SyscallArg::Addr(42),
                SyscallArg::Int(65_536),
                SyscallArg::Int(0),
            ],
            RetCode::Address(65_536),
        ),
        0,
        b"/work/file",
    );
    assert_eq!(
        access.observe(&read, root).unwrap().reads,
        [PathBuf::from("file")]
    );
}

#[test]
fn writing_intent_requires_edit_even_without_a_payload_change() {
    let mut access = Access::default();
    let effect = access
        .observe(
            &open(b"/work/file", libc::O_WRONLY | libc::O_TRUNC),
            Path::new("/work"),
        )
        .unwrap();
    assert_eq!(effect.writes, [PathBuf::from("file")]);
    assert!(effect.reads.is_empty());
}

#[test]
fn retaking_refuses_old_descriptors_but_not_fresh_opens_with_the_same_inode() {
    let mut access = Access::default();
    let root = Path::new("/work");
    let opened = open(b"/work/file", libc::O_RDONLY);
    access.observe(&opened, root).unwrap();
    let read = read_fd(b"/work/file");
    access.generation = access.generation.next().unwrap();
    assert!(access.observe(&read, root).is_err());
    access.observe(&opened, root).unwrap();
    assert_eq!(
        access.observe(&read, root).unwrap().reads,
        [PathBuf::from("file")]
    );
}

#[test]
fn close_range_marks_and_unshares_without_forgetting_another_descriptor_owner() {
    let root = Path::new("/work");
    for flags in [libc::CLOSE_RANGE_CLOEXEC, libc::CLOSE_RANGE_UNSHARE] {
        let mut access = Access::default();
        access
            .observe(&open(b"/work/file", libc::O_RDONLY), root)
            .unwrap();
        let mut close = info(
            Sysno::close_range,
            vec![
                SyscallArg::Int(3),
                SyscallArg::Int(3),
                SyscallArg::Int(i64::from(flags)),
            ],
            RetCode::Ok(0),
        );
        if flags == libc::CLOSE_RANGE_UNSHARE {
            let clone = info(
                Sysno::clone,
                vec![SyscallArg::Int(i64::from(libc::CLONE_FILES))],
                RetCode::Ok(2),
            );
            access.observe(&clone, root).unwrap();
            close.info.pid = nix_observer::unistd::Pid::from_raw(2);
        }
        access.observe(&close, root).unwrap();
        access.generation = access.generation.next().unwrap();
        let read = read_fd(b"/work/file");
        assert!(
            access.observe(&read, root).is_err(),
            "old parent descriptor remains a retired-generation access"
        );
    }
}

#[test]
fn non_utf8_protected_names_refuse_but_literal_deleted_suffixes_do_not() {
    let mut access = Access::default();
    let root = Path::new("/work");
    assert!(access.observe(&read_fd(b"/work/\xff"), root).is_err());
    assert_eq!(
        access
            .observe(&read_fd(b"/work/name (deleted)"), root)
            .unwrap()
            .reads,
        [PathBuf::from("name (deleted)")]
    );
    assert!(
        access
            .observe(&read_fd(b"/outside/\xff"), root)
            .unwrap()
            .reads
            .is_empty()
    );
}

#[test]
fn hard_link_reads_claim_every_alias() {
    let mut access = Access::default();
    access
        .aliases
        .insert(Inode(1, 2), vec!["/work/a".into(), "/work/b".into()]);
    let read = read_fd(b"/work/a");
    assert_eq!(
        access.observe(&read, Path::new("/work")).unwrap().reads,
        [PathBuf::from("a"), PathBuf::from("b")]
    );
}

#[test]
fn shared_mappings_require_edits_and_partial_unmaps_preserve_the_rest() {
    let mut access = Access::default();
    let root = Path::new("/work");
    let mapping = shared_mapping();
    assert_eq!(
        access.observe(&mapping, root).unwrap().reads,
        [PathBuf::from("db")]
    );
    let unmap = info(
        Sysno::munmap,
        vec![SyscallArg::Addr(0x10000), SyscallArg::Int(4096)],
        RetCode::Ok(0),
    );
    access.observe(&unmap, root).unwrap();
    assert!(
        access
            .observe(&protect_for_write(0x10000), root)
            .unwrap()
            .writes
            .is_empty()
    );
    assert_eq!(
        access
            .observe(&protect_for_write(0x11000), root)
            .unwrap()
            .writes,
        [PathBuf::from("db")]
    );
}

fn unlinkat(path: &[u8], flags: i32, result: RetCode) -> Syscall {
    let mut info = info(
        Sysno::unlinkat,
        vec![
            SyscallArg::Int(i64::from(libc::AT_FDCWD)),
            SyscallArg::Addr(0x1000),
            SyscallArg::Int(i64::from(flags)),
        ],
        result,
    );
    info.paths.push((1, path.to_vec()));
    info.descriptors.push((0, Some(target(b"/work"))));
    info
}

#[test]
fn only_a_successful_removal_releases_and_only_the_removed_name() {
    let root = Path::new("/work");
    let mut access = Access::default();
    let failed = access
        .observe(&unlinkat(b"a", 0, RetCode::Err(libc::ENOENT)), root)
        .unwrap();
    assert!(!failed.removed && failed.writes.is_empty());
    let directory = access
        .observe(&unlinkat(b"d", libc::AT_REMOVEDIR, RetCode::Ok(0)), root)
        .unwrap();
    assert!(directory.removed);
    assert_eq!(directory.writes, [PathBuf::from("d")]);

    // A hard-link alias survives its sibling's removal but no longer reaches the removed name.
    access
        .aliases
        .insert(Inode(1, 2), vec!["/work/a".into(), "/work/b".into()]);
    let removed = access
        .observe(&unlinkat(b"a", 0, RetCode::Ok(0)), root)
        .unwrap();
    assert!(removed.removed);
    assert_eq!(removed.writes, [PathBuf::from("a")]);
    assert_eq!(
        access.observe(&read_fd(b"/work/b"), root).unwrap().reads,
        [PathBuf::from("b")]
    );

    // Unlinking a final symlink removes the link's own resource, never its target.
    access
        .links
        .insert(PathBuf::from("/work/alias"), PathBuf::from("actual"));
    let link = access
        .observe(&unlinkat(b"alias", 0, RetCode::Ok(0)), root)
        .unwrap();
    assert_eq!(link.writes, [PathBuf::from("alias")]);
}

#[test]
fn a_removed_name_retires_its_cached_mappings() {
    let root = Path::new("/work");
    let mut access = Access::default();
    access.observe(&shared_mapping(), root).unwrap();
    access
        .observe(&unlinkat(b"db", 0, RetCode::Ok(0)), root)
        .unwrap();
    assert!(access.observe(&protect_for_write(0x10000), root).is_err());
    let unmap = info(
        Sysno::munmap,
        vec![SyscallArg::Addr(0x10000), SyscallArg::Int(8192)],
        RetCode::Ok(0),
    );
    assert!(access.observe(&unmap, root).unwrap().writes.is_empty());
}

#[test]
fn release_names_one_protected_entry_without_following_its_leaf() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    std::fs::create_dir_all(root.join("real")).unwrap();
    std::os::unix::fs::symlink(root.join("real"), root.join("dir")).unwrap();
    std::os::unix::fs::symlink(root.join("real/file"), root.join("leaf")).unwrap();
    let mut access = Access::default();
    access.prepare(&root, false).unwrap();

    let through = access.release(&root.join("dir/missing"), &root).unwrap();
    assert_eq!(through.release, Some(PathBuf::from("real/missing")));
    assert!(through.reads.is_empty() && through.writes.is_empty());
    assert_eq!(
        access.release(&root.join("leaf"), &root).unwrap().release,
        Some(PathBuf::from("leaf"))
    );
    assert!(access.release(&root, &root).is_err());
    assert!(
        access
            .release(&directory.path().join("outside"), &root)
            .is_err()
    );
}

#[test]
fn symlink_lookups_track_both_lookup_and_resolved_resources() {
    let mut access = Access::default();
    access
        .links
        .insert(PathBuf::from("/work/alias"), PathBuf::from("actual"));
    let mut probe = info(
        Sysno::newfstatat,
        vec![
            SyscallArg::Int(i64::from(libc::AT_FDCWD)),
            SyscallArg::Addr(0x1000),
            SyscallArg::Addr(42),
            SyscallArg::Int(0),
        ],
        RetCode::Ok(0),
    );
    probe.paths.push((1, b"alias/file".to_vec()));
    probe.descriptors.push((0, Some(target(b"/work"))));
    let effect = access.observe(&probe, Path::new("/work")).unwrap();
    assert!(effect.dependencies.contains(&PathBuf::from("alias")));
    assert!(effect.dependencies.contains(&PathBuf::from("actual/file")));
    let mut chmod = info(
        Sysno::chmod,
        vec![SyscallArg::Addr(0x1000), SyscallArg::Int(0o600)],
        RetCode::Ok(0),
    );
    chmod.paths.push((0, b"alias/file".to_vec()));
    assert_eq!(
        access.observe(&chmod, Path::new("/work")).unwrap().writes,
        [PathBuf::from("actual/file")]
    );
}

#[test]
fn relative_at_paths_use_the_stopped_descriptor_instead_of_cwd() {
    let root = Path::new("/work");
    for number in [libc::AT_FDCWD, 7] {
        let mut access = Access::default();
        let mut chmod = with_descriptor(
            info(
                Sysno::fchmodat2,
                vec![
                    SyscallArg::Int(i64::from(number)),
                    SyscallArg::Addr(0x1000),
                    SyscallArg::Int(0o600),
                    SyscallArg::Int(0),
                ],
                RetCode::Ok(0),
            ),
            0,
            b"/work/nested",
        );
        chmod.cwd = Some(target(b"/outside"));
        chmod.paths.push((1, b"file".to_vec()));
        let effect = access.observe(&chmod, root).unwrap();
        assert_eq!(effect.dependencies, [PathBuf::from("nested/file")]);
        assert_eq!(effect.writes, [PathBuf::from("nested/file")]);
        assert!(effect.outside_writes.is_empty());
    }
}

#[test]
fn openat2_flags_preserve_symlink_dependencies_and_edit_intent() {
    let mut access = Access::default();
    access
        .links
        .insert(PathBuf::from("/work/alias"), PathBuf::from("actual"));
    let mut opened = with_descriptor(
        info(
            Sysno::openat2,
            vec![
                SyscallArg::Int(i64::from(libc::AT_FDCWD)),
                SyscallArg::Addr(0x1000),
                SyscallArg::Addr(0x2000),
                SyscallArg::Int(24),
            ],
            RetCode::Ok(3),
        ),
        0,
        b"/work",
    );
    opened.cwd = None;
    opened.paths.push((1, b"alias/file".to_vec()));
    opened.flags = Some(u64::from((libc::O_WRONLY | libc::O_TRUNC).cast_unsigned()));
    opened.return_fd = Some(target(b"/work/actual/file"));
    let effect = access.observe(&opened, Path::new("/work")).unwrap();
    assert!(effect.dependencies.contains(&PathBuf::from("alias")));
    assert!(effect.dependencies.contains(&PathBuf::from("actual/file")));
    assert_eq!(effect.writes, [PathBuf::from("actual/file")]);
    assert!(effect.reads.is_empty());
    opened.flags = None;
    assert!(access.observe(&opened, Path::new("/work")).is_err());
}

#[test]
fn clone3_shares_descriptor_origins_for_later_parent_opens() {
    let mut access = Access::default();
    let root = Path::new("/work");
    let mut clone = info(
        Sysno::clone3,
        vec![SyscallArg::Addr(0x1000), SyscallArg::Int(88)],
        RetCode::Ok(2),
    );
    clone.flags = Some(u64::from(libc::CLONE_FILES.cast_unsigned()));
    access.observe(&clone, root).unwrap();
    access
        .observe(&open(b"/work/file", libc::O_RDONLY), root)
        .unwrap();
    let read = in_child(read_fd(b"/work/file"));
    assert_eq!(
        access.observe(&read, root).unwrap().reads,
        [PathBuf::from("file")]
    );
    access.generation = access.generation.next().unwrap();
    assert!(
        access.observe(&read, root).is_err(),
        "child retains the parent's descriptor origin after a work retake"
    );
}

fn read_set(access: &mut Access, root: &Path, file: &Path) -> std::collections::BTreeSet<PathBuf> {
    let meta = std::fs::symlink_metadata(file).unwrap();
    let captured = FileTarget {
        path: file.as_os_str().as_bytes().to_vec(),
        device: meta.dev(),
        inode: meta.ino(),
        mode: meta.mode(),
        links: meta.nlink(),
        mount_id: 3,
    };
    let mut read = info(
        Sysno::read,
        vec![SyscallArg::Int(3), SyscallArg::Addr(42), SyscallArg::Int(2)],
        RetCode::Ok(2),
    );
    read.descriptors.push((0, Some(captured)));
    access
        .observe(&read, root)
        .unwrap()
        .reads
        .into_iter()
        .collect()
}

#[test]
fn preparation_indexes_protected_aliases_without_following_directory_links() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let outside = directory.path().join("outside");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(root.join("a"), b"shared").unwrap();
    std::fs::hard_link(root.join("a"), root.join("sub/b")).unwrap();
    std::fs::hard_link(root.join("a"), outside.join("c")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
    std::os::unix::fs::symlink(root.join("missing"), root.join("dangling")).unwrap();
    let mut access = Access::default();
    access.prepare(&root, false).unwrap();
    assert_eq!(
        read_set(&mut access, &root, &root.join("a")),
        std::collections::BTreeSet::from([PathBuf::from("a"), PathBuf::from("sub/b")])
    );
    std::fs::remove_file(root.join("sub/b")).unwrap();
    access.prepare(&root, false).unwrap();
    assert_eq!(
        read_set(&mut access, &root, &root.join("a")),
        std::collections::BTreeSet::from([PathBuf::from("a")])
    );
}

const CLONE_FLAG_CASES: [i32; 4] = [
    0,
    libc::CLONE_FILES,
    libc::CLONE_VM,
    libc::CLONE_FILES | libc::CLONE_VM,
];

fn clone_child(flags: i32) -> Syscall {
    info(
        Sysno::clone,
        vec![SyscallArg::Int(i64::from(flags))],
        RetCode::Ok(2),
    )
}
fn in_child(mut info: Syscall) -> Syscall {
    info.info.pid = nix_observer::unistd::Pid::from_raw(2);
    info
}
fn shared_mapping() -> Syscall {
    with_descriptor(
        info(
            Sysno::mmap,
            vec![
                SyscallArg::Addr(0),
                SyscallArg::Int(8192),
                SyscallArg::Int(i64::from(libc::PROT_READ)),
                SyscallArg::Int(i64::from(libc::MAP_SHARED)),
                SyscallArg::Int(3),
                SyscallArg::Int(0),
            ],
            RetCode::Address(0x10000),
        ),
        4,
        b"/work/db",
    )
}
fn protect_for_write(address: usize) -> Syscall {
    info(
        Sysno::mprotect,
        vec![
            SyscallArg::Addr(address),
            SyscallArg::Int(4096),
            SyscallArg::Int(i64::from(libc::PROT_WRITE)),
        ],
        RetCode::Ok(0),
    )
}

#[test]
fn clone_files_alone_decides_whether_a_child_sees_parent_descriptor_replacements() {
    let root = Path::new("/work");
    for flags in CLONE_FLAG_CASES {
        let mut access = Access::default();
        access
            .observe(&open(b"/work/file", libc::O_RDONLY), root)
            .unwrap();
        access.observe(&clone_child(flags), root).unwrap();
        access.generation = access.generation.next().unwrap();
        access
            .observe(&open(b"/work/file", libc::O_RDONLY), root)
            .unwrap();
        let read = in_child(read_fd(b"/work/file"));
        let effect = access.observe(&read, root);
        if flags & libc::CLONE_FILES != 0 {
            assert_eq!(
                effect.unwrap().reads,
                [PathBuf::from("file")],
                "flags {flags:#x}"
            );
        } else {
            assert!(
                effect.is_err(),
                "flags {flags:#x}: an unshared child keeps its retired descriptor"
            );
        }
    }
}

#[test]
fn clone_vm_alone_decides_whether_a_child_sees_parent_unmaps() {
    let root = Path::new("/work");
    for flags in CLONE_FLAG_CASES {
        let mut access = Access::default();
        access.observe(&shared_mapping(), root).unwrap();
        access.observe(&clone_child(flags), root).unwrap();
        access
            .observe(
                &info(
                    Sysno::munmap,
                    vec![SyscallArg::Addr(0x10000), SyscallArg::Int(4096)],
                    RetCode::Ok(0),
                ),
                root,
            )
            .unwrap();
        let unmapped = access
            .observe(&in_child(protect_for_write(0x10000)), root)
            .unwrap()
            .writes;
        if flags & libc::CLONE_VM != 0 {
            assert!(unmapped.is_empty(), "flags {flags:#x}");
        } else {
            assert_eq!(unmapped, [PathBuf::from("db")], "flags {flags:#x}");
        }
        assert_eq!(
            access
                .observe(&in_child(protect_for_write(0x11000)), root)
                .unwrap()
                .writes,
            [PathBuf::from("db")],
            "flags {flags:#x}"
        );
    }
}

#[test]
fn clone_vm_shares_an_address_space_that_had_no_mappings_yet() {
    let root = Path::new("/work");
    let mut access = Access::default();
    access.observe(&clone_child(libc::CLONE_VM), root).unwrap();
    access.observe(&shared_mapping(), root).unwrap();
    assert_eq!(
        access
            .observe(&in_child(protect_for_write(0x10000)), root)
            .unwrap()
            .writes,
        [PathBuf::from("db")]
    );
}
