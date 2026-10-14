//! A leak fuzzer for the agent policy.
//!
//! It runs each corpus case against managed marsh shells judged by an agent policy, captures every
//! byte the traced commands send to a stand-in external server, and fails on two outcomes:
//!
//! * an **attack** case whose canary reached the external sink in any configured encoding (a leak
//!   the policy should have blocked), unless the case is a registered known gap;
//! * a **control** case whose canary never reached the sink (a false denial of legitimate work).
//!
//! The harness never invents an attack: every case is read from the hand-written corpus under
//! `tests/leak-corpus/`. The whole suite runs inside a user+network namespace where TEST-NET-1
//! (`192.0.2.0/24`) routes to loopback, so the sink can stand in for an external host and nothing
//! leaves the machine. The entry point is `#[ignore]`d because it needs that namespace and a real
//! Python; run it with `--ignored`.

// The case-running futures hold non-`Send` shells across await points, which is fine: they are
// driven by `block_on` and never spawned onto another thread.
#![allow(clippy::future_not_send)]

mod case;
mod oracle;
mod shrink;
mod sink;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use case::{Case, Encoding, Expect, Kind, Step};
use oracle::Found;
use sink::Sink;

use super::{SandboxPolicy, Shell, ShellErrorKind};
use crate::{AgentPolicy, PolicyDecision, PolicyObserver, UntracedLocal};

/// The stand-in external server. TEST-NET-1 is reserved for documentation, never routed on the
/// public internet, and the namespace routes its /24 to loopback.
const SINK_HOST: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
/// Set in the re-executed child so it runs the corpus instead of re-entering `unshare`.
const IN_NAMESPACE: &str = "MARSH_LEAK_NS";
/// How long a single command may run before the runner moves on and records a timeout.
const STEP_TIMEOUT: Duration = Duration::from_secs(45);
/// A background step (one ending in `&`) should return at once; this only bounds a misbehaving one.
const BACKGROUND_TIMEOUT: Duration = Duration::from_secs(6);
/// Time left for in-flight bytes to reach the sink after a case's steps finish.
const SINK_GRACE: Duration = Duration::from_millis(750);

fn enforcement() -> marsh_instrument::Enforcement {
    match std::env::var("MARSH_LEAK_ENFORCEMENT")
        .as_deref()
        .unwrap_or("unprivileged")
    {
        "unprivileged" => marsh_instrument::Enforcement::Unprivileged,
        "privileged" => marsh_instrument::Enforcement::Privileged,
        other => panic!("unknown MARSH_LEAK_ENFORCEMENT {other:?}"),
    }
}

fn namespace_args() -> &'static [&'static str] {
    // Keep initial-user-namespace BPF capabilities when running as real root.
    // SAFETY: geteuid takes no arguments and has no memory effects.
    if unsafe { libc::geteuid() } == 0 {
        &["--net"]
    } else {
        &["--user", "--map-root-user", "--net"]
    }
}
/// Runs the whole leak corpus. Ignored: it needs a user+network namespace and a real `python3`.
#[test]
#[ignore = "needs a user+network namespace and python3; run with --ignored"]
fn leak_corpus() {
    if std::env::var_os(IN_NAMESPACE).is_none() {
        match reexec_in_namespace() {
            Bootstrap::Ran => {}
            Bootstrap::Skipped(reason) => {
                assert!(
                    enforcement() != marsh_instrument::Enforcement::Privileged,
                    "privileged leak corpus cannot skip: {reason}"
                );
                eprintln!("leak_corpus skipped: {reason}");
            }
        }
        return;
    }
    enter_loopback_network();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let failures = runtime.block_on(run_corpus());
    assert!(failures == 0, "{failures} leak-corpus case(s) failed");
}

/// Whether the namespace re-execution ran or was skipped.
enum Bootstrap {
    Ran,
    Skipped(String),
}

