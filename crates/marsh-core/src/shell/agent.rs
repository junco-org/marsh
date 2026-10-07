//! Agent sandbox policies: past-time rules over what an agent's managed commands were observed
//! doing, decided in the same atomic batch as the Git capability policy.
//!
//! The Git policy speaks about exact source paths. An agent policy speaks about *classes*: a
//! shell's [`AgentPolicy`] labels every observation it is shown — a path read or written inside or
//! outside the source, a program executed, a socket peer contacted, an operator approval — and its
//! [`AgentRule`]s are junco-policy past-regexes over that labelled stream. Each source keeps one
//! raw, unlabelled observation log shared by all of its shells, so each shell's classifier and
//! rules judge the same history, including other principals' events.
//!
//! The labelled vocabulary is a junco [`Event`] whose resource is a two-segment pair:
//!
//! | Observation | Action | Resource |
//! | --- | --- | --- |
//! | read of an existing path | `Read` | `["path", class]` |
//! | modification of an existing path | `Edit` | `["path", class]` |
//! | creation of a path | `Edit` | `["new", class]` |
//! | removal of a path | `Delete` | `["path", class]` |
//! | program executed | `Read` | `["exec", basename]` |
//! | socket peer contacted | `Edit` | `["net", class]` |
//! | operator approval | `Stage` | `["approve", label]` |
//!
//! Enforcement happens when a managed command's effects are published, so it governs what reaches
//! the source and what later commands may do. A denied `connect` has already been attempted, and
//! writes outside the source are not rolled back; the denial refuses the command's source effects
//! and keeps them out of history. Observations are kept in memory for the source's lifetime and
//! are not recovered from the WAL.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use glob::{MatchOptions, Pattern};
use junco_policy::{
    AtomPattern, Bump, Comparison, ComponentPattern, Head, PolicyDecision, RegexExpr, Rule,
    RuleMode, TestExpr, TracePolicy, TraceRule,
};

use super::policy::{Action, Denial, Event, PolicyObserver, Principal, Resource};

const PRINCIPAL: &str = "p";
const MATCH: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

/// The socket peer a `connect`, `sendto` or `sendmsg` named.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Peer {
    /// A Unix-domain socket: its path, or an abstract name beginning with NUL.
    Unix(Vec<u8>),
    /// An IPv4 or IPv6 endpoint.
    Ip(SocketAddr),
    /// A family this module does not decode, or an address that could not be read.
    Unknown,
}
impl Peer {
    /// Decodes a native `sockaddr`.
    pub(super) fn decode(bytes: &[u8]) -> Self {
        let Some(family) = bytes.get(..2).and_then(|family| family.try_into().ok()) else {
            return Self::Unknown;
        };
        let port = |bytes: &[u8]| {
            bytes
                .get(2..4)
                .and_then(|port| port.try_into().ok())
                .map(u16::from_be_bytes)
        };
        match i32::from(u16::from_ne_bytes(family)) {
            libc::AF_UNIX => {
                let path = bytes.get(2..).unwrap_or_default();
                let end = if path.first() == Some(&0) {
                    path.len()
                } else {
                    path.iter()
                        .position(|&byte| byte == 0)
                        .unwrap_or(path.len())
                };
                Self::Unix(path.get(..end).unwrap_or_default().to_vec())
            }
            libc::AF_INET => {
                let address = bytes
                    .get(4..8)
                    .and_then(|octets| <[u8; 4]>::try_from(octets).ok());
                match (port(bytes), address) {
                    (Some(port), Some(octets)) => {
                        Self::Ip(SocketAddrV4::new(Ipv4Addr::from(octets), port).into())
                    }
                    _ => Self::Unknown,
                }
            }
            libc::AF_INET6 => {
                let address = bytes
                    .get(8..24)
                    .and_then(|octets| <[u8; 16]>::try_from(octets).ok());
                match (port(bytes), address) {
                    (Some(port), Some(octets)) => {
                        Self::Ip(SocketAddrV6::new(Ipv6Addr::from(octets), port, 0, 0).into())
                    }
                    _ => Self::Unknown,
                }
            }
            _ => Self::Unknown,
        }
    }
}

