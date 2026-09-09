use super::CredentialAuthMethod;
use super::CredentialBroker;
use super::CredentialProviderConfig;
use super::brokered_credential_marker_env_keys;
use super::brokered_credential_value_env_keys;
use crate::NetworkProxyConfig;
use base64::Engine as _;
use pretty_assertions::assert_eq;
use rama_http::HeaderMap;
use rama_http::HeaderValue;
use rama_http::header::AUTHORIZATION;
use std::collections::BTreeMap;
use std::collections::HashMap;

fn broker_for(provider: CredentialProviderConfig) -> CredentialBroker {
    let broker = CredentialBroker::new(/*enabled*/ true);
    broker.configure(&NetworkProxyConfig {
        credential_broker: true,
        credential_providers: BTreeMap::from([("custom".to_string(), provider)]),
        ..NetworkProxyConfig::default()
    });
    broker
}

#[test]
fn local_proxy_bypass_preserves_credentials_and_aliases_across_reload() {
    let token = "vendor_abcdefghijklmnopqrstuvwx";
    let github = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
    for (destination, bypassed) in [
        ("http://localhost:1234/v1", true),
        ("http://127.0.0.1:1234/v1", true),
        ("http://[::1]:1234/v1", true),
        ("https://10.1.2.3/v1", true),
        ("https://172.16.2.3/v1", true),
        ("https://192.168.2.3/v1", true),
        ("https://api.localhost/v1", true),
        ("https://127.0.0.2/v1", false),
        ("https://172.32.2.3/v1", false),
        ("https://api.vendor.example/v1", false),
    ] {
        let broker = CredentialBroker::new(/*enabled*/ true);
        let mut config = NetworkProxyConfig {
            credential_broker: true,
            credential_providers: BTreeMap::from([(
                "vendor".to_string(),
                CredentialProviderConfig {
                    env: vec!["VENDOR_TOKEN".to_string()],
                    patterns: vec!["^vendor_[a-z]{24}$".to_string()],
                    url_prefixes: vec!["https://public.vendor.example/v1".to_string()],
                    url_prefix_from_env: Some("VENDOR_URL".to_string()),
                    ..CredentialProviderConfig::default()
                },
            )]),
            ..NetworkProxyConfig::default()
        };
        broker.configure(&config);
        let mut env = HashMap::from([
            ("VENDOR_TOKEN".to_string(), token.to_string()),
            ("VENDOR_URL".to_string(), destination.to_string()),
            ("AUTH_HEADER".to_string(), format!("Bearer {token}")),
            ("GH_TOKEN".to_string(), github.to_string()),
        ]);
        broker.virtualize_child_env(&mut env);
        let dummy = env["VENDOR_TOKEN"].clone();
        let github_dummy = env["GH_TOKEN"].clone();
        assert_ne!(dummy, token);
        assert_ne!(github_dummy, github);

        config.allow_local_binding = true;
        let revision = broker.config_revision();
        broker.configure(&config);
        assert_eq!(broker.config_revision(), revision + 1);
        for _ in 0..2 {
            broker.virtualize_child_env(&mut env);
            let expected = if bypassed { token } else { &dummy };
            assert_eq!(
                (&env["VENDOR_TOKEN"], &env["AUTH_HEADER"], &env["GH_TOKEN"]),
                (
                    &expected.to_string(),
                    &format!("Bearer {expected}"),
                    &github_dummy
                ),
                "{destination}"
            );
            assert_eq!(
                brokered_credential_value_env_keys(&env),
                if bypassed {
                    vec!["GH_TOKEN"]
                } else {
                    vec!["AUTH_HEADER", "GH_TOKEN", "VENDOR_TOKEN"]
                }
            );
        }
        config.allow_local_binding = false;
        let mut snapshot_env = env.clone();
        broker.virtualize_snapshot_env(&mut snapshot_env, /*environment_id*/ None);
        assert_eq!(snapshot_env["VENDOR_TOKEN"], dummy);
        assert_eq!(snapshot_env["AUTH_HEADER"], format!("Bearer {dummy}"));
        broker.configure(&config);
        broker.virtualize_child_env(&mut env);
        assert_eq!(env["VENDOR_TOKEN"], dummy);
        assert_eq!(env["AUTH_HEADER"], format!("Bearer {dummy}"));
    }
}

#[test]
fn child_alias_identity_survives_scoped_dummies_and_partial_direct_restoration() {
    let local = "local_abcdefghijklmnopqrstuvwx";
    let remote = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
    for allow_local_binding in [false, true] {
        let broker = CredentialBroker::new(/*enabled*/ true);
        broker.configure(&NetworkProxyConfig {
            credential_broker: true,
            allow_local_binding,
            credential_providers: BTreeMap::from([(
                "local".to_string(),
                CredentialProviderConfig {
                    env: vec!["LOCAL_TOKEN".to_string()],
                    patterns: vec!["^local_[a-z]{24}$".to_string()],
                    url_prefixes: vec!["http://127.0.0.1:1234".to_string()],
                    ..CredentialProviderConfig::default()
                },
            )]),
            ..NetworkProxyConfig::default()
        });
        let real_alias = format!("Local {local}; Remote {remote}");
        let mut env = HashMap::from([
            ("LOCAL_TOKEN".to_string(), local.to_string()),
            ("GH_TOKEN".to_string(), remote.to_string()),
            ("AUTH_HEADER".to_string(), real_alias.clone()),
        ]);
        let mut snapshot = env.clone();
        broker.virtualize_snapshot_env(&mut snapshot, Some("snapshot"));
        let snapshot_alias = snapshot["AUTH_HEADER"].clone();
        broker.virtualize_child_env_for_environment(&mut env, Some("child"));
        env.remove("LOCAL_TOKEN");
        env.remove("GH_TOKEN");
        assert_eq!(env["AUTH_HEADER"].contains(local), allow_local_binding);
        assert!(!env["AUTH_HEADER"].contains(remote));
        assert!(broker.child_alias_matches(
            "AUTH_HEADER",
            &env["AUTH_HEADER"],
            &snapshot_alias,
            Some("child")
        ));
        assert!(!broker.child_alias_matches(
            "AUTH_HEADER",
            &real_alias,
            &snapshot_alias,
            Some("child")
        ));
        assert!(!broker.child_alias_matches(
            "AUTH_HEADER",
            &format!("{} altered", env["AUTH_HEADER"]),
            &snapshot_alias,
            Some("child")
        ));
        assert!(!broker.child_alias_matches(
            "OTHER_HEADER",
            &env["AUTH_HEADER"],
            &snapshot_alias,
            Some("child")
        ));
    }
}

