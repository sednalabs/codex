use super::*;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnItemContributor;
use codex_protocol::ResponseItemId;
use codex_protocol::items::AgentMessageContent;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tracing_subscriber::prelude::*;

static POST_SAMPLING_TOKEN_ESTIMATE_METADATA: tracing::Metadata<'static> = tracing::metadata! {
    name: "post_sampling_token_estimate",
    target: POST_SAMPLING_TOKEN_ESTIMATE_TARGET,
    level: tracing::Level::TRACE,
    fields: &["turn_id", "estimated_token_count", "message"],
    callsite: &POST_SAMPLING_TOKEN_ESTIMATE_CALLSITE,
    kind: tracing::metadata::Kind::EVENT,
};
static POST_SAMPLING_TOKEN_ESTIMATE_CALLSITE: tracing::callsite::DefaultCallsite =
    tracing::callsite::DefaultCallsite::new(&POST_SAMPLING_TOKEN_ESTIMATE_METADATA);

static ORDINARY_TRACE_METADATA: tracing::Metadata<'static> = tracing::metadata! {
    name: "ordinary_trace_control",
    target: "codex_core::ordinary_trace_control",
    level: tracing::Level::TRACE,
    fields: &["message"],
    callsite: &ORDINARY_TRACE_CALLSITE,
    kind: tracing::metadata::Kind::EVENT,
};
static ORDINARY_TRACE_CALLSITE: tracing::callsite::DefaultCallsite =
    tracing::callsite::DefaultCallsite::new(&ORDINARY_TRACE_METADATA);

struct RewriteAgentMessageContributor;

impl TurnItemContributor for RewriteAgentMessageContributor {
    fn contribute<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        item: &'a mut TurnItem,
    ) -> codex_extension_api::ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if let TurnItem::AgentMessage(agent_message) = item {
                agent_message.content = vec![AgentMessageContent::Text {
                    text: "plan contributed assistant text".to_string(),
                }];
            }
            Ok(())
        })
    }
}

fn assistant_output_text(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", "1")),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn post_sampling_token_estimate_is_disabled_by_always_on_sinks() {
    let feedback = codex_feedback::CodexFeedback::new();
    let subscriber = tracing_subscriber::registry()
        .with(feedback.logger_layer())
        .with(tracing_subscriber::fmt::layer().with_filter(codex_state::log_db::default_filter()));

    assert!(
        tracing::Subscriber::register_callsite(
            &subscriber,
            &POST_SAMPLING_TOKEN_ESTIMATE_METADATA,
        )
        .is_never()
    );
    assert!(
        tracing::Subscriber::register_callsite(&subscriber, &ORDINARY_TRACE_METADATA).is_always()
    );
}

#[tokio::test]
async fn plan_mode_uses_contributed_turn_item_for_last_agent_message() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    session.services.extensions = Arc::new(builder.build());
    let turn_store = ExtensionData::new(turn_context.sub_id.clone());
    let mut state = PlanModeStreamState::new(&turn_context.sub_id);
    let mut last_agent_message = None;
    let item = assistant_output_text("original assistant text");

    let handled = handle_assistant_item_done_in_plan_mode(
        &session,
        &turn_context,
        &turn_store,
        &item,
        &mut state,
        /*previously_active_item*/ None,
        &mut last_agent_message,
    )
    .await;

    assert!(handled);
    assert_eq!(
        last_agent_message.as_deref(),
        Some("plan contributed assistant text")
    );
}
