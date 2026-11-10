use anyhow::Result;
use codex_config::InstructionOverlayComponent;
use codex_config::InstructionOverlaySelection;
use codex_config::InstructionOverlaysToml;
use codex_config::MAX_INSTRUCTION_OVERLAY_INSTANCES;
use codex_config::MAX_INSTRUCTION_OVERLAY_REQUEST_BYTES;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ContentItemProvenance;
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

pub(super) fn has_eligible_plan(
    source: &SessionSource,
    plan: &InstructionOverlaysToml,
    model: &str,
) -> bool {
    eligible_components(source).is_some_and(|components| {
        components.iter().any(|component| {
            plan.component(*component)
                .is_some_and(|p| !p.edits.is_empty() && p.applies_to_model(model))
        })
    })
}

pub(super) fn has_configured_plan(source: &SessionSource, plan: &InstructionOverlaysToml) -> bool {
    eligible_components(source).is_some_and(|components| {
        components.iter().any(|component| {
            plan.component(*component)
                .is_some_and(|p| !p.edits.is_empty())
        })
    })
}

pub(super) fn only_optional_removals(
    source: &SessionSource,
    plan: &InstructionOverlaysToml,
    model: &str,
) -> bool {
    eligible_components(source).is_some_and(|components| {
        components
            .iter()
            .filter_map(|component| plan.component(*component))
            .filter(|part| !part.edits.is_empty() && part.applies_to_model(model))
            .all(|part| {
                part.edits.iter().all(|edit| {
                    matches!(
                        edit,
                        codex_config::InstructionOverlayEdit::RemoveExactIfPresent { .. }
                    )
                })
            })
    })
}

/// Classify exactly one retained baseline by its complete text, on the request copy.
/// A historical marker alone is not proof that the text belongs to the current baseline.
pub(super) fn classify_retained_baseline(
    input: &mut [ResponseItem],
    expected: &[&str],
    allow_multiple: bool,
) -> Result<usize> {
    let mut found = 0;
    for item in input {
        let ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough: Some(metadata),
            ..
        } = item
        else {
            continue;
        };
        if role != "developer" {
            continue;
        }
        let Some(kinds) = metadata.content_item_kinds.as_mut() else {
            continue;
        };
        if !kinds.iter().any(|kind| {
            matches!(
                kind.as_str(),
                "model.base_instructions" | "client.base_instructions"
            )
        }) {
            continue;
        }
        if kinds.len() != content.len()
            || metadata
                .content_item_metadata
                .as_ref()
                .is_some_and(|m| m.len() != content.len())
        {
            anyhow::bail!("instruction overlay blocked: misaligned baseline metadata");
        }
        for (kind, content) in kinds.iter_mut().zip(content.iter()) {
            if !matches!(
                kind.as_str(),
                "model.base_instructions" | "client.base_instructions"
            ) {
                continue;
            }
            let ContentItem::InputText { text } = content else {
                anyhow::bail!("instruction overlay blocked: baseline is not text");
            };
            if !expected.contains(&text.as_str()) {
                anyhow::bail!("instruction overlay blocked: ambiguous baseline identity");
            }
            found += 1;
            if found > 1 && !allow_multiple {
                anyhow::bail!("instruction overlay blocked: ambiguous baseline selection");
            }
            *kind = ContentItemKind("client.base_instructions".to_string());
        }
    }
    Ok(found)
}