#[test]
fn local_proxy_bypass_is_scoped_for_inherited_credentials_and_aliases() {
    let token = "vendor_abcdefghijklmnopqrstuvwx";
    for parent_is_local in [false, true] {
        for retain_source in [false, true] {
            let broker = CredentialBroker::new(/*enabled*/ true);
            broker.configure(&NetworkProxyConfig {
                credential_broker: true,
                allow_local_binding: true,
                credential_providers: BTreeMap::from([(
                    "vendor".to_string(),
                    CredentialProviderConfig {
                        env: vec!["VENDOR_TOKEN".to_string()],
                        patterns: vec!["^vendor_[a-z]{24}$".to_string()],
                        url_prefix_from_env: Some("VENDOR_URL".to_string()),
                        ..CredentialProviderConfig::default()
                    },
                )]),
                ..NetworkProxyConfig::default()
            });
            let local = "http://127.0.0.1:1234/v1";
            let public = "https://api.vendor.example/v1";
            let mut snapshot = HashMap::from([
                ("VENDOR_TOKEN".to_string(), token.to_string()),
                (
                    "VENDOR_URL".to_string(),
                    if parent_is_local { local } else { public }.to_string(),
                ),
                ("AUTH_HEADER".to_string(), format!("Bearer {token}")),
            ]);
            broker.virtualize_snapshot_env(&mut snapshot, Some("parent"));
            let dummy = snapshot["VENDOR_TOKEN"].clone();
            let snapshot_alias = snapshot["AUTH_HEADER"].clone();
            let mut child = snapshot.clone();
            child.insert(
                "VENDOR_URL".to_string(),
                if parent_is_local { public } else { local }.to_string(),
            );
            if !retain_source {
                child.remove("VENDOR_TOKEN");
            }
            for (id, env, bypasses) in [
                ("child", &mut child, !parent_is_local),
                ("parent", &mut snapshot, parent_is_local),
            ] {
                broker.virtualize_child_env_for_environment(env, Some(id));
                let expected = if bypasses { token } else { &dummy };
                assert_eq!(
                    env.get("VENDOR_TOKEN").map(String::as_str),
                    (retain_source || id == "parent").then_some(expected)
                );
                assert_eq!(env["AUTH_HEADER"], format!("Bearer {expected}"));
                assert!(broker.child_alias_matches(
                    "AUTH_HEADER",
                    &env["AUTH_HEADER"],
                    &snapshot_alias,
                    Some(id)
                ));
                if !bypasses {
                    assert!(!broker.child_alias_matches(
                        "AUTH_HEADER",
                        &format!("Bearer {token}"),
                        &snapshot_alias,
                        Some(id)
                    ));
                }
            }
        }
    }
}

#[test]
fn configured_provider_virtualizes_credentials_aliases_and_snapshots() {
    let token = "stripe_live_abcdefghijklmnopqrstuvwx";
    let broker = broker_for(CredentialProviderConfig {
        env: vec!["STRIPE_API_KEY".to_string()],
        patterns: vec!["^stripe_live_[a-z]{24}$".to_string()],
        url_prefixes: vec!["api.stripe.com".to_string(), "*.stripe.example".to_string()],
        ..CredentialProviderConfig::default()
    });
    let mut env = HashMap::from([
        ("STRIPE_API_KEY".to_string(), token.to_string()),
        ("AUTH_HEADER".to_string(), format!("Bearer {token}")),
    ]);

    broker.virtualize_child_env(&mut env);

    let dummy = &env["STRIPE_API_KEY"];
    assert_ne!(dummy, token);
    assert!(
        regex::Regex::new("^stripe_live_[a-z]{24}$")
            .expect("valid credential pattern")
            .is_match(dummy)
    );
    assert_eq!(env["AUTH_HEADER"], format!("Bearer {dummy}"));
    assert_eq!(
        brokered_credential_value_env_keys(&env),
        vec!["AUTH_HEADER", "STRIPE_API_KEY"]
    );
    let mut snapshot = format!("token={token}");
    assert!(broker.virtualize_text(&mut snapshot, &env));
    assert_eq!(snapshot, format!("token={dummy}"));
    assert!(broker.host_requires_mitm("api.stripe.com", /*port*/ 443));
    assert!(broker.host_requires_mitm("billing.stripe.example", /*port*/ 443));
    assert!(!broker.host_requires_mitm("stripe.example", /*port*/ 443));
    assert!(!broker.host_requires_mitm("attacker.example", /*port*/ 443));
    let dummy_header = format!("Bearer {dummy}");
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&dummy_header).expect("valid dummy authentication"),
    );
    broker.inject_request_headers("https://attacker.example/", &mut headers);
    assert_eq!(
        headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some(dummy_header.as_str())
    );
    env.remove("STRIPE_API_KEY");
    broker.virtualize_child_env(&mut env);
    assert_eq!(
        brokered_credential_marker_env_keys(&env),
        vec!["AUTH_HEADER", "STRIPE_API_KEY"]
    );
    assert_eq!(
        brokered_credential_value_env_keys(&env),
        vec!["AUTH_HEADER"]
    );
}

#[test]
fn configured_provider_preserves_credentials_without_a_resolved_destination() {
    let token = "stripe_live_abcdefghijklmnopqrstuvwx";
    let broker = broker_for(CredentialProviderConfig {
        env: vec!["STRIPE_API_KEY".to_string()],
        patterns: vec!["^stripe_live_[a-z]{24}$".to_string()],
        url_prefix_from_env: Some("STRIPE_HOST".to_string()),
        ..CredentialProviderConfig::default()
    });
    let mut bound_env = HashMap::from([
        ("STRIPE_API_KEY".to_string(), token.to_string()),
        ("STRIPE_HOST".to_string(), "api.stripe.com".to_string()),
    ]);
    broker.virtualize_child_env(&mut bound_env);
    assert_ne!(bound_env["STRIPE_API_KEY"], token);

    let mut env = HashMap::from([("STRIPE_API_KEY".to_string(), token.to_string())]);

    broker.virtualize_child_env(&mut env);

    assert_eq!(env["STRIPE_API_KEY"], token);
}

