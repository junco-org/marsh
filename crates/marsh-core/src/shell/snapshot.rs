//! Stage 1: clean work generation, immutable baseline, and owned evaluation resources.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use marsh_instrument::{
    ExecCommand, ExecDecision, ExecHooks, FileTarget, InvocationId, RootId, Syscall, TraceRun,
};
use marsh_lib::RecoverPoison as _;

use super::access::{Access, Effects};
use super::agent::{self, AgentPolicy, Observation, Observed, Peer};
use super::builtins::gitcmd::GitAction;
use super::builtins::gitexec::{self, Runner};
use super::completion::{Completion, Finalize};
use super::execution::Run;
use super::policy::{self, Denial, Event, PolicyDecision, PolicyKind, PolicyObserver};
use super::session::Session;
use super::taint::Taint;
use super::{Principal, ShellError};

#[derive(Default)]
pub(super) struct CommandEvidence {
    /// The command's frozen baseline, against which its source changes are classified.
    pub baseline: PathBuf,
    pub records: Vec<Syscall>,
    pub effects: Vec<(u64, Option<InvocationId>, Effects)>,
    pub git: Vec<GitEffectRecord>,
    pub failure: Option<String>,
    /// Git invocations admitted and not yet finalized.
    git_active: HashSet<InvocationId>,
    git_exclusive: bool,
}

/// What a contact of the active command is judged after, beyond committed history.
struct Prior {
    /// The command's own observations so far, then taint received from other principals.
    observations: Vec<Observation>,
    /// The taint table's revision when the received taint was read.
    taint: u64,
    /// How many effects the command's own observations were drawn from, their latest entry
    /// order, and the last observation they recorded; `None` when the run is not active.
    effects: Option<(usize, u64, Option<Observed>)>,
}

/// The agent policy's answers for the active command's contacts, by peer class, and what they
/// were decided from. The policy sees a contact only as its peer's class, so a later contact of
/// the same class reuses the answer while none of that has changed.
#[derive(Default)]
struct Verdicts {
    basis: Option<Basis>,
    answers: HashMap<String, Option<Denial>>,
}

/// What a set of [`Verdicts`] was decided from.
#[derive(Clone, PartialEq, Eq)]
struct Basis {
    run: TraceRun,
    /// The policy validator's revision, which advances before any change to committed history.
    history: u64,
    /// The taint table's revision, which advances whenever any principal's taint changes.
    taint: u64,
    /// How many of the command's effects were observed, and their latest entry order.
    effects: usize,
    order: u64,
    /// The last observation those effects recorded.
    last: Option<Observed>,
}

impl Basis {
    /// Extends this basis over the command's `effects` observed since, when they record nothing
    /// new. A call that records nothing at all may be ordered anywhere: a blocking call exits,
    /// and is observed, long after later calls entered. Any other must have entered after every
    /// earlier one, so it is ordered last, and only contacted the last peer again, which
    /// collapses as a consecutive repeat. A file a shared mapping changes between calls is
    /// classified as of the last call that recorded something.
    fn advance(
        &mut self,
        agent: &AgentPolicy,
        effects: &[(u64, Option<InvocationId>, Effects)],
    ) -> bool {
        let Some(since) = effects.get(self.effects..) else {
            return false;
        };
        for (order, _, effect) in since {
            if agent::only_repeats(Some(agent), effect, None) {
                continue;
            }
            if *order < self.order || !agent::only_repeats(Some(agent), effect, self.last.as_ref())
            {
                return false;
            }
            self.order = *order;
        }
        self.effects = effects.len();
        true
    }
}

