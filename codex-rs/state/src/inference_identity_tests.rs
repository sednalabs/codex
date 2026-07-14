use codex_protocol::models::ThreadInferenceIdentity;
use codex_protocol::models::ThreadInferenceIdentityAuthority;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;

use super::ThreadInferenceIdentityAuthorityEncodeError;
use super::decode_thread_inference_identity_authority;
use super::encode_thread_inference_identity_authority;

#[test]
fn authority_codec_round_trips_valid_clear_and_legacy_missing() {
    let valid = ThreadInferenceIdentityAuthority::Valid(
        ThreadInferenceIdentity::new(
            "configured-alias",
            "test-provider",
            Some(ReasoningEffort::High),
        )
        .expect("identity should be valid"),
    );
    for authority in [valid, ThreadInferenceIdentityAuthority::Cleared] {
        let encoded = encode_thread_inference_identity_authority(&authority)
            .expect("authority should encode");
        assert_eq!(
            decode_thread_inference_identity_authority(encoded.as_deref()),
            authority
        );
    }
    assert_eq!(
        encode_thread_inference_identity_authority(
            &ThreadInferenceIdentityAuthority::LegacyMissing
        )
        .expect("legacy absence should encode"),
        None
    );
    assert_eq!(
        decode_thread_inference_identity_authority(None),
        ThreadInferenceIdentityAuthority::LegacyMissing
    );
}

#[test]
fn authority_codec_preserves_exact_malformed_diagnostics() {
    let malformed = [
        "{malformed",
        r#"{"version":2,"authority":{"status":"cleared"}}"#,
        r#"{"version":1,"authority":{"status":"legacy_missing"}}"#,
        r#"{"version":1,"authority":{"status":"malformed","value":{"raw":"nested"}}}"#,
        r#"{"version":1,"authority":{"status":"valid","value":{"model":" ","model_provider_id":"provider","reasoning_effort":null}}}"#,
        r#" {"version":1,"authority":{"status":"valid","value":{"model":"model","model_provider_id":"\t","reasoning_effort":null}}} "#,
    ];
    for raw in malformed {
        assert_eq!(
            decode_thread_inference_identity_authority(Some(raw)),
            ThreadInferenceIdentityAuthority::Malformed {
                raw: raw.to_string(),
            }
        );
    }
}

#[test]
fn authority_codec_rejects_malformed_typed_writes() {
    assert!(matches!(
        encode_thread_inference_identity_authority(&ThreadInferenceIdentityAuthority::Malformed {
            raw: "diagnostic".to_string(),
        }),
        Err(ThreadInferenceIdentityAuthorityEncodeError::MalformedAuthority)
    ));
}