#[test]
fn configured_provider_accepts_anchored_alternatives_and_ascii_character_classes() {
    for (pattern, token) in [
        ("(?i)^token_[a-z]{24}$", "TOKEN_abcdefghijklmnopqrstuvwx"),
        ("(?i:^token_[a-z]{24}$)", "TOKEN_abcdefghijklmnopqrstuvwx"),
        (
            "(?x)   ^ token_[a-z]{24} $",
            "token_abcdefghijklmnopqrstuvwx",
        ),
        (
            "(?x)^token_[a-z]{24}$ # provider token",
            "token_abcdefghijklmnopqrstuvwx",
        ),
        (
            "(?x)^token_[a-z]{24} # [legacy provider\n$",
            "token_abcdefghijklmnopqrstuvwx",
        ),
        (r"\btoken_[a-z]{24}\b", "token_abcdefghijklmnopqrstuvwx"),
        (r"\Atoken_[a-z]{24}\z", "token_abcdefghijklmnopqrstuvwx"),
        (
            "^(token_[a-z]{8}|token_[a-z]{24})$",
            "token_abcdefghijklmnopqrstuvwx",
        ),
        (r"^token_\d{24}$", "token_012345678901234567890123"),
        ("^token_.{24}$", "token_abcdefghijklmnopqrstuvwx"),
        ("^token_[^:]{24}$", "token_abcdefghijklmnopqrstuvwx"),
        ("^token_[]$a-z]{8}$", "token_a$bcd]ef"),
    ] {
        let broker = broker_for(CredentialProviderConfig {
            env: vec!["PROVIDER_TOKEN".to_string()],
            patterns: vec![pattern.to_string()],
            url_prefixes: vec!["api.provider.example".to_string()],
            ..CredentialProviderConfig::default()
        });
        let mut env = HashMap::from([("PROVIDER_TOKEN".to_string(), token.to_string())]);

        broker.virtualize_child_env(&mut env);

        let dummy = &env["PROVIDER_TOKEN"];
        assert_ne!(dummy, token, "credential pattern: {pattern}");
        assert!(dummy.is_ascii(), "credential pattern: {pattern}");
        assert!(
            regex::Regex::new(pattern)
                .expect("valid credential pattern")
                .is_match(dummy),
            "credential pattern: {pattern}"
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {dummy}")).expect("valid dummy authentication"),
        );
        broker.inject_request_headers("https://api.provider.example/", &mut headers);
        assert_eq!(
            headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some(format!("Bearer {token}").as_str()),
            "credential pattern: {pattern}"
        );
    }
}

#[test]
fn configured_provider_generates_bounded_independent_dummies() {
    let long_token = format!("token_{}", "a".repeat(4096));
    let long_broker = broker_for(CredentialProviderConfig {
        env: vec!["PROVIDER_TOKEN".to_string()],
        patterns: vec!["^token_[a-z]+$".to_string()],
        url_prefixes: vec!["api.provider.example".to_string()],
        ..CredentialProviderConfig::default()
    });
    let mut long_env = HashMap::from([("PROVIDER_TOKEN".to_string(), long_token.clone())]);

    long_broker.virtualize_child_env(&mut long_env);

    let long_dummy = &long_env["PROVIDER_TOKEN"];
    assert_ne!(long_dummy, &long_token);
    assert!(long_dummy.len() <= 2048);

    let narrow_token = format!("token_{}", "a".repeat(64));
    for _ in 0..3 {
        let narrow_broker = broker_for(CredentialProviderConfig {
            env: vec!["PROVIDER_TOKEN".to_string()],
            patterns: vec!["^token_[ab]{64}$".to_string()],
            url_prefixes: vec!["api.provider.example".to_string()],
            ..CredentialProviderConfig::default()
        });
        let mut narrow_env = HashMap::from([("PROVIDER_TOKEN".to_string(), narrow_token.clone())]);

        narrow_broker.virtualize_child_env(&mut narrow_env);

        let changed = narrow_token
            .bytes()
            .zip(narrow_env["PROVIDER_TOKEN"].bytes())
            .filter(|(real, dummy)| real != dummy)
            .count();
        assert!(changed >= 12, "dummy changed only {changed} bytes");
    }
}

#[test]
fn configured_provider_reload_preserves_credentials_and_rejects_overlapping_sources() {
    let token = "provider_abcdefghijklmnopqrstuvwx";
    let provider = CredentialProviderConfig {
        env: vec!["PROVIDER_TOKEN".to_string()],
        patterns: vec!["provider_[a-z]{24}".to_string()],
        url_prefixes: vec!["api.provider.example".to_string()],
        ..CredentialProviderConfig::default()
    };
    let broker = broker_for(provider.clone());
    let mut env = HashMap::from([("PROVIDER_TOKEN".to_string(), token.to_string())]);
    broker.virtualize_child_env(&mut env);
    let dummy = env["PROVIDER_TOKEN"].clone();

    let overlapping_builtin = CredentialProviderConfig {
        env: vec!["GH_TOKEN".to_string()],
        url_prefixes: vec!["builtin-overlap.example".to_string()],
        ..provider.clone()
    };
    let overlapping_configured = CredentialProviderConfig {
        url_prefixes: vec!["configured-overlap.example".to_string()],
        ..provider.clone()
    };
    let unrelated = CredentialProviderConfig {
        env: vec!["ANOTHER_TOKEN".to_string()],
        url_prefixes: vec!["another.example".to_string()],
        ..provider.clone()
    };
    broker.configure(&NetworkProxyConfig {
        credential_broker: true,
        credential_providers: BTreeMap::from([
            ("aaa-overlap".to_string(), overlapping_configured),
            ("builtin".to_string(), overlapping_builtin),
            ("custom".to_string(), provider),
            ("second".to_string(), unrelated),
        ]),
        ..NetworkProxyConfig::default()
    });

    broker.virtualize_child_env(&mut env);
    assert_eq!(env["PROVIDER_TOKEN"], dummy);
    assert!(!broker.host_requires_mitm("builtin-overlap.example", /*port*/ 443));
    assert!(!broker.host_requires_mitm("configured-overlap.example", /*port*/ 443));
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {dummy}")).expect("valid authentication"),
    );
    broker.inject_request_headers("https://api.provider.example/", &mut headers);
    assert_eq!(
        headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some(format!("Bearer {token}").as_str())
    );
}

