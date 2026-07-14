use codex_protocol::models::ThreadInferenceIdentity;
use codex_protocol::models::ThreadInferenceIdentityAuthority;
use pretty_assertions::assert_eq;

use super::ThreadInferenceIdentitySidecar;

#[test]
fn inference_identity_sidecar_defaults_and_serializes_compatibly() {
    assert_eq!(
        serde_json::from_str::<ThreadInferenceIdentitySidecar>("{}")
            .expect("missing sidecar fields should default"),
        ThreadInferenceIdentitySidecar::default()
    );

    let sidecar = ThreadInferenceIdentitySidecar {
        configured: ThreadInferenceIdentityAuthority::Valid(
            ThreadInferenceIdentity::new("configured", "provider", None)
                .expect("identity should be valid"),
        ),
        latest_request: ThreadInferenceIdentityAuthority::Cleared,
    };
    assert_eq!(
        serde_json::from_value::<ThreadInferenceIdentitySidecar>(
            serde_json::to_value(&sidecar).expect("sidecar should serialize")
        )
        .expect("sidecar should deserialize"),
        sidecar
    );
}
