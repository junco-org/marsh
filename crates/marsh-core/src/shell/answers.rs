//! Privileged mode's answers: every peer class decided ahead of the contacts the kernel judges.
//!
//! The kernel looks up one precomputed answer per class, so a root's answers are recomputed after
//! every observed call that records something, before that call's exit stop resumes the task.
//! Doing so by deciding each class against the whole history would cost a walk of the history per
//! call. Instead each class keeps a junco [`TraceCursor`]: the rules' derivative state after the
//! committed history and the command's own observations, advanced by one step per new event. Only
//! taint received from other principals, which the lazy path appends after everything else, is
//! replayed per decision; it is a handful of events.
//!
//! The command's own observations are extended in place while that matches what
//! [`agent::observations`] would produce from scratch: each recording effect ordered after the
//! last, and no write changing whether a path an earlier write named exists (its classification
//! is re-read from the work view on every decision). Anything else rebuilds them from the
//! committed history's cursors.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use junco_policy::{Bump, PolicyDecision, TraceCursor, TracePolicy};
use marsh_instrument::{InvocationId, KernelPolicy, TraceRun};
use marsh_lib::RecoverPoison as _;
use marsh_peer_class::{Answers, LOOPBACK, MAX_CLASSES, MAX_TAINTS, UNIX};

use super::access::Effects;
use super::agent::{self, AgentPolicy, Observation, Observed};
use super::policy::{self, Action, Denial, Event, Principal};
use super::taint::Taint;

/// Process-wide IDs of propagated taint classes: kernel socket tags and receipt counters carry
/// them as bits, whichever root sent or reads.
static TAINTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The kernel ID of a propagated taint class, assigned on first use.
pub(super) fn taint_id(class: &str) -> io::Result<usize> {
    let mut taints = TAINTS.lock().recover();
    if let Some(id) = taints.iter().position(|known| known == class) {
        return Ok(id);
    }
    if taints.len() == MAX_TAINTS {
        return Err(io::Error::other(format!(
            "privileged enforcement carries at most {MAX_TAINTS} propagated taint classes"
        )));
    }
    taints.push(class.to_owned());
    Ok(taints.len() - 1)
}

/// The propagated classes with kernel IDs, by ID.
pub(super) fn taint_classes() -> Vec<String> {
    TAINTS.lock().recover().clone()
}

/// One compiled policy and the cursors borrowing its arena.
struct Decider<'arena> {
    policy: TracePolicy<'arena>,
    /// Each class's contact, the candidate its cursors decide.
    candidates: Vec<Event>,
    /// The labelled committed history `base` consumed.
    committed: Vec<Event>,
    base: Vec<TraceCursor<'arena>>,
    /// The labelled own observations `cursors` consumed after `base`.
    own: Vec<Event>,
    cursors: Vec<TraceCursor<'arena>>,
}

self_cell::self_cell!(
    struct Engine {
        owner: Bump,
        #[not_covariant]
        dependent: Decider,
    }
);

#[expect(
    clippy::non_send_fields_in_send_ty,
    reason = "the arena's references never leave the cell; see the SAFETY note"
)]
// SAFETY: the arena and every reference into it (the compiled policy and its cursors) live in
// this one cell and move together; nothing borrowed from the arena escapes it. junco keeps no
// thread-local or global state tied to an arena, so moving the cell is moving its `Bump`, which
// is `Send`. Callers serialize all access behind a mutex.
unsafe impl Send for Engine {}

impl<'arena> Decider<'arena> {
    fn new(
        arena: &'arena Bump,
        agent: &AgentPolicy,
        candidates: Vec<Event>,
    ) -> Result<Self, junco_policy::CompileError> {
        let mut policy = TracePolicy::new(arena, agent.trace_rules())?;
        let base: Vec<_> = candidates.iter().map(|event| policy.cursor(event)).collect();
        Ok(Self {
            policy,
            candidates,
            committed: Vec::new(),
            cursors: base.clone(),
            base,
            own: Vec::new(),
        })
    }

    /// Brings `base` to the committed history `labelled`, extending it when it only grew.
    fn commit(&mut self, labelled: Vec<Event>) {
        if !labelled.starts_with(&self.committed) {
            self.committed.clear();
            self.base = self
                .candidates
                .iter()
                .map(|event| self.policy.cursor(event))
                .collect();
        }
        for event in &labelled[self.committed.len()..] {
            for cursor in &mut self.base {
                self.policy.advance(cursor, event);
            }
        }
        self.committed = labelled;
        let own = std::mem::take(&mut self.own);
        self.restart(own);
    }

