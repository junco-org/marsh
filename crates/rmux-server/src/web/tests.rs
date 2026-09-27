use rmux_core::events::OutputCursorItem;
use rmux_proto::{
    CreateWebShareRequest, ListWebSharesRequest, PaneId, PaneTargetRef, SessionName,
    StopAllWebSharesRequest, WebShareScope, WebShareUrlOptions, WebTerminalTheme,
};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::pane_io::pane_output_channel_with_limits;
use crate::test_fixtures::{operator_token, spectator_token, token_from_url, Fixture};
use crate::web::origin::validate_public_base_url;
use crate::web::secrets::{derive_spectator_token, random_token};
use crate::web::{WebShareRegistry, WebShareSettings};

fn available_registry() -> WebShareRegistry {
    available_registry_with_settings(WebShareSettings::default())
}

fn available_registry_with_settings(settings: WebShareSettings) -> WebShareRegistry {
    let registry = WebShareRegistry::new(settings);
    registry.mark_listener_available();
    registry
}

#[test]
fn subscribe_from_future_sequence_skips_snapshot_covered_event() {
    let sender = pane_output_channel_with_limits(8, 1024);
    let mut receiver = sender.subscribe_from_sequence(1);

    assert_eq!(sender.send(b"covered-by-snapshot".to_vec()), 0);
    assert!(
        receiver.try_recv().is_none(),
        "event 0 is covered by the snapshot watermark and must be skipped"
    );

    assert_eq!(sender.send(b"post-snapshot".to_vec()), 1);
    let Some(OutputCursorItem::Event(event)) = receiver.try_recv() else {
        panic!("receiver should replay the first post-snapshot event");
    };
    assert_eq!(event.sequence(), 1);
    assert_eq!(event.bytes(), b"post-snapshot");
}

#[test]
fn subscribe_from_retained_sequence_replays_available_events() {
    let sender = pane_output_channel_with_limits(8, 1024);
    assert_eq!(sender.send(b"zero".to_vec()), 0);
    assert_eq!(sender.send(b"one".to_vec()), 1);

    let mut receiver = sender.subscribe_from_sequence(1);
    let Some(OutputCursorItem::Event(event)) = receiver.try_recv() else {
        panic!("receiver should replay retained event 1");
    };
    assert_eq!(event.sequence(), 1);
    assert_eq!(event.bytes(), b"one");
}

#[test]
fn create_returns_secret_urls_but_list_is_redacted() {
    let registry = available_registry();
    let created = registry
        .create(CreateWebShareRequest {
            public_base_url: Some("https://share.example".to_owned()),
            ttl_seconds: Some(60),
            max_spectators: Some(2),
            operator: true,
            ..target_share()
        })
        .expect("share creates");

    assert!(created
        .spectator_url
        .as_deref()
        .expect("spectator URL")
        .contains("#e=wss://share.example/share&t="));
    assert!(created
        .operator_url
        .as_deref()
        .is_some_and(|url| url.contains("#e=wss://share.example/share&t=")));
    let stdout = String::from_utf8_lossy(created.output.stdout());
    assert!(stdout.contains("spectator "));
    assert!(stdout.contains("operator URL emitted on stderr"));

    let listed = registry.list(ListWebSharesRequest);
    assert_eq!(listed.shares.len(), 1);
    let redacted = listed.shares[0].spectator_url.as_deref().expect("url");
    assert_eq!(
        redacted,
        format!("https://share.rmux.io/#e=wss://share.example/share&t=[REDACTED]")
    );
}

#[tokio::test]
async fn default_local_share_uses_hosted_frontend_and_local_websocket_endpoint() {
    let spectator_url = assert_spectator_frontend(
        &available_registry(),
        CreateWebShareRequest {
            ttl_seconds: Some(60),
            max_spectators: Some(2),
            ..target_share()
        },
        "https://share.rmux.io/#t=",
        &[
            "https://share.rmux.io",
            "http://localhost:4321",
            "http://127.0.0.1:5173",
        ],
        &["https://evil.example"],
    )
    .await;
    assert!(!spectator_url.contains("role="));
}

