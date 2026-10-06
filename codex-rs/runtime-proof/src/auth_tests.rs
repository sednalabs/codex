//! Protected-failure and per-invocation secret-redaction controls.

use super::BootstrapAuth;
use super::McpRedactionContext;
use super::clear_auth_state;
use super::failed_target_match;
use super::protected_mcp_target_from_store;
use pretty_assertions::assert_eq;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use zeroize::Zeroizing;

#[test]
fn protected_runtime_failure_never_downgrades_to_unconfigured_target() {
    assert!(failed_target_match("other", "https://other.invalid/mcp").is_err());
}

#[test]
fn invalidation_between_getter_precheck_and_lock_still_fails_closed() {
    let ever_active = AtomicBool::new(true);
    let failed = AtomicBool::new(false);
    let store = Mutex::new(Some(BootstrapAuth {
        home: std::path::PathBuf::new(),
        server: "ops".to_string(),
        recipient: "https://ops.example/mcp".to_string(),
        provider_recipient: "https://api.example/v1".to_string(),
        expires_at: i64::MAX,
        bearer: Zeroizing::new("synthetic protected credential".to_string()),
    }));
    let (prechecked_tx, prechecked_rx) = mpsc::channel();
    let (continue_tx, continue_rx) = mpsc::channel();

    thread::scope(|scope| {
        let getter_store = &store;
        let getter_failed = &failed;
        let getter = scope.spawn(move || {
            assert!(!getter_failed.load(Ordering::Acquire));
            prechecked_tx.send(()).unwrap();
            continue_rx.recv().unwrap();

            protected_mcp_target_from_store(getter_store, getter_failed)
        });

        prechecked_rx.recv().unwrap();
        {
            let mut state = store.lock().unwrap();
            clear_auth_state(&mut state, &ever_active, &failed);
        }
        continue_tx.send(()).unwrap();

        let result = getter.join().unwrap();
        assert!(result.is_err(), "getter must not return ordinary absence");
        assert!(
            store.lock().unwrap().is_none(),
            "invalidation cleared the protected state"
        );
        assert!(
            failed.load(Ordering::Acquire),
            "invalidation latched failure"
        );
    });
}

#[test]
fn redaction_scrubs_json_values_and_keys_without_overwriting_collisions() {
    let context = McpRedactionContext {
        bearer: Zeroizing::new("private-bearer".to_string()),
        proof_tokens: vec![Zeroizing::new("private-proof".to_string())],
    };
    let mut value = serde_json::json!({
        "private-bearer": "private-proof",
        "[runtime secret redacted]": "keep",
        "nested": [{"echo": "Bearer private-bearer"}]
    });
    context.redact(&mut value);
    assert_eq!(
        value,
        serde_json::json!({
            "[runtime secret redacted] 1": "[runtime secret redacted]",
            "[runtime secret redacted]": "keep",
            "nested": [{"echo": "Bearer [runtime secret redacted]"}]
        })
    );
}

#[test]
fn protected_cloud_config_refuses_eligible_or_unclassified_accounts() {
    for plan in [
        "business",
        "ent26",
        "enterprise_cbp_usage_based",
        "enterprise",
        "hc",
        "edu",
        "education",
        "future_plan",
    ] {
        assert!(
            crate::provider_auth::ensure_protected_cloud_config_ineligible(Some(plan)).is_err(),
            "plan {plan} must not start a credential-bearing cloud-config loader"
        );
    }
    assert!(
        crate::provider_auth::ensure_protected_cloud_config_ineligible(/*plan_type*/ None).is_err()
    );
    for plan in [
        "free",
        "go",
        "plus",
        "pro",
        "prolite",
        "team",
        "self_serve_business_usage_based",
    ] {
        assert!(
            crate::provider_auth::ensure_protected_cloud_config_ineligible(Some(plan)).is_ok(),
            "known cloud-config-ineligible plan {plan} remains supported"
        );
    }
}