#[test]
fn configured_provider_preserves_bearer_token_basic_and_custom_header_auth() {
    let token = "provider_abcdefghijklmnopqrstuvwx";
    let github_token = "ghp_abcdefghijklmnopqrstuvwxyz1234567890";
    for (method, header_name, header_value) in [
        (
            CredentialAuthMethod::Bearer,
            "authorization",
            format!("Bearer {token}"),
        ),
        (
            CredentialAuthMethod::Token,
            "authorization",
            format!("token {token}"),
        ),
        (
            CredentialAuthMethod::Basic,
            "authorization",
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("user:{token}"))
            ),
        ),
        (
            CredentialAuthMethod::Header,
            "x-api-key",
            format!("Key {token}"),
        ),
    ] {
        let host = if method == CredentialAuthMethod::Header {
            "api.github.com"
        } else {
            "api.provider.example"
        };
        let broker = broker_for(CredentialProviderConfig {
            env: vec!["PROVIDER_TOKEN".to_string()],
            patterns: vec!["provider_[a-z]{24}".to_string()],
            url_prefixes: vec![host.to_string()],
            auth: if method == CredentialAuthMethod::Header {
                vec![CredentialAuthMethod::Bearer, method]
            } else {
                vec![method]
            },
            header: (method == CredentialAuthMethod::Header).then(|| "x-api-key".to_string()),
            prefix: (method == CredentialAuthMethod::Header).then(|| "Key ".to_string()),
            ..CredentialProviderConfig::default()
        });
        let mut env = HashMap::from([("PROVIDER_TOKEN".to_string(), token.to_string())]);
        if method == CredentialAuthMethod::Header {
            env.insert("GH_TOKEN".to_string(), github_token.to_string());
        }
        broker.virtualize_child_env(&mut env);
        let dummy = &env["PROVIDER_TOKEN"];
        let dummy_header = if method == CredentialAuthMethod::Basic {
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("user:{dummy}"))
            )
        } else {
            header_value.replace(token, dummy)
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            rama_http::HeaderName::from_bytes(header_name.as_bytes()).expect("valid header"),
            HeaderValue::from_str(&dummy_header).expect("valid dummy authentication"),
        );
        if method == CredentialAuthMethod::Header {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_static("Bearer unrelated-session-token"),
            );
        }

        broker.inject_request_headers(&format!("https://{host}/"), &mut headers);

        assert_eq!(
            headers
                .get(header_name)
                .and_then(|value| value.to_str().ok()),
            Some(header_value.as_str()),
            "authentication method: {method:?}"
        );
        if method == CredentialAuthMethod::Header {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {dummy}"))
                    .expect("valid dummy authentication"),
            );
            headers.insert(
                "x-api-key",
                HeaderValue::from_str(&dummy_header).expect("valid dummy authentication"),
            );
            broker.inject_request_headers(&format!("https://{host}/"), &mut headers);
            assert_eq!(
                headers
                    .get(AUTHORIZATION)
                    .and_then(|value| value.to_str().ok()),
                Some(format!("Bearer {token}").as_str())
            );
            assert_eq!(
                headers
                    .get("x-api-key")
                    .and_then(|value| value.to_str().ok()),
                Some(header_value.as_str())
            );

            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", env["GH_TOKEN"]))
                    .expect("valid dummy authentication"),
            );
            headers.insert(
                "x-api-key",
                HeaderValue::from_str(&dummy_header).expect("valid dummy authentication"),
            );
            broker.inject_request_headers(&format!("https://{host}/"), &mut headers);
            assert_eq!(
                headers
                    .get(AUTHORIZATION)
                    .and_then(|value| value.to_str().ok()),
                Some(format!("Bearer {github_token}").as_str())
            );
            assert_eq!(
                headers
                    .get("x-api-key")
                    .and_then(|value| value.to_str().ok()),
                Some(header_value.as_str())
            );
        }
    }
}

#[test]
fn configured_provider_translates_full_basic_pairs_only_on_exact_match() {
    let token = "user:provider_abcdefghijklmnopqrstuvwx:abcd";
    for (pattern, brokered) in [
        ("^user:provider_[a-z]{24}:[a-z]{4}$", true),
        ("^[a-z]{4}(?::provider_[a-z]{24}:[a-z]{4})?$", true),
        (
            "^(?:user:provider_abcdefghijklmnopqrstuvwx:abcd|[a-z]{32})$",
            false,
        ),
    ] {
        let broker = broker_for(CredentialProviderConfig {
            env: vec!["PROVIDER_CREDENTIALS".to_string()],
            patterns: vec![pattern.to_string()],
            url_prefixes: vec!["api.provider.example".to_string()],
            auth: vec![CredentialAuthMethod::Basic],
            ..CredentialProviderConfig::default()
        });
        let mut env = HashMap::from([("PROVIDER_CREDENTIALS".to_string(), token.to_string())]);
        broker.virtualize_child_env(&mut env);
        let dummy = &env["PROVIDER_CREDENTIALS"];
        assert_eq!(dummy != token, brokered);

        for (destination, suffix, injected) in [
            ("https://api.provider.example/", "", true),
            ("https://api.provider.example/", "extra", false),
            ("https://other.example/", "", false),
        ] {
            // curl --user adds an empty password when the argument has no colon.
            let pair = if dummy.contains(':') {
                dummy.clone()
            } else {
                format!("{dummy}:")
            };
            let input = format!("{pair}{suffix}");
            let header = |value: &str| {
                HeaderMap::from_iter([(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!(
                        "bAsIc {}",
                        base64::engine::general_purpose::STANDARD.encode(value)
                    ))
                    .expect("valid Basic authentication"),
                )])
            };
            let mut headers = header(&input);

            broker.inject_request_headers(destination, &mut headers);

            assert_eq!(headers, header(if injected { token } else { &input }));
        }
    }
}