/// What one observation did to a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum PathAction {
    Read,
    Create,
    Edit,
    Delete,
}

/// One unlabelled observation in a source's shared log.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Observed {
    /// A source-relative path, or (when `inside` is false) an absolute path outside the source.
    Path {
        action: PathAction,
        path: PathBuf,
        inside: bool,
    },
    /// A program a successful exec ran.
    Exec(PathBuf),
    /// A socket peer a command contacted.
    Connect(Peer),
    /// An operator's approval of a label.
    Approve(String),
}

/// One principal's observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Observation {
    pub principal: Principal,
    pub observed: Observed,
}

/// A malformed class pattern or network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPolicyError(String);
impl fmt::Display for AgentPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for AgentPolicyError {}

/// An IPv4 or IPv6 network in CIDR form.
#[derive(Clone, Debug)]
struct Network {
    address: IpAddr,
    prefix: u32,
}
impl Network {
    fn parse(text: &str) -> Result<Self, AgentPolicyError> {
        let invalid = || AgentPolicyError(format!("invalid network {text:?}"));
        let (address, prefix) = text
            .split_once('/')
            .map_or((text, None), |(a, p)| (a, Some(p)));
        let address: IpAddr = address.parse().map_err(|_| invalid())?;
        let width = if address.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.map_or(Ok(width), |p| p.parse().map_err(|_| invalid()))?;
        if prefix > width {
            return Err(invalid());
        }
        Ok(Self { address, prefix })
    }
    fn contains(&self, address: IpAddr) -> bool {
        let mask = |bits: u32, width: u32| {
            u128::MAX
                .checked_shl(width - bits)
                .unwrap_or(0)
                .checked_shl(128 - width)
                .unwrap_or(0)
        };
        let widen = |address: IpAddr| match address {
            IpAddr::V4(address) => (u128::from(u32::from(address)) << 96, 32),
            IpAddr::V6(address) => (u128::from(address), 128),
        };
        let address = match address {
            IpAddr::V6(v6) if self.address.is_ipv4() => {
                v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4)
            }
            address => address,
        };
        let ((network, width), (candidate, other)) = (widen(self.address), widen(address));
        width == other && network & mask(self.prefix, width) == candidate & mask(self.prefix, width)
    }
}

