use anyhow::Result;
use codex_config::InstructionOverlayComponent;
use codex_config::InstructionOverlaySelection;
use codex_config::InstructionOverlaysToml;
use codex_config::MAX_INSTRUCTION_OVERLAY_REQUEST_BYTES;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::COLLABORATION_MODE_CLOSE_TAG;
use codex_protocol::protocol::COLLABORATION_MODE_OPEN_TAG;
use codex_protocol::protocol::MULTI_AGENT_MODE_CLOSE_TAG;
use codex_protocol::protocol::MULTI_AGENT_MODE_OPEN_TAG;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

const NORMAL_COMPONENTS: &[InstructionOverlayComponent] = &[
    InstructionOverlayComponent::ModelBaseline,
    InstructionOverlayComponent::DeveloperGuidance,
    InstructionOverlayComponent::MultiAgentMode,
    InstructionOverlayComponent::MultiAgentRole,
    InstructionOverlayComponent::MultiAgentUsageHint,
    InstructionOverlayComponent::ModelSwitchGuidance,
    InstructionOverlayComponent::CollaborationGuidance,
    InstructionOverlayComponent::PersistentGuidance,
];
const REVIEW_COMPONENTS: &[InstructionOverlayComponent] =
    &[InstructionOverlayComponent::ReviewBaseline];

pub(super) fn transform_request(
    source: &SessionSource,
    plan: &InstructionOverlaysToml,
    model_slug: &str,
    baseline_is_client_owned: bool,
    fenced_components: &[InstructionOverlayComponent],
    input: Vec<ResponseItem>,
) -> Result<Vec<ResponseItem>> {
    let Some(components) = eligible_components(source) else {
        return Ok(input);
    };

    plan.validate_components(components)
        .map_err(|error| anyhow::anyhow!("instruction overlay plan invalid: {error}"))?;
    // Do not let a managed/unknown current selection inherit authority from an earlier
    // client-owned classification. A required edit against a fenced target fails closed.
    for component in components
        .iter()
        .filter(|component| fenced_components.contains(component))
    {
        if component_applies_to_model(plan, *component, model_slug)
            && plan
                .component(*component)
                .is_some_and(|component| !component.edits.is_empty())
        {
            let _ = apply_component(plan, *component, /*body*/ None)?;
        }
    }

    let mut transformed_input = input;
    let mut selected_bytes = 0usize;
    let baseline_component = if matches!(source, SessionSource::SubAgent(SubAgentSource::Review)) {
        InstructionOverlayComponent::ReviewBaseline
    } else {
        InstructionOverlayComponent::ModelBaseline
    };
    if !fenced_components.contains(&baseline_component)
        && component_applies_to_model(plan, baseline_component, model_slug)
        && plan
            .component(baseline_component)
            .is_some_and(|component| !component.edits.is_empty())
    {
        if baseline_is_client_owned {
            apply_tagged_component(
                plan,
                baseline_component,
                &mut transformed_input,
                &mut selected_bytes,
            )?;
        } else {
            let _ = apply_component(plan, baseline_component, /*body*/ None)?;
        }
    }
    for component in components.iter().copied().filter(|component| {
        !fenced_components.contains(component)
            && component_applies_to_model(plan, *component, model_slug)
            && !matches!(
                component,
                InstructionOverlayComponent::ModelBaseline
                    | InstructionOverlayComponent::ReviewBaseline
            )
    }) {
        apply_tagged_component(plan, component, &mut transformed_input, &mut selected_bytes)?;
    }
    Ok(transformed_input)
}

fn eligible_components(source: &SessionSource) -> Option<&'static [InstructionOverlayComponent]> {
    match source {
        SessionSource::Cli | SessionSource::VSCode | SessionSource::Exec | SessionSource::Mcp => {
            Some(NORMAL_COMPONENTS)
        }
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }) => Some(NORMAL_COMPONENTS),
        SessionSource::SubAgent(SubAgentSource::Review) => Some(REVIEW_COMPONENTS),
        SessionSource::Custom(_)
        | SessionSource::Internal(_)
        | SessionSource::Unknown
        | SessionSource::SubAgent(SubAgentSource::Compact)
        | SessionSource::SubAgent(SubAgentSource::MemoryConsolidation)
        | SessionSource::SubAgent(SubAgentSource::Other(_)) => None,
    }
}

