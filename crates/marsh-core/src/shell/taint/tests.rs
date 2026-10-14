use std::path::{Path, PathBuf};

use super::*;
use crate::shell::access::Content;

fn policy() -> AgentPolicy {
    AgentPolicy::recommended().unwrap()
}
fn reads(path: &str) -> Effects {
    Effects {
        reads: vec![PathBuf::from(path)],
        ..Effects::default()
    }
}
fn writes(path: &str) -> Effects {
    Effects {
        writes: vec![PathBuf::from(path)],
        ..Effects::default()
    }
}
fn content(inode: u64, read: bool, write: bool) -> Effects {
    Effects {
        outside_contents: vec![Content {
            path: Path::new("/tmp/stage").to_path_buf(),
            device: 1,
            inode,
            read,
            write,
        }],
        ..Effects::default()
    }
}

#[test]
fn a_file_written_after_a_secret_read_taints_its_reader() {
    let (policy, mut state) = (policy(), TaintState::default());
    let (a, b) = (Principal::new("a"), Principal::new("b"));
    assert!(state.observe(&a, &policy, &reads(".env")).is_empty());
    assert!(state.observe(&a, &policy, &content(7, false, true)).is_empty());
    let acquired = state.observe(&b, &policy, &content(7, true, false));
    assert_eq!(acquired.len(), 1);
    assert_eq!(acquired[0].class, "secret");
    assert_eq!(acquired[0].from, a);
    assert_eq!(state.received(&b), acquired);
    // Acquired once; a second read adds nothing new to the log.
    assert!(state.observe(&b, &policy, &content(7, true, false)).is_empty());
    assert!(state.received(&a).is_empty());
}

#[test]
fn files_written_before_the_read_or_by_untainted_principals_carry_nothing() {
    let (policy, mut state) = (policy(), TaintState::default());
    let (a, b) = (Principal::new("a"), Principal::new("b"));
    state.observe(&a, &policy, &content(7, false, true));
    state.observe(&a, &policy, &reads(".env"));
    assert!(state.observe(&b, &policy, &content(7, true, false)).is_empty());
    assert!(state.observe(&b, &policy, &reads("README.md")).is_empty());
}

#[test]
fn taint_passes_on_through_source_paths_and_chains() {
    let (policy, mut state) = (policy(), TaintState::default());
    let (a, b, c) = (Principal::new("a"), Principal::new("b"), Principal::new("c"));
    state.observe(&a, &policy, &reads(".env"));
    state.observe(&a, &policy, &writes("notes.txt"));
    assert_eq!(state.observe(&b, &policy, &reads("notes.txt")).len(), 1);
    // b now carries it on, and c's taint still names the principal b received it from.
    state.observe(&b, &policy, &content(9, false, true));
    let acquired = state.observe(&c, &policy, &content(9, true, false));
    assert_eq!(acquired.len(), 1);
    assert_eq!(acquired[0].from, b);
}

#[test]
fn looking_up_carriers_records_nothing() {
    let (policy, mut state) = (policy(), TaintState::default());
    let (a, b) = (Principal::new("a"), Principal::new("b"));
    state.observe(&a, &policy, &reads(".env"));
    state.observe(&a, &policy, &content(7, false, true));
    assert_eq!(state.carried(&content(7, true, false)).len(), 1);
    assert!(state.received(&b).is_empty());
    assert_eq!(state.observe(&b, &policy, &content(7, true, false)).len(), 1);
}

#[test]
fn classes_no_rule_propagates_do_not_taint() {
    let mut state = TaintState::default();
    let policy = AgentPolicy::new().class("**/.env", "secret").unwrap();
    let (a, b) = (Principal::new("a"), Principal::new("b"));
    state.observe(&a, &policy, &reads(".env"));
    state.observe(&a, &policy, &content(7, false, true));
    assert!(state.observe(&b, &policy, &content(7, true, false)).is_empty());
}