    /// Replaces the own observations: the cursors restart from `base`.
    fn restart(&mut self, own: Vec<Event>) {
        self.cursors = self.base.clone();
        self.own.clear();
        self.extend(own);
    }

    fn extend(&mut self, events: Vec<Event>) {
        for event in &events {
            for cursor in &mut self.cursors {
                self.policy.advance(cursor, event);
            }
        }
        self.own.extend(events);
    }

    /// Each class's refusal after the own observations and then `suffix`.
    fn decide(&mut self, suffix: &[Event]) -> Vec<Option<(String, Vec<String>)>> {
        (0..self.cursors.len())
            .map(|class| {
                let cursor = self.after(class, suffix);
                match self.policy.verdict(&cursor) {
                    PolicyDecision::Grant => None,
                    PolicyDecision::Deny {
                        failed_precondition,
                        allowed_fixes,
                    } => Some((failed_precondition, allowed_fixes)),
                }
            })
            .collect()
    }

    /// Whether one more contact, of any class, could change a `guarded` class's decision state.
    fn contact_sensitive(&mut self, suffix: &[Event], guarded: u64) -> bool {
        (0..self.cursors.len())
            .filter(|class| guarded & (1 << class) != 0)
            .any(|class| {
                let cursor = self.after(class, suffix);
                (0..self.candidates.len()).any(|contact| {
                    let mut next = cursor.clone();
                    self.policy.advance(&mut next, &self.candidates[contact]);
                    next != cursor
                })
            })
    }

    fn after(&mut self, class: usize, suffix: &[Event]) -> TraceCursor<'arena> {
        let mut cursor = self.cursors[class].clone();
        for event in suffix {
            self.policy.advance(&mut cursor, event);
        }
        cursor
    }
}

/// The command's own observations, in the order and form [`agent::observations`] gives them.
#[derive(Default)]
struct OwnLog {
    run: Option<TraceRun>,
    /// How many of the command's effects were consumed.
    effects: usize,
    /// The latest entry order of an effect that recorded something.
    order: u64,
    observations: Vec<Observation>,
    /// Every path a consumed write named, and whether it existed in the work view when that
    /// write's observation was classified.
    written: HashMap<PathBuf, bool>,
}

/// What [`OwnLog::update`] did to the observations.
enum Update {
    /// Observations from this index on are new; earlier ones are unchanged.
    Appended(usize),
    /// Every observation may have changed.
    Rebuilt,
}

/// The command's evidence, as the answers need it.
pub(super) struct Command<'a> {
    pub run: TraceRun,
    pub effects: &'a [(u64, Option<InvocationId>, Effects)],
    pub baseline: &'a Path,
}

impl OwnLog {
    fn update(
        &mut self,
        command: &Command<'_>,
        work: &Path,
        agent: &AgentPolicy,
        principal: &Principal,
    ) -> Update {
        let exists = |path: &Path| work.join(path).symlink_metadata().is_ok();
        let classify = |path: &Path| policy::change(command.baseline, work, path);
        let Some(new) = command
            .effects
            .get(self.effects..)
            .filter(|_| self.run == Some(command.run))
        else {
            return self.rebuild(command, work, agent, principal);
        };
        let first = self.observations.len();
        for entry in new {
            let (order, _, effect) = entry;
            if agent::only_repeats(Some(agent), effect, None) {
                continue;
            }
            let reclassified = effect.writes.iter().any(|path| {
                self.written
                    .get(path)
                    .is_some_and(|existed| *existed != exists(path))
            }) || effect.recursive_writes.iter().any(|directory| {
                self.written
                    .iter()
                    .any(|(path, existed)| path.starts_with(directory) && *existed != exists(path))
            });
            if *order < self.order || reclassified {
                return self.rebuild(command, work, agent, principal);
            }
            let mut observed = agent::observations(
                std::slice::from_ref(entry),
                principal,
                Some(agent),
                classify,
                |_| Vec::new(),
            );
            if observed.first().map(|observation| &observation.observed)
                == self.observations.last().map(|observation| &observation.observed)
            {
                observed.remove(0);
            }
            self.observations.extend(observed);
            for path in &effect.writes {
                self.written.insert(path.clone(), exists(path));
            }
            self.order = *order;
        }
        self.effects = command.effects.len();
        Update::Appended(first)
    }