/// A shell's agent policy: how observations are labelled and which rules judge them.
///
/// Path classes are first-match globs. Relative patterns label source-relative paths; an absolute
/// pattern, or one beginning with `~/` (the policy owner's `$HOME`), labels paths outside the
/// source. A source path no pattern labels has class `other`; an outside path no pattern labels is
/// not recorded. Socket peers are `unix`, `loopback`, the first matching declared network's label,
/// or `external`; an unreadable peer is `unknown`.
#[derive(Clone, Default)]
pub struct AgentPolicy {
    inside: Vec<(Pattern, String)>,
    outside: Vec<(Pattern, String)>,
    networks: Vec<(Network, String)>,
    rules: Vec<AgentRule>,
    audit: bool,
}
impl fmt::Debug for AgentPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentPolicy")
            .field("inside", &self.inside)
            .field("outside", &self.outside)
            .field("networks", &self.networks)
            .field("rules", &self.rules.len())
            .field("audit", &self.audit)
            .finish()
    }
}
impl AgentPolicy {
    /// An empty policy: labels nothing but `other`, `exec` and network classes, and denies nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A conservative starting point for coding agents.
    ///
    /// Classes: `secret` (dotenv files, keys, credential files, `~/.ssh`, `~/.aws`, ...), `ci`,
    /// `vcs-hooks`, `lockfile`, `tests`, and `src` for every other source path. Rules: no external
    /// or unknown network peer after reading a secret; approval before changing `secret`, `ci` or
    /// `vcs-hooks`; tests are locked once the same principal changed `src`; no `ci`, `lockfile` or
    /// `vcs-hooks` change after running a downloader; at most 100 deletions between approvals.
    ///
    /// # Errors
    /// Never for the built-in table; the signature matches [`AgentPolicy::class`].
    pub fn recommended() -> Result<Self, AgentPolicyError> {
        let mut policy = Self::new();
        for pattern in [
            "**/.env",
            "**/.env.*",
            "**/*.pem",
            "**/*.key",
            "**/id_rsa*",
            "**/id_ed25519*",
            "**/.npmrc",
            "**/.pypirc",
            "**/.netrc",
            "**/credentials",
            "**/credentials.json",
            "~/.ssh/**",
            "~/.aws/**",
            "~/.config/gcloud/**",
            "~/.azure/**",
            "~/.kube/**",
            "~/.docker/config.json",
            "~/.git-credentials",
            "~/.netrc",
            "~/.npmrc",
            "~/.pypirc",
            "~/.config/gh/hosts.yml",
            "/etc/shadow",
        ] {
            policy = policy.class(pattern, "secret")?;
        }
        for pattern in [
            ".github/**",
            ".gitlab-ci.yml",
            ".circleci/**",
            ".buildkite/**",
            "Jenkinsfile",
            "azure-pipelines.yml",
        ] {
            policy = policy.class(pattern, "ci")?;
        }
        for pattern in [".husky/**", ".pre-commit-config.yaml", ".githooks/**"] {
            policy = policy.class(pattern, "vcs-hooks")?;
        }
        for pattern in [
            "**/Cargo.lock",
            "**/package-lock.json",
            "**/npm-shrinkwrap.json",
            "**/yarn.lock",
            "**/pnpm-lock.yaml",
            "**/poetry.lock",
            "**/uv.lock",
            "**/Pipfile.lock",
            "**/go.sum",
            "**/Gemfile.lock",
            "**/composer.lock",
        ] {
            policy = policy.class(pattern, "lockfile")?;
        }
        for pattern in [
            "test/**",
            "tests/**",
            "**/test/**",
            "**/tests/**",
            "**/__tests__/**",
            "**/*_test.*",
            "**/test_*.py",
            "**/*.test.*",
            "**/*.spec.*",
        ] {
            policy = policy.class(pattern, "tests")?;
        }
        policy = policy.class("**", "src")?;
        Ok(policy
            .rule(AgentRule::no_egress_after_read("secret"))
            .rule(AgentRule::requires_approval("secret"))
            .rule(AgentRule::requires_approval("ci"))
            .rule(AgentRule::requires_approval("vcs-hooks"))
            .rule(AgentRule::locked_after("src", "tests"))
            .rule(AgentRule::tainted_by_exec(
                &["curl", "wget"],
                &["ci", "lockfile", "vcs-hooks"],
            ))
            .rule(AgentRule::deletion_budget(100)))
    }

    /// Appends a first-match path class.
    ///
    /// # Errors
    /// When `pattern` is not a valid glob, or `~/` is used without `$HOME`.
    pub fn class(mut self, pattern: &str, label: &str) -> Result<Self, AgentPolicyError> {
        let invalid = |error: glob::PatternError| {
            AgentPolicyError(format!("invalid class pattern {pattern:?}: {error}"))
        };
        if let Some(rest) = pattern.strip_prefix("~/") {
            let home = std::env::var("HOME")
                .map_err(|_| AgentPolicyError(format!("{pattern:?} needs a UTF-8 $HOME")))?;
            let home = Pattern::escape(home.trim_end_matches('/'));
            let compiled = Pattern::new(&format!("{home}/{rest}")).map_err(invalid)?;
            self.outside.push((compiled, label.to_owned()));
        } else if pattern.starts_with('/') {
            self.outside
                .push((Pattern::new(pattern).map_err(invalid)?, label.to_owned()));
        } else {
            self.inside
                .push((Pattern::new(pattern).map_err(invalid)?, label.to_owned()));
        }
        Ok(self)
    }

