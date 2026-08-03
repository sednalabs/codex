use super::SessionTelemetry;
use crate::metrics::tags::APP_VERSION_TAG;
use crate::metrics::tags::MODEL_TAG;
use crate::metrics::tags::ORIGINATOR_TAG;
use crate::metrics::tags::SESSION_SOURCE_TAG;
use crate::sanitize_metric_tag_value;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use pretty_assertions::assert_eq;

#[test]
fn session_metric_tags_sanitize_only_app_version_metric_value() {
    let mut telemetry = SessionTelemetry::new(
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
    let exact_app_version = "0.136.0-alpha.1+frodex.1";
    telemetry.metadata.app_version = exact_app_version;
    telemetry.metric_app_version = sanitize_metric_tag_value(exact_app_version);

    let metric_tags = telemetry.metadata_tag_refs().expect("metric tags");

    assert_eq!(
        (telemetry.metadata.app_version, metric_tags),
        (
            exact_app_version,
            vec![
                (SESSION_SOURCE_TAG, "cli"),
                (ORIGINATOR_TAG, "codex_desktop"),
                (MODEL_TAG, "model"),
                (APP_VERSION_TAG, "0.136.0-alpha.1_frodex.1"),
            ],
        )
    );
}
