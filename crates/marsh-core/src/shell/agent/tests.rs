use super::*;

fn path(principal: &str, action: PathAction, path: &str) -> Observation {
    Observation {
        principal: Principal::from(principal),
        observed: Observed::Path {
            action,
            path: PathBuf::from(path),
            inside: !path.starts_with('/'),
        },
    }
}
fn exec(principal: &str, program: &str) -> Observation {
    Observation {
        principal: Principal::from(principal),
        observed: Observed::Exec(PathBuf::from(program)),
    }
}
fn connect(principal: &str, peer: Peer) -> Observation {
    Observation {
        principal: Principal::from(principal),
        observed: Observed::Connect(peer),
    }
}
fn approve(label: &str) -> Observation {
    Observation {
        principal: Principal::from("operator"),
        observed: Observed::Approve(label.to_owned()),
    }
}
fn external() -> Peer {
    Peer::Ip("93.184.216.34:443".parse().unwrap())
}

/// Decides `batch` after `history`, returning the first denial's precondition.
fn verdict(policy: &AgentPolicy, history: &[Observation], batch: &[Observation]) -> Option<String> {
    policy
        .check(history, batch, None)
        .err()
        .map(|denials| denials[0].failed_precondition.clone())
}

fn sockaddr_in(octets: [u8; 4], port: u16) -> Vec<u8> {
    let mut bytes = u16::try_from(libc::AF_INET).unwrap().to_ne_bytes().to_vec();
    bytes.extend(port.to_be_bytes());
    bytes.extend(octets);
    bytes.extend([0; 8]);
    bytes
}

#[test]
fn peers_decode_from_native_sockaddrs() {
    assert_eq!(
        Peer::decode(&sockaddr_in([127, 0, 0, 1], 8080)),
        Peer::Ip("127.0.0.1:8080".parse().unwrap())
    );
    let mut unix = u16::try_from(libc::AF_UNIX).unwrap().to_ne_bytes().to_vec();
    unix.extend(b"/run/x.sock\0\0\0");
    assert_eq!(Peer::decode(&unix), Peer::Unix(b"/run/x.sock".to_vec()));
    let mut v6 = u16::try_from(libc::AF_INET6)
        .unwrap()
        .to_ne_bytes()
        .to_vec();
    v6.extend(443u16.to_be_bytes());
    v6.extend([0; 4]);
    v6.extend(Ipv6Addr::LOCALHOST.octets());
    v6.extend([0; 4]);
    assert_eq!(Peer::decode(&v6), Peer::Ip("[::1]:443".parse().unwrap()));
    assert_eq!(Peer::decode(&[]), Peer::Unknown);
    assert_eq!(
        Peer::decode(&sockaddr_in([1, 2, 3, 4], 1)[..6]),
        Peer::Unknown
    );
}

#[test]
fn networks_match_by_prefix() {
    let policy = AgentPolicy::new()
        .network("10.0.0.0/8", "internal")
        .unwrap()
        .network("2001:db8::/32", "lab")
        .unwrap();
    let class = |address: &str| {
        policy
            .peer_class(&Peer::Ip(SocketAddr::new(address.parse().unwrap(), 1)))
            .to_owned()
    };
    assert_eq!(class("10.200.1.1"), "internal");
    assert_eq!(class("::ffff:10.0.0.1"), "internal");
    assert_eq!(class("11.0.0.1"), "external");
    assert_eq!(class("2001:db8:1::1"), "lab");
    assert_eq!(class("127.0.0.53"), "loopback");
    assert_eq!(policy.peer_class(&Peer::Unix(Vec::new())), "unix");
    assert!(AgentPolicy::new().network("10.0.0.0/33", "x").is_err());
    assert!(AgentPolicy::new().network("nonsense", "x").is_err());
}

#[test]
fn classes_are_first_match_inside_and_outside_the_source() {
    let policy = AgentPolicy::recommended().unwrap();
    let class = |name: &str| {
        let observation = path("a", PathAction::Edit, name);
        policy
            .label(&observation)
            .map(|event| event.resource.to_string())
    };
    assert_eq!(class(".env").as_deref(), Some("path/secret"));
    assert_eq!(class("deploy/.env.prod").as_deref(), Some("path/secret"));
    assert_eq!(
        class(".github/workflows/ci.yml").as_deref(),
        Some("path/ci")
    );
    assert_eq!(
        class("crates/x/Cargo.lock").as_deref(),
        Some("path/lockfile")
    );
    assert_eq!(class("tests/shell.rs").as_deref(), Some("path/tests"));
    assert_eq!(class("pkg/a_test.go").as_deref(), Some("path/tests"));
    assert_eq!(class("src/lib.rs").as_deref(), Some("path/src"));
    let home = std::env::var("HOME").unwrap();
    assert_eq!(
        class(&format!("{home}/.ssh/id_ed25519")).as_deref(),
        Some("path/secret")
    );
    assert_eq!(class("/usr/lib/libc.so.6"), None);
    assert!(!AgentPolicy::records(
        Some(&policy),
        &path("a", PathAction::Read, "/usr/lib/libc.so.6").observed
    ));
    assert!(!AgentPolicy::records(
        None,
        &path("a", PathAction::Read, &format!("{home}/.ssh/id_rsa")).observed
    ));
    assert_eq!(
        AgentPolicy::new()
            .label(&path("a", PathAction::Create, "x"))
            .map(|event| event.resource.to_string())
            .as_deref(),
        Some("new/other")
    );
    assert_eq!(
        policy
            .label(&exec("a", "/usr/bin/curl"))
            .map(|event| event.resource.to_string())
            .as_deref(),
        Some("exec/curl")
    );
}