/// Re-runs this test binary inside a fresh user+network namespace.
///
/// `unshare(CLONE_NEWUSER)` refuses a multithreaded caller and the test harness has threads, so the
/// `unshare` command forks a single-threaded process that maps the current user to root, makes a
/// private network namespace, and execs the test binary afresh. The child takes the namespace
/// branch because [`IN_NAMESPACE`] is set.
fn reexec_in_namespace() -> Bootstrap {
    if !namespaces_available() {
        return Bootstrap::Skipped("user namespaces are unavailable".to_owned());
    }
    let exe = std::env::current_exe().expect("current exe");
    // libtest names a test by its module path without the crate prefix.
    let test_name = format!(
        "{}::leak_corpus",
        module_path!()
            .strip_prefix("marsh_core::")
            .unwrap_or(module_path!())
    );
    let mut command = Command::new("unshare");
    command
        .args(namespace_args())
        .arg("--")
        .arg(&exe)
        .args([
            &test_name,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(IN_NAMESPACE, "1");
    if let Some(path) = std::env::var_os("LD_LIBRARY_PATH") {
        command.env("LD_LIBRARY_PATH", path);
    }
    let status = command.status().expect("run unshare");
    assert!(
        status.success(),
        "leak_corpus failed inside the namespace: {status}"
    );
    Bootstrap::Ran
}

/// Whether an unprivileged user+network namespace can be created at all.
fn namespaces_available() -> bool {
    Command::new("unshare")
        .args(namespace_args())
        .arg("true")
        .status()
        .is_ok_and(|status| status.success())
}

/// Brings up loopback and routes TEST-NET-1 to it, so the sink can bind `192.0.2.1`.
fn enter_loopback_network() {
    for arguments in [
        ["link", "set", "lo", "up"].as_slice(),
        ["route", "add", "local", "192.0.2.0/24", "dev", "lo"].as_slice(),
    ] {
        let status = Command::new("ip").args(arguments).status().expect("run ip");
        assert!(status.success(), "ip {arguments:?} failed: {status}");
    }
}

/// The directory the cases are read from.
fn corpus_dir() -> PathBuf {
    std::env::var_os("MARSH_LEAK_CORPUS").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/leak-corpus"),
        PathBuf::from,
    )
}

/// A small, deterministic PRNG (`SplitMix64`), so a seed reproduces a run without a dependency.
struct Rng(u64);
impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        match u64::try_from(n) {
            Ok(0) | Err(_) => 0,
            // The remainder is below `n`, so it fits back into `usize`.
            Ok(n) => usize::try_from(self.next_u64() % n).unwrap_or(0),
        }
    }
}

/// A fresh 24-character canary of `[A-Za-z0-9]`.
fn canary(rng: &mut Rng) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..24)
        .map(|_| ALPHABET[rng.below(ALPHABET.len())] as char)
        .collect()
}

/// A nanosecond-resolution seed when none is given.
fn random_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x1234_5678, |elapsed| {
            // Any 64 bits of the nanosecond count make a fine seed.
            u64::try_from(elapsed.as_nanos() & u128::from(u64::MAX)).unwrap_or(0x1234_5678)
        })
}