    /// Labels socket peers inside `cidr` (for example `10.0.0.0/8` or `2001:db8::/32`).
    ///
    /// # Errors
    /// When `cidr` is not an address with an optional in-range prefix length.
    pub fn network(mut self, cidr: &str, label: &str) -> Result<Self, AgentPolicyError> {
        self.networks
            .push((Network::parse(cidr)?, label.to_owned()));
        Ok(self)
    }

    /// Reports every decision to the shell's [`PolicyObserver`] but never denies: a would-be
    /// denial is recorded as granted. For measuring a policy's false denials on unmodified runs.
    #[must_use]
    pub const fn audit(mut self) -> Self {
        self.audit = true;
        self
    }

    /// Appends a rule or a constructor's rule set. Rules apply in order and the first violation
    /// is reported.
    #[must_use]
    pub fn rule(mut self, rules: impl Into<AgentRuleSet>) -> Self {
        self.rules.extend(rules.into());
        self
    }

    /// The class of a source-relative path.
    fn inside_class(&self, path: &Path) -> &str {
        self.inside
            .iter()
            .find(|(pattern, _)| pattern.matches_path_with(path, MATCH))
            .map_or("other", |(_, label)| label)
    }

    /// The class of an absolute path outside the source, if any pattern labels it.
    fn outside_class(&self, path: &Path) -> Option<&str> {
        self.outside
            .iter()
            .find(|(pattern, _)| pattern.matches_path_with(path, MATCH))
            .map(|(_, label)| label.as_str())
    }

    fn peer_class(&self, peer: &Peer) -> &str {
        match peer {
            Peer::Unix(_) => "unix",
            Peer::Unknown => "unknown",
            Peer::Ip(address) => {
                let ip = address.ip();
                let ip = match ip {
                    IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
                    IpAddr::V4(_) => ip,
                };
                if ip.is_loopback() {
                    return "loopback";
                }
                self.networks
                    .iter()
                    .find(|(network, _)| network.contains(ip))
                    .map_or("external", |(_, label)| label)
            }
        }
    }

    /// Whether a command checked by `policy` records `observed` in the shared log. Paths outside
    /// the source are recorded only when the acting shell's policy labels them.
    pub(super) fn records(policy: Option<&Self>, observed: &Observed) -> bool {
        match observed {
            Observed::Path {
                inside: false,
                path,
                ..
            } => policy.is_some_and(|policy| policy.outside_class(path).is_some()),
            _ => true,
        }
    }

    /// The labelled event for one observation, or `None` when this policy does not label it.
    pub(super) fn label(&self, observation: &Observation) -> Option<Event> {
        let (action, resource) = match &observation.observed {
            Observed::Path {
                action,
                path,
                inside,
            } => {
                let class = if *inside {
                    self.inside_class(path)
                } else {
                    self.outside_class(path)?
                };
                match action {
                    PathAction::Read => (Action::Read, ["path", class]),
                    PathAction::Create => (Action::Edit, ["new", class]),
                    PathAction::Edit => (Action::Edit, ["path", class]),
                    PathAction::Delete => (Action::Delete, ["path", class]),
                }
            }
            Observed::Exec(program) => {
                let name = program
                    .file_name()
                    .map_or_else(|| program.as_os_str().as_bytes(), std::ffi::OsStr::as_bytes);
                let name = String::from_utf8_lossy(name);
                return Some(Event::new(
                    observation.principal.clone(),
                    Action::Read,
                    Resource::new(["exec", name.as_ref()]),
                ));
            }
            Observed::Connect(peer) => (Action::Edit, ["net", self.peer_class(peer)]),
            Observed::Approve(label) => (Action::Stage, ["approve", label.as_str()]),
        };
        Some(Event::new(
            observation.principal.clone(),
            action,
            Resource::new(resource),
        ))
    }