pub(super) struct Snapshot {
    pub session: Arc<Session>,
    pub uid: Principal,
    path: PathBuf,
    root: OnceLock<RootId>,
    pub retained: AtomicBool,
    closed: AtomicBool,
    reclaimed: AtomicBool,
    deferred: Mutex<Vec<PathBuf>>,
    pub state: Mutex<SnapshotState>,
    /// Judges socket peers while commands run, before the call reaches the kernel.
    agent: Option<Arc<AgentPolicy>>,
    /// Notified of each peer the agent policy refuses (or, auditing, would refuse).
    observer: Option<Arc<PolicyObserver>>,
    /// The agent policy's answers for the active command's contacts.
    verdicts: Mutex<Verdicts>,
    /// The run whose refused peers were reported, and the refusals reported for it: a program
    /// retrying a refused call is reported once.
    reported: Mutex<(Option<TraceRun>, HashSet<Event>)>,
}

pub(super) struct SnapshotState {
    pub tree_seq: super::session::TreeVersion,
    /// The source's direct epoch this generation was taken at.
    epoch: u64,
    pub dirty: bool,
    access: Access,
    pub evidence: Option<(TraceRun, CommandEvidence)>,
    /// External git invocations of the active command, between their exec and their end.
    runners: HashMap<InvocationId, Runner>,
}

impl Snapshot {
    pub fn new(
        session: Arc<Session>,
        uid: Principal,
        agent: Option<Arc<AgentPolicy>>,
        observer: Option<Arc<PolicyObserver>>,
    ) -> Result<Arc<Self>, ShellError> {
        session.check()?;
        let path = session.persistence.work(uid.as_str());
        let authority = session.validator.read();
        let tree_seq = authority.tree_seq;
        let epoch = session.epoch.load(Ordering::Acquire);
        let internal = session.tracing.internal_scope()?;
        let _guard = internal.enter();
        session.fs.snapshot(&session.persistence.seed, &path)?;
        let path = path.canonicalize()?;
        drop(authority);
        let snapshot = Arc::new(Self {
            session,
            uid,
            path,
            root: OnceLock::new(),
            retained: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            reclaimed: AtomicBool::new(false),
            deferred: Mutex::new(Vec::new()),
            state: Mutex::new(SnapshotState {
                tree_seq,
                epoch,
                dirty: false,
                access: Access::default(),
                evidence: None,
                runners: HashMap::new(),
            }),
            agent,
            observer,
            verdicts: Mutex::new(Verdicts::default()),
            reported: Mutex::new((None, HashSet::new())),
        });
        let weak = Arc::downgrade(&snapshot);
        let (select, begin, end) = (weak.clone(), weak.clone(), weak.clone());
        let exec = ExecHooks {
            select: Arc::new(move |run, owner, path| {
                select
                    .upgrade()
                    .is_some_and(|snapshot| snapshot.selects_git(run, owner, path))
            }),
            begin: Arc::new(move |run, _, command| match begin.upgrade() {
                Some(snapshot) => snapshot.begin_external(run, command),
                None => Ok(ExecDecision::Continue),
            }),
            end: Arc::new(move |run, invocation, status| {
                if let Some(snapshot) = end.upgrade() {
                    snapshot.end_external(run, invocation, status);
                }
                Ok(())
            }),
        };
        let root = snapshot.session.tracing.register_root(
            snapshot.path(),
            Arc::new(move |run, builtin, info| {
                if let Some(snapshot) = weak.upgrade() {
                    snapshot.observe(run, builtin, info);
                }
                Ok(())
            }),
            Some(exec),
        )?;
        snapshot
            .root
            .set(root)
            .map_err(|_| ShellError::infrastructure("snapshot registered twice"))?;
        if snapshot.agent.is_some() {
            let gate = Arc::downgrade(&snapshot);
            snapshot.session.tracing.gate_peers(
                root,
                Arc::new(move |run, _, peer, source| {
                    gate.upgrade()
                        .is_none_or(|snapshot| snapshot.permits_peer(run, peer, source))
                }),
            )?;
        }
        snapshot
            .session
            .snapshots
            .lock()
            .recover()
            .insert(snapshot.path.clone(), Arc::downgrade(&snapshot));
        Ok(snapshot)
    }