/// Runs the whole corpus and returns how many cases failed.
async fn run_corpus() -> usize {
    // Keep the process-wide service alive across cases instead of re-verifying BPF each time.
    let _tracing = marsh_instrument::Tracing::shared_with_enforcement(enforcement())
        .expect("start requested leak-corpus enforcement");
    let dir = corpus_dir();
    let (cases, gaps) =
        case::load(&dir).unwrap_or_else(|error| panic!("load {}: {error}", dir.display()));
    let filter = std::env::var("MARSH_LEAK_FILTER").ok();
    let iterations: usize = std::env::var("MARSH_LEAK_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1)
        .max(1);
    let seed = std::env::var("MARSH_LEAK_SEED")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(random_seed);

    eprintln!(
        "leak_corpus: {} case(s) from {}",
        cases.len(),
        dir.display()
    );
    eprintln!("leak_corpus: seed={seed} iterations={iterations}");
    if let Some(filter) = &filter {
        eprintln!("leak_corpus: filter={filter:?}");
    }

    let mut rng = Rng::new(seed);
    let mut rows: Vec<Row> = Vec::new();
    let mut failures = 0;

    for case in &cases {
        if filter
            .as_ref()
            .is_some_and(|needle| !case.name.contains(needle.as_str()))
        {
            continue;
        }
        let gap_reason = gaps
            .get(&case.name)
            .filter(|gap| {
                gap.enforcement.as_deref().is_none_or(|mode| {
                    mode == if enforcement() == marsh_instrument::Enforcement::Privileged {
                        "privileged"
                    } else {
                        "unprivileged"
                    }
                })
            })
            .map(|gap| gap.reason.clone());
        let known_gap = gap_reason.is_some();
        for iteration in 0..iterations {
            let variants = sample_variants(case, &mut rng, iteration);
            let token = canary(&mut rng);
            let outcome = run_once(case, &variants, &token, PolicyMode::Enforce).await;
            let verdict = assess(case, gap_reason.as_deref(), &outcome);
            if verdict.is_failure() {
                failures += 1;
                let (minimal, minimal_variants) =
                    minimize_failure(case, &variants, &token, gap_reason.clone()).await;
                eprintln!(
                    "\n=== FAIL {} [{}] ===\n{}",
                    case.name,
                    case::describe(&variants),
                    outcome.report(case, &verdict)
                );
                eprintln!(
                    "--- minimal reproducer ---\n{}",
                    minimal.to_toml(&minimal_variants)
                );
            }
            rows.push(Row {
                name: case.name.clone(),
                kind: case.kind,
                variants: case::describe(&variants),
                status: verdict.status,
            });
            // The audit cross-check runs once per case, on its first iteration.
            if iteration == 0 && case.kind == Kind::Attack && !known_gap {
                if let Some(problem) = audit_cross_check(case, &variants, &mut rng).await {
                    failures += 1;
                    eprintln!("\n=== FAIL {} (audit) ===\n{problem}", case.name);
                    rows.push(Row {
                        name: format!("{} (audit)", case.name),
                        kind: case.kind,
                        variants: "-".to_owned(),
                        status: Status::AuditGap,
                    });
                }
            }
        }
    }

    print_summary(&rows);
    failures
}

/// How a policy is applied for a run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PolicyMode {
    Enforce,
    Audit,
}

/// Everything one run of a case produced.
struct Outcome {
    /// Bytes each sink listener received.
    hits: Vec<sink::Hit>,
    /// Every denial the observer saw, as `"<resource> <precondition>"`.
    denials: Vec<String>,
    /// Each step's index paired with how it ended.
    steps: Vec<(usize, StepOutcome)>,
    /// The canary this run used, so the oracle can search for it.
    token: String,
    /// The encodings the case's oracle searches for.
    encodings: Vec<Encoding>,
}

/// How one step ended.
#[derive(Clone)]
enum StepOutcome {
    Accepted,
    Exited(u8),
    Refused,
    Error(String),
    Timeout,
}

impl StepOutcome {
    fn label(&self) -> String {
        match self {
            Self::Accepted => "accepted".to_owned(),
            Self::Exited(code) => format!("exit {code}"),
            Self::Refused => "refused".to_owned(),
            Self::Error(message) => format!("error: {message}"),
            Self::Timeout => "timeout".to_owned(),
        }
    }
}

impl Outcome {
    /// Where the canary reached the sink, if anywhere.
    fn leak(&self) -> Option<Found> {
        oracle::find(self.token.as_bytes(), &self.encodings, &self.hits)
    }