    /// Checks `batch` in order against the labelled `history`, as one atomic decision. Every
    /// decision is reported to `observer`; on any denial, every denial is returned, unless this
    /// policy only audits.
    pub(super) fn check(
        &self,
        history: &[Observation],
        batch: &[Observation],
        observer: Option<&PolicyObserver>,
    ) -> Result<(), Vec<Denial>> {
        if self.rules.is_empty() {
            return Ok(());
        }
        let arena = Bump::new();
        let mut policy = TracePolicy::new(&arena, self.rules.iter().map(|rule| rule.0.clone()))
            .map_err(|error| {
                vec![Denial {
                    event: Event::new("agent-policy", Action::Read, Resource::new(["rules"])),
                    failed_precondition: format!("agent policy does not compile: {error}"),
                    allowed_fixes: Vec::new(),
                }]
            })?;
        let mut labelled: Vec<Event> = history.iter().filter_map(|o| self.label(o)).collect();
        let mut denials = Vec::new();
        for event in batch.iter().filter_map(|o| self.label(o)) {
            let decision = policy.decide(&labelled, &event);
            if let Some(observer) = observer {
                observer.notify(&event, &decision);
            }
            match decision {
                PolicyDecision::Grant => labelled.push(event),
                PolicyDecision::Deny { .. } if self.audit => labelled.push(event),
                PolicyDecision::Deny {
                    failed_precondition,
                    allowed_fixes,
                } => denials.push(Denial {
                    event,
                    failed_precondition,
                    allowed_fixes,
                }),
            }
        }
        if denials.is_empty() {
            Ok(())
        } else {
            Err(denials)
        }
    }
}

/// One rule of an [`AgentPolicy`]: a junco-policy past-regex over labelled observations.
#[derive(Clone)]
pub struct AgentRule(TraceRule);
impl fmt::Debug for AgentRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AgentRule").field(&self.0.rule).finish()
    }
}

fn principal_is(comparison: Comparison) -> TestExpr {
    TestExpr::atom(
        comparison,
        AtomPattern::Principal(ComponentPattern::variable(PRINCIPAL)),
    )
}
fn action_is(action: Action, comparison: Comparison) -> TestExpr {
    TestExpr::atom(
        comparison,
        AtomPattern::Action(ComponentPattern::constant(action)),
    )
}
fn resource_is(kind: &str, class: &str, comparison: Comparison) -> TestExpr {
    TestExpr::atom(
        comparison,
        AtomPattern::Resource(ComponentPattern::constant(Resource::new([kind, class]))),
    )
}
/// The acting principal changed a path of `class`: modified, created or removed it.
fn changed_by_p(class: &str) -> TestExpr {
    TestExpr::and([
        principal_is(Comparison::Eq),
        TestExpr::or([
            TestExpr::and([
                action_is(Action::Edit, Comparison::Eq),
                TestExpr::or([
                    resource_is("path", class, Comparison::Eq),
                    resource_is("new", class, Comparison::Eq),
                ]),
            ]),
            TestExpr::and([
                action_is(Action::Delete, Comparison::Eq),
                resource_is("path", class, Comparison::Eq),
            ]),
        ]),
    ])
}
/// Any event that is not an approval of `label`.
fn not_approval(label: &str) -> TestExpr {
    TestExpr::or([
        action_is(Action::Stage, Comparison::Neq),
        resource_is("approve", label, Comparison::Neq),
    ])
}
/// Histories with no approval of `label` after their last `trigger`.
fn since_unapproved(trigger: TestExpr, label: &str) -> RegexExpr {
    RegexExpr::concat([
        RegexExpr::all(),
        RegexExpr::test(trigger),
        RegexExpr::star(RegexExpr::test(not_approval(label))),
    ])
}
/// Histories that contain `event` somewhere.
fn ever(event: TestExpr) -> RegexExpr {
    RegexExpr::concat([RegexExpr::all(), RegexExpr::test(event), RegexExpr::all()])
}
/// Forbid rules with an action-constant, resource-constant head over the acting principal.
fn forbid(action: Action, resource: [&str; 2], tail: RegexExpr) -> Rule {
    Rule::new(
        RuleMode::Forbid,
        Head::new(
            ComponentPattern::variable(PRINCIPAL),
            ComponentPattern::constant(action),
            ComponentPattern::constant(Resource::new(resource)),
        ),
        tail,
    )
}
/// The three heads that change a path of `class`.
const fn change_heads(class: &str) -> [(Action, [&str; 2]); 3] {
    [
        (Action::Edit, ["path", class]),
        (Action::Edit, ["new", class]),
        (Action::Delete, ["path", class]),
    ]
}

