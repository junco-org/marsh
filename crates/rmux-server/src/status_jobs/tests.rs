use super::{
    ensure_status_job_cache_capacity, status_job_shell_id, ActiveStatusJob, StatusJobCacheEntry,
    StatusJobKey, StatusJobRuntime, STATUS_JOB_ACTIVE_LIMIT, STATUS_JOB_CACHE_LIMIT,
};
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::watch;

#[test]
fn status_job_key_canonicalizes_profile_environment_order() {
    let profile = test_profile(&[("RMUX_STATUS_KEY", "shared")]);
    let key = StatusJobKey::new("printf probe", Some(&profile));
    let environment = key.environment.as_ref().expect("profile environment key");
    let mut sorted = environment.as_ref().clone();

    sorted.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    assert_eq!(environment.as_ref(), &sorted);
}

#[test]
fn status_job_cache_evicts_old_completed_entries() {
    let now = Instant::now();
    let mut jobs = HashMap::new();
    for index in 0..STATUS_JOB_CACHE_LIMIT {
        let slot = u64::try_from(index).expect("cache limit fits u64");
        jobs.insert(
            StatusJobKey::new(&format!("job-{index}"), None),
            StatusJobCacheEntry {
                output: String::new(),
                updated_at: Some(now + Duration::from_millis(slot)),
                in_flight: false,
                shell_id: status_job_shell_id(slot),
            },
        );
    }

    ensure_status_job_cache_capacity(&mut jobs, &StatusJobKey::new("job-new", None), now);

    assert_eq!(jobs.len(), STATUS_JOB_CACHE_LIMIT - 1);
    assert!(!jobs.contains_key(&StatusJobKey::new("job-0", None)));
}

#[test]
fn status_job_cache_honors_render_ttl() {
    let runtime = StatusJobRuntime::new();
    let command = format!("ttl-job-{}", std::process::id());
    let key = StatusJobKey::new(&command, None);
    runtime.seed_cache(
        key.clone(),
        StatusJobCacheEntry {
            output: "cached".to_owned(),
            updated_at: Some(Instant::now()),
            in_flight: false,
            shell_id: status_job_shell_id(0),
        },
    );

    let rendered = runtime.cached_output(None, &command, None, Duration::from_secs(3600));

    assert_eq!(rendered, "cached");
    assert!(
        !runtime.cache_entry_in_flight(&key),
        "fresh cache entries must not spawn a replacement job"
    );
}

/// The 32-generation admission limit refuses rather than detaching an untracked worker.
///
/// The workers here never finish, which is what an occupied slot means: a slot is released by its
/// worker reporting completion or by that worker's task ending, and neither has happened.
#[tokio::test]
async fn status_job_runtime_bounds_active_workers() {
    let runtime = StatusJobRuntime::new();
    {
        let mut state = runtime.inner.lock_state();
        for job_id in 0..u64::try_from(STATUS_JOB_ACTIVE_LIMIT).expect("limit fits u64") {
            let (cancel, _cancelled) = watch::channel(false);
            state.active.insert(
                job_id,
                ActiveStatusJob {
                    cancel,
                    worker: tokio::spawn(std::future::pending::<()>()),
                    job: None,
                    completed: false,
                },
            );
        }
    }
    let command = format!("bounded-job-{}", std::process::id());
    let key = StatusJobKey::new(&command, None);

    assert_eq!(
        runtime.cached_output(None, &command, None, Duration::ZERO),
        ""
    );
    assert_eq!(runtime.active_job_count(), STATUS_JOB_ACTIVE_LIMIT);
    assert!(
        !runtime.cache_entry_in_flight(&key),
        "the active limit must reject rather than detach an untracked worker"
    );
}