    fn rebuild(
        &mut self,
        command: &Command<'_>,
        work: &Path,
        agent: &AgentPolicy,
        principal: &Principal,
    ) -> Update {
        self.run = Some(command.run);
        self.effects = command.effects.len();
        self.observations = agent::observations(
            command.effects,
            principal,
            Some(agent),
            |path| policy::change(command.baseline, work, path),
            |_| Vec::new(),
        );
        self.written = command
            .effects
            .iter()
            .flat_map(|(_, _, effect)| &effect.writes)
            .map(|path| (path.clone(), work.join(path).symlink_metadata().is_ok()))
            .collect();
        self.order = command
            .effects
            .iter()
            .filter(|(_, _, effect)| !agent::only_repeats(Some(agent), effect, None))
            .map(|(order, _, _)| *order)
            .max()
            .unwrap_or_default();
        Update::Rebuilt
    }
}

/// What a root's answers are decided from, beyond its own command.
pub(super) struct Inputs<'a> {
    pub agent: &'a AgentPolicy,
    pub principal: &'a Principal,
    pub work: &'a Path,
    /// The policy validator's revision and committed observations.
    pub history: (u64, &'a [Observation]),
    /// The taint table's revision, which covers `received` and `carried`.
    pub taint: u64,
    /// Taint other principals handed this one, appended after its own observations.
    pub received: &'a [Taint],
    /// Classes this root's sends carry: those it holds, and a copied source's.
    pub carried: &'a [Taint],
    /// Observations of a file the stopped call copies into a socket in-kernel.
    pub source: &'a [Observation],
}

/// One root's kernel answers and the incremental state they are decided from.
pub(super) struct KernelAnswers {
    /// `Err` when the policy does not compile: every class a rule could judge is refused.
    engine: Result<Engine, String>,
    classes: Vec<String>,
    history: Option<u64>,
    own: OwnLog,
    /// The taint revision and receipts the published answers were decided with.
    folded: Option<(u64, [u32; MAX_TAINTS])>,
    /// Kernel taint receipts of this root already folded into its taint, per class ID.
    acknowledged: [u32; MAX_TAINTS],
    published: Option<KernelPolicy>,
    /// The latest base refusal of each class, for reporting the kernel's denials.
    denials: Vec<Option<Denial>>,
}

impl KernelAnswers {
    pub fn new(agent: &AgentPolicy, principal: &Principal) -> io::Result<Self> {
        let classes: Vec<String> = agent
            .kernel_classes()
            .into_iter()
            .map(str::to_owned)
            .collect();
        if classes.len() > MAX_CLASSES {
            return Err(io::Error::other(format!(
                "agent policy exceeds the {MAX_CLASSES} kernel peer classes"
            )));
        }
        for class in agent.propagated() {
            taint_id(class)?;
        }
        let candidates = classes
            .iter()
            .map(|class| contact(principal, class))
            .collect();
        let engine = Engine::try_new(Bump::new(), |arena| {
            Decider::new(arena, agent, candidates)
        })
        .map_err(|error| agent::uncompiled(&contact(principal, "unknown"), &error).failed_precondition);
        Ok(Self {
            engine,
            classes,
            history: None,
            own: OwnLog::default(),
            folded: None,
            acknowledged: [0; MAX_TAINTS],
            published: None,
            denials: Vec::new(),
        })
    }

    /// The kernel peer class names, by class ID.
    pub fn classes(&self) -> &[String] {
        &self.classes
    }

    /// The latest base refusal of class `class`, if any.
    pub fn denial(&self, class: usize) -> Option<&Denial> {
        self.denials.get(class)?.as_ref()
    }

    /// Notes one kernel receipt of taint class `id`, now in the taint table.
    pub fn acknowledge(&mut self, id: usize) {
        if let Some(count) = self.acknowledged.get_mut(id) {
            *count = count.wrapping_add(1);
        }
    }