#[tokio::test]
async fn both_role_share_has_no_expiry_and_default_role_caps() {
    let registry = available_registry();
    let created = registry
        .create(CreateWebShareRequest {
            operator: true,
            ..target_share()
        })
        .expect("share creates");

    assert!(created.operator_url.is_some());
    assert!(created.spectator_url.is_some());
    assert!(created.operator);
    assert!(created.spectator);
    assert_eq!(created.expires_at_unix, None);
    assert_eq!(created.max_operators, Some(1));
    assert_eq!(created.max_spectators, Some(12));
    let stdout = String::from_utf8_lossy(created.output.stdout());
    assert!(stdout.contains("spectator "));
    assert!(stdout.contains("operator URL emitted on stderr"));
    assert!(stdout.contains("share does not expire"));

    let spectator_token = spectator_token(&created);
    let first = registry
        .connect(&spectator_token, None)
        .await
        .expect("first spectator connects");
    let second = registry
        .connect(&spectator_token, None)
        .await
        .expect("second spectator fits default spectator cap");
    assert_eq!(second.connection_counts().spectators_active, 2);
    drop((first, second));
}

#[test]
fn operator_only_share_does_not_mint_spectator_url() {
    let registry = available_registry();
    let created = registry
        .create(CreateWebShareRequest {
            operator: true,
            spectator: false,
            ..target_share()
        })
        .expect("operator-only share creates");

    assert!(created.spectator_url.is_none());
    assert!(created.operator_url.is_some());
    let stdout = String::from_utf8_lossy(created.output.stdout());
    assert!(!stdout.contains("spectator "));
    assert!(stdout.contains("operator URL emitted on stderr"));

    let listed = registry.list(ListWebSharesRequest);
    assert_eq!(listed.shares[0].spectator_url, None);
    assert_eq!(
        String::from_utf8_lossy(listed.output.stdout()).trim_end(),
        format!("{} {} -", created.share_id, target())
    );
}

#[test]
fn max_spectators_requires_spectator_url() {
    assert_create_rejected(
        &available_registry(),
        CreateWebShareRequest {
            max_spectators: Some(1),
            operator: true,
            spectator: false,
            ..target_share()
        },
        "spectator cap without spectator is invalid",
        "web-share --max-spectators cannot be used without a spectator URL",
    );
}

#[test]
fn max_operators_requires_operator_url() {
    assert_create_rejected(
        &available_registry(),
        CreateWebShareRequest {
            max_operators: Some(1),
            ..target_share()
        },
        "operator cap without operator is invalid",
        "web-share --max-operators cannot be used without an operator URL",
    );
}

#[tokio::test]
async fn known_token_origin_precheck_does_not_consume_a_read_slot() {
    let registry = available_registry();
    let created = registry
        .create(CreateWebShareRequest {
            ttl_seconds: Some(60),
            max_spectators: Some(1),
            ..target_share()
        })
        .expect("share creates");
    let token = spectator_token(&created);

    assert_eq!(
        registry.known_token_origin_allowed(&token, "https://evil.example"),
        Some(false)
    );
    assert!(registry
        .connect(&token, None)
        .await
        .expect("spectator connects after rejected origin precheck")
        .origin_allowed("https://share.rmux.io"));
}

#[tokio::test]
async fn frontend_override_changes_browser_origin_without_changing_local_endpoint() {
    let registry = available_registry_with_settings(
        WebShareSettings::from_options(9778, Some("https://share.fork.example".to_owned()))
            .expect("settings"),
    );
    assert_spectator_frontend(
        &registry,
        CreateWebShareRequest {
            ttl_seconds: Some(60),
            max_spectators: Some(2),
            ..target_share()
        },
        "https://share.fork.example/#e=ws://127.0.0.1:9778/share&t=",
        &["https://share.fork.example"],
        &["https://share.rmux.io"],
    )
    .await;
}

#[tokio::test]
async fn per_share_frontend_url_overrides_daemon_default() {
    assert_spectator_frontend(
        &available_registry(),
        CreateWebShareRequest {
            public_base_url: Some("https://terminal.example".to_owned()),
            frontend_url: Some("https://share.fork.example/share".to_owned()),
            ttl_seconds: Some(60),
            max_spectators: Some(2),
            ..target_share()
        },
        "https://share.fork.example/share/#e=wss://terminal.example/share&t=",
        &["https://share.fork.example"],
        &["https://share.rmux.io"],
    )
    .await;
}

#[test]
fn public_base_url_rejects_query_and_fragment() {
    assert!(validate_public_base_url("https://x.test?a=1").is_err());
    assert!(validate_public_base_url("https://x.test#frag").is_err());
    assert!(validate_public_base_url("ssh://x.test").is_err());
}