fn apply_component(
    plan: &InstructionOverlaysToml,
    component: InstructionOverlayComponent,
    body: Option<&str>,
) -> Result<Option<String>> {
    plan.apply_component(component, body)
        .map_err(|error| anyhow::anyhow!("instruction overlay for {}: {error}", component.as_str()))
}

fn component_applies_to_model(
    plan: &InstructionOverlaysToml,
    component: InstructionOverlayComponent,
    model_slug: &str,
) -> bool {
    plan.component(component)
        .is_some_and(|component_plan| component_plan.applies_to_model(model_slug))
}

fn apply_tagged_component(
    plan: &InstructionOverlaysToml,
    component: InstructionOverlayComponent,
    input: &mut Vec<ResponseItem>,
    selected_bytes: &mut usize,
) -> Result<()> {
    if plan
        .component(component)
        .is_none_or(|component_plan| component_plan.edits.is_empty())
    {
        return Ok(());
    }
    let selected = find_candidate(plan, input, component, target_kind(component))?;
    apply_candidates(plan, component, input, selected, selected_bytes)
}

struct Candidate {
    item_index: usize,
    content_index: usize,
}

fn find_candidate(
    plan: &InstructionOverlaysToml,
    input: &[ResponseItem],
    component: InstructionOverlayComponent,
    target_kind: &str,
) -> Result<Vec<Candidate>> {
    let mut selected = Vec::new();
    let unique_selection = plan
        .component(component)
        .is_none_or(|config| config.selection == InstructionOverlaySelection::Unique);
    for (item_index, item) in input.iter().enumerate() {
        let ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = item
        else {
            continue;
        };
        if role != "developer" {
            continue;
        }
        let Some(kinds) = internal_chat_message_metadata_passthrough
            .as_ref()
            .and_then(|metadata| metadata.content_item_kinds.as_ref())
        else {
            continue;
        };
        if !kinds.iter().any(|kind| kind.as_str() == target_kind) {
            continue;
        }
        if kinds.len() != content.len() {
            anyhow::bail!(
                "instruction overlay target for {} has misaligned metadata at request item {item_index}",
                component.as_str()
            );
        }
        for (content_index, kind) in kinds.iter().enumerate() {
            if kind.as_str() == target_kind {
                if unique_selection && !selected.is_empty() {
                    anyhow::bail!(
                        "instruction overlay target for {} is ambiguous",
                        component.as_str()
                    );
                }
                let ContentItem::InputText { .. } = &content[content_index] else {
                    anyhow::bail!(
                        "instruction overlay target for {} is not text at request item {item_index}",
                        component.as_str()
                    );
                };
                selected.push(Candidate {
                    item_index,
                    content_index,
                });
            }
        }
    }
    Ok(selected)
}