    pub fn close(&self) -> Result<(), ShellError> {
        if self.reclaimed.load(Ordering::Acquire) {
            return Ok(());
        }
        if !self.closed.load(Ordering::Acquire)
            && let Some(root) = self.root.get()
        {
            if let Err(error) = self.session.tracing.unregister_root(*root) {
                self.retained.store(true, Ordering::Release);
                return Err(error.into());
            }
        }
        self.closed.store(true, Ordering::Release);
        if self.retained.load(Ordering::Acquire) {
            return Err(ShellError::infrastructure(
                "command resources retained because recovery or quiescence is required",
            ));
        }
        let internal = self.session.tracing.internal_scope().ok();
        let _scope = internal.as_ref().map(marsh_instrument::TraceScope::enter);
        let mut pending = std::mem::take(&mut *self.deferred.lock().recover());
        if !pending.contains(&self.path) {
            pending.push(self.path.clone());
        }
        let mut failed = Vec::new();
        let mut failure = None;
        for path in pending {
            if let Err(error) = self.session.fs.delete_subvolume(&path) {
                failed.push(path);
                failure.get_or_insert(error);
            }
        }
        *self.deferred.lock().recover() = failed;
        if let Some(error) = failure {
            return Err(error.into());
        }
        self.reclaimed.store(true, Ordering::Release);
        Ok(())
    }
    pub fn reclaim(&self, path: &Path) -> Result<(), ShellError> {
        let internal = self.session.tracing.internal_scope().ok();
        let _scope = internal.as_ref().map(marsh_instrument::TraceScope::enter);
        if let Err(error) = self.session.fs.delete_subvolume(path) {
            let mut deferred = self.deferred.lock().recover();
            if !deferred.iter().any(|held| held == path) {
                deferred.push(path.to_path_buf());
            }
            drop(deferred);
            return Err(error.into());
        }
        Ok(())
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn root(&self) -> Result<RootId, ShellError> {
        self.root
            .get()
            .copied()
            .ok_or_else(|| ShellError::infrastructure("snapshot registration incomplete"))
    }
    pub fn logical(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.path).map_or_else(
            |_| path.to_path_buf(),
            |relative| self.session.persistence.seed.join(relative),
        )
    }
    pub fn physical(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.session.persistence.seed)
            .map_or_else(|_| path.to_path_buf(), |relative| self.path.join(relative))
    }

    fn observe(&self, run: TraceRun, builtin: Option<InvocationId>, info: Syscall) {
        let mut state = self.state.lock().recover();
        if state
            .evidence
            .as_ref()
            .is_none_or(|(active, _)| *active != run)
        {
            return;
        }
        let mut effect = state.access.observe(&info, &self.path);
        drop(state);
        // Before the task resumes: a carrier is known before its writer can signal a reader, and
        // a reader's taint before it can reach a peer.
        if let (Some(agent), Ok(effect)) = (&self.agent, &mut effect) {
            effect.taints = self.session.taint.lock().recover().observe(
                &agent.acting(&self.uid),
                agent,
                effect,
            );
        }
        let foreign = effect
            .as_ref()
            .is_ok_and(|effect| Session::invalidate_foreign(&self.path, &effect.outside_writes));
        let mut state = self.state.lock().recover();
        if let Some((_, evidence)) = &mut state.evidence {
            if foreign {
                evidence.failure.get_or_insert_with(|| {
                    "command wrote into another shell's private work view".into()
                });
            }
            match effect {
                Ok(effect) => evidence.effects.push((info.entry_order, builtin, effect)),
                Err(error) => {
                    evidence.failure.get_or_insert(error);
                }
            }
            evidence.records.push(info);
        }
    }

    /// Whether the active command may reach `peer` (a native `sockaddr`, empty when unknown): its
    /// agent policy decides against committed history plus the command's observations so far,
    /// including a read of `source`, the file this same call copies into the socket in-kernel.
    /// When the call sends a propagated class to a local `peer`, the policy may also refuse a peer
    /// only untraced processes hold, and a permitted contact hands the class to the other
    /// principals of this source whose commands hold it. Called on the tracer thread while the
    /// calling task is stopped at the call's entry.
    fn permits_peer(&self, run: TraceRun, peer: &[u8], source: Option<&FileTarget>) -> bool {
        let Some(agent) = &self.agent else {
            return true;
        };
        let principal = agent.acting(&self.uid);
        // The call's own in-kernel read is recorded only at its exit, after the bytes left.
        let source = match source {
            None => None,
            Some(source) => {
                let read = self
                    .state
                    .lock()
                    .recover()
                    .access
                    .read_effect(source, &self.path);
                match read {
                    Ok(mut effects) => {
                        // A carrier's source taints as it will at the call's exit, which records
                        // it.
                        effects.taints = self.session.taint.lock().recover().carried(&effects);
                        Some(effects)
                    }
                    // An unclassifiable source fails the command at exit; its bytes must not
                    // leave.
                    Err(_) => return agent.audits(),
                }
            }
        };
        let sending =
            self.session
                .taint
                .lock()
                .recover()
                .sending(&principal, agent, source.as_ref());
        // Who holds a local peer matters only to a call that sends a propagated class.
        let receivers = if sending.is_empty() {
            None
        } else {
            self.session
                .tracing
                .receivers(peer)
                .map(|sockets| (sockets, self.followed()))
        };
        let peer = Peer::decode(peer);
        let denial = self
            .judge_peer(run, agent, &principal, &peer, source)
            .or_else(|| {
                let (sockets, followed) = receivers.as_ref()?;
                untraced_denial(agent, &principal, &peer, &sending, sockets, followed)
            });
        if let Some(denial) = denial {
            self.report(run, agent, &denial);
            if !agent.audits() {
                return false;
            }
        }
        if let Some((sockets, followed)) = &receivers {
            self.hand_taint(&principal, &peer, &sending, sockets, followed);
        }
        true
    }

    /// The principals of this source's agent-judged shells, by the root tracing their commands:
    /// those whose taint the source's table follows.
    fn followed(&self) -> HashMap<RootId, Principal> {
        self.session
            .snapshots
            .lock()
            .recover()
            .values()
            .filter_map(Weak::upgrade)
            .filter_map(|snapshot| {
                let agent = snapshot.agent.as_ref()?;
                Some((*snapshot.root.get()?, agent.acting(&snapshot.uid)))
            })
            .collect()
    }

    /// Hands the `sending` classes to every other principal of this source whose commands hold
    /// one of `sockets`, those bound to the local `peer`, before a permitted contact delivers
    /// anything to it. Holders outside this source's agent-judged commands are not tainted.
    fn hand_taint(
        &self,
        principal: &Principal,
        peer: &Peer,
        sending: &[Taint],
        sockets: &[HashSet<RootId>],
        followed: &HashMap<RootId, Principal>,
    ) {
        let receivers: HashSet<&Principal> = sockets
            .iter()
            .flatten()
            .filter_map(|root| followed.get(root))
            .filter(|receiver| *receiver != principal)
            .collect();
        if receivers.is_empty() {
            return;
        }
        let via = format!("socket {peer}");
        let mut table = self.session.taint.lock().recover();
        for receiver in receivers {
            for taint in sending {
                table.hand(
                    receiver,
                    Taint {
                        class: taint.class.clone(),
                        from: principal.clone(),
                        via: via.clone(),
                    },
                );
            }
        }
    }

    /// The agent policy's refusal, if any, of the active command reaching `peer`; see
    /// [`Self::permits_peer`]. `source` is the call's own in-kernel read.
    fn judge_peer(
        &self,
        run: TraceRun,
        agent: &AgentPolicy,
        principal: &Principal,
        peer: &Peer,
        source: Option<Effects>,
    ) -> Option<Denial> {
        let candidate = Observation {
            principal: principal.clone(),
            observed: Observed::Connect(peer.clone()),
        };
        if !agent.judges(&candidate) {
            return None;
        }
        let Some(source) = source else {
            return self.verdict(run, agent, principal, peer, &candidate);
        };
        let mut prior = self.prior(run, agent, principal).observations;
        prior.extend(agent::observations(
            &[(u64::MAX, None, source)],
            principal,
            Some(agent),
            |_| None,
            |_| Vec::new(),
        ));
        let authority = self.session.validator.read();
        agent.decide_now(&authority.observations, &prior, &candidate)
    }

    /// The refusal of `candidate`, a contact with `peer` that copies nothing in-kernel: the
    /// answer for `peer`'s class, decided again only when what it was decided from has changed.
    fn verdict(
        &self,
        run: TraceRun,
        agent: &AgentPolicy,
        principal: &Principal,
        peer: &Peer,
        candidate: &Observation,
    ) -> Option<Denial> {
        let class = agent.peer_class(peer).to_owned();
        // Held throughout, so answers and their basis change together. Every other lock is taken
        // under it, never the reverse.
        let mut verdicts = self.verdicts.lock().recover();
        let taint = self.session.taint.lock().recover().revision();
        let history = self.session.validator.revision();
        let current = {
            let state = self.state.lock().recover();
            match (&state.evidence, &mut verdicts.basis) {
                (Some((active, evidence)), Some(basis)) if *active == run => {
                    basis.run == run
                        && basis.history == history
                        && basis.taint == taint
                        && basis.advance(agent, &evidence.effects)
                }
                _ => false,
            }
        };
        if current && let Some(answer) = verdicts.answers.get(&class) {
            return answer.clone();
        }
        let prior = self.prior(run, agent, principal);
        let (denial, history) = {
            let authority = self.session.validator.read();
            // Read under the guard: no change to the history it guards is under way.
            let history = self.session.validator.revision();
            (
                agent.decide_now(&authority.observations, &prior.observations, candidate),
                history,
            )
        };
        if let Some((count, order, last)) = prior.effects {
            let basis = Basis {
                run,
                history,
                taint: prior.taint,
                effects: count,
                order,
                last,
            };
            if verdicts.basis.as_ref() != Some(&basis) {
                verdicts.answers.clear();
                verdicts.basis = Some(basis);
            }
            verdicts.answers.insert(class, denial.clone());
        }
        denial
    }

    /// What a contact of the command `run` is judged after, beyond committed history.
    fn prior(&self, run: TraceRun, agent: &AgentPolicy, principal: &Principal) -> Prior {
        // Taken one after the other, never nested: `prepare` locks this state under the authority.
        let (mut prior, effects) = match &self.state.lock().recover().evidence {
            Some((active, evidence)) if *active == run => {
                let own = agent::observations(
                    &evidence.effects,
                    principal,
                    Some(agent),
                    |path| policy::change(&evidence.baseline, &self.path, path),
                    |_| Vec::new(),
                );
                let order = evidence
                    .effects
                    .iter()
                    .map(|(order, _, _)| *order)
                    .max()
                    .unwrap_or_default();
                let last = own.last().map(|observation| observation.observed.clone());
                (own, Some((evidence.effects.len(), order, last)))
            }
            _ => (Vec::new(), None),
        };
        // Taint received from another principal, including through a command still running.
        let table = self.session.taint.lock().recover();
        prior.extend(
            table
                .received(principal)
                .into_iter()
                .map(|taint| Observation {
                    principal: principal.clone(),
                    observed: Observed::Taint(taint),
                }),
        );
        Prior {
            observations: prior,
            taint: table.revision(),
            effects,
        }
    }

    /// Reports `denial` of a contact to the observer, once per run.
    fn report(&self, run: TraceRun, agent: &AgentPolicy, denial: &Denial) {
        if let Some(observer) = &self.observer
            && self.first_report(run, &denial.event)
        {
            let verb = if agent.audits() {
                "would block"
            } else {
                "blocked"
            };
            observer.notify(
                PolicyKind::Agent,
                &denial.event,
                &PolicyDecision::Deny {
                    failed_precondition: format!("{verb}: {}", denial.failed_precondition),
                    allowed_fixes: denial.allowed_fixes.clone(),
                },
            );
        }
    }

    fn first_report(&self, run: TraceRun, event: &Event) -> bool {
        let mut reported = self.reported.lock().recover();
        if reported.0 != Some(run) {
            *reported = (Some(run), HashSet::new());
        }
        reported.1.insert(event.clone())
    }

    /// Marks the active command's evidence incomplete, so it is never published.
    pub fn fail_evidence(&self, cause: String) {
        if let Some((_, evidence)) = &mut self.state.lock().recover().evidence {
            evidence.failure.get_or_insert(cause);
        }
    }

    pub fn take_evidence(&self, run: TraceRun) -> Result<CommandEvidence, ShellError> {
        // Declared first so they drop after the lock on every path: their guards relock it.
        let stale;
        let mut state = self.state.lock().recover();
        stale = std::mem::take(&mut state.runners);
        let Some((active, mut evidence)) = state.evidence.take() else {
            return Err(ShellError::infrastructure("missing command evidence"));
        };
        if active != run {
            return Err(ShellError::infrastructure(
                "command evidence identity mismatch",
            ));
        }
        if !evidence.git_active.is_empty() || !stale.is_empty() {
            evidence
                .failure
                .get_or_insert_with(|| "git invocation has not completed".into());
        }
        drop(state);
        // Their guards settle against the snapshot's state, which is no longer locked here.
        drop(stale);
        for git in &mut evidence.git {
            let (first, last) = self
                .session
                .tracing
                .invocation_orders(run, git.invocation)?;
            git.started_order = first;
            git.finished_order = last;
        }
        Ok(evidence)
    }

    pub fn writes_for(&self, invocation: InvocationId) -> Vec<PathBuf> {
        let state = self.state.lock().recover();
        let mut paths = std::collections::BTreeSet::new();
        if let Some((_, evidence)) = &state.evidence {
            for (_, owner, effects) in &evidence.effects {
                if *owner == Some(invocation) {
                    paths.extend(effects.writes.iter().cloned());
                }
            }
        }
        drop(state);
        paths.into_iter().collect()
    }
    /// Admits `invocation` of `run` as a git invocation of `kind`; an exclusive one runs alone.
    pub(crate) fn begin_git(
        self: &Arc<Self>,
        run: TraceRun,
        invocation: InvocationId,
        kind: GitCohortKind,
    ) -> Result<Completion<GitGuard>, String> {
        let mut state = self.state.lock().recover();
        let (_, evidence) = state
            .evidence
            .as_mut()
            .filter(|(active, _)| *active == run)
            .ok_or_else(|| "git has no owning run".to_string())?;
        if !evidence.git_active.is_empty()
            && (evidence.git_exclusive || kind == GitCohortKind::Exclusive)
        {
            evidence
                .failure
                .get_or_insert_with(|| "unordered overlapping git invocations".into());
            return Err("git operation overlaps another invocation".into());
        }
        evidence.git_exclusive = kind == GitCohortKind::Exclusive;
        evidence.git_active.insert(invocation);
        drop(state);
        Ok(Completion::new(GitGuard {
            snapshot: Arc::downgrade(self),
            run,
            invocation,
        }))
    }

    /// Runs `update` on the evidence of `run`, when it is still the active command's.
    fn with_evidence(&self, run: TraceRun, update: impl FnOnce(&mut CommandEvidence)) {
        if let Some((active, evidence)) = &mut self.state.lock().recover().evidence
            && *active == run
        {
            update(evidence);
        }
    }

    /// Whether an exec of `path` by `owner` in `run` is a git invocation the active command must
    /// observe: any executable named `git`, unless it already belongs to an observed git
    /// invocation — its own helpers and probes.
    fn selects_git(&self, run: TraceRun, owner: Option<InvocationId>, path: &Path) -> bool {
        if path.file_name().is_none_or(|name| name != "git") {
            return false;
        }
        let state = self.state.lock().recover();
        state.evidence.as_ref().is_some_and(|(active, evidence)| {
            *active == run && owner.is_none_or(|owner| !evidence.git_active.contains(&owner))
        })
    }

    /// Admits an external git exactly as the builtin is admitted, before its image runs: its
    /// probes reproduce its own program, directory and environment. A refusal latches its
    /// diagnostic, so nothing of the command publishes, and the process is ended.
    fn begin_external(
        self: &Arc<Self>,
        run: TraceRun,
        command: ExecCommand,
    ) -> std::io::Result<ExecDecision> {
        let tracing = Arc::clone(&self.session.tracing);
        let invocation = tracing.invocation(run)?;
        let (argv, template) = match gitexec::external(command) {
            Ok(external) => external,
            Err(refusal) => {
                self.with_evidence(run, |evidence| {
                    evidence.failure.get_or_insert_with(|| refusal.into());
                });
                return Ok(ExecDecision::Refuse);
            }
        };
        let runner = match Runner::new(argv).prepare(template, self, tracing, run, invocation) {
            Ok(runner) => runner,
            Err(refusal) => {
                self.with_evidence(run, |evidence| {
                    evidence.failure.get_or_insert(refusal);
                });
                return Ok(ExecDecision::Refuse);
            }
        };
        // A displaced runner settles only after the lock is released.
        let displaced = self
            .state
            .lock()
            .recover()
            .runners
            .insert(invocation, runner);
        drop(displaced);
        Ok(ExecDecision::Track(invocation))
    }

    /// Attributes an external git whose last process ended, with its own status; without one,
    /// its observation is incomplete and it never publishes.
    fn end_external(&self, run: TraceRun, invocation: InvocationId, status: Option<ExitStatus>) {
        let runner = self.state.lock().recover().runners.remove(&invocation);
        let Some(runner) = runner else {
            self.with_evidence(run, |evidence| {
                evidence
                    .failure
                    .get_or_insert_with(|| "git invocation did not finish observation".into());
            });
            return;
        };
        match status {
            Some(status) => {
                if let Err(failure) = runner.finish(status.success(), false) {
                    self.with_evidence(run, |evidence| {
                        evidence.failure.get_or_insert(failure);
                    });
                }
            }
            None => drop(runner),
        }
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[derive(Clone, Copy)]
pub(super) struct CommandNumber(pub(super) u64);
impl std::fmt::Display for CommandNumber {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Owned command baseline; its finalizer never publishes and leaves refused work dirty for reset.
pub(super) struct PreparedCommand {
    pub snapshot: Arc<Snapshot>,
    pub baseline: PathBuf,
    pub run: Arc<Run>,
    pub tree_seq: super::session::TreeVersion,
    pub number: CommandNumber,
}

/// Refreshes a stale work generation, freezes its baseline and begins the traced run. `cwd` is
/// the span's logical initial directory.
pub(super) fn prepare(
    snapshot: &Arc<Snapshot>,
    runtime: tokio::runtime::Handle,
    number: CommandNumber,
    cwd: Arc<PathBuf>,
) -> Result<Completion<PreparedCommand>, ShellError> {
    let session = &snapshot.session;
    session.check()?;
    let scope = session.tracing.internal_scope()?;
    let _guard = scope.enter();
    let authority = session.validator.read();
    let epoch = session.epoch.load(Ordering::Acquire);
    let mut state = snapshot.state.lock().recover();
    let retake = state.dirty || state.tree_seq != authority.tree_seq || state.epoch != epoch;
    if retake {
        session.fs.delete_subvolume(snapshot.path())?;
        session
            .fs
            .snapshot(&session.persistence.seed, snapshot.path())?;
        state.tree_seq = authority.tree_seq;
        state.epoch = epoch;
        state.dirty = false;
    }
    state.access.prepare(snapshot.path(), retake)?;
    let baseline = session
        .persistence
        .snap()
        .join(format!("{}-base-{number}", snapshot.uid));
    session.fs.snapshot_readonly(snapshot.path(), &baseline)?;
    let tree_seq = authority.tree_seq;
    let trace = match session.tracing.begin_run(snapshot.root()?) {
        Ok(run) => run,
        Err(error) => {
            session.fs.delete_subvolume(&baseline)?;
            return Err(error.into());
        }
    };
    state.evidence = Some((
        trace,
        CommandEvidence {
            baseline: baseline.clone(),
            ..CommandEvidence::default()
        },
    ));
    state.dirty = true;
    drop(state);
    drop(authority);
    let run = Arc::new(Run::managed(
        runtime,
        Arc::clone(&session.tracing),
        trace,
        Arc::downgrade(snapshot),
        cwd,
    ));
    Ok(Completion::new(PreparedCommand {
        snapshot: Arc::clone(snapshot),
        baseline,
        run,
        tree_seq,
        number,
    }))
}

impl Completion<PreparedCommand> {
    /// Reclaims the baseline once; a refused reclaim is not retried by the finalizer.
    pub fn reclaim(&mut self) -> Result<(), ShellError> {
        self.completed = true;
        self.payload.snapshot.reclaim(&self.payload.baseline)
    }
}
impl Finalize for PreparedCommand {
    fn finalize(&mut self, completed: bool) {
        if !completed {
            let _ = self.snapshot.reclaim(&self.baseline);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GitEffectRecord {
    pub invocation: InvocationId,
    pub started_order: u64,
    pub finished_order: u64,
    pub requests: Vec<(GitAction, PathBuf)>,
    pub metadata: Vec<PathBuf>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GitCohortKind {
    Inspect,
    Exclusive,
}

/// One admitted git invocation of one run. It holds its snapshot weakly, and settles nothing
/// once that run's evidence has been taken.
pub(super) struct GitGuard {
    snapshot: Weak<Snapshot>,
    run: TraceRun,
    invocation: InvocationId,
}
impl Completion<GitGuard> {
    pub fn record(mut self, record: GitEffectRecord) {
        if let Some(snapshot) = self.payload.snapshot.upgrade() {
            snapshot.with_evidence(self.payload.run, |evidence| evidence.git.push(record));
        }
        self.completed = true;
    }
}
impl GitGuard {
    pub fn fail(&self, message: String) {
        if let Some(snapshot) = self.snapshot.upgrade() {
            snapshot.with_evidence(self.run, |evidence| {
                evidence.failure.get_or_insert(message);
            });
        }
    }
}
impl Finalize for GitGuard {
    fn finalize(&mut self, completed: bool) {
        let Some(snapshot) = self.snapshot.upgrade() else {
            return;
        };
        snapshot.with_evidence(self.run, |evidence| {
            evidence.git_active.remove(&self.invocation);
            if !completed {
                evidence
                    .failure
                    .get_or_insert_with(|| "git invocation did not finish observation".into());
            }
        });
    }
}

/// The refusal of a contact sending the propagated `sending` classes to the local `peer`, when
/// `agent` denies untraced local peers and one of `sockets`, those bound to `peer`, is held by no
/// command whose principal the source's taint table follows.
fn untraced_denial(
    agent: &AgentPolicy,
    principal: &Principal,
    peer: &Peer,
    sending: &[Taint],
    sockets: &[HashSet<RootId>],
    followed: &HashMap<RootId, Principal>,
) -> Option<Denial> {
    let untraced = sockets
        .iter()
        .any(|holders| !holders.iter().any(|root| followed.contains_key(root)));
    if !agent.denies_untraced_local() || sending.is_empty() || !untraced {
        return None;
    }
    let event = agent.label(&Observation {
        principal: principal.clone(),
        observed: Observed::Connect(peer.clone()),
    })?;
    let classes: Vec<&str> = sending.iter().map(|taint| taint.class.as_str()).collect();
    Some(Denial {
        event,
        failed_precondition: format!(
            "{principal} sent {} data to {peer}, a local peer no traced command of this source holds",
            classes.join(", ")
        ),
        allowed_fixes: classes
            .iter()
            .map(|class| format!("declassify {class} for {principal}"))
            .chain(["allow untraced local peers in the agent policy".to_owned()])
            .collect(),
    })
}