    /// Decides every class for the command's current state. The generation advances only when
    /// the answers changed.
    #[expect(
        clippy::too_many_lines,
        reason = "one decision pass over every class, its contingent answers and its sensitivity"
    )]
    pub fn refresh(
        &mut self,
        inputs: &Inputs<'_>,
        command: &Command<'_>,
    ) -> io::Result<KernelPolicy> {
        let Inputs {
            agent, principal, ..
        } = *inputs;
        let update = self.own.update(command, inputs.work, agent, principal);
        let folded = Some((inputs.taint, self.acknowledged));
        // Most calls record nothing new: the answers cannot have changed.
        if let (Update::Appended(first), Some(published)) = (&update, &self.published)
            && *first == self.own.observations.len()
            && self.history == Some(inputs.history.0)
            && self.folded == folded
            && inputs.source.is_empty()
        {
            return Ok(published.clone());
        }
        let label = |observations: &[Observation]| -> Vec<Event> {
            observations
                .iter()
                .filter_map(|observation| agent.label(observation))
                .collect()
        };
        let received: Vec<Observation> = inputs
            .received
            .iter()
            .map(|taint| Observation {
                principal: principal.clone(),
                observed: Observed::Taint(taint.clone()),
            })
            .collect();
        let mut suffix = label(&received);
        suffix.extend(label(inputs.source));
        let carried: Vec<&str> = inputs
            .carried
            .iter()
            .map(|taint| taint.class.as_str())
            .collect();
        let taints = taint_classes();
        let guarded = self.guarded(agent, principal);
        let untraced = if agent.denies_untraced_local() {
            (1 << UNIX) | (1 << LOOPBACK)
        } else {
            0
        };
        let mut answers = Answers {
            guarded: guarded | untraced,
            untraced,
            audit: u32::from(agent.audits()),
            acknowledged: self.acknowledged,
            ..Answers::default()
        };
        for class in &carried {
            answers.carried |= 1 << taint_id(class)?;
        }
        let mut contact_sensitive = false;
        let decisions = match &mut self.engine {
            Err(uncompiled) => (0..self.classes.len())
                .map(|class| {
                    (guarded & (1 << class) != 0).then(|| (uncompiled.clone(), Vec::new()))
                })
                .collect(),
            Ok(engine) => {
                let (history, own) = (&mut self.history, &self.own);
                engine.with_dependent_mut(|_, decider| {
                    if *history != Some(inputs.history.0) {
                        decider.commit(label(inputs.history.1));
                        *history = Some(inputs.history.0);
                    }
                    match update {
                        Update::Appended(first) => {
                            decider.extend(label(&own.observations[first..]));
                        }
                        Update::Rebuilt => decider.restart(label(&own.observations)),
                    }
                    for (id, class) in taints.iter().enumerate() {
                        if carried.contains(&class.as_str()) {
                            continue;
                        }
                        let mut contingent = suffix.clone();
                        contingent.extend(agent.label(&Observation {
                            principal: principal.clone(),
                            observed: Observed::Taint(Taint {
                                class: class.clone(),
                                from: Principal::new("kernel receipt"),
                                via: "a local socket".into(),
                            }),
                        }));
                        answers.contingent[id] = mask(&decider.decide(&contingent));
                    }
                    contact_sensitive = decider.contact_sensitive(&suffix, guarded);
                    decider.decide(&suffix)
                })
            }
        };
        answers.deny = mask(&decisions);
        for id in 0..MAX_TAINTS {
            // Receiving a class already carried, or one no policy propagates, changes nothing.
            if answers.carried & (1 << id) != 0 || id >= taints.len() {
                answers.contingent[id] = answers.deny;
            }
        }
        self.denials = decisions
            .into_iter()
            .zip(&self.classes)
            .map(|(decision, class)| {
                decision.map(|(failed_precondition, allowed_fixes)| Denial {
                    event: contact(principal, class),
                    failed_precondition,
                    allowed_fixes,
                })
            })
            .collect();
        let mut policy = KernelPolicy {
            answers,
            networks: agent.kernel_networks()?,
            contact_sensitive,
        };
        let previous = self
            .published
            .as_ref()
            .map(|previous| previous.answers.generation);
        policy.answers.generation = previous.unwrap_or_default();
        if self.published.as_ref() != Some(&policy) {
            policy.answers.generation += 1;
            self.published = Some(policy.clone());
        }
        // A copied source's answers hold for one call: never reuse them.
        self.folded = inputs.source.is_empty().then_some(folded).flatten();
        Ok(policy)
    }

    /// Classes some rule's head can match: refused whenever the answers are undecidable.
    fn guarded(&self, agent: &AgentPolicy, principal: &Principal) -> u64 {
        self.classes
            .iter()
            .enumerate()
            .filter(|(_, class)| agent.judges_event(&contact(principal, class)))
            .fold(0, |mask, (id, _)| mask | 1 << id)
    }
}

fn contact(principal: &Principal, class: &str) -> Event {
    Event::new(principal.clone(), Action::Edit, ["net", class])
}

fn mask(decisions: &[Option<(String, Vec<String>)>]) -> u64 {
    decisions
        .iter()
        .enumerate()
        .filter(|(_, decision)| decision.is_some())
        .fold(0, |mask, (id, _)| mask | 1 << id)
}
