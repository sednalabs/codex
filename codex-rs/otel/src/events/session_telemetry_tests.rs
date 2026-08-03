use super::SessionTelemetry;
use crate::metrics::tags::APP_VERSION_TAG;
use crate::metrics::tags::MODEL_TAG;
use crate::metrics::tags::ORIGINATOR_TAG;
use crate::metrics::tags::SESSION_SOURCE_TAG;
use crate::sanitize_metric_tag_value;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_utils_version::RELEASE_VERSION;
use pretty_assertions::assert_eq;

#[test]
fn session_constructor_sanitizes_only_the_metric_app_version() {
    let release_telemetry = SessionTelemetry::new(
        ThreadId::new(),
        "model",
        "slug",
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "codex_desktop".to_string(),
        /*log_user_prompts*/ false,
        "unknown".to_string(),
        SessionSource::Cli,
    );
    let exact_app_version = "0.136.0-alpha.1+downstream.1";
    let telemetry = SessionTelemetry::new_with_app_version(
        ThreadId::new(),
        "model",
        "slug",
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "codex_desktop".to_string(),
        /*log_user_prompts*/ false,
        "unknown".to_string(),
        SessionSource::Cli,
        exact_app_version,
    );

    let metric_tags = telemetry.metadata_tag_refs().expect("metric tags");

    assert_eq!(
        (
            release_telemetry.metadata.app_version,
            release_telemetry.metric_app_version,
            telemetry.metadata.app_version,
            metric_tags,
        ),
        (
            RELEASE_VERSION,
            sanitize_metric_tag_value(RELEASE_VERSION),
            exact_app_version,
            vec![
                (SESSION_SOURCE_TAG, "cli"),
                (ORIGINATOR_TAG, "codex_desktop"),
                (MODEL_TAG, "model"),
                (APP_VERSION_TAG, "0.136.0-alpha.1_downstream.1"),
            ],
        )
    );
}