#[test]
fn configured_provider_generates_unicode_basic_dummies() {
    for (pattern, secret, token) in [
        (
            "^pass_wörd[a-z]{8} $",
            "pass_wördabcdefgh ".to_string(),
            "pass_wördabcdefgh ".to_string(),
        ),
        (
            r"^pass_\B[éöü]{24}[a-z]$",
            "éöü".repeat(8),
            format!("pass_{}a", "éöü".repeat(8)),
        ),
        (r"^(?:α{32}|β{32})$", "α".repeat(32), "α".repeat(32)),
        (
            r"^(?:α{32}|β{32}|\x00{32})$",
            "α".repeat(32),
            "α".repeat(32),
        ),
        (
            r"^pass_\B(?:[éöü]{24}|[-+]{24})[a-z]$",
            "éöü".repeat(8),
            format!("pass_{}a", "éöü".repeat(8)),
        ),
    ] {
        let broker = broker_for(CredentialProviderConfig {
            env: vec!["PROVIDER_PASSWORD".to_string()],
            patterns: vec![pattern.to_string()],
            url_prefixes: vec!["api.provider.example".to_string()],
            auth: vec![CredentialAuthMethod::Basic],
            ..CredentialProviderConfig::default()
        });
        let mut env = HashMap::from([("PROVIDER_PASSWORD".to_string(), token.clone())]);

        broker.virtualize_child_env(&mut env);

        let dummy = &env["PROVIDER_PASSWORD"];
        assert_ne!(dummy, &token);
        assert!(!dummy.contains('\0'));
        assert!(!dummy.contains(&secret));
        assert!(regex::Regex::new(pattern).unwrap().is_match(dummy));
        let mut headers = HeaderMap::from_iter([(
            AUTHORIZATION,
            HeaderValue::from_str(&format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("user:{dummy}"))
            ))
            .expect("valid dummy authentication"),
        )]);
        broker.inject_request_headers("https://api.provider.example/", &mut headers);
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("user:{token}"))
        );
        assert_eq!(
            headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some(expected.as_str())
        );
    }
}

#[test]
fn configured_provider_does_not_break_usable_auth_methods_when_generating_dummies() {
    let token = "provider_abcdefghijklmnopqrstuvwx";
    for (alternative, method) in [
        "β{32}",
        "provider_[a-z]{23} ",
        " provider_[a-z]{23}",
        "provider_[a-z]{23}\\t",
        "provider_[a-z]{12}:[a-z]{12}",
    ]
    .into_iter()
    .flat_map(|alternative| {
        [CredentialAuthMethod::Bearer, CredentialAuthMethod::Header]
            .map(|method| (alternative, method))
    })
    .chain(std::iter::once((r"\x00{24}", CredentialAuthMethod::Basic)))
    {
        let broker = broker_for(CredentialProviderConfig {
            env: vec!["PROVIDER_PASSWORD".to_string()],
            patterns: vec![format!("^(?:{token}|{alternative})$")],
            url_prefixes: vec!["api.provider.example".to_string()],
            auth: vec![method, CredentialAuthMethod::Basic],
            header: (method == CredentialAuthMethod::Header).then(|| "x-api-key".to_string()),
            ..CredentialProviderConfig::default()
        });
        let mut env = HashMap::from([("PROVIDER_PASSWORD".to_string(), token.to_string())]);

        broker.virtualize_child_env(&mut env);

        assert_eq!(env["PROVIDER_PASSWORD"], token);
    }
}

#[test]
fn configured_basic_dummies_preserve_username_password_and_whole_value_auth() {
    let token = "provider_abcdefghijklmnopqrstuvwx";
    let second = "provider_zyxwvutsrqponmlkjihgfedcb";
    let github = "ghp_abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH";
    let broker = broker_for(CredentialProviderConfig {
        env: vec![
            "PROVIDER_TOKEN".to_string(),
            "PROVIDER_SECRET".to_string(),
            "PROVIDER_COPY".to_string(),
        ],
        patterns: vec!["^provider_(?:[a-z]{24}|[a-z]{12}:[a-z]{12})$".to_string()],
        url_prefixes: vec!["https://api.github.com/v1".to_string()],
        auth: vec![CredentialAuthMethod::Basic],
        ..CredentialProviderConfig::default()
    });
    let mut env = HashMap::from([
        ("PROVIDER_TOKEN".to_string(), token.to_string()),
        ("PROVIDER_SECRET".to_string(), second.to_string()),
        ("PROVIDER_COPY".to_string(), token.to_string()),
        ("GH_TOKEN".to_string(), github.to_string()),
    ]);
    broker.virtualize_child_env(&mut env);
    let dummy = &env["PROVIDER_TOKEN"];
    assert_ne!(dummy, token);
    assert!(!dummy.contains(':'));
    for (input, expected) in [
        (
            format!("{dummy}:x-oauth-basic"),
            format!("{token}:x-oauth-basic"),
        ),
        (format!("user:{dummy}"), format!("user:{token}")),
        (format!("{dummy}:{dummy}"), format!("{token}:{token}")),
        (
            format!("{dummy}:{}", env["PROVIDER_COPY"]),
            format!("{token}:{token}"),
        ),
        (
            format!("{}:{dummy}", env["PROVIDER_COPY"]),
            format!("{token}:{token}"),
        ),
        (
            format!("{dummy}:{}", env["PROVIDER_SECRET"]),
            format!("{token}:{second}"),
        ),
        (
            format!("{}:{dummy}", env["PROVIDER_SECRET"]),
            format!("{second}:{token}"),
        ),
        (
            format!("{}:{dummy}", env["GH_TOKEN"]),
            format!("{github}:{token}"),
        ),
        (
            format!("{dummy}:{}", env["GH_TOKEN"]),
            format!("{token}:{github}"),
        ),
        (dummy.clone(), token.to_string()),
    ] {
        let headers_for = |value: &str| {
            HeaderMap::from_iter([(
                AUTHORIZATION,
                HeaderValue::from_str(&format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(value)
                ))
                .unwrap(),
            )])
        };
        let mut headers = headers_for(&input);
        broker.inject_request_headers("https://other.example/", &mut headers);
        assert_eq!(headers, headers_for(&input));
        broker.inject_request_headers(
            "https://api.github.com/public/%2e%2e/v1/models",
            &mut headers,
        );
        assert_eq!(headers, headers_for(&input));
        broker.inject_request_headers("https://api.github.com/v1/models", &mut headers);
        assert_eq!(headers, headers_for(&expected));
    }
}