#[test]
fn tunnel_provider_and_tunnel_url_are_mutually_exclusive() {
    assert_create_rejected(
        &available_registry(),
        CreateWebShareRequest {
            public_base_url: Some("https://share.example".to_owned()),
            tunnel_provider: Some("srv-us".to_owned()),
            ttl_seconds: Some(60),
            max_spectators: Some(2),
            ..target_share()
        },
        "mutually exclusive tunnel options are rejected",
        "mutually exclusive",
    );
}

#[test]
fn local_web_share_requires_bound_listener_and_valid_port() {
    assert!(WebShareSettings::from_options(0, None).is_err());

    let cold_registry = WebShareRegistry::default();
    assert!(cold_registry
        .config(rmux_proto::WebShareConfigRequest)
        .expect_err("cold listener must reject config")
        .to_string()
        .contains("not started"));

    let registry = available_registry();
    registry.mark_listener_unavailable("address already in use");
    let local_share = || CreateWebShareRequest {
        ttl_seconds: Some(60),
        max_spectators: Some(2),
        ..target_share()
    };
    assert_create_rejected(
        &registry,
        local_share(),
        "dead listener must reject local share URLs",
        "listener unavailable",
    );
    assert!(registry
        .config(rmux_proto::WebShareConfigRequest)
        .expect_err("dead listener must reject config")
        .to_string()
        .contains("listener unavailable"));

    registry.mark_listener_available();
    assert!(registry.create(local_share()).is_ok());
}

#[test]
fn public_url_scheme_is_case_insensitive_for_websocket_endpoint() {
    let registry = available_registry();
    let created = registry
        .create(CreateWebShareRequest {
            public_base_url: Some("HTTPS://terminal.example".to_owned()),
            ttl_seconds: Some(60),
            max_spectators: Some(2),
            ..target_share()
        })
        .expect("uppercase HTTPS is valid");

    assert!(created
        .spectator_url
        .as_deref()
        .expect("spectator URL")
        .starts_with("https://share.rmux.io/#e=wss://terminal.example/share&t="));
}

#[tokio::test]
async fn url_options_are_encoded_in_spectator_urls() {
    let registry = available_registry();
    let created = registry
        .create(CreateWebShareRequest {
            ttl_seconds: Some(60),
            max_spectators: Some(2),
            url_options: WebShareUrlOptions {
                no_navbar: true,
                no_disclaimer: true,
                show_viewers: true,
                terminal_theme: Some(WebTerminalTheme::Light),
            },
            operator: true,
            ..target_share()
        })
        .expect("share creates");

    let spectator_url = created.spectator_url.as_deref().expect("spectator URL");
    assert!(spectator_url.contains("&navbar=off"));
    assert!(spectator_url.contains("&disclaimer=off"));
    assert!(!spectator_url.contains("&viewers=on"));
    assert!(spectator_url.contains("&theme=light"));
    assert!(created
        .operator_url
        .as_deref()
        .is_some_and(|url| url.contains("&navbar=off")
            && url.contains("&disclaimer=off")
            && !url.contains("&viewers=on")
            && url.contains("&theme=light")));

    let access = registry
        .connect(&spectator_token(&created), None)
        .await
        .expect("spectator token connects");
    assert!(access.show_viewers());
}

#[tokio::test]
async fn pairing_code_is_required_out_of_band_when_pin_enabled() {
    let registry = available_registry();
    let created = registry
        .create(CreateWebShareRequest {
            ttl_seconds: Some(60),
            max_spectators: Some(2),
            require_pin: true,
            ..target_share()
        })
        .expect("share creates");

    assert!(
        created.operator_pairing_code.is_none(),
        "spectator-only share should not mint an operator PIN"
    );
    let pairing_code = created
        .spectator_pairing_code
        .as_deref()
        .expect("pin-enabled spectator share returns pairing code");
    assert_eq!(pairing_code.len(), 6);
    assert!(pairing_code.bytes().all(|byte| byte.is_ascii_digit()));
    let spectator_url = created.spectator_url.as_deref().expect("spectator URL");
    assert!(!spectator_url.contains("&pin=required"));
    assert!(!spectator_url.contains(pairing_code));
    let stdout = String::from_utf8_lossy(created.output.stdout());
    assert!(stdout.contains(&format!("spectator pin {pairing_code}\n")));

    let spectator_token = spectator_token(&created);
    assert!(registry
        .connect(&spectator_token, None)
        .await
        .expect_err("pin must be supplied")
        .to_string()
        .contains("missing web-share pairing code"));
    assert!(registry
        .connect(&spectator_token, Some("000000"))
        .await
        .is_err());
    assert!(registry
        .connect(&spectator_token, Some(pairing_code))
        .await
        .is_ok());
}

