#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::tests_outside_test_module,
    reason = "an integration test file is the test module"
)]
//! The crate through a real, stock brush shell.
//!
//! The unit tests exercise the recorder in isolation: ids pair, stamps advance, a dump round-trips.
//! What none of them can show is that a `brush_core::Shell` built from the map [`instrument`]
//! produces actually reports the builtins it runs — and only those. That is what this file is for,
//! and it is why it builds the shell out of stock `brush-builtins` rather than a fixture.
//!
//! One test, because it is one process: the installation is process-global.

use std::collections::HashMap;
use std::sync::Arc;

use brush_builtins::BuiltinSet;
use brush_core::{ProfileLoadBehavior, RcLoadBehavior, Shell, SourceInfo};
use brush_instrument::{BuiltinRecord, RecordingHook, instrument};

/// Runs one line, returning its exit code.
async fn run(shell: &mut Shell, line: &str) -> u8 {
    let params = shell.default_exec_params();
    shell
        .run_string(line, &SourceInfo::from("test"), &params)
        .await
        .expect("run the line")
        .exit_code
        .into()
}

/// The single begin record for `builtin`, with the id its end must echo.
fn begin<'a>(records: &'a [BuiltinRecord], wanted: &str) -> &'a BuiltinRecord {
    let mut matching = records.iter().filter(
        |record| matches!(record, BuiltinRecord::Begin { builtin, .. } if builtin == wanted),
    );
    let found = matching
        .next()
        .unwrap_or_else(|| panic!("expected a {wanted} begin record in {records:?}"));
    assert!(
        matching.next().is_none(),
        "expected exactly one {wanted} begin record in {records:?}"
    );
    found
}

/// The exit code of the end record echoing `id`, which must be the only one.
fn exit_of(records: &[BuiltinRecord], wanted: u64) -> u8 {
    let mut matching = records.iter().filter_map(|record| match record {
        BuiltinRecord::End { id, exit, .. } if *id == wanted => Some(*exit),
        _ => None,
    });
    let found = matching
        .next()
        .unwrap_or_else(|| panic!("expected an end record for id {wanted} in {records:?}"));
    assert!(
        matching.next().is_none(),
        "expected exactly one end record for id {wanted} in {records:?}"
    );
    found
}

#[tokio::test]
async fn a_stock_shell_reports_the_builtins_it_runs() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let work = scratch.path().canonicalize().expect("canonical work tree");

    let hook = Arc::new(RecordingHook::default());
    let builtins = instrument(
        brush_builtins::default_builtins(BuiltinSet::BashMode),
        hook.clone(),
    );
    let mut shell = Shell::builder()
        .interactive(false)
        .no_editing(true)
        .profile(ProfileLoadBehavior::Skip)
        .rc(RcLoadBehavior::Skip)
        .working_dir(work.clone())
        .builtins(builtins)
        .build()
        .await
        .expect("build the shell");

    assert_eq!(run(&mut shell, "cd .").await, 0);
    assert_eq!(run(&mut shell, "false").await, 1);
    // An external command is the tracer's business, not the hook's: nothing records it.
    assert_eq!(run(&mut shell, "touch p").await, 0);
    assert!(work.join("p").exists(), "the external command ran");

    let records = hook.records();

    let BuiltinRecord::Begin {
        id, argv, cwd, ts, ..
    } = begin(&records, "cd")
    else {
        panic!("begin returns begin records");
    };
    assert_eq!(argv, &["cd".to_string(), ".".to_string()]);
    assert_eq!(
        cwd, &work,
        "the shell's logical working directory is recorded"
    );
    assert!(*ts > 0, "a record carries a realtime stamp");
    assert_eq!(exit_of(&records, *id), 0);

    let BuiltinRecord::Begin { id, argv, .. } = begin(&records, "false") else {
        panic!("begin returns begin records");
    };
    assert_eq!(argv, &["false".to_string()]);
    assert_eq!(
        exit_of(&records, *id),
        1,
        "the exit code reaches the record"
    );

    assert!(
        !records.iter().any(|record| matches!(
            record,
            BuiltinRecord::Begin { builtin, .. } if builtin == "touch"
        )),
        "an external command is not a builtin: {records:?}"
    );

    // Every invocation terminated, exactly once.
    let mut ends: HashMap<u64, usize> = HashMap::new();
    for record in &records {
        if let BuiltinRecord::End { id, .. } = record {
            *ends.entry(*id).or_default() += 1;
        }
    }
    for record in &records {
        if let BuiltinRecord::Begin { id, .. } = record {
            assert_eq!(
                ends.get(id).copied(),
                Some(1),
                "begin {id} has exactly one end in {records:?}"
            );
        }
    }

    let stamps: Vec<u64> = records.iter().map(BuiltinRecord::ts).collect();
    assert!(
        stamps.windows(2).all(|pair| pair[0] <= pair[1]),
        "record order is time order: {stamps:?}"
    );
}