#[test]
fn egress_is_denied_only_after_the_same_principal_reads_a_secret() {
    let policy = AgentPolicy::recommended().unwrap();
    let read = path("a", PathAction::Read, ".env");
    assert_eq!(verdict(&policy, &[], &[connect("a", external())]), None);
    assert_eq!(
        verdict(
            &policy,
            std::slice::from_ref(&read),
            &[connect("b", external())]
        ),
        None
    );
    assert_eq!(
        verdict(
            &policy,
            std::slice::from_ref(&read),
            &[connect("a", Peer::Ip("127.0.0.1:1".parse().unwrap()))]
        ),
        None
    );
    assert!(
        verdict(
            &policy,
            std::slice::from_ref(&read),
            &[connect("a", external())]
        )
        .unwrap()
        .contains("after reading secret")
    );
    assert!(verdict(&policy, &[], &[read, connect("a", Peer::Unknown)]).is_some());
}

#[test]
fn approval_gates_protected_classes() {
    let policy = AgentPolicy::recommended().unwrap();
    let edit = path("a", PathAction::Edit, ".github/workflows/ci.yml");
    let create = path("a", PathAction::Create, ".github/workflows/new.yml");
    assert!(verdict(&policy, &[], std::slice::from_ref(&edit)).is_some());
    assert!(verdict(&policy, &[], std::slice::from_ref(&create)).is_some());
    assert!(verdict(&policy, &[approve("tests")], std::slice::from_ref(&edit)).is_some());
    assert_eq!(verdict(&policy, &[approve("ci")], &[edit, create]), None);
}

#[test]
fn tests_lock_once_source_changes_until_approved() {
    let policy = AgentPolicy::recommended().unwrap();
    let src = path("a", PathAction::Edit, "src/lib.rs");
    let test = path("a", PathAction::Edit, "tests/lib.rs");
    assert_eq!(verdict(&policy, &[], &[test.clone(), src.clone()]), None);
    assert!(verdict(&policy, &[], &[src.clone(), test.clone()]).is_some());
    assert_eq!(
        verdict(
            &policy,
            std::slice::from_ref(&src),
            &[path("b", PathAction::Edit, "tests/lib.rs")]
        ),
        None
    );
    assert_eq!(
        verdict(
            &policy,
            &[src.clone(), approve("tests")],
            std::slice::from_ref(&test)
        ),
        None
    );
    assert!(verdict(&policy, &[src.clone(), approve("tests"), src], &[test]).is_some());
}

#[test]
fn downloaders_taint_supply_chain_paths() {
    let policy = AgentPolicy::recommended().unwrap();
    let lock = path("a", PathAction::Edit, "Cargo.lock");
    assert_eq!(
        verdict(
            &policy,
            &[exec("a", "/usr/bin/cargo")],
            std::slice::from_ref(&lock)
        ),
        None
    );
    assert!(
        verdict(
            &policy,
            &[exec("a", "/usr/bin/wget")],
            std::slice::from_ref(&lock)
        )
        .is_some()
    );
    assert_eq!(
        verdict(
            &policy,
            &[exec("a", "/usr/bin/curl"), approve("lockfile")],
            &[lock]
        ),
        None
    );
}

#[test]
fn deletions_are_budgeted_between_approvals() {
    let policy = AgentPolicy::new().rule(AgentRule::deletion_budget(2));
    let delete = |name: &str| path("a", PathAction::Delete, name);
    assert_eq!(verdict(&policy, &[], &[delete("x"), delete("y")]), None);
    assert!(verdict(&policy, &[delete("x"), delete("y")], &[delete("z")]).is_some());
    assert_eq!(
        verdict(
            &policy,
            &[delete("x"), delete("y"), approve("delete")],
            &[delete("z")]
        ),
        None
    );
    assert_eq!(
        verdict(
            &policy,
            &[delete("x"), delete("y")],
            &[path("b", PathAction::Delete, "z")]
        ),
        None
    );
}

#[test]
fn append_only_classes_accept_only_creation() {
    let policy = AgentPolicy::new()
        .class("migrations/**", "migrations")
        .unwrap()
        .rule(AgentRule::append_only("migrations"));
    assert_eq!(
        verdict(
            &policy,
            &[],
            &[path("a", PathAction::Create, "migrations/2.sql")]
        ),
        None
    );
    assert!(
        verdict(
            &policy,
            &[],
            &[path("a", PathAction::Edit, "migrations/1.sql")]
        )
        .is_some()
    );
    assert!(
        verdict(
            &policy,
            &[],
            &[path("a", PathAction::Delete, "migrations/1.sql")]
        )
        .is_some()
    );
}

#[test]
fn network_classes_can_be_forbidden_outright() {
    let policy = AgentPolicy::new()
        .network("10.0.0.0/8", "internal")
        .unwrap()
        .rule(AgentRule::no_network("external"));
    assert!(verdict(&policy, &[], &[connect("a", external())]).is_some());
    assert_eq!(
        verdict(
            &policy,
            &[],
            &[connect("a", Peer::Ip("10.1.1.1:22".parse().unwrap()))]
        ),
        None
    );
}

#[test]
fn a_denied_batch_reports_every_denial() {
    let policy = AgentPolicy::recommended().unwrap();
    let denials = policy
        .check(
            &[],
            &[
                path("a", PathAction::Edit, ".github/a.yml"),
                path("a", PathAction::Edit, "src/ok.rs"),
                path("a", PathAction::Edit, ".husky/pre-commit"),
            ],
            None,
        )
        .unwrap_err();
    assert_eq!(denials.len(), 2);
}