    /// Each step whose outcome did not match its `expect`, as a human-readable line. Background
    /// steps are exempt: their recorded timeout is expected, not a result.
    fn expectation_violations(&self, case: &Case) -> Vec<String> {
        let mut violations = Vec::new();
        for (index, outcome) in &self.steps {
            let step = &case.steps[*index];
            if step.background().is_some() {
                continue;
            }
            let ok = match step.expect {
                Expect::Any => true,
                Expect::Accepted => matches!(outcome, StepOutcome::Accepted),
                Expect::AcceptedPrivileged => {
                    enforcement() != marsh_instrument::Enforcement::Privileged
                        || matches!(outcome, StepOutcome::Accepted)
                }
                Expect::Refused => matches!(outcome, StepOutcome::Refused),
            };
            if !ok {
                violations.push(format!(
                    "step [{index}] expected {:?} but was {}",
                    step.expect,
                    outcome.label()
                ));
            }
        }
        violations
    }

    /// A multi-line explanation for a failing case.
    fn report(&self, case: &Case, verdict: &Verdict) -> String {
        let mut out = format!(
            "kind={:?} status={}\nsteps:",
            case.kind,
            verdict.status.label()
        );
        for (index, outcome) in &self.steps {
            let _ = write!(out, "\n  [{index}] {}", outcome.label());
        }
        for violation in &verdict.violations {
            let _ = write!(out, "\n  ! {violation}");
        }
        if self.denials.is_empty() {
            out.push_str("\ndenials: none");
        } else {
            out.push_str("\ndenials:");
            for denial in &self.denials {
                let _ = write!(out, "\n  {denial}");
            }
        }
        if let Some(found) = self.leak() {
            let _ = write!(
                out,
                "\nsink hit: {:?} at {} offset {}",
                found.encoding, found.listener, found.offset
            );
        }
        if let Status::KnownGap(reason) = &verdict.status {
            let _ = write!(out, "\nknown gap: {reason}");
        }
        out
    }
}

/// The verdict on one run. Where a leak landed is reported from the outcome, not stored here.
#[derive(Clone)]
enum Status {
    /// Attack blocked: the canary never reached the sink. Pass.
    Blocked,
    /// Control delivered: the canary reached the sink. Pass.
    Delivered,
    /// Attack leaked and the case is not a registered gap. Fail.
    Leaked,
    /// Attack leaked but the case is a registered known gap, with its reason. Pass.
    KnownGap(String),
    /// Control did not deliver: a false denial. Fail.
    FalseDenial,
    /// Audit mode did not report a block that enforcement makes. Fail.
    AuditGap,
}

impl Status {
    const fn is_failure(&self) -> bool {
        matches!(self, Self::Leaked | Self::FalseDenial | Self::AuditGap)
    }
    const fn label(&self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Delivered => "delivered",
            Self::Leaked => "LEAKED",
            Self::KnownGap(_) => "known-gap",
            Self::FalseDenial => "FALSE-DENIAL",
            Self::AuditGap => "AUDIT-GAP",
        }
    }
}

/// The leak verdict plus any step-expectation violations.
struct Verdict {
    status: Status,
    violations: Vec<String>,
}

impl Verdict {
    /// A run fails on a real leak, a false denial, or any unmet step expectation.
    fn is_failure(&self) -> bool {
        self.status.is_failure() || !self.violations.is_empty()
    }
}

/// The verdict for a completed run. `gap_reason` is `Some` when the case is a registered gap.
fn assess(case: &Case, gap_reason: Option<&str>, outcome: &Outcome) -> Verdict {
    let status = match (case.kind, outcome.leak().is_some()) {
        (Kind::Attack, true) => match gap_reason {
            Some(reason) => Status::KnownGap(reason.to_owned()),
            None => Status::Leaked,
        },
        (Kind::Attack, false) => Status::Blocked,
        (Kind::Control, true) => Status::Delivered,
        (Kind::Control, false) => Status::FalseDenial,
    };
    Verdict {
        status,
        violations: outcome.expectation_violations(case),
    }
}