/// Remove the one exact retained duplicate when the request also has a scalar prefix.
/// This operates only on the request copy and keeps all parallel metadata aligned.
pub(super) fn remove_duplicate_retained_baseline(input: &mut Vec<ResponseItem>, expected: &str) {
    input.retain_mut(|item| {
        let ResponseItem::Message {
            id,
            role,
            content,
            internal_chat_message_metadata_passthrough: Some(metadata),
            ..
        } = item
        else {
            return true;
        };
        if role != "developer" {
            return true;
        }
        let Some(kinds) = metadata.content_item_kinds.as_mut() else {
            return true;
        };
        let Some(index) = kinds
            .iter()
            .zip(content.iter())
            .position(|(kind, content)| {
                kind.as_str() == "client.base_instructions"
                    && matches!(content, ContentItem::InputText { text } if text == expected)
            })
        else {
            return true;
        };
        content.remove(index);
        kinds.remove(index);
        if kinds.is_empty() {
            metadata.content_item_kinds = None;
        }
        if let Some(sources) = metadata.content_item_metadata.as_mut() {
            sources.remove(index);
            if sources.is_empty() {
                metadata.content_item_metadata = None;
            }
        }
        *id = None;
        !content.is_empty()
    });
}

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
            anyhow::bail!(
                "instruction overlay blocked: protected or unknown {}",
                component.as_str()
            );
        }
    }

    let mut transformed_input = input;
    let mut selected_bytes = 0usize;
    let mut selected_instances = 0usize;
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
                &mut selected_instances,
                &mut selected_bytes,
            )?;
        } else {
            anyhow::bail!(
                "instruction overlay blocked: protected or unknown {}",
                baseline_component.as_str()
            );
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
        apply_tagged_component(
            plan,
            component,
            &mut transformed_input,
            &mut selected_instances,
            &mut selected_bytes,
        )?;
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
    selected_instances: &mut usize,
    selected_bytes: &mut usize,
) -> Result<()> {
    if plan
        .component(component)
        .is_none_or(|component_plan| component_plan.edits.is_empty())
    {
        return Ok(());
    }
    let selected = find_candidate(
        plan,
        input,
        component,
        target_kind(component),
        *selected_instances,
    )?;
    *selected_instances += selected.len();
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
    already_selected: usize,
) -> Result<Vec<Candidate>> {
    let mut selected = Vec::new();
    let mut protected_or_unknown = false;
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
        if !kinds.iter().any(|kind| {
            kind.as_str() == target_kind
                || (component == InstructionOverlayComponent::DeveloperGuidance
                    && kind.as_str() == "generic.developer_instructions")
        }) {
            continue;
        }
        if kinds.len() != content.len() {
            anyhow::bail!(
                "instruction overlay target for {} has misaligned metadata at request item {item_index}",
                component.as_str()
            );
        }
        if internal_chat_message_metadata_passthrough
            .as_ref()
            .and_then(|metadata| metadata.content_item_metadata.as_ref())
            .is_some_and(|metadata| metadata.len() != content.len())
        {
            anyhow::bail!(
                "instruction overlay target for {} has misaligned source metadata at request item {item_index}",
                component.as_str()
            );
        }
        for (content_index, kind) in kinds.iter().enumerate() {
            let client_tool_guidance = component == InstructionOverlayComponent::DeveloperGuidance
                && kind.as_str() == "generic.developer_instructions"
                && internal_chat_message_metadata_passthrough
                    .as_ref()
                    .and_then(|metadata| metadata.content_item_metadata.as_ref())
                    .and_then(|metadata| metadata.get(content_index))
                    .is_some_and(|metadata| {
                        matches!(metadata.provenance, ContentItemProvenance::Tool { .. })
                    });
            if component == InstructionOverlayComponent::DeveloperGuidance
                && kind.as_str() == "generic.developer_instructions"
                && !client_tool_guidance
            {
                protected_or_unknown = true;
            }
            if kind.as_str() == target_kind || client_tool_guidance {
                if unique_selection && !selected.is_empty() {
                    anyhow::bail!(
                        "instruction overlay target for {} is ambiguous",
                        component.as_str()
                    );
                }
                if already_selected.saturating_add(selected.len())
                    >= MAX_INSTRUCTION_OVERLAY_INSTANCES
                {
                    anyhow::bail!("instruction overlay selected-instance limit exceeded");
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
    if selected.is_empty() && protected_or_unknown {
        anyhow::bail!("instruction overlay blocked: protected or unknown developer_guidance");
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
                if let Some(metadata) = internal_chat_message_metadata_passthrough.as_mut()
                    && let Some(sources) = metadata.content_item_metadata.as_mut()
                {
                    sources.remove(content_index);
                    if sources.is_empty() {
                        metadata.content_item_metadata = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_config::InstructionOverlayComponentToml;
    use codex_config::InstructionOverlayEdit;
    use serde_json::json;

    fn retained(baseline: &str) -> ResponseItem {
        serde_json::from_value(json!({
            "type": "message", "role": "developer",
            "content": [
                {"type": "input_text", "text": baseline},
                {"type": "input_text", "text": "protected synthetic guidance"}
            ],
            "internal_chat_message_metadata_passthrough": {
                "content_item_kinds": ["model.base_instructions", "generic.developer_instructions"],
                "content_item_metadata": [{"harness_injected": true}, {"harness_injected": false}]
            }
        }))
        .unwrap()
    }

    fn tagged(kind: &str, text: &str) -> ResponseItem {
        serde_json::from_value(json!({
            "type": "message", "role": "developer",
            "content": [{"type": "input_text", "text": text}],
            "internal_chat_message_metadata_passthrough": {"content_item_kinds": [kind]}
        }))
        .unwrap()
    }

    #[test]
    fn configured_components_apply_only_to_classified_request_copies() {
        use InstructionOverlayComponent::*;
        for component in [
            ModelBaseline,
            ReviewBaseline,
            DeveloperGuidance,
            MultiAgentMode,
            MultiAgentRole,
            MultiAgentUsageHint,
            ModelSwitchGuidance,
            CollaborationGuidance,
            PersistentGuidance,
        ] {
            let part = InstructionOverlayComponentToml {
                on_absent: codex_config::InstructionOverlayOnAbsent::Skip,
                edits: vec![InstructionOverlayEdit::Append {
                    text: " synthetic-edit".into(),
                }],
                ..Default::default()
            };
            let mut plan = InstructionOverlaysToml::default();
            match component {
                ModelBaseline => plan.model_baseline = Some(part),
                ReviewBaseline => plan.review_baseline = Some(part),
                DeveloperGuidance => plan.developer_guidance = Some(part),
                MultiAgentMode => plan.multi_agent_mode = Some(part),
                MultiAgentRole => plan.multi_agent_role = Some(part),
                MultiAgentUsageHint => plan.multi_agent_usage_hint = Some(part),
                ModelSwitchGuidance => plan.model_switch_guidance = Some(part),
                CollaborationGuidance => plan.collaboration_guidance = Some(part),
                PersistentGuidance => plan.persistent_guidance = Some(part),
            }
            let source = if component == ReviewBaseline {
                SessionSource::SubAgent(SubAgentSource::Review)
            } else {
                SessionSource::Cli
            };
            let text = match component {
                MultiAgentMode => {
                    format!("{MULTI_AGENT_MODE_OPEN_TAG}synthetic{MULTI_AGENT_MODE_CLOSE_TAG}")
                }
                MultiAgentRole => "<multi_agent_role>synthetic</multi_agent_role>".into(),
                ModelSwitchGuidance => "<model_switch>synthetic</model_switch>".into(),
                CollaborationGuidance => {
                    format!("{COLLABORATION_MODE_OPEN_TAG}synthetic{COLLABORATION_MODE_CLOSE_TAG}")
                }
                PersistentGuidance => "<persistent_mode>synthetic</persistent_mode>".into(),
                _ => "synthetic".into(),
            };
            let original = vec![
                tagged(target_kind(component), &text),
                tagged("managed.instructions", "protected synthetic"),
            ];
            let output = transform_request(
                &source,
                &plan,
                "synthetic-model",
                true,
                &[],
                original.clone(),
            )
            .unwrap();
            assert!(
                serde_json::to_string(&output)
                    .unwrap()
                    .contains("synthetic-edit")
            );
            assert_eq!(output[1], original[1]);
            assert!(
                !serde_json::to_string(&original)
                    .unwrap()
                    .contains("synthetic-edit")
            );
            assert!(
                transform_request(
                    &source,
                    &plan,
                    "synthetic-model",
                    true,
                    &[component],
                    original
                )
                .is_err()
            );
        }
    }

    #[test]
    fn generic_tool_guidance_requires_tool_attribution() {
        let make = |provenance: serde_json::Value| -> ResponseItem {
            serde_json::from_value(json!({
                "type": "message", "role": "developer",
                "content": [{"type": "input_text", "text": "synthetic guidance"}],
                "internal_chat_message_metadata_passthrough": {
                    "content_item_kinds": ["generic.developer_instructions"],
                    "content_item_metadata": [{"provenance": provenance}]
                }
            }))
            .unwrap()
        };
        let tool = make(json!({"type": "tool", "namespace": "functions"}));
        let protected =
            make(json!({"type": "developer_instructions", "from_additional_requirements": true}));
        let plan = InstructionOverlaysToml {
            developer_guidance: Some(InstructionOverlayComponentToml {
                edits: vec![InstructionOverlayEdit::Append {
                    text: " synthetic-edit".into(),
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let output = transform_request(
            &SessionSource::Cli,
            &plan,
            "synthetic-model",
            false,
            &[],
            vec![tool, protected.clone()],
        )
        .unwrap();
        assert!(
            serde_json::to_string(&output[0])
                .unwrap()
                .contains("synthetic-edit")
        );
        assert_eq!(output[1], protected);
        let mut optional = plan.clone();
        optional.developer_guidance.as_mut().unwrap().on_absent =
            codex_config::InstructionOverlayOnAbsent::Skip;
        assert!(
            transform_request(
                &SessionSource::Cli,
                &optional,
                "synthetic-model",
                false,
                &[],
                vec![protected]
            )
            .is_err()
        );
    }

    #[test]
    fn retained_baseline_edit_preserves_history_and_metadata_alignment() {
        let original = vec![retained("synthetic target")];
        let mut request = original.clone();
        assert_eq!(
            classify_retained_baseline(&mut request, &["synthetic target"], false).unwrap(),
            1
        );
        let plan = InstructionOverlaysToml {
            model_baseline: Some(InstructionOverlayComponentToml {
                edits: vec![InstructionOverlayEdit::RemoveExact {
                    text: "synthetic target".into(),
                    expected_matches: 1,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let output = transform_request(
            &SessionSource::Cli,
            &plan,
            "synthetic-model",
            true,
            &[],
            request,
        )
        .unwrap();
        assert_eq!(original, vec![retained("synthetic target")]);
        let ResponseItem::Message {
            content,
            internal_chat_message_metadata_passthrough: Some(metadata),
            ..
        } = &output[0]
        else {
            panic!("expected message")
        };
        assert_eq!(content.len(), 1);
        assert_eq!(metadata.content_item_kinds.as_ref().unwrap().len(), 1);
        assert_eq!(metadata.content_item_metadata.as_ref().unwrap().len(), 1);
        assert_eq!(
            content[0],
            ContentItem::InputText {
                text: "protected synthetic guidance".into()
            }
        );
    }

    #[test]
    fn retained_baseline_requires_exact_unambiguous_identity() {
        let original = vec![retained("other baseline")];
        let mut request = original.clone();
        assert!(classify_retained_baseline(&mut request, &["expected baseline"], false).is_err());
        assert_eq!(original, vec![retained("other baseline")]);
        let mut duplicate = vec![retained("expected baseline"), retained("expected baseline")];
        assert!(classify_retained_baseline(&mut duplicate, &["expected baseline"], false).is_err());
    }

    #[test]
    fn all_classified_is_bounded() {
        let mut input = (0..=MAX_INSTRUCTION_OVERLAY_INSTANCES)
            .map(|_| retained("synthetic target"))
            .collect::<Vec<_>>();
        classify_retained_baseline(&mut input, &["synthetic target"], true).unwrap();
        let plan = InstructionOverlaysToml {
            model_baseline: Some(InstructionOverlayComponentToml {
                selection: InstructionOverlaySelection::AllClassified,
                edits: vec![InstructionOverlayEdit::RemoveExact {
                    text: "synthetic target".into(),
                    expected_matches: 1,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            transform_request(
                &SessionSource::Cli,
                &plan,
                "synthetic-model",
                true,
                &[],
                input
            )
            .is_err()
        );
    }
}
