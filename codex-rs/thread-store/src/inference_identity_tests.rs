use codex_protocol::models::ThreadInferenceIdentity;
use codex_protocol::models::ThreadInferenceIdentityAuthority;
use pretty_assertions::assert_eq;

use super::ThreadInferenceIdentitySidecar;

#[test]
fn inference_identity_sidecar_has_exact_json_defaults_and_legacy_missing() {
    let default_json = r#"{"configured":{"status":"legacy_missing"},"latest_request":{"status":"legacy_missing"}}"#;
    assert_eq!(
        serde_json::to_string(&ThreadInferenceIdentitySidecar::default())
            .expect("default sidecar should serialize"),
        default_json
    );
    assert_eq!(
        serde_json::from_str::<ThreadInferenceIdentitySidecar>("{}")
            .expect("missing fields should default"),
        ThreadInferenceIdentitySidecar::default()
    );

    let configured = ThreadInferenceIdentityAuthority::Valid(
        ThreadInferenceIdentity::new("configured", "provider", /*reasoning_effort*/ None)
            .expect("identity should be valid"),
    );
    let sidecar = ThreadInferenceIdentitySidecar {
        configured: configured.clone(),
        latest_request: ThreadInferenceIdentityAuthority::cleared(),
    };
    let exact_json = r#"{"configured":{"status":"valid","value":{"model":"configured","model_provider_id":"provider","reasoning_effort":null}},"latest_request":{"status":"cleared"}}"#;
    assert_eq!(
        serde_json::to_string(&sidecar).expect("sidecar should serialize"),
        exact_json
    );
    assert_eq!(
        serde_json::from_str::<ThreadInferenceIdentitySidecar>(exact_json)
            .expect("sidecar should deserialize"),
        sidecar
    );

    let configured_only = r#"{"configured":{"status":"valid","value":{"model":"configured","model_provider_id":"provider","reasoning_effort":null}}}"#;
    assert_eq!(
        serde_json::from_str::<ThreadInferenceIdentitySidecar>(configured_only)
            .expect("legacy sidecar should default the missing field"),
        ThreadInferenceIdentitySidecar {
            configured,
            latest_request: ThreadInferenceIdentityAuthority::LegacyMissing,
        }
    );

    let malformed = ThreadInferenceIdentitySidecar {
        configured: ThreadInferenceIdentityAuthority::Malformed {
            raw: "{exact invalid configured}".to_string(),
        },
        latest_request: ThreadInferenceIdentityAuthority::Malformed {
            raw: "[exact invalid request]".to_string(),
        },
    };
    assert_eq!(
        serde_json::from_str::<ThreadInferenceIdentitySidecar>(
            &serde_json::to_string(&malformed).expect("malformed sidecar should serialize"),
        )
        .expect("malformed sidecar should deserialize"),
        malformed
    );
}