/// Builds the agent policy a case names, in the requested mode and with its principal.
fn build_policy(case: &Case, mode: PolicyMode) -> AgentPolicy {
    let mut policy = match case.policy.as_str() {
        "recommended" => AgentPolicy::recommended().expect("recommended policy"),
        "untraced-local-deny" => AgentPolicy::recommended()
            .expect("recommended policy")
            .untraced_local(UntracedLocal::Deny),
        other => panic!("case {} names unknown policy {other}", case.name),
    };
    if mode == PolicyMode::Audit {
        policy = policy.audit();
    }
    if let Some(principal) = &case.principal {
        policy = policy.principal(principal.clone());
    }
    policy
}

/// Runs one case once and reports what the sink saw and the policy denied.
async fn run_once(
    case: &Case,
    variants: &BTreeMap<String, String>,
    token: &str,
    mode: PolicyMode,
) -> Outcome {
    let sink = Sink::start(IpAddr::V4(SINK_HOST)).expect("bind sink");
    // A local daemon marsh does not trace: what reaches it has left marsh's sight too.
    let local = Sink::start(IpAddr::V4(Ipv4Addr::LOCALHOST)).expect("bind local sink");
    let values = case.values(
        token,
        &SINK_HOST.to_string(),
        (sink.port, local.port),
        variants,
    );

    let fixture = super::Fixture::new();
    for file in &case.files {
        let path = case::substitute(&file.path, &values).expect("file path placeholders");
        let contents =
            case::substitute(&file.contents, &values).expect("file contents placeholders");
        let full = fixture.seed.join(&path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("seed directory");
        }
        std::fs::write(&full, contents).expect("seed file");
    }

    let denials = Arc::new(Mutex::new(Vec::new()));
    let observer = {
        let denials = Arc::clone(&denials);
        Arc::new(PolicyObserver::new(move |event, decision| {
            if let PolicyDecision::Deny {
                failed_precondition,
                ..
            } = decision
            {
                denials
                    .lock()
                    .unwrap()
                    .push(format!("{} {failed_precondition}", event.resource));
            }
        }))
    };

    let mut shells = Vec::new();
    for _ in 0..case.shells() {
        shells.push(open_shell(&fixture, build_policy(case, mode), Arc::clone(&observer)).await);
    }

    let mut steps = Vec::new();
    for (index, step) in case.steps.iter().enumerate() {
        let outcome = run_step(&shells[step.shell], step, &values).await;
        steps.push((index, outcome));
    }

    let mut hits = sink.drain(SINK_GRACE);
    hits.extend(local.drain(Duration::ZERO).into_iter().map(|mut hit| {
        hit.listener = format!("untraced local {}", hit.listener);
        hit
    }));
    for shell in shells {
        let _ = shell.close(true).await;
    }
    let denials = std::mem::take(&mut *denials.lock().unwrap());
    Outcome {
        hits,
        denials,
        steps,
        token: token.to_owned(),
        encodings: case.oracle.encodings.clone(),
    }
}

/// Opens one managed shell over the fixture's seed, judged by `policy` and reporting to `observer`.
async fn open_shell(
    fixture: &super::Fixture,
    policy: AgentPolicy,
    observer: Arc<PolicyObserver>,
) -> Shell {
    let mut builder = Shell::builder()
        .working_dir(fixture.seed.clone())
        .enforcement(enforcement())
        .sandbox_policy(SandboxPolicy::allow())
        .agent_policy(Some(Arc::new(policy)))
        .policy_observer(Some(observer));
    builder.backend = Some(fixture.fs.clone());
    builder.build().await.expect("build leak-fuzz shell")
}