fn apply_candidates(
    plan: &InstructionOverlaysToml,
    component: InstructionOverlayComponent,
    input: &mut Vec<ResponseItem>,
    mut selected: Vec<Candidate>,
    selected_bytes: &mut usize,
) -> Result<()> {
    if selected.is_empty() {
        let _ = apply_component(plan, component, /*body*/ None)?;
        return Ok(());
    }
    // Discovery is in request order. Reverse it so removals cannot invalidate
    // the indices of later replacements, without sorting a long history.
    selected.reverse();
    let mut replacements = Vec::with_capacity(selected.len());
    for Candidate {
        item_index,
        content_index,
    } in selected
    {
        let ResponseItem::Message { content, .. } = &input[item_index] else {
            unreachable!("selected item is a message")
        };
        let ContentItem::InputText { text } = &content[content_index] else {
            unreachable!("selected content is input text")
        };
        let (prefix, body, suffix) = split_wrapper(component, text, item_index)?;
        let Some(updated_body) = apply_component(plan, component, /*body*/ Some(body))? else {
            continue;
        };
        let final_text_len = prefix
            .len()
            .saturating_add(updated_body.len())
            .saturating_add(suffix.len());
        *selected_bytes = selected_bytes.saturating_add(final_text_len);
        if *selected_bytes > MAX_INSTRUCTION_OVERLAY_REQUEST_BYTES {
            anyhow::bail!("instruction overlay request output limit exceeded");
        }
        if updated_body == body {
            continue;
        }
        replacements.push((
            item_index,
            content_index,
            format!("{prefix}{updated_body}{suffix}"),
            updated_body.is_empty(),
        ));
    }
    for (item_index, content_index, updated_text, remove_content) in replacements {
        let remove_message = {
            let item = &mut input[item_index];
            let ResponseItem::Message {
                id,
                content,
                internal_chat_message_metadata_passthrough,
                ..
            } = item
            else {
                unreachable!("selected item is a message")
            };
            if remove_content {
                content.remove(content_index);
                if let Some(metadata) = internal_chat_message_metadata_passthrough.as_mut()
                    && let Some(kinds) = metadata.content_item_kinds.as_mut()
                {
                    kinds.remove(content_index);
                    if kinds.is_empty() {
                        metadata.content_item_kinds = None;
                    }
                }
                if internal_chat_message_metadata_passthrough
                    .as_ref()
                    .is_some_and(|metadata| metadata == &Default::default())
                {
                    *internal_chat_message_metadata_passthrough = None;
                }
            } else {
                content[content_index] = ContentItem::InputText { text: updated_text };
                *id = None;
            }
            remove_content && content.is_empty()
        };
        if remove_message {
            input.remove(item_index);
        } else if remove_content && let ResponseItem::Message { id, .. } = &mut input[item_index] {
            *id = None;
        }
    }
    Ok(())
}

fn target_kind(component: InstructionOverlayComponent) -> &'static str {
    match component {
        InstructionOverlayComponent::DeveloperGuidance => "client.developer_instructions",
        InstructionOverlayComponent::ModelBaseline
        | InstructionOverlayComponent::ReviewBaseline => "client.base_instructions",
        InstructionOverlayComponent::MultiAgentMode => "client.multi_agent.mode_instructions",
        InstructionOverlayComponent::MultiAgentRole => "client.multi_agent.role_instructions",
        InstructionOverlayComponent::MultiAgentUsageHint => "client.multi_agent.usage_hint",
        InstructionOverlayComponent::ModelSwitchGuidance => "model_switch.instructions",
        InstructionOverlayComponent::CollaborationGuidance => "collaboration_mode.instructions",
        InstructionOverlayComponent::PersistentGuidance => "persistent_mode.instructions",
    }
}

fn split_wrapper(
    component: InstructionOverlayComponent,
    text: &str,
    item_index: usize,
) -> Result<(&str, &str, &str)> {
    let required = match component {
        InstructionOverlayComponent::MultiAgentMode => {
            Some((MULTI_AGENT_MODE_OPEN_TAG, MULTI_AGENT_MODE_CLOSE_TAG))
        }
        InstructionOverlayComponent::ModelSwitchGuidance => {
            Some(("<model_switch>", "</model_switch>"))
        }
        InstructionOverlayComponent::CollaborationGuidance => {
            Some((COLLABORATION_MODE_OPEN_TAG, COLLABORATION_MODE_CLOSE_TAG))
        }
        InstructionOverlayComponent::PersistentGuidance => {
            Some(("<persistent_mode>", "</persistent_mode>"))
        }
        _ => None,
    };
    let optional = match component {
        InstructionOverlayComponent::MultiAgentRole => {
            Some(("<multi_agent_role>", "</multi_agent_role>"))
        }
        _ => None,
    };
    let markers = required.or(optional);
    let Some((open, close)) = markers else {
        return Ok(("", text, ""));
    };
    let has_open = text.starts_with(open);
    let has_close = text.ends_with(close);
    if required.is_some() && !(has_open && has_close) || has_open != has_close {
        anyhow::bail!(
            "instruction overlay target for {} has invalid wrapper at request item {item_index}",
            component.as_str()
        );
    }
    if has_open {
        let body_start = open.len();
        let body_end = text.len().saturating_sub(close.len());
        if body_end < body_start {
            anyhow::bail!(
                "instruction overlay target for {} has invalid wrapper at request item {item_index}",
                component.as_str()
            );
        }
        Ok((
            &text[..body_start],
            &text[body_start..body_end],
            &text[body_end..],
        ))
    } else {
        Ok(("", text, ""))
    }
}
