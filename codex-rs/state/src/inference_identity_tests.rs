use codex_protocol::models::ThreadInferenceIdentity;
use codex_protocol::models::ThreadInferenceIdentityAuthority;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;

use super::ThreadInferenceIdentityAuthorityEncodeError;
use super::decode_thread_inference_identity_authority;
use super::encode_thread_inference_identity_authority;

#[test]
fn authority_codec_enforces_strict_v1_wire_and_preserves_raw_diagnostics() {
    let valid = ThreadInferenceIdentityAuthority::Valid(
        ThreadInferenceIdentity::new(
            "configured-alias",
            "test-provider",
            Some(ReasoningEffort::High),
        )
        .expect("identity should be valid"),
    );
    let valid_json = r#"{"version":1,"authority":{"status":"valid","value":{"model":"configured-alias","model_provider_id":"test-provider","reasoning_effort":"high"}}}"#;
    assert_eq!(
        encode_thread_inference_identity_authority(&valid)
            .expect("valid authority should encode")
            .as_deref(),
        Some(valid_json)
    );
    assert_eq!(
        decode_thread_inference_identity_authority(Some(valid_json)),
        valid
    );

    let cleared = ThreadInferenceIdentityAuthority::cleared();
    let cleared_json = r#"{"version":1,"authority":{"status":"cleared","value":{}}}"#;
    assert_eq!(
        encode_thread_inference_identity_authority(&cleared)
            .expect("cleared authority should encode")
            .as_deref(),
        Some(cleared_json)
    );
    assert_eq!(
        decode_thread_inference_identity_authority(Some(cleared_json)),
        cleared
    );
    assert_eq!(
        encode_thread_inference_identity_authority(
            &ThreadInferenceIdentityAuthority::LegacyMissing
        )
        .expect("legacy absence should encode"),
        None
    );
    assert_eq!(
        decode_thread_inference_identity_authority(/*raw*/ None),
        ThreadInferenceIdentityAuthority::LegacyMissing
    );

    let explicit_null = r#"{"version":1,"authority":{"status":"valid","value":{"model":"model","model_provider_id":"provider","reasoning_effort":null}}}"#;
    let no_effort = ThreadInferenceIdentityAuthority::Valid(
        ThreadInferenceIdentity::new("model", "provider", /*reasoning_effort*/ None)
            .expect("identity without effort should be valid"),
    );
    assert_eq!(
        decode_thread_inference_identity_authority(Some(explicit_null)),
        no_effort
    );
    assert_eq!(
        encode_thread_inference_identity_authority(&no_effort)
            .expect("explicit null effort should encode")
            .as_deref(),
        Some(explicit_null)
    );

    for raw in [
        "{malformed",
        r#"{"version":2,"authority":{"status":"cleared","value":{}}}"#,
        r#"{"version":1,"authority":{"status":"legacy_missing"}}"#,
        r#"{"version":1,"authority":{"status":"malformed","value":{"raw":"nested"}}}"#,
        r#"{"version":1,"authority":{"status":"valid","value":{"model":"model","model_provider_id":"provider"}}}"#,
        r#"{"version":1,"authority":{"status":"valid","value":{"model":" ","model_provider_id":"provider","reasoning_effort":null}}}"#,
        r#"{"version":1,"authority":{"status":"valid","value":{"model":"model","model_provider_id":"\t","reasoning_effort":null}}}"#,
        r#"{"version":1,"authority":{"status":"cleared","value":{}},"extra":true}"#,
        r#"{"version":1,"authority":{"status":"cleared","value":{},"extra":true}}"#,
        r#"{"version":1,"authority":{"status":"valid","value":{"model":"model","model_provider_id":"provider","reasoning_effort":null,"extra":true}}}"#,
        r#"{"version":1,"authority":{"status":"cleared","value":{"extra":true}}}"#,
    ] {
        assert_malformed(raw);
    }
    assert_malformed(
        r#" { "authority": { "value": { "reasoning_effort": null, "model_provider_id": "provider", "model": "\u0020" }, "status": "valid" }, "version": 1 } "#,
    );

    assert!(matches!(
        encode_thread_inference_identity_authority(&ThreadInferenceIdentityAuthority::Malformed {
            raw: "diagnostic".to_string(),
        }),
        Err(ThreadInferenceIdentityAuthorityEncodeError::MalformedAuthority)
    ));
}

fn assert_malformed(raw: &str) {
    assert_eq!(
        decode_thread_inference_identity_authority(Some(raw)),
        ThreadInferenceIdentityAuthority::Malformed {
            raw: raw.to_string(),
        }
    );
}