#[test]
fn configured_provider_destination_history_is_scoped_to_the_environment() {
    let token = "stripe_live_abcdefghijklmnopqrstuvwx";
    let broker = broker_for(CredentialProviderConfig {
        env: vec!["STRIPE_API_KEY".to_string()],
        patterns: vec!["^stripe_live_[a-z]{24}$".to_string()],
        url_prefixes: vec!["static.example".to_string()],
        url_prefix_from_env: Some("STRIPE_HOST".to_string()),
        ..CredentialProviderConfig::default()
    });
    let mut first_env = HashMap::from([
        ("STRIPE_API_KEY".to_string(), token.to_string()),
        ("STRIPE_HOST".to_string(), "first.example".to_string()),
    ]);
    broker.virtualize_child_env_for_environment(&mut first_env, Some("first-environment"));
    let first_dummy = first_env["STRIPE_API_KEY"].clone();
    let mut second_env = HashMap::from([
        ("STRIPE_HOST".to_string(), "second.example".to_string()),
        ("AUTH_HEADER".to_string(), format!("Bearer {first_dummy}")),
    ]);

    broker.virtualize_child_env_for_environment(&mut second_env, Some("second-environment"));

    let second_dummy = second_env["AUTH_HEADER"]
        .strip_prefix("Bearer ")
        .expect("dummy bearer credential")
        .to_string();
    assert_eq!(second_dummy, first_dummy);
    assert!(!second_env.contains_key("STRIPE_API_KEY"));
    assert_eq!(second_env["AUTH_HEADER"], format!("Bearer {second_dummy}"));
    let translated = |environment_id: &str, destination: &str, dummy: &str| {
        let mut headers = HeaderMap::from_iter([(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {dummy}")).expect("valid dummy authentication"),
        )]);
        broker.inject_request_headers_for_environment(
            &format!("https://{destination}/"),
            &mut headers,
            Some(environment_id),
        );
        headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .expect("authorization header")
            .to_string()
    };
    assert_eq!(
        translated("first-environment", "first.example", &first_dummy),
        format!("Bearer {token}")
    );
    assert_eq!(
        translated("second-environment", "second.example", &second_dummy),
        format!("Bearer {token}")
    );
    assert_eq!(
        translated("first-environment", "static.example", &first_dummy),
        format!("Bearer {token}")
    );

    second_env.insert("STRIPE_HOST".to_string(), "third.example".to_string());
    broker.virtualize_child_env_for_environment(&mut second_env, Some("second-environment"));

    assert_eq!(second_env["AUTH_HEADER"], format!("Bearer {second_dummy}"));
    assert_eq!(
        translated("second-environment", "second.example", &second_dummy),
        format!("Bearer {token}")
    );
    assert_eq!(
        translated("second-environment", "third.example", &second_dummy),
        format!("Bearer {token}")
    );
    assert_eq!(
        translated("first-environment", "first.example", &first_dummy),
        format!("Bearer {token}")
    );
    assert_eq!(
        translated("second-environment", "first.example", &second_dummy),
        format!("Bearer {second_dummy}")
    );
    for host in ["second.example", "third.example"] {
        assert_eq!(
            translated("first-environment", host, &first_dummy),
            format!("Bearer {first_dummy}")
        );
    }

    let revision = broker.config_revision();
    broker.configure(&NetworkProxyConfig {
        credential_broker: true,
        ..NetworkProxyConfig::default()
    });
    assert_eq!(broker.config_revision(), revision + 1);
    for host in ["second.example", "third.example", "static.example"] {
        assert_eq!(
            translated("second-environment", host, &second_dummy),
            format!("Bearer {second_dummy}")
        );
        assert!(!broker.host_requires_mitm_for_environment(
            host,
            /*port*/ 443,
            Some("second-environment"),
        ));
    }
}

#[test]
fn configured_provider_restores_credentials_when_the_dynamic_destination_disappears() {
    let token = "stripe_live_abcdefghijklmnopqrstuvwx";
    let broker = broker_for(CredentialProviderConfig {
        env: vec!["STRIPE_API_KEY".to_string()],
        patterns: vec!["^stripe_live_[a-z]{24}$".to_string()],
        url_prefix_from_env: Some("STRIPE_HOST".to_string()),
        ..CredentialProviderConfig::default()
    });
    let mut env = HashMap::from([
        ("STRIPE_API_KEY".to_string(), token.to_string()),
        ("STRIPE_HOST".to_string(), "first.example".to_string()),
        ("AUTH_HEADER".to_string(), format!("Bearer {token}")),
    ]);
    broker.virtualize_child_env_for_environment(&mut env, Some("environment"));
    let dummy = env["STRIPE_API_KEY"].clone();

    env.remove("STRIPE_HOST");
    broker.virtualize_child_env_for_environment(&mut env, Some("environment"));

    assert_eq!(env["STRIPE_API_KEY"], token);
    assert_eq!(env["AUTH_HEADER"], format!("Bearer {token}"));
    let mut headers = HeaderMap::from_iter([(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {dummy}")).expect("valid dummy authentication"),
    )]);
    broker.inject_request_headers_for_environment(
        "https://first.example/",
        &mut headers,
        Some("environment"),
    );
    assert_eq!(
        headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some(format!("Bearer {dummy}").as_str())
    );
    assert!(!broker.host_requires_mitm_for_environment(
        "first.example",
        /*port*/ 443,
        Some("environment")
    ));
}

