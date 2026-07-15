use pretty_assertions::assert_eq;
use serde_json::json;

use super::ThreadInferenceIdentity;
use super::ThreadInferenceIdentityValidationError;
use crate::openai_models::ReasoningEffort;

#[test]
fn thread_inference_identity_constructor_and_direct_serde_validate_without_normalizing() {
    let normal = ThreadInferenceIdentity::new(
        "configured-alias",
        "test-provider",
        Some(ReasoningEffort::High),
    )
    .expect("normal identity should be valid");
    let padded = ThreadInferenceIdentity::new(
        "  configured-alias  ",
        "\ttest-provider\n",
        /*reasoning_effort*/ None,
    )
    .expect("padded identity should be valid");
    assert_eq!(
        (
            padded.model(),
            padded.model_provider_id(),
            padded.reasoning_effort(),
        ),
        ("  configured-alias  ", "\ttest-provider\n", None)
    );

    for (model, model_provider_id, expected) in [
        (
            "",
            "provider",
            ThreadInferenceIdentityValidationError::EmptyModel,
        ),
        (
            " \t\n",
            "provider",
            ThreadInferenceIdentityValidationError::EmptyModel,
        ),
        (
            "model",
            "",
            ThreadInferenceIdentityValidationError::EmptyModelProviderId,
        ),
        (
            "model",
            " \t\n",
            ThreadInferenceIdentityValidationError::EmptyModelProviderId,
        ),
    ] {
        assert_eq!(
            ThreadInferenceIdentity::new(model, model_provider_id, /*reasoning_effort*/ None,),
            Err(expected)
        );
    }

    for value in [
        json!({"model": "", "model_provider_id": "provider", "reasoning_effort": null}),
        json!({"model": " \t", "model_provider_id": "provider", "reasoning_effort": null}),
        json!({"model": "model", "model_provider_id": "", "reasoning_effort": null}),
        json!({"model": "model", "model_provider_id": "\n", "reasoning_effort": null}),
    ] {
        assert!(serde_json::from_value::<ThreadInferenceIdentity>(value).is_err());
    }
    for (value, expected) in [
        (
            json!({"model": "configured-alias", "model_provider_id": "test-provider", "reasoning_effort": "high"}),
            normal,
        ),
        (
            json!({"model": "  configured-alias  ", "model_provider_id": "\ttest-provider\n", "reasoning_effort": null}),
            padded,
        ),
    ] {
        assert_eq!(
            serde_json::from_value::<ThreadInferenceIdentity>(value)
                .expect("normal or padded identity should deserialize"),
            expected
        );
    }
}