#[tokio::test]
async fn role_specific_pairing_codes_are_bound_to_their_access_role() {
    let registry = available_registry();
    let created = registry
        .create(CreateWebShareRequest {
            scope: WebShareScope::Session("alpha".parse().expect("session name")),
            ttl_seconds: Some(60),
            max_spectators: Some(2),
            max_operators: Some(2),
            require_pin: true,
            operator_pin: Some("123456".to_owned()),
            spectator_pin: Some("654321".to_owned()),
            operator: true,
            controls: true,
            ..target_share()
        })
        .expect("share creates");

    assert_eq!(created.operator_pairing_code.as_deref(), Some("123456"));
    assert_eq!(created.spectator_pairing_code.as_deref(), Some("654321"));

    let operator_token = operator_token(&created);
    let spectator_token = spectator_token(&created);

    assert!(registry
        .connect(&operator_token, Some("654321"))
        .await
        .expect_err("spectator PIN must not unlock operator token")
        .to_string()
        .contains("invalid web-share pairing code"));
    assert!(registry
        .connect(&spectator_token, Some("123456"))
        .await
        .expect_err("operator PIN must not unlock spectator token")
        .to_string()
        .contains("invalid web-share pairing code"));

    let operator = registry
        .connect(&operator_token, Some("123456"))
        .await
        .expect("operator token accepts operator PIN");
    assert!(operator.is_operator());

    let spectator = registry
        .connect(&spectator_token, Some("654321"))
        .await
        .expect("spectator token accepts spectator PIN");
    assert!(!spectator.is_operator());
}

#[test]
fn controls_are_derived_for_operator_session_shares() {
    let registry = available_registry();
    let session = SessionName::new("alpha").expect("valid session");

    let spectator_share = registry
        .create(CreateWebShareRequest {
            scope: WebShareScope::Session(session.clone()),
            controls: true,
            ..target_share()
        })
        .expect("spectator session share creates");
    assert!(!spectator_share.controls);

    let pane = registry
        .create(CreateWebShareRequest {
            operator: true,
            controls: true,
            ..target_share()
        })
        .expect("operator pane share creates");
    assert!(!pane.controls);

    let created = registry
        .create(CreateWebShareRequest {
            scope: WebShareScope::Session(session.clone()),
            operator: true,
            ..target_share()
        })
        .expect("operator session share creates");
    assert!(matches!(&created.scope, WebShareScope::Session(actual) if actual == &session));
    assert!(created.controls);

    let listed = registry.list(ListWebSharesRequest);
    let summary = listed
        .shares
        .iter()
        .find(|share| share.share_id == created.share_id)
        .expect("created share should be listed");
    assert!(matches!(
        &summary.scope,
        WebShareScope::Session(actual) if actual == &session
    ));
    assert!(summary.controls);
}

#[test]
fn expiration_accepts_absolute_deadline_and_rejects_invalid_combinations() {
    let registry = available_registry();
    let future = unix_seconds(SystemTime::now() + Duration::from_secs(60));
    let created = registry
        .create(CreateWebShareRequest {
            expires_at_unix: Some(future),
            ..target_share()
        })
        .expect("absolute expiry creates");
    assert_eq!(created.expires_at_unix, Some(future));
    assert!(String::from_utf8_lossy(created.output.stdout()).contains("share expires at "));

    assert_create_rejected(
        &registry,
        CreateWebShareRequest {
            ttl_seconds: Some(10),
            expires_at_unix: Some(future),
            ..target_share()
        },
        "ttl and absolute expiry conflict",
        "mutually exclusive",
    );
    assert_create_rejected(
        &registry,
        CreateWebShareRequest {
            expires_at_unix: Some(1),
            ..target_share()
        },
        "past expiry is rejected",
        "must be in the future",
    );
    assert_create_rejected(
        &registry,
        CreateWebShareRequest {
            expires_at_unix: Some(u64::MAX),
            ..target_share()
        },
        "overflowing expiry is rejected",
        "out of range",
    );
}