impl AgentRule {
    /// A caller-built junco-policy rule over the labelled vocabulary in the module docs.
    #[must_use]
    pub const fn custom(rule: TraceRule) -> Self {
        Self(rule)
    }

    fn many(rules: Vec<TraceRule>) -> Vec<Self> {
        rules.into_iter().map(Self).collect()
    }

    /// No `external` or `unknown` socket peer once the same principal has read a path of
    /// `secret_class`.
    #[must_use]
    pub fn no_egress_after_read(secret_class: &str) -> AgentRuleSet {
        let read = TestExpr::and([
            principal_is(Comparison::Eq),
            action_is(Action::Read, Comparison::Eq),
            resource_is("path", secret_class, Comparison::Eq),
        ]);
        let class = secret_class.to_owned();
        AgentRuleSet(Self::many(
            ["external", "unknown"]
                .into_iter()
                .map(|peer| {
                    let class = class.clone();
                    TraceRule::new(
                        forbid(Action::Edit, ["net", peer], ever(read.clone())),
                        move |event| {
                            (
                                format!(
                                    "{} contacted an {peer} network peer after reading {class} data",
                                    event.principal
                                ),
                                vec![format!("contact only loopback or declared networks after reading {class}")],
                            )
                        },
                    )
                })
                .collect(),
        ))
    }

    /// No socket peer of `peer_class` at all (`external`, `unknown`, `unix`, `loopback`, or a
    /// declared network label).
    #[must_use]
    pub fn no_network(peer_class: &str) -> AgentRuleSet {
        let class = peer_class.to_owned();
        AgentRuleSet(vec![Self(TraceRule::new(
            forbid(Action::Edit, ["net", peer_class], RegexExpr::all()),
            move |event| {
                (
                    format!(
                        "{} contacted a forbidden {class} network peer",
                        event.principal
                    ),
                    Vec::new(),
                )
            },
        ))])
    }

    /// Changing (modifying, creating or removing) a path of `class` requires an earlier operator
    /// approval of `class`.
    #[must_use]
    pub fn requires_approval(class: &str) -> AgentRuleSet {
        let approval = TestExpr::and([
            action_is(Action::Stage, Comparison::Eq),
            resource_is("approve", class, Comparison::Eq),
        ]);
        let unapproved = RegexExpr::complement(ever(approval));
        AgentRuleSet(Self::many(
            change_heads(class)
                .into_iter()
                .map(|(action, resource)| {
                    let class = class.to_owned();
                    TraceRule::new(forbid(action, resource, unapproved.clone()), move |event| {
                        (
                            format!(
                                "{} may not change {class} paths without approval",
                                event.principal
                            ),
                            vec![format!("an operator approves {class}")],
                        )
                    })
                })
                .collect(),
        ))
    }

    /// Once a principal has changed a path of `edited`, it may not change a path of `locked`
    /// until an operator approves `locked`. With `("src", "tests")`: no editing tests to make
    /// changed code pass.
    #[must_use]
    pub fn locked_after(edited: &str, locked: &str) -> AgentRuleSet {
        let tail = since_unapproved(changed_by_p(edited), locked);
        AgentRuleSet(Self::many(
            change_heads(locked)
                .into_iter()
                .map(|(action, resource)| {
                    let (edited, locked) = (edited.to_owned(), locked.to_owned());
                    TraceRule::new(forbid(action, resource, tail.clone()), move |event| {
                        (
                            format!(
                                "{} changed {edited} paths, so it may not also change {locked} paths",
                                event.principal
                            ),
                            vec![format!("an operator approves {locked}")],
                        )
                    })
                })
                .collect(),
        ))
    }