/// Runs one step with a timeout and classifies how it ended.
async fn run_step(
    shell: &Shell,
    step: &Step,
    values: &std::collections::HashMap<String, String>,
) -> StepOutcome {
    let line = case::substitute(&step.run, values).expect("command placeholders");
    let timeout = if step.background().is_some() {
        BACKGROUND_TIMEOUT
    } else {
        STEP_TIMEOUT
    };
    match tokio::time::timeout(timeout, shell.run(&line)).await {
        Ok(Ok(result)) => {
            let code = u8::from(result.exit_code);
            if code == 0 {
                StepOutcome::Accepted
            } else {
                StepOutcome::Exited(code)
            }
        }
        Ok(Err(error)) => {
            if matches!(error.kind(), ShellErrorKind::Denied { .. }) {
                StepOutcome::Refused
            } else {
                StepOutcome::Error(error.to_string())
            }
        }
        // A background step is meant to keep running; a foreground timeout is recorded as one.
        Err(_) => StepOutcome::Timeout,
    }
}

/// Checks that audit mode reports the block enforcement makes.
///
/// Under audit the egress is not blocked, so the canary reaches the sink; the policy must still
/// have reported a would-block for an external peer. A missing report means audit does not see what
/// enforcement stops. Returns a description of the problem, or `None` when consistent.
async fn audit_cross_check(
    case: &Case,
    variants: &BTreeMap<String, String>,
    rng: &mut Rng,
) -> Option<String> {
    let token = canary(rng);
    let outcome = run_once(case, variants, &token, PolicyMode::Audit).await;
    // Audit never blocks, so the canary is expected to reach the sink. The check is only that the
    // policy reported the block enforcement would make; a run that never egressed proves nothing.
    outcome.leak()?;
    let reported = outcome
        .denials
        .iter()
        .any(|denial| denial.contains("net/") || denial.contains("network peer"));
    if reported {
        return None;
    }
    let verdict = Verdict {
        status: Status::AuditGap,
        violations: Vec::new(),
    };
    Some(format!(
        "audit leaked but produced no would-block report for an external peer.\n{}",
        outcome.report(case, &verdict)
    ))
}

/// Shrinks a failing case to its smallest reproducer.
async fn minimize_failure(
    case: &Case,
    variants: &BTreeMap<String, String>,
    token: &str,
    gap_reason: Option<String>,
) -> (Case, BTreeMap<String, String>) {
    let token = token.to_owned();
    let fails: Box<shrink::Reproduces<'_>> =
        Box::new(move |candidate: &Case, choice: &BTreeMap<String, String>| {
            let token = token.clone();
            let candidate = candidate.clone();
            let choice = choice.clone();
            let gap_reason = gap_reason.clone();
            Box::pin(async move {
                let outcome = run_once(&candidate, &choice, &token, PolicyMode::Enforce).await;
                assess(&candidate, gap_reason.as_deref(), &outcome).is_failure()
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = bool>>>
        });
    shrink::minimize(case, variants, &fails).await
}

/// Picks a variant value per slot: the first value on iteration 0, a sampled one afterwards.
fn sample_variants(case: &Case, rng: &mut Rng, iteration: usize) -> BTreeMap<String, String> {
    if iteration == 0 {
        return case.first_variants();
    }
    case.variants
        .iter()
        .map(|(slot, values)| {
            let value = &values[rng.below(values.len())];
            (
                slot.clone(),
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned),
            )
        })
        .collect()
}

/// One line of the end-of-run summary.
struct Row {
    name: String,
    kind: Kind,
    variants: String,
    status: Status,
}

/// Prints the per-case result table and a count of failures.
fn print_summary(rows: &[Row]) {
    eprintln!("\n=== leak_corpus summary ===");
    let width = rows
        .iter()
        .map(|row| row.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    for row in rows {
        eprintln!(
            "  {:<width$}  {:<7}  {:<12}  {}",
            row.name,
            format!("{:?}", row.kind).to_lowercase(),
            row.status.label(),
            row.variants,
        );
    }
    let failures = rows.iter().filter(|row| row.status.is_failure()).count();
    eprintln!("  {} case-run(s), {failures} failure(s)", rows.len());
}