/// A `#(command)` that never stops writing must not cost the daemon a worker thread.
///
/// This is reachable by accident rather than by malice. A status string is expanded through
/// `strftime` before the shell ever sees it, exactly as tmux expands one, so the reported
/// `#(printf "%s%s" C D)` reaches the interpreter as a format with no conversion specifications
/// and two arguments it can never consume: a loop that only a write error ends. Every generation
/// therefore outruns its budget and is abandoned — and an abandoned generation whose producer is
/// still running is one worker thread that never comes back, because a builtin writing its output
/// runs inline on the daemon's runtime and blocks there.
///
/// The assertion is the observable a user has — after more such generations than there are
/// workers, does a well-behaved producer still produce? — rather than a count of anything
/// internal. The generations are started one at a time, spaced past the per-generation budget,
/// because that is both the status line's own shape and the only way each one reaches the timeout
/// that is supposed to end it: started all at once they would take the runtime before anything
/// could stop them, which is a different failure. Each command is distinct so that a slot whose
/// previous generation never reported back cannot swallow the next request. The drive loop runs
/// on this thread, which is not one of that runtime's workers, so a starved runtime fails this on
/// its deadline instead of hanging it.
#[test]
fn a_status_producer_that_never_stops_writing_leaves_the_daemon_answering() {
    /// Small on purpose: the defect costs one worker per generation, so the fewer there are, the
    /// fewer generations are needed to prove it, and the test does not depend on the machine.
    const WORKERS: usize = 3;
    /// More than there are workers.
    const GENERATIONS: usize = WORKERS + 1;
    /// Long enough for a generation to reach its budget and be abandoned. Waited out in full only
    /// when the generation never reports back, which is the defect.
    const PER_GENERATION: Duration = Duration::from_secs(3);
    /// Generous, because it is only ever waited out when the test is failing.
    const ANSWER_BY: Duration = Duration::from_secs(30);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKERS)
        .enable_all()
        .build()
        .expect("a daemon runtime");
    let handler = crate::handler::RequestHandler::new();
    let Some(io) =
        runtime.block_on(async { crate::managed_workload::test_engine::install(&handler) })
    else {
        panic!("the test engine is what makes these producers real work");
    };

    let jobs = StatusJobRuntime::new();
    for index in 0..GENERATIONS {
        // `printf` reuses its format while arguments remain, and this format consumes none of
        // them, so it writes until something stops it.
        let runaway = format!("printf never-ends-{index} a b");
        let key = StatusJobKey::new(&runaway, None);
        assert_eq!(
            jobs.cached_output(Some(&io), &runaway, None, Duration::ZERO),
            "",
            "a producer's first generation renders nothing, as tmux's does"
        );
        let started = Instant::now();
        while jobs.cache_entry_in_flight(&key) && started.elapsed() < PER_GENERATION {
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    let answering = "printf ready";
    let deadline = Instant::now() + ANSWER_BY;
    let mut answered = String::new();
    while Instant::now() < deadline {
        answered = jobs.cached_output(Some(&io), answering, None, Duration::ZERO);
        if !answered.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    // Before the assertion, and bounded, so a failure reports rather than hangs: a worker parked
    // in a blocking write is a thread neither this shutdown nor any other can join.
    jobs.shutdown_and_join();
    runtime.shutdown_timeout(Duration::from_secs(2));

    assert_eq!(
        answered, "ready",
        "the daemon stopped answering after {GENERATIONS} runaway status generations"
    );
}

#[test]
fn status_job_cache_is_partitioned_by_profile_environment() {
    let first = test_profile(&[("TMUX_PANE", "%1")]);
    let second = test_profile(&[("TMUX_PANE", "%2")]);

    assert_ne!(
        StatusJobKey::new("printf probe", Some(&first)),
        StatusJobKey::new("printf probe", Some(&second))
    );
}

fn test_profile(environment: &[(&str, &str)]) -> crate::terminal::TerminalProfile {
    use rmux_core::{EnvironmentStore, OptionStore};
    use rmux_proto::SessionName;

    let mut spawn_environment = HashMap::new();
    for (name, value) in environment {
        spawn_environment.insert((*name).to_owned(), (*value).to_owned());
    }
    let session_name = SessionName::new("alpha").expect("valid session name");
    crate::terminal::TerminalProfile::for_run_shell(
        &EnvironmentStore::default(),
        &OptionStore::default(),
        Some(&session_name),
        Some(1),
        Path::new("/tmp/rmux-status-job-test.sock"),
        None,
        false,
        None,
        None,
    )
    .expect("profile")
    .with_test_environment(spawn_environment)
}