#[test]
fn explicit_invalid_destinations_clear_previous_dynamic_bindings() {
    for (key, host_key, token, static_host) in [
        (
            "GH_ENTERPRISE_TOKEN",
            "GH_HOST",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            None,
        ),
        (
            "GITHUB_ENTERPRISE_TOKEN",
            "GH_HOST",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            None,
        ),
        (
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789",
            Some("api.openai.com"),
        ),
        (
            "PROVIDER_TOKEN",
            "PROVIDER_ENDPOINT",
            "provider_abcdefghijklmnopqrstuvwx",
            Some("static.example"),
        ),
        (
            "PROVIDER_TOKEN",
            "PROVIDER_ENDPOINT",
            "provider_abcdefghijklmnopqrstuvwx",
            None,
        ),
    ] {
        for use_real in [false, true] {
            for use_alias in [false, true] {
                for invalid in [
                    "",
                    " ",
                    "not a valid destination",
                    "http://untrusted.example",
                ]
                .into_iter()
                .chain((key == "PROVIDER_TOKEN").then_some("https://*.example"))
                .chain((host_key == "GH_HOST").then_some("github.com"))
                .chain(
                    (host_key == "GH_HOST")
                        .then_some([
                            "second.example:bad-port",
                            "127.0.0.1:bad-port",
                            "[::1]bad",
                            "[::1]:bad-port",
                            "second.example:99999",
                            "[example.com]",
                        ])
                        .into_iter()
                        .flatten(),
                ) {
                    let broker = broker_for(CredentialProviderConfig {
                        env: vec!["PROVIDER_TOKEN".to_string()],
                        patterns: vec!["^provider_[a-z]{24}$".to_string()],
                        url_prefixes: static_host
                            .map(|host| format!("https://{host}"))
                            .into_iter()
                            .collect(),
                        url_prefix_from_env: Some("PROVIDER_ENDPOINT".to_string()),
                        ..CredentialProviderConfig::default()
                    });
                    let mut env = HashMap::from([
                        (key.to_string(), token.to_string()),
                        (
                            host_key.to_string(),
                            if host_key == "GH_HOST" {
                                "first.example".to_string()
                            } else {
                                "https://first.example/v1".to_string()
                            },
                        ),
                    ]);
                    broker.virtualize_child_env(&mut env);
                    let dummy = env[key].clone();
                    let second_host = static_host.unwrap_or("second.example");
                    env.insert(
                        host_key.to_string(),
                        if host_key == "GH_HOST" {
                            second_host.to_string()
                        } else {
                            format!("https://{second_host}/v1")
                        },
                    );
                    broker.virtualize_child_env(&mut env);
                    assert!(broker.host_requires_mitm("first.example", /*port*/ 443));
                    env.clear();
                    let value = if use_real { token } else { &dummy };
                    let (input_key, input_value) = if use_alias {
                        ("AUTH_HEADER", format!("Bearer {value}"))
                    } else {
                        (key, value.to_string())
                    };
                    env.insert(input_key.to_string(), input_value);
                    env.insert(host_key.to_string(), invalid.to_string());
                    broker.virtualize_child_env(&mut env);

                    for host in ["first.example", second_host] {
                        let expected = if static_host == Some(host) {
                            token
                        } else {
                            &dummy
                        };
                        let mut headers = HeaderMap::from_iter([(
                            AUTHORIZATION,
                            HeaderValue::from_str(&format!("Bearer {dummy}")).unwrap(),
                        )]);
                        broker.inject_request_headers(&format!("https://{host}/v1"), &mut headers);
                        assert_eq!(
                            headers[AUTHORIZATION],
                            format!("Bearer {expected}"),
                            "{key}, {invalid:?}, {host}, real={use_real}, alias={use_alias}"
                        );
                    }
                    assert!(!broker.host_requires_mitm("first.example", /*port*/ 443));
                    if host_key == "GH_HOST" {
                        for _ in 0..2 {
                            broker.virtualize_child_env(&mut env);
                            assert_eq!(
                                env[input_key],
                                if use_alias {
                                    format!("Bearer {token}")
                                } else {
                                    token.to_string()
                                },
                                "{key}, {invalid:?}, real={use_real}, alias={use_alias}"
                            );
                            for cloud in ["api.github.com", "github.com", "tenant.ghe.com"] {
                                assert!(!broker.host_requires_mitm(cloud, /*port*/ 443));
                            }
                        }

                        env.insert(host_key.to_string(), "restored.example".to_string());
                        broker.virtualize_child_env(&mut env);
                        let restored_dummy = if use_alias {
                            env[input_key].strip_prefix("Bearer ").unwrap()
                        } else {
                            &env[input_key]
                        };
                        assert_ne!(restored_dummy, token);
                        let mut headers = HeaderMap::from_iter([(
                            AUTHORIZATION,
                            HeaderValue::from_str(&format!("Bearer {restored_dummy}")).unwrap(),
                        )]);
                        broker.inject_request_headers("https://restored.example/", &mut headers);
                        assert_eq!(headers[AUTHORIZATION], format!("Bearer {token}"));
                        assert!(!broker.host_requires_mitm("first.example", /*port*/ 443));
                        assert!(!broker.host_requires_mitm("second.example", /*port*/ 443));
                        assert!(!broker.host_requires_mitm("api.github.com", /*port*/ 443));
                        env.clear();
                        env.insert("GH_TOKEN".to_string(), token.to_string());
                        broker.virtualize_child_env(&mut env);
                        let mut headers = HeaderMap::from_iter([(
                            AUTHORIZATION,
                            HeaderValue::from_str(&format!("Bearer {}", env["GH_TOKEN"])).unwrap(),
                        )]);
                        broker.inject_request_headers("https://api.github.com/", &mut headers);
                        assert_eq!(headers[AUTHORIZATION], format!("Bearer {token}"));
                    }
                }
            }
        }
    }
}