#[test]
fn kill_session_on_expire_requires_session_scope() {
    let registry = available_registry();
    assert_create_rejected(
        &registry,
        CreateWebShareRequest {
            ttl_seconds: Some(60),
            kill_session_on_expire: true,
            ..target_share()
        },
        "pane expiry cannot kill a session",
        "requires a session target",
    );

    let session = SessionName::new("expiry").expect("valid session");
    let created = registry
        .create(CreateWebShareRequest {
            scope: WebShareScope::Session(session),
            ttl_seconds: Some(60),
            operator: true,
            kill_session_on_expire: true,
            ..target_share()
        })
        .expect("session kill-on-expiry share creates");
    assert!(created.kill_session_on_expire);
    assert!(String::from_utf8_lossy(created.output.stdout())
        .contains("session will be killed on expiry"));
}

#[test]
fn stop_all_reports_removed_share_count() {
    let registry = available_registry();
    for _ in 0..2 {
        registry.create(target_share()).expect("share creates");
    }
    assert_eq!(registry.stop_all(StopAllWebSharesRequest).stopped, 2);
    assert!(registry.list(ListWebSharesRequest).shares.is_empty());
}

#[tokio::test]
async fn connect_enforces_role_caps() {
    let registry = available_registry();
    let created = registry
        .create(CreateWebShareRequest {
            max_spectators: Some(1),
            max_operators: Some(2),
            operator: true,
            ..target_share()
        })
        .expect("share creates");
    let spectator_token = spectator_token(&created);
    let operator_token = operator_token(&created);

    let spectator = registry
        .connect(&spectator_token, None)
        .await
        .expect("spectator connects");
    assert!(!spectator.is_operator());
    assert_eq!(spectator.connection_counts().spectators_active, 1);
    assert_eq!(spectator.connection_counts().spectators_max, Some(1));
    assert_eq!(spectator.connection_counts().operators_active, 0);
    assert_eq!(spectator.connection_counts().operators_max, Some(2));
    assert_eq!(spectator.connection_counts().viewers_connected, 1);
    assert!(registry.connect(&spectator_token, None).await.is_err());

    let operator = registry
        .connect(&operator_token, None)
        .await
        .expect("operator connects");
    assert!(operator.is_operator());
    assert_eq!(operator.connection_counts().spectators_active, 1);
    assert_eq!(operator.connection_counts().operators_active, 1);
    assert_eq!(operator.connection_counts().viewers_connected, 2);

    let second_operator = registry
        .connect(&operator_token, None)
        .await
        .expect("second operator connects");
    assert_eq!(second_operator.connection_counts().operators_active, 2);
    assert_eq!(second_operator.connection_counts().viewers_connected, 3);
    assert!(registry.connect(&operator_token, None).await.is_err());

    drop(spectator);
    assert!(registry.connect(&spectator_token, None).await.is_ok());
}

#[tokio::test]
async fn connect_enforces_authenticated_process_capacity() {
    let registry = WebShareRegistry::new_with_authenticated_connection_limit(1);
    registry.mark_listener_available();
    let first = registry
        .create(target_share())
        .expect("first share creates");
    let second = registry
        .create(target_share())
        .expect("second share creates");
    let first_token = spectator_token(&first);
    let second_token = spectator_token(&second);

    let first_access = registry
        .connect(&first_token, None)
        .await
        .expect("first connection fits global capacity");
    assert!(
        registry.connect(&second_token, None).await.is_err(),
        "global authenticated capacity rejects a second connection even across shares"
    );

    drop(first_access);
    assert!(
        registry.connect(&second_token, None).await.is_ok(),
        "dropping a connection releases global capacity"
    );
}

