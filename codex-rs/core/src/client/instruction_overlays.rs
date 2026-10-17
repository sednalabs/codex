use anyhow::Result;
use codex_config::InstructionOverlayComponent;
use codex_config::InstructionOverlaySelection;
use codex_config::InstructionOverlaysToml;
#[cfg(test)]
use codex_config::MAX_INSTRUCTION_OVERLAY_BODY_BYTES;
use codex_config::MAX_INSTRUCTION_OVERLAY_INSTANCES;
use codex_config::MAX_INSTRUCTION_OVERLAY_REQUEST_BYTES;
use codex_protocol::models::BaseInstructionsProvenance;
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
    baseline: &str,
    baseline_provenance: Option<&BaseInstructionsProvenance>,
    input: Vec<ResponseItem>,
) -> Result<(Option<String>, Vec<ResponseItem>)> {
    let Some(components) = eligible_components(source) else {
        return Ok((None, input));
    };

    plan.validate_components(components)
        .map_err(|error| anyhow::anyhow!("instruction overlay plan invalid: {error}"))?;
    let mut transformed_baseline = None;
    let mut transformed_input = input;
    let mut selected_instances = 0usize;
    let mut selected_bytes = 0usize;
    let baseline_component = if matches!(source, SessionSource::SubAgent(SubAgentSource::Review)) {
        InstructionOverlayComponent::ReviewBaseline
    } else {
        InstructionOverlayComponent::ModelBaseline
    };
    if plan
        .component(baseline_component)
        .is_some_and(|p| !p.edits.is_empty())
    {
        // Incremental-tools requests carry their selected base as a typed input item and use
        // empty text with no provenance as the scalar placeholder. Empty explicit bases still
        // have provenance and remain independently selectable.
        let scalar_selected = !baseline.is_empty() || baseline_provenance.is_some();
        if scalar_selected {
            selected_instances = selected_instances.saturating_add(1);
            if selected_instances > MAX_INSTRUCTION_OVERLAY_INSTANCES {
                anyhow::bail!("instruction overlay selected-instance limit exceeded");
            }
        }
        let tagged_base = find_candidate(
            plan,
            &transformed_input,
            baseline_component,
            target_kind(baseline_component),
            selected_instances,
            scalar_selected,
        )?;
        if scalar_selected {
            if let Some(updated) =
                apply_component(plan, baseline_component, /*body*/ Some(baseline))?
            {
                transformed_baseline = Some(updated);
            }
            let transformed_len = transformed_baseline
                .as_ref()
                .map_or(baseline.len(), String::len);
            selected_bytes = selected_bytes.saturating_add(transformed_len);
            if selected_bytes > MAX_INSTRUCTION_OVERLAY_REQUEST_BYTES {
                anyhow::bail!("instruction overlay request output limit exceeded");
            }
        }
        if !tagged_base.is_empty() {
            apply_candidates(
                plan,
                baseline_component,
                &mut transformed_input,
                tagged_base,
                &mut selected_instances,
                &mut selected_bytes,
            )?;
        } else if !scalar_selected {
            let _ = apply_component(plan, baseline_component, /*body*/ None)?;
        }
    }
    for component in components.iter().copied().filter(|component| {
        !matches!(
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
    Ok((transformed_baseline, transformed_input))
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
        false,
    )?;
    apply_candidates(
        plan,
        component,
        input,
        selected,
        selected_instances,
        selected_bytes,
    )
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
    already_selected_instances: usize,
    scalar_selected: bool,
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
                if unique_selection && (scalar_selected || !selected.is_empty()) {
                    anyhow::bail!(
                        "instruction overlay target for {} is ambiguous",
                        component.as_str()
                    );
                }
                if already_selected_instances.saturating_add(selected.len())
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
    Ok(selected)
}

fn apply_candidates(
    plan: &InstructionOverlaysToml,
    component: InstructionOverlayComponent,
    input: &mut Vec<ResponseItem>,
    mut selected: Vec<Candidate>,
    selected_instances: &mut usize,
    selected_bytes: &mut usize,
) -> Result<()> {
    if selected.is_empty() {
        let _ = apply_component(plan, component, /*body*/ None)?;
        return Ok(());
    }
    *selected_instances = selected_instances.saturating_add(selected.len());
    if *selected_instances > MAX_INSTRUCTION_OVERLAY_INSTANCES {
        anyhow::bail!("instruction overlay selected-instance limit exceeded");
    }
    selected.sort_by_key(|candidate| {
        (
            std::cmp::Reverse(candidate.item_index),
            std::cmp::Reverse(candidate.content_index),
        )
    });
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
                if let Some(metadata) = internal_chat_message_metadata_passthrough.as_mut() {
                    if let Some(kinds) = metadata.content_item_kinds.as_mut() {
                        kinds.remove(content_index);
                        if kinds.is_empty() {
                            metadata.content_item_kinds = None;
                        }
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
        } else if remove_content {
            if let ResponseItem::Message { id, .. } = &mut input[item_index] {
                *id = None;
            }
        }
    }
    Ok(())
}

fn target_kind(component: InstructionOverlayComponent) -> &'static str {
    match component {
        InstructionOverlayComponent::DeveloperGuidance => "generic.developer_instructions",
        InstructionOverlayComponent::ModelBaseline => "model.base_instructions",
        InstructionOverlayComponent::ReviewBaseline => "model.base_instructions",
        InstructionOverlayComponent::MultiAgentMode => "multi_agent.mode_instructions",
        InstructionOverlayComponent::MultiAgentRole => "multi_agent.role_instructions",
        InstructionOverlayComponent::MultiAgentUsageHint => "multi_agent.usage_hint",
        InstructionOverlayComponent::ModelSwitchGuidance => "model_switch.instructions",
        InstructionOverlayComponent::CollaborationGuidance => "collaboration_mode.instructions",
        InstructionOverlayComponent::PersistentGuidance => "persistent_mode.instructions",
    }
}

fn split_wrapper<'a>(
    component: InstructionOverlayComponent,
    text: &'a str,
    item_index: usize,
) -> Result<(&'a str, &'a str, &'a str)> {
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
    use codex_config::InstructionOverlayOnAbsent;
    use codex_protocol::ThreadId;
    use codex_protocol::models::ContentItemKind;
    use codex_protocol::models::InternalChatMessageMetadataPassthrough;
    use codex_protocol::protocol::InternalSessionSource;

    fn transform_request(
        source: &SessionSource,
        plan: &InstructionOverlaysToml,
        baseline: &str,
        input: &[ResponseItem],
    ) -> Result<(String, Vec<ResponseItem>)> {
        transform_request_with_provenance(source, plan, baseline, None, input)
    }

    fn transform_request_with_provenance(
        source: &SessionSource,
        plan: &InstructionOverlaysToml,
        baseline: &str,
        provenance: Option<&BaseInstructionsProvenance>,
        input: &[ResponseItem],
    ) -> Result<(String, Vec<ResponseItem>)> {
        let (transformed_baseline, output) =
            super::transform_request(source, plan, baseline, provenance, input.to_vec())?;
        Ok((
            transformed_baseline.unwrap_or_else(|| baseline.to_string()),
            output,
        ))
    }

    fn active(text: &str) -> InstructionOverlayComponentToml {
        InstructionOverlayComponentToml {
            on_absent: InstructionOverlayOnAbsent::Error,
            selection: InstructionOverlaySelection::Unique,
            edits: vec![InstructionOverlayEdit::Replace {
                text: text.to_string(),
            }],
        }
    }

    fn developer_fragment(kind: &str, text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: Some(codex_protocol::ResponseItemId::from_server(
                "msg_old".to_string(),
            )),
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: Some(
                InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(vec![ContentItemKind(kind.to_string())]),
                    ..Default::default()
                },
            ),
        }
    }

    #[test]
    fn source_gate_is_exhaustive_and_custom_strings_do_not_promote() {
        let thread_spawn = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        });
        for source in [
            SessionSource::Cli,
            SessionSource::VSCode,
            SessionSource::Exec,
            SessionSource::Mcp,
            thread_spawn,
        ] {
            assert!(eligible_components(&source).is_some());
        }
        for source in [
            SessionSource::Custom("cli".to_string()),
            SessionSource::Custom("exec".to_string()),
            SessionSource::Internal(InternalSessionSource::Guardian),
            SessionSource::Internal(InternalSessionSource::MemoryConsolidation),
            SessionSource::SubAgent(SubAgentSource::Review),
            SessionSource::SubAgent(SubAgentSource::Compact),
            SessionSource::SubAgent(SubAgentSource::MemoryConsolidation),
            SessionSource::SubAgent(SubAgentSource::Other("cli".to_string())),
            SessionSource::Unknown,
        ] {
            let mut plan = InstructionOverlaysToml::default();
            plan.model_baseline = Some(InstructionOverlayComponentToml {
                on_absent: InstructionOverlayOnAbsent::Error,
                selection: InstructionOverlaySelection::Unique,
                edits: vec![InstructionOverlayEdit::ReplaceExact {
                    from: String::new(),
                    to: "must not run".to_string(),
                    expected_matches: 1,
                }],
            });
            plan.developer_guidance = Some(active("must not run"));
            plan.model_switch_guidance = Some(active("must not run"));
            let baseline = "source-owned baseline";
            let input = vec![developer_fragment(
                "generic.developer_instructions",
                "guarded",
            )];
            let (actual_baseline, actual_input) =
                transform_request(&source, &plan, baseline, &input)
                    .expect("ineligible passthrough");
            assert_eq!(actual_baseline, baseline);
            assert_eq!(actual_input, input);
        }
    }

    #[test]
    fn review_uses_only_review_baseline_and_guardian_clone_is_unchanged() {
        let mut plan = InstructionOverlaysToml {
            model_baseline: Some(active("normal baseline")),
            review_baseline: Some(active("review baseline")),
            developer_guidance: Some(active("normal developer")),
            model_switch_guidance: Some(InstructionOverlayComponentToml {
                on_absent: InstructionOverlayOnAbsent::Skip,
                selection: InstructionOverlaySelection::Unique,
                edits: vec![InstructionOverlayEdit::Replace {
                    text: "normal transition".to_string(),
                }],
            }),
            ..Default::default()
        };
        let input = vec![developer_fragment(
            "generic.developer_instructions",
            "security policy",
        )];
        let (review_baseline, review_input) = transform_request(
            &SessionSource::SubAgent(SubAgentSource::Review),
            &plan,
            "rubric",
            &input,
        )
        .expect("review selection");
        assert_eq!(review_baseline, "review baseline");
        assert_eq!(review_input, input);

        let (normal_baseline, normal_input) =
            transform_request(&SessionSource::Cli, &plan, "normal baseline", &input)
                .expect("normal source ignores review-only plan");
        assert_eq!(normal_baseline, "normal baseline");
        assert_ne!(normal_input, input);

        plan.review_baseline = None;
        let guardian_source = SessionSource::Internal(InternalSessionSource::Guardian);
        let (guardian_baseline, guardian_input) =
            transform_request(&guardian_source, &plan, "guardian custom policy", &input)
                .expect("guardian source ignores inherited plan");
        assert_eq!(guardian_baseline, "guardian custom policy");
        assert_eq!(guardian_input, input);
    }

    #[test]
    fn tagged_components_require_unique_aligned_typed_text_and_clear_changed_ids() {
        let plan = InstructionOverlaysToml {
            developer_guidance: Some(active("replacement")),
            ..Default::default()
        };
        let source = SessionSource::Cli;
        let input = vec![developer_fragment("generic.developer_instructions", "old")];
        let (_, output) = transform_request(&source, &plan, "base", &input).expect("edit");
        let ResponseItem::Message {
            id,
            content,
            internal_chat_message_metadata_passthrough: Some(metadata),
            ..
        } = &output[0]
        else {
            panic!("developer message retained")
        };
        assert!(id.is_none());
        assert_eq!(
            content,
            &[ContentItem::InputText {
                text: "replacement".to_string()
            }]
        );
        assert_eq!(
            metadata.content_item_kinds.as_ref().expect("kind metadata")[0].as_str(),
            "generic.developer_instructions"
        );
        assert_eq!(
            input,
            vec![developer_fragment("generic.developer_instructions", "old")]
        );

        let ambiguous = vec![
            developer_fragment("generic.developer_instructions", "one"),
            developer_fragment("generic.developer_instructions", "two"),
        ];
        assert!(transform_request(&source, &plan, "base", &ambiguous).is_err());
    }

    #[test]
    fn model_baseline_edits_the_separate_request_instruction_without_touching_input() {
        let plan = InstructionOverlaysToml {
            model_baseline: Some(active("overlaid baseline")),
            ..Default::default()
        };
        let input = vec![
            developer_fragment("generic.developer_instructions", "policy"),
            developer_fragment(
                "model_switch.instructions",
                "<model_switch>transition text</model_switch>",
            ),
        ];
        let (baseline, output) =
            transform_request(&SessionSource::Cli, &plan, "model baseline", &input)
                .expect("baseline overlay");
        assert_eq!(baseline, "overlaid baseline");
        assert_eq!(output, input);
    }

    #[test]
    fn empty_explicit_bases_are_selected_but_incremental_placeholders_use_typed_base() {
        for (source, component, provenance) in [
            (
                SessionSource::Cli,
                InstructionOverlayComponent::ModelBaseline,
                BaseInstructionsProvenance::Custom,
            ),
            (
                SessionSource::Cli,
                InstructionOverlayComponent::ModelBaseline,
                BaseInstructionsProvenance::Model {
                    model: "fixture-model".to_string(),
                },
            ),
            (
                SessionSource::SubAgent(SubAgentSource::Review),
                InstructionOverlayComponent::ReviewBaseline,
                BaseInstructionsProvenance::Custom,
            ),
            (
                SessionSource::SubAgent(SubAgentSource::Review),
                InstructionOverlayComponent::ReviewBaseline,
                BaseInstructionsProvenance::Model {
                    model: "fixture-model".to_string(),
                },
            ),
        ] {
            let mut plan = InstructionOverlaysToml::default();
            match component {
                InstructionOverlayComponent::ModelBaseline => {
                    plan.model_baseline = Some(active("edited explicit empty base"));
                }
                InstructionOverlayComponent::ReviewBaseline => {
                    plan.review_baseline = Some(active("edited explicit empty review base"));
                }
                _ => unreachable!("only baseline components are covered"),
            }
            let (actual, input) =
                transform_request_with_provenance(&source, &plan, "", Some(&provenance), &[])
                    .expect("empty base with explicit provenance is selected");
            assert_eq!(
                actual,
                match component {
                    InstructionOverlayComponent::ModelBaseline => "edited explicit empty base",
                    InstructionOverlayComponent::ReviewBaseline => {
                        "edited explicit empty review base"
                    }
                    _ => unreachable!("only baseline components are covered"),
                }
            );
            assert!(input.is_empty());
        }

        let typed_base = vec![developer_fragment(
            "model.base_instructions",
            "typed incremental base",
        )];
        let plan = InstructionOverlaysToml {
            model_baseline: Some(active("edited typed base")),
            ..Default::default()
        };
        let (placeholder, output) =
            transform_request_with_provenance(&SessionSource::Cli, &plan, "", None, &typed_base)
                .expect("empty no-provenance placeholder selects typed base");
        assert!(placeholder.is_empty());
        let ResponseItem::Message { content, .. } = &output[0] else {
            panic!("typed base message retained")
        };
        assert_eq!(
            content,
            &[ContentItem::InputText {
                text: "edited typed base".to_string()
            }]
        );

        assert!(
            transform_request_with_provenance(&SessionSource::Cli, &plan, "", None, &[],).is_err()
        );
        let mut skipped = plan.clone();
        skipped.model_baseline.as_mut().unwrap().on_absent = InstructionOverlayOnAbsent::Skip;
        let (placeholder, input) =
            transform_request_with_provenance(&SessionSource::Cli, &skipped, "", None, &[])
                .expect("explicit skip accepts an absent typed base");
        assert!(placeholder.is_empty());
        assert!(input.is_empty());
    }

    #[test]
    fn scalar_and_typed_baselines_obey_selection_without_mutating_raw_input() {
        let source = SessionSource::Cli;
        let raw = vec![developer_fragment(
            "model.base_instructions",
            "typed source base",
        )];
        let unique = InstructionOverlaysToml {
            model_baseline: Some(active("overlay")),
            ..Default::default()
        };
        assert!(
            transform_request_with_provenance(
                &source,
                &unique,
                "scalar source base",
                Some(&BaseInstructionsProvenance::Custom),
                &raw,
            )
            .is_err()
        );

        let mut all = unique;
        all.model_baseline.as_mut().unwrap().selection = InstructionOverlaySelection::AllClassified;
        let (scalar, typed) = transform_request_with_provenance(
            &source,
            &all,
            "scalar source base",
            Some(&BaseInstructionsProvenance::Custom),
            &raw,
        )
        .expect("all_classified edits scalar and typed instances");
        assert_eq!(scalar, "overlay");
        let ResponseItem::Message { content, .. } = &typed[0] else {
            panic!("typed base message retained")
        };
        assert_eq!(
            content,
            &[ContentItem::InputText {
                text: "overlay".to_string()
            }]
        );
        assert_eq!(
            raw,
            vec![developer_fragment(
                "model.base_instructions",
                "typed source base",
            )]
        );
    }

    #[test]
    fn oversized_scalar_and_typed_sources_can_be_replaced_without_mutating_history() {
        let oversized = "source".repeat(MAX_INSTRUCTION_OVERLAY_BODY_BYTES / 6 + 1);
        let mut baseline_plan = active("short scalar");
        baseline_plan.edits = vec![InstructionOverlayEdit::Replace {
            text: "short scalar".to_string(),
        }];
        let mut developer_plan = active("short typed");
        developer_plan.edits = vec![InstructionOverlayEdit::Replace {
            text: "short typed".to_string(),
        }];
        let plan = InstructionOverlaysToml {
            model_baseline: Some(baseline_plan),
            developer_guidance: Some(developer_plan),
            ..Default::default()
        };
        let raw = vec![developer_fragment(
            "generic.developer_instructions",
            &oversized,
        )];
        let original = raw.clone();
        let (baseline, output) = transform_request_with_provenance(
            &SessionSource::Cli,
            &plan,
            &oversized,
            Some(&BaseInstructionsProvenance::Custom),
            &raw,
        )
        .expect("bounded replacements shrink oversized source fragments");
        assert_eq!(baseline, "short scalar");
        let mut expected = developer_fragment("generic.developer_instructions", "short typed");
        let ResponseItem::Message { id, .. } = &mut expected else {
            unreachable!("developer fragment is a message")
        };
        *id = None;
        assert_eq!(output, vec![expected]);
        assert_eq!(raw, original);
    }

    #[test]
    fn sixty_fifth_candidate_fails_before_any_candidate_preparation() {
        let mut component = active("unreachable");
        component.selection = InstructionOverlaySelection::AllClassified;
        component.edits = vec![InstructionOverlayEdit::ReplaceExact {
            from: "deliberately absent".to_string(),
            to: "unreachable".to_string(),
            expected_matches: 1,
        }];
        let plan = InstructionOverlaysToml {
            developer_guidance: Some(component),
            ..Default::default()
        };
        let raw = (0..=MAX_INSTRUCTION_OVERLAY_INSTANCES)
            .map(|_| developer_fragment("generic.developer_instructions", "bad match"))
            .collect::<Vec<_>>();
        let error = transform_request(&SessionSource::Cli, &plan, "base", &raw)
            .expect_err("the 65th classified target exceeds the cap first");
        assert!(error.to_string().contains("selected-instance limit"));
        assert!(!error.to_string().contains("match count"));
        assert!(raw.iter().all(|item| matches!(
            item,
            ResponseItem::Message { content, .. }
                if content == &vec![ContentItem::InputText { text: "bad match".to_string() }]
        )));
    }

    #[test]
    fn model_switch_guidance_edits_one_retained_candidate_per_raw_request_copy() {
        let plan = InstructionOverlaysToml {
            model_switch_guidance: Some(InstructionOverlayComponentToml {
                on_absent: InstructionOverlayOnAbsent::Skip,
                selection: InstructionOverlaySelection::Unique,
                edits: vec![InstructionOverlayEdit::Append {
                    text: " additional rule".to_string(),
                }],
            }),
            ..Default::default()
        };
        let raw = vec![developer_fragment(
            "model_switch.instructions",
            "<model_switch>transition text</model_switch>",
        )];
        let (_, first) = transform_request(&SessionSource::Cli, &plan, "base", &raw)
            .expect("single retained transition is eligible");
        let (_, second) = transform_request(&SessionSource::Cli, &plan, "base", &raw)
            .expect("each request starts from raw history");
        assert_eq!(first, second);
        assert_eq!(
            raw[0],
            developer_fragment(
                "model_switch.instructions",
                "<model_switch>transition text</model_switch>",
            )
        );
        let ResponseItem::Message { content, .. } = &first[0] else {
            panic!("transition message retained")
        };
        assert_eq!(
            content,
            &[ContentItem::InputText {
                text: "<model_switch>transition text additional rule</model_switch>".to_string(),
            }]
        );

        let absent_error = InstructionOverlaysToml {
            model_switch_guidance: Some(active("required")),
            ..Default::default()
        };
        assert!(transform_request(&SessionSource::Cli, &absent_error, "base", &[]).is_err());
        let (_, absent_skipped) =
            transform_request(&SessionSource::Cli, &plan, "base", &[]).expect("skip absent");
        assert!(absent_skipped.is_empty());

        let ambiguous = vec![raw[0].clone(), raw[0].clone()];
        assert!(transform_request(&SessionSource::Cli, &plan, "base", &ambiguous).is_err());
        assert_eq!(ambiguous.len(), 2);
        let unwrapped = vec![developer_fragment(
            "model_switch.instructions",
            "transition without wrapper",
        )];
        assert!(transform_request(&SessionSource::Cli, &plan, "base", &unwrapped).is_err());
    }

    #[test]
    fn all_classified_edits_each_known_component_instance_and_unique_stays_strict() {
        let mut config = active("updated");
        config.selection = InstructionOverlaySelection::AllClassified;
        let plan = InstructionOverlaysToml {
            developer_guidance: Some(config),
            ..Default::default()
        };
        let raw = vec![
            developer_fragment("generic.developer_instructions", "first"),
            developer_fragment("generic.developer_instructions", "second"),
        ];
        let (_, output) = transform_request(&SessionSource::Cli, &plan, "base", &raw)
            .expect("all classified instances selected");
        let (_, repeated) = transform_request(&SessionSource::Cli, &plan, "base", &raw)
            .expect("repeat starts from raw input");
        assert_eq!(output, repeated);
        assert_eq!(raw.len(), 2);
        for item in output {
            let ResponseItem::Message { content, id, .. } = item else {
                panic!("message retained")
            };
            assert!(id.is_none());
            assert_eq!(
                content,
                vec![ContentItem::InputText {
                    text: "updated".to_string()
                }]
            );
        }
        let strict = InstructionOverlaysToml {
            developer_guidance: Some(active("updated")),
            ..Default::default()
        };
        assert!(transform_request(&SessionSource::Cli, &strict, "base", &raw).is_err());
    }

    #[test]
    fn all_classified_is_atomic_and_obeys_global_instance_cap() {
        let mut edit = InstructionOverlayComponentToml {
            on_absent: InstructionOverlayOnAbsent::Error,
            selection: InstructionOverlaySelection::AllClassified,
            edits: vec![InstructionOverlayEdit::ReplaceExact {
                from: "match".to_string(),
                to: "ok".to_string(),
                expected_matches: 1,
            }],
        };
        let plan = InstructionOverlaysToml {
            developer_guidance: Some(edit.clone()),
            ..Default::default()
        };
        let raw = vec![
            developer_fragment("generic.developer_instructions", "match"),
            developer_fragment("generic.developer_instructions", "unrelated"),
        ];
        assert!(transform_request(&SessionSource::Cli, &plan, "base", &raw).is_err());
        assert_eq!(
            raw[0],
            developer_fragment("generic.developer_instructions", "match")
        );
        edit.edits = vec![InstructionOverlayEdit::Append {
            text: "x".repeat(128).to_string(),
        }];
        let crowded = InstructionOverlaysToml {
            developer_guidance: Some(edit),
            ..Default::default()
        };
        let many = (0..=MAX_INSTRUCTION_OVERLAY_INSTANCES)
            .map(|_| developer_fragment("generic.developer_instructions", "x"))
            .collect::<Vec<_>>();
        assert!(transform_request(&SessionSource::Cli, &crowded, "base", &many).is_err());
    }

    #[test]
    fn selected_instance_cap_is_shared_across_components() {
        let mut developer = active("changed");
        developer.selection = InstructionOverlaySelection::AllClassified;
        let mut usage = active("changed");
        usage.selection = InstructionOverlaySelection::AllClassified;
        let plan = InstructionOverlaysToml {
            developer_guidance: Some(developer),
            multi_agent_usage_hint: Some(usage),
            ..Default::default()
        };
        let mut input = (0..32)
            .map(|_| developer_fragment("generic.developer_instructions", "dev"))
            .collect::<Vec<_>>();
        input.extend((0..32).map(|_| developer_fragment("multi_agent.usage_hint", "usage")));
        assert!(transform_request(&SessionSource::Cli, &plan, "base", &input).is_ok());

        input.push(developer_fragment("multi_agent.usage_hint", "one too many"));
        assert!(transform_request(&SessionSource::Cli, &plan, "base", &input).is_err());
    }

    #[test]
    fn all_classified_preflights_combined_request_output_limit() {
        let mut component = active("");
        component.selection = InstructionOverlaySelection::AllClassified;
        component.edits = (0..8)
            .map(|_| InstructionOverlayEdit::Append {
                text: "x".repeat(32 * 1024),
            })
            .collect();
        let plan = InstructionOverlaysToml {
            developer_guidance: Some(component),
            ..Default::default()
        };
        let input = (0..17)
            .map(|_| developer_fragment("generic.developer_instructions", ""))
            .collect::<Vec<_>>();
        assert!(transform_request(&SessionSource::Cli, &plan, "base", &input).is_err());
        assert!(input.iter().all(|item| matches!(item, ResponseItem::Message { content, .. } if content == &vec![ContentItem::InputText { text: String::new() }])));
    }

    #[test]
    fn collaboration_and_persistent_overlays_preserve_required_wrappers() {
        let plan = InstructionOverlaysToml {
            collaboration_guidance: Some(active("collaboration")),
            persistent_guidance: Some(active("persistent")),
            ..Default::default()
        };
        let input = vec![
            developer_fragment(
                "collaboration_mode.instructions",
                "<collaboration_mode>old</collaboration_mode>",
            ),
            developer_fragment(
                "persistent_mode.instructions",
                "<persistent_mode>old</persistent_mode>",
            ),
        ];
        let (_, output) = transform_request(&SessionSource::Cli, &plan, "base", &input)
            .expect("known client fragments");
        let texts = output
            .iter()
            .filter_map(|item| match item {
                ResponseItem::Message { content, .. } => {
                    content.iter().find_map(|item| match item {
                        ContentItem::InputText { text } => Some(text.as_str()),
                        _ => None,
                    })
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(texts.contains(&"<collaboration_mode>collaboration</collaboration_mode>"));
        assert!(texts.contains(&"<persistent_mode>persistent</persistent_mode>"));
        assert!(
            transform_request(
                &SessionSource::Cli,
                &plan,
                "base",
                &[developer_fragment(
                    "persistent_mode.instructions",
                    "unwrapped persistent",
                )],
            )
            .is_err()
        );
    }

    #[test]
    fn review_baseline_can_edit_its_typed_request_copy() {
        let plan = InstructionOverlaysToml {
            review_baseline: Some(active("review")),
            ..Default::default()
        };
        let raw = vec![developer_fragment(
            "model.base_instructions",
            "<review>old</review>",
        )];
        let (baseline, output) = transform_request(
            &SessionSource::SubAgent(SubAgentSource::Review),
            &plan,
            "",
            &raw,
        )
        .expect("review baseline fragment");
        assert_eq!(baseline, "");
        let ResponseItem::Message { content, id, .. } = &output[0] else {
            panic!("message retained")
        };
        assert_eq!(
            content,
            &[ContentItem::InputText {
                text: "review".to_string()
            }]
        );
        assert!(id.is_none());
        assert_eq!(
            raw[0],
            developer_fragment("model.base_instructions", "<review>old</review>")
        );
    }

    #[test]
    fn all_classified_baseline_edits_separate_and_typed_request_copies() {
        let mut component = active("updated");
        component.selection = InstructionOverlaySelection::AllClassified;
        let plan = InstructionOverlaysToml {
            model_baseline: Some(component),
            ..Default::default()
        };
        let raw = vec![developer_fragment(
            "model.base_instructions",
            "typed original",
        )];
        let (baseline, output) =
            transform_request(&SessionSource::Cli, &plan, "separate original", &raw)
                .expect("explicit all-classified selection includes known baseline instances");
        assert_eq!(baseline, "updated");
        let ResponseItem::Message { content, .. } = &output[0] else {
            panic!("typed message retained")
        };
        assert_eq!(
            content,
            &[ContentItem::InputText {
                text: "updated".to_string()
            }]
        );
        assert_eq!(
            raw[0],
            developer_fragment("model.base_instructions", "typed original")
        );
        let strict = InstructionOverlaysToml {
            model_baseline: Some(active("updated")),
            ..Default::default()
        };
        assert!(
            transform_request(&SessionSource::Cli, &strict, "separate original", &raw).is_err()
        );
    }

    #[test]
    fn wrapped_mode_target_preserves_protocol_owned_markers() {
        let plan = InstructionOverlaysToml {
            multi_agent_mode: Some(active("new mode body")),
            ..Default::default()
        };
        let original = format!("{MULTI_AGENT_MODE_OPEN_TAG}old mode{MULTI_AGENT_MODE_CLOSE_TAG}");
        let input = vec![developer_fragment(
            "multi_agent.mode_instructions",
            &original,
        )];
        let (_, output) = transform_request(&SessionSource::Cli, &plan, "base", &input)
            .expect("wrapped mode overlay");
        let ResponseItem::Message { content, .. } = &output[0] else {
            panic!("mode message retained")
        };
        assert_eq!(
            content,
            &[ContentItem::InputText {
                text: format!(
                    "{MULTI_AGENT_MODE_OPEN_TAG}new mode body{MULTI_AGENT_MODE_CLOSE_TAG}"
                ),
            }]
        );
    }

    #[test]
    fn missing_tagged_target_obeys_explicit_absence_policy() {
        let source = SessionSource::Cli;
        let plan = InstructionOverlaysToml {
            developer_guidance: Some(active("replacement")),
            ..Default::default()
        };
        assert!(transform_request(&source, &plan, "base", &[]).is_err());

        let plan = InstructionOverlaysToml {
            developer_guidance: Some(InstructionOverlayComponentToml {
                on_absent: InstructionOverlayOnAbsent::Skip,
                selection: InstructionOverlaySelection::Unique,
                edits: vec![InstructionOverlayEdit::Replace {
                    text: "replacement".to_string(),
                }],
            }),
            ..Default::default()
        };
        let (baseline, input) = transform_request(&source, &plan, "base", &[])
            .expect("explicitly optional missing target");
        assert_eq!(baseline, "base");
        assert!(input.is_empty());
    }
}