#[test]
fn configured_provider_limits_injection_to_url_prefixes() {
    let token = "provider_abcdefghijklmnopqrstuvwx";
    let broker = broker_for(CredentialProviderConfig {
        env: vec!["PROVIDER_TOKEN".to_string()],
        patterns: vec!["provider_[a-z]{24}".to_string()],
        url_prefixes: vec![
            "https://root.provider.example".to_string(),
            "https://api.provider.example/v1".to_string(),
            "enterprise.example/v2/".to_string(),
            "https://*.provider.example:8443/private".to_string(),
            "http://localhost:443/v1".to_string(),
            "127.0.0.1:443/v1".to_string(),
            "http://[::1]:443/v1".to_string(),
            "http://localhost:8080/v1".to_string(),
            "localhost/v2".to_string(),
            "https://localhost:443/v3".to_string(),
            "https://localhost:80/v4".to_string(),
        ],
        ..CredentialProviderConfig::default()
    });
    let mut env = HashMap::from([("PROVIDER_TOKEN".to_string(), token.to_string())]);
    broker.virtualize_child_env(&mut env);
    let dummy = &env["PROVIDER_TOKEN"];
    assert!(broker.host_requires_mitm("team.provider.example", /*port*/ 8443));
    assert!(!broker.host_requires_mitm("team.provider.example", /*port*/ 443));

    for (destination, injected) in [
        ("root.provider.example", false),
        ("https://root.provider.example/models", true),
        ("https://api.provider.example/v1", true),
        ("https://api.provider.example/v1/models?limit=1", true),
        ("https://api.provider.example/v10/models", false),
        ("https://api.provider.example/private", false),
        (
            "https://api.provider.example/public/%2e%2e/v1/models",
            false,
        ),
        ("https://api.provider.example/v1%2f../private", false),
        ("http://api.provider.example/v1", false),
        ("https://enterprise.example/v2/models", true),
        ("https://enterprise.example/v2", false),
        ("https://team.provider.example:8443/private/models", true),
        ("https://team.provider.example/private/models", false),
        ("https://provider.example:8443/private/models", false),
        ("http://localhost:443/v1/models", true),
        ("http://localhost/v1/models", false),
        ("https://localhost/v1/models", false),
        ("http://127.0.0.1:443/v1/models", true),
        ("http://127.0.0.1/v1/models", false),
        ("http://[::1]:443/v1/models", true),
        ("http://[::1]/v1/models", false),
        ("http://localhost:8080/v1/models", true),
        ("http://localhost/v2/models", true),
        ("http://localhost:443/v2/models", false),
        ("https://localhost/v3/models", true),
        ("http://localhost:443/v3/models", false),
        ("https://localhost:80/v4/models", true),
        ("https://localhost/v4/models", false),
    ] {
        let dummy_header = format!("Bearer {dummy}");
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&dummy_header).expect("valid dummy authentication"),
        );

        broker.inject_request_headers(destination, &mut headers);

        let expected = if injected {
            format!("Bearer {token}")
        } else {
            dummy_header
        };
        assert_eq!(
            headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some(expected.as_str()),
            "destination: {destination}"
        );
    }
}

#[test]
fn configured_provider_accepts_hostname_or_https_url_from_one_environment_key() {
    let token = "provider_abcdefghijklmnopqrstuvwx";
    for (host_value, expected_host) in [
        ("enterprise.example", Some("enterprise.example")),
        ("https://gateway.example/v1", Some("gateway.example")),
        ("http://plaintext.example/v1", None),
    ] {
        let broker = broker_for(CredentialProviderConfig {
            env: vec!["PROVIDER_TOKEN".to_string()],
            patterns: vec!["provider_[a-z]{24}".to_string()],
            url_prefix_from_env: Some("PROVIDER_HOST".to_string()),
            ..CredentialProviderConfig::default()
        });
        let mut env = HashMap::from([
            ("PROVIDER_TOKEN".to_string(), token.to_string()),
            ("PROVIDER_HOST".to_string(), host_value.to_string()),
        ]);

        broker.virtualize_child_env(&mut env);

        match expected_host {
            Some(host) => {
                assert_ne!(env["PROVIDER_TOKEN"], token);
                assert!(broker.host_requires_mitm(host, /*port*/ 443));
            }
            None => {
                assert_eq!(env["PROVIDER_TOKEN"], token);
                let mut snapshot = token.to_string();
                assert!(broker.virtualize_text(&mut snapshot, &env));
                assert_eq!(snapshot, token);
            }
        }
    }
}

#[test]
fn configured_provider_rejects_unsafe_destinations_and_unusable_dummy_patterns() {
    let impossible_assertion = CredentialProviderConfig {
        env: vec!["PROVIDER_PASSWORD".to_string()],
        patterns: vec![r"^pass_\b[a-z]{24}$".to_string()],
        url_prefixes: vec!["api.example".to_string()],
        ..CredentialProviderConfig::default()
    };
    assert!(
        super::configured::ConfiguredCredentialProvider::compile("custom", &impossible_assertion)
            .is_err()
    );

    for (pattern, url_prefixes, first, second) in [
        (
            "provider_[a-z]{24}",
            vec!["*"],
            "provider_abcdefghijklmnopqrstuvwx",
            None,
        ),
        (
            "token_[01]",
            vec!["api.example"],
            "token_0",
            Some("token_1"),
        ),
        (
            "only_one_token",
            vec!["api.example"],
            "only_one_token",
            None,
        ),
    ] {
        let broker = broker_for(CredentialProviderConfig {
            env: vec!["FIRST_TOKEN".to_string(), "SECOND_TOKEN".to_string()],
            patterns: vec![pattern.to_string()],
            url_prefixes: url_prefixes.into_iter().map(str::to_string).collect(),
            ..CredentialProviderConfig::default()
        });
        let mut env = HashMap::from([("FIRST_TOKEN".to_string(), first.to_string())]);
        if let Some(second) = second {
            env.insert("SECOND_TOKEN".to_string(), second.to_string());
        }

        broker.virtualize_child_env(&mut env);

        assert_eq!(env["FIRST_TOKEN"], first);
        if let Some(second) = second {
            assert_eq!(env["SECOND_TOKEN"], second);
        }
        let mut snapshot = first.to_string();
        assert!(broker.virtualize_text(&mut snapshot, &env));
        assert_eq!(snapshot, first);
    }
}