#[tokio::test]
async fn authentication_wait_capacity_is_per_key_and_releases_on_cancel() {
    let registry = Arc::new(WebShareRegistry::new_with_authentication_limits(1, 2, 1, 2));
    registry.mark_listener_available();
    let (token, pairing_code) = create_protected_share(&registry);

    let first_error = registry
        .connect(&token, Some("definitely-not-the-pairing-code"))
        .await
        .expect_err("wrong PIN seeds the next attempt's backoff delay");
    assert!(first_error.to_string().contains("pairing code"));
    assert_eq!(registry.authenticated_connection_count(), 0);

    let waiting_registry = Arc::clone(&registry);
    let waiting_token = token.clone();
    let waiting = tokio::spawn(async move {
        waiting_registry
            .connect(&waiting_token, Some(&pairing_code))
            .await
    });
    tokio::time::timeout(Duration::from_millis(50), async {
        while registry.authentication_wait_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("backoff waiter must acquire authentication wait capacity");
    assert_eq!(
        registry.authenticated_connection_count(),
        0,
        "a backoff sleep must not consume established connection capacity"
    );

    let capped = tokio::time::timeout(Duration::from_millis(50), registry.connect(&token, None))
        .await
        .expect("a capped same-key attempt must not enter another backoff sleep")
        .expect_err("the per-key authentication wait slot is occupied");
    assert!(capped.to_string().contains("authentication queue limit"));

    waiting.abort();
    assert!(waiting
        .await
        .expect_err("waiter was cancelled")
        .is_cancelled());
    assert_eq!(
        registry.authentication_wait_count(),
        0,
        "cancelling a backoff waiter must release its wait permit"
    );
}

#[tokio::test]
async fn backoff_waiter_does_not_block_an_unrelated_share() {
    let registry = Arc::new(WebShareRegistry::new_with_authentication_limits(1, 2, 1, 2));
    registry.mark_listener_available();
    let (protected_token, protected_pin) = create_protected_share(&registry);
    let unrelated = registry
        .create(target_share())
        .expect("unrelated share creates");
    let unrelated_token = spectator_token(&unrelated);

    registry
        .connect(&protected_token, Some("wrong-pin"))
        .await
        .expect_err("wrong PIN seeds a backoff sleep");
    let waiting_registry = Arc::clone(&registry);
    let waiting_token = protected_token.clone();
    let waiter = tokio::spawn(async move {
        waiting_registry
            .connect(&waiting_token, Some(&protected_pin))
            .await
    });
    tokio::time::timeout(Duration::from_millis(50), async {
        while registry.authentication_wait_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("protected share enters its bounded backoff wait");
    assert_eq!(
        registry.authenticated_connection_count(),
        0,
        "the sleeping auth attempt must not consume established capacity"
    );

    let unrelated_access = registry
        .connect(&unrelated_token, None)
        .await
        .expect("an unrelated share must remain available during another share's backoff");
    drop(unrelated_access);
    waiter.abort();
    let _ = waiter.await;
}

#[tokio::test]
async fn authentication_wait_capacity_isolated_by_network_peer() {
    let registry = Arc::new(WebShareRegistry::new_with_authentication_limits(4, 3, 2, 1));
    registry.mark_listener_available();
    let (first_token, first_pin) = create_protected_share(&registry);
    let (second_token, second_pin) = create_protected_share(&registry);
    let busy_peer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10));
    let other_peer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 11));

    registry
        .connect(&first_token, Some("wrong-pin"))
        .await
        .expect_err("first share gets a backoff delay");
    registry
        .connect(&second_token, Some("wrong-pin"))
        .await
        .expect_err("second share gets a backoff delay");

    let first_registry = Arc::clone(&registry);
    let first_waiter = tokio::spawn(async move {
        first_registry
            .connect_from_peer(&first_token, Some(&first_pin), busy_peer)
            .await
    });
    tokio::time::timeout(Duration::from_millis(50), async {
        while registry.authentication_wait_count() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first peer enters a backoff wait");

    let same_peer_error = registry
        .connect_from_peer(&second_token, Some(&second_pin), busy_peer)
        .await
        .expect_err("one peer cannot occupy waiters across shares");
    assert!(same_peer_error
        .to_string()
        .contains("authentication queue limit"));

    let second_registry = Arc::clone(&registry);
    let other_peer_waiter = tokio::spawn(async move {
        second_registry
            .connect_from_peer(&second_token, Some(&second_pin), other_peer)
            .await
    });
    tokio::time::timeout(Duration::from_millis(50), async {
        while registry.authentication_wait_count() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("another peer can wait on another share");

    first_waiter.abort();
    other_peer_waiter.abort();
    let _ = first_waiter.await;
    let _ = other_peer_waiter.await;
    assert_eq!(registry.authentication_wait_count(), 0);
}

#[tokio::test]
async fn capability_tokens_grant_only_their_daemon_owned_roles() {
    let registry = available_registry();
    let created = registry
        .create(CreateWebShareRequest {
            max_spectators: Some(2),
            operator: true,
            ..target_share()
        })
        .expect("share creates");

    for url in [
        created.spectator_url.as_deref().expect("spectator URL"),
        created.operator_url.as_deref().expect("operator URL"),
    ] {
        for parameter in ["id=", "key=", "role="] {
            assert!(!url.contains(parameter), "{url}");
        }
    }

    let spectator_token = spectator_token(&created);
    let operator_token = operator_token(&created);
    assert_eq!(
        spectator_token,
        derive_spectator_token(&operator_token).expect("derived spectator token")
    );

    let spectator_access = registry
        .connect(&spectator_token, None)
        .await
        .expect("spectator token connects");
    assert!(!spectator_access.is_operator());
    assert!(!spectator_access.controls());
    drop(spectator_access);

    let operator_access = registry
        .connect(&operator_token, None)
        .await
        .expect("operator token connects");
    assert!(operator_access.is_operator());
    assert!(!operator_access.controls());
}

#[tokio::test]
async fn stopped_or_expired_share_rejects_previous_tokens() {
    let registry = available_registry();
    let created = registry
        .create(CreateWebShareRequest {
            max_spectators: Some(2),
            operator: true,
            ..target_share()
        })
        .expect("share creates");
    let spectator_token = spectator_token(&created);
    let operator_token = operator_token(&created);

    assert!(
        registry
            .stop(rmux_proto::StopWebShareRequest {
                share_id: created.share_id,
            })
            .stopped
    );
    assert!(registry.connect(&spectator_token, None).await.is_err());
    assert!(registry.connect(&operator_token, None).await.is_err());
}

#[tokio::test]
async fn auth_failures_backoff_per_share_id() {
    let registry = available_registry();
    let _created = registry
        .create(CreateWebShareRequest {
            max_spectators: Some(2),
            ..target_share()
        })
        .expect("share creates");
    let wrong_token = random_token().expect("test token");

    let start = Instant::now();
    for _ in 0..4 {
        assert!(registry.connect(&wrong_token, None).await.is_err());
    }

    assert!(
        start.elapsed() >= Duration::from_millis(650),
        "expected exponential backoff to delay repeated failures"
    );
}

fn target() -> PaneTargetRef {
    PaneTargetRef::by_id(
        SessionName::new("alpha").expect("valid session"),
        PaneId::new(7),
    )
}

/// A spectator-only share of [`target`] with every other option off; session shares override
/// `scope`.
fn target_share() -> CreateWebShareRequest {
    CreateWebShareRequest::fixture(WebShareScope::Pane(target()))
}

/// Creates a PIN-protected spectator share; returns its access token and pairing code.
fn create_protected_share(registry: &WebShareRegistry) -> (String, String) {
    let created = registry
        .create(CreateWebShareRequest {
            require_pin: true,
            ..target_share()
        })
        .expect("protected share creates");
    let token = spectator_token(&created);
    let pairing_code = created
        .spectator_pairing_code
        .expect("protected share has a pairing code");
    (token, pairing_code)
}

/// Asserts that `registry` rejects `request` (`reason` explains why) with an error mentioning
/// `expected`.
#[track_caller]
fn assert_create_rejected(
    registry: &WebShareRegistry,
    request: CreateWebShareRequest,
    reason: &str,
    expected: &str,
) {
    let error = registry.create(request).expect_err(reason);
    assert!(error.to_string().contains(expected), "{error}");
}

/// Creates `request`, asserts its spectator URL starts with `url_prefix`, then connects as that
/// spectator and asserts the access admits each `allowed` browser origin and no `denied` one.
/// Returns the spectator URL.
async fn assert_spectator_frontend(
    registry: &WebShareRegistry,
    request: CreateWebShareRequest,
    url_prefix: &str,
    allowed: &[&str],
    denied: &[&str],
) -> String {
    let created = registry.create(request).expect("share creates");
    let spectator_url = created.spectator_url.expect("spectator URL");
    assert!(spectator_url.starts_with(url_prefix), "{spectator_url}");
    let access = registry
        .connect(&token_from_url(&spectator_url), None)
        .await
        .expect("spectator connects");
    for origin in allowed {
        assert!(access.origin_allowed(origin), "{origin}");
    }
    for origin in denied {
        assert!(!access.origin_allowed(origin), "{origin}");
    }
    spectator_url
}

fn unix_seconds(value: SystemTime) -> u64 {
    value
        .duration_since(UNIX_EPOCH)
        .expect("test deadline after epoch")
        .as_secs()
}