    /// Once a principal ran any of `programs`, it may not change paths of `protected` classes
    /// until an operator approves that class.
    #[must_use]
    pub fn tainted_by_exec(programs: &[&str], protected: &[&str]) -> AgentRuleSet {
        let ran = TestExpr::and([
            principal_is(Comparison::Eq),
            action_is(Action::Read, Comparison::Eq),
            TestExpr::or(
                programs
                    .iter()
                    .map(|program| resource_is("exec", program, Comparison::Eq)),
            ),
        ]);
        let names = programs.join(", ");
        let mut rules = Vec::new();
        for class in protected {
            let tail = since_unapproved(ran.clone(), class);
            for (action, resource) in change_heads(class) {
                let (class, names) = ((*class).to_owned(), names.clone());
                rules.push(TraceRule::new(
                    forbid(action, resource, tail.clone()),
                    move |event| {
                        (
                            format!(
                                "{} ran {names} and may not then change {class} paths",
                                event.principal
                            ),
                            vec![format!("an operator approves {class}")],
                        )
                    },
                ));
            }
        }
        AgentRuleSet(Self::many(rules))
    }

    /// Paths of `class` may be created but never modified or removed.
    #[must_use]
    pub fn append_only(class: &str) -> AgentRuleSet {
        AgentRuleSet(Self::many(
            [Action::Edit, Action::Delete]
                .into_iter()
                .map(|action| {
                    let class = class.to_owned();
                    TraceRule::new(
                        forbid(action, ["path", &class], RegexExpr::all()),
                        move |event| {
                            (
                                format!(
                                    "{class} paths are append-only; {} may only create them",
                                    event.principal
                                ),
                                Vec::new(),
                            )
                        },
                    )
                })
                .collect(),
        ))
    }

    /// A principal may remove at most `limit` paths between operator approvals of `delete`.
    #[must_use]
    pub fn deletion_budget(limit: usize) -> AgentRuleSet {
        let deletion = TestExpr::and([
            principal_is(Comparison::Eq),
            action_is(Action::Delete, Comparison::Eq),
        ]);
        let quiet = RegexExpr::star(RegexExpr::test(not_approval("delete")));
        let mut tail = vec![RegexExpr::all()];
        for _ in 0..limit {
            tail.push(RegexExpr::test(deletion.clone()));
            tail.push(quiet.clone());
        }
        let rule = Rule::new(
            RuleMode::Forbid,
            Head::new(
                ComponentPattern::variable(PRINCIPAL),
                ComponentPattern::constant(Action::Delete),
                ComponentPattern::variable("r"),
            ),
            RegexExpr::concat(tail),
        );
        AgentRuleSet(vec![Self(TraceRule::new(rule, move |event| {
            (
                format!(
                    "{} exceeded its budget of {limit} deletions",
                    event.principal
                ),
                vec!["an operator approves delete".to_owned()],
            )
        }))])
    }
}

/// The rules one [`AgentRule`] constructor produces; [`AgentPolicy::rule`] adds them all.
#[derive(Clone, Debug)]
pub struct AgentRuleSet(Vec<AgentRule>);
impl From<AgentRule> for AgentRuleSet {
    fn from(rule: AgentRule) -> Self {
        Self(vec![rule])
    }
}
impl IntoIterator for AgentRuleSet {
    type Item = AgentRule;
    type IntoIter = std::vec::IntoIter<AgentRule>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// Shared handle to a shell's agent policy.
pub(super) type SharedAgentPolicy = Arc<AgentPolicy>;

#[cfg(test)]
#[path = "agent/tests.rs"]
mod tests;
