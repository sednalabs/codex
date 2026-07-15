use codex_protocol::models::ThreadInferenceIdentity;
use codex_protocol::models::ThreadInferenceIdentityAuthority;
use pretty_assertions::assert_eq;

use super::ThreadInferenceIdentitySidecar;
use super::ThreadInferenceIdentitySidecarPatch;

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

#[test]
fn inference_identity_sidecar_patch_has_strict_presence_serde_contract() {
    let identity =
        ThreadInferenceIdentity::new("m", "p", /*reasoning_effort*/ None).expect("valid identity");
    let valid = r#"{"configured":{"model":"m","model_provider_id":"p","reasoning_effort":null}}"#;
    let expected = |configured, latest_request| ThreadInferenceIdentitySidecarPatch {
        configured,
        latest_request,
    };
    let cases = [
        ("{}", expected(None, None)),
        (r#"{"configured":null}"#, expected(Some(None), None)),
        (valid, expected(Some(Some(identity)), None)),
        (
            r#"{"configured":null,"latest_request":null}"#,
            expected(Some(None), Some(None)),
        ),
    ];
    for (raw, expected) in cases {
        let decoded =
            serde_json::from_str::<ThreadInferenceIdentitySidecarPatch>(raw).expect("valid patch");
        assert_eq!(decoded, expected);
        assert_eq!(serde_json::to_string(&decoded).expect("encode patch"), raw);
    }
    for invalid in [
        r#"{"configuredd":null}"#,
        r#"{"configured":{"status":"malformed","raw":"bad"}}"#,
        r#"{"configured":{"model":"m","model_provider_id":"p","reasoning_effort":null,"status":"valid","raw":"bad"}}"#,
        r#"{"configured":{"model":"","model_provider_id":"p","reasoning_effort":null}}"#,
        r#"{"configured":null,"configured":null}"#,
    ] {
        assert!(serde_json::from_str::<ThreadInferenceIdentitySidecarPatch>(invalid).is_err());
    }
}
