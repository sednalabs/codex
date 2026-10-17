use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde::de::Error as SerdeError;
use std::fmt;

pub const MAX_INSTRUCTION_OVERLAY_OPERATIONS: usize = 64;
pub const MAX_INSTRUCTION_OVERLAY_LITERAL_BYTES: usize = 32 * 1024;
pub const MAX_INSTRUCTION_OVERLAY_TOTAL_LITERAL_BYTES: usize = 256 * 1024;
pub const MAX_INSTRUCTION_OVERLAY_BODY_BYTES: usize = 256 * 1024;
pub const MAX_INSTRUCTION_OVERLAY_INSTANCES: usize = 64;
pub const MAX_INSTRUCTION_OVERLAY_REQUEST_BYTES: usize = 4 * 1024 * 1024;

const ALL_COMPONENTS: &[InstructionOverlayComponent] = &[
    InstructionOverlayComponent::ModelBaseline,
    InstructionOverlayComponent::ReviewBaseline,
    InstructionOverlayComponent::DeveloperGuidance,
    InstructionOverlayComponent::MultiAgentMode,
    InstructionOverlayComponent::MultiAgentRole,
    InstructionOverlayComponent::MultiAgentUsageHint,
    InstructionOverlayComponent::ModelSwitchGuidance,
    InstructionOverlayComponent::CollaborationGuidance,
    InstructionOverlayComponent::PersistentGuidance,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InstructionOverlayComponent {
    ModelBaseline,
    ReviewBaseline,
    DeveloperGuidance,
    MultiAgentMode,
    MultiAgentRole,
    MultiAgentUsageHint,
    ModelSwitchGuidance,
    CollaborationGuidance,
    PersistentGuidance,
}

impl InstructionOverlayComponent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelBaseline => "model_baseline",
            Self::ReviewBaseline => "review_baseline",
            Self::DeveloperGuidance => "developer_guidance",
            Self::MultiAgentMode => "multi_agent_mode",
            Self::MultiAgentRole => "multi_agent_role",
            Self::MultiAgentUsageHint => "multi_agent_usage_hint",
            Self::ModelSwitchGuidance => "model_switch_guidance",
            Self::CollaborationGuidance => "collaboration_guidance",
            Self::PersistentGuidance => "persistent_guidance",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructionOverlayOnAbsent {
    #[default]
    Error,
    Skip,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructionOverlaySelection {
    #[default]
    Unique,
    AllClassified,
}

impl<'de> Deserialize<'de> for InstructionOverlaySelection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "unique" => Ok(Self::Unique),
            "all_classified" => Ok(Self::AllClassified),
            _ => Err(D::Error::custom("invalid instruction overlay selection")),
        }
    }
}

impl<'de> Deserialize<'de> for InstructionOverlayOnAbsent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "error" => Ok(Self::Error),
            "skip" => Ok(Self::Skip),
            _ => Err(D::Error::custom(
                "invalid instruction overlay on_absent value",
            )),
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct InstructionOverlayComponentToml {
    #[serde(default)]
    pub on_absent: InstructionOverlayOnAbsent,
    #[serde(default)]
    pub selection: InstructionOverlaySelection,
    #[serde(default)]
    pub edits: Vec<InstructionOverlayEdit>,
}

impl fmt::Debug for InstructionOverlayComponentToml {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstructionOverlayComponentToml")
            .field("on_absent", &self.on_absent)
            .field("selection", &self.selection)
            .field("edits", &self.edits)
            .finish()
    }
}

#[derive(Clone, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionOverlayEdit {
    Prepend {
        text: String,
    },
    Append {
        text: String,
    },
    Replace {
        text: String,
    },
    ReplaceExact {
        from: String,
        to: String,
        #[schemars(range(min = 1))]
        expected_matches: usize,
    },
    RemoveExact {
        text: String,
        #[schemars(range(min = 1))]
        expected_matches: usize,
    },
}

impl<'de> Deserialize<'de> for InstructionOverlayEdit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
        enum Repr {
            Prepend {
                text: String,
            },
            Append {
                text: String,
            },
            Replace {
                text: String,
            },
            ReplaceExact {
                from: String,
                to: String,
                expected_matches: usize,
            },
            RemoveExact {
                text: String,
                expected_matches: usize,
            },
        }

        let repr = Repr::deserialize(deserializer)
            .map_err(|_| D::Error::custom("invalid instruction overlay operation"))?;
        Ok(match repr {
            Repr::Prepend { text } => Self::Prepend { text },
            Repr::Append { text } => Self::Append { text },
            Repr::Replace { text } => Self::Replace { text },
            Repr::ReplaceExact {
                from,
                to,
                expected_matches,
            } => Self::ReplaceExact {
                from,
                to,
                expected_matches,
            },
            Repr::RemoveExact {
                text,
                expected_matches,
            } => Self::RemoveExact {
                text,
                expected_matches,
            },
        })
    }
}

impl InstructionOverlayEdit {
    fn operation_name(&self) -> &'static str {
        match self {
            Self::Prepend { .. } => "prepend",
            Self::Append { .. } => "append",
            Self::Replace { .. } => "replace",
            Self::ReplaceExact { .. } => "replace_exact",
            Self::RemoveExact { .. } => "remove_exact",
        }
    }
}

impl fmt::Debug for InstructionOverlayEdit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstructionOverlayEdit")
            .field("op", &self.operation_name())
            .field("text", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Default, Serialize, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct InstructionOverlaysToml {
    pub model_baseline: Option<InstructionOverlayComponentToml>,
    pub review_baseline: Option<InstructionOverlayComponentToml>,
    pub developer_guidance: Option<InstructionOverlayComponentToml>,
    pub multi_agent_mode: Option<InstructionOverlayComponentToml>,
    pub multi_agent_role: Option<InstructionOverlayComponentToml>,
    pub multi_agent_usage_hint: Option<InstructionOverlayComponentToml>,
    pub model_switch_guidance: Option<InstructionOverlayComponentToml>,
    pub collaboration_guidance: Option<InstructionOverlayComponentToml>,
    pub persistent_guidance: Option<InstructionOverlayComponentToml>,
}

impl<'de> Deserialize<'de> for InstructionOverlaysToml {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(clippy::struct_field_names)]
        struct Repr {
            #[serde(default)]
            model_baseline: Option<InstructionOverlayComponentToml>,
            #[serde(default)]
            review_baseline: Option<InstructionOverlayComponentToml>,
            #[serde(default)]
            developer_guidance: Option<InstructionOverlayComponentToml>,
            #[serde(default)]
            multi_agent_mode: Option<InstructionOverlayComponentToml>,
            #[serde(default)]
            multi_agent_role: Option<InstructionOverlayComponentToml>,
            #[serde(default)]
            multi_agent_usage_hint: Option<InstructionOverlayComponentToml>,
            #[serde(default)]
            model_switch_guidance: Option<InstructionOverlayComponentToml>,
            #[serde(default)]
            collaboration_guidance: Option<InstructionOverlayComponentToml>,
            #[serde(default)]
            persistent_guidance: Option<InstructionOverlayComponentToml>,
        }

        let raw = Repr::deserialize(deserializer)
            .map_err(|_| D::Error::custom("invalid instruction overlay configuration"))?;
        Ok(Self {
            model_baseline: raw.model_baseline,
            review_baseline: raw.review_baseline,
            developer_guidance: raw.developer_guidance,
            multi_agent_mode: raw.multi_agent_mode,
            multi_agent_role: raw.multi_agent_role,
            multi_agent_usage_hint: raw.multi_agent_usage_hint,
            model_switch_guidance: raw.model_switch_guidance,
            collaboration_guidance: raw.collaboration_guidance,
            persistent_guidance: raw.persistent_guidance,
        })
    }
}

impl fmt::Debug for InstructionOverlaysToml {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstructionOverlaysToml")
            .field("model_baseline", &self.model_baseline)
            .field("review_baseline", &self.review_baseline)
            .field("developer_guidance", &self.developer_guidance)
            .field("multi_agent_mode", &self.multi_agent_mode)
            .field("multi_agent_role", &self.multi_agent_role)
            .field("multi_agent_usage_hint", &self.multi_agent_usage_hint)
            .field("model_switch_guidance", &self.model_switch_guidance)
            .field("collaboration_guidance", &self.collaboration_guidance)
            .field("persistent_guidance", &self.persistent_guidance)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstructionOverlayError {
    TooManyOperations,
    LiteralTooLarge,
    TotalLiteralTooLarge,
    EmptySearch,
    ZeroExpectedMatches,
    TargetAbsent,
    MatchCountMismatch { expected: usize, actual: usize },
    BodyTooLarge,
}

impl fmt::Display for InstructionOverlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyOperations => f.write_str("instruction overlay operation limit exceeded"),
            Self::LiteralTooLarge => f.write_str("instruction overlay literal size limit exceeded"),
            Self::TotalLiteralTooLarge => {
                f.write_str("instruction overlay total literal size limit exceeded")
            }
            Self::EmptySearch => f.write_str("instruction overlay search text must not be empty"),
            Self::ZeroExpectedMatches => {
                f.write_str("instruction overlay expected_matches must be greater than zero")
            }
            Self::TargetAbsent => f.write_str("instruction overlay target is absent"),
            Self::MatchCountMismatch { expected, actual } => write!(
                f,
                "instruction overlay match count mismatch (expected {expected}, found {actual})"
            ),
            Self::BodyTooLarge => f.write_str("instruction overlay output size limit exceeded"),
        }
    }
}

impl std::error::Error for InstructionOverlayError {}

impl InstructionOverlaysToml {
    pub fn component(
        &self,
        component: InstructionOverlayComponent,
    ) -> Option<&InstructionOverlayComponentToml> {
        match component {
            InstructionOverlayComponent::ModelBaseline => self.model_baseline.as_ref(),
            InstructionOverlayComponent::ReviewBaseline => self.review_baseline.as_ref(),
            InstructionOverlayComponent::DeveloperGuidance => self.developer_guidance.as_ref(),
            InstructionOverlayComponent::MultiAgentMode => self.multi_agent_mode.as_ref(),
            InstructionOverlayComponent::MultiAgentRole => self.multi_agent_role.as_ref(),
            InstructionOverlayComponent::MultiAgentUsageHint => {
                self.multi_agent_usage_hint.as_ref()
            }
            InstructionOverlayComponent::ModelSwitchGuidance => self.model_switch_guidance.as_ref(),
            InstructionOverlayComponent::CollaborationGuidance => {
                self.collaboration_guidance.as_ref()
            }
            InstructionOverlayComponent::PersistentGuidance => self.persistent_guidance.as_ref(),
        }
    }

    pub fn validate_all(&self) -> Result<(), InstructionOverlayError> {
        self.validate_components(ALL_COMPONENTS)
    }

    pub fn validate_components(
        &self,
        components: &[InstructionOverlayComponent],
    ) -> Result<(), InstructionOverlayError> {
        let mut operations = 0usize;
        let mut literal_bytes = 0usize;
        for component in components {
            let Some(plan) = self.component(*component) else {
                continue;
            };
            // An empty higher-precedence edits array deliberately disables an inherited plan.
            if plan.edits.is_empty() {
                continue;
            }
            operations = operations.saturating_add(plan.edits.len());
            if operations > MAX_INSTRUCTION_OVERLAY_OPERATIONS {
                return Err(InstructionOverlayError::TooManyOperations);
            }
            for edit in &plan.edits {
                let literal_sizes = match edit {
                    InstructionOverlayEdit::Prepend { text }
                    | InstructionOverlayEdit::Append { text }
                    | InstructionOverlayEdit::Replace { text }
                    | InstructionOverlayEdit::RemoveExact { text, .. } => [text.len(), 0],
                    InstructionOverlayEdit::ReplaceExact { from, to, .. } => [from.len(), to.len()],
                };
                for bytes in literal_sizes {
                    if bytes > MAX_INSTRUCTION_OVERLAY_LITERAL_BYTES {
                        return Err(InstructionOverlayError::LiteralTooLarge);
                    }
                    literal_bytes = literal_bytes.saturating_add(bytes);
                    if literal_bytes > MAX_INSTRUCTION_OVERLAY_TOTAL_LITERAL_BYTES {
                        return Err(InstructionOverlayError::TotalLiteralTooLarge);
                    }
                }
                match edit {
                    InstructionOverlayEdit::ReplaceExact {
                        from,
                        expected_matches,
                        ..
                    }
                    | InstructionOverlayEdit::RemoveExact {
                        text: from,
                        expected_matches,
                    } => {
                        if from.is_empty() {
                            return Err(InstructionOverlayError::EmptySearch);
                        }
                        if *expected_matches == 0 {
                            return Err(InstructionOverlayError::ZeroExpectedMatches);
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    pub fn apply_component(
        &self,
        component: InstructionOverlayComponent,
        body: Option<&str>,
    ) -> Result<Option<String>, InstructionOverlayError> {
        self.validate_components(&[component])?;
        let Some(plan) = self
            .component(component)
            .filter(|plan| !plan.edits.is_empty())
        else {
            return Ok(body.map(str::to_owned));
        };
        let Some(body) = body else {
            return match plan.on_absent {
                InstructionOverlayOnAbsent::Skip => Ok(None),
                InstructionOverlayOnAbsent::Error => Err(InstructionOverlayError::TargetAbsent),
            };
        };

        let mut output: Option<String> = None;
        for edit in &plan.edits {
            let current = output.as_deref().unwrap_or(body);
            output = Some(apply_edit(current, edit)?);
        }
        Ok(output)
    }
}

fn apply_edit(
    body: &str,
    edit: &InstructionOverlayEdit,
) -> Result<String, InstructionOverlayError> {
    let output = match edit {
        InstructionOverlayEdit::Prepend { text } => {
            check_output_length(body.len(), text.len())?;
            let mut output = String::with_capacity(text.len().saturating_add(body.len()));
            output.push_str(text);
            output.push_str(body);
            output
        }
        InstructionOverlayEdit::Append { text } => {
            check_output_length(body.len(), text.len())?;
            let mut output = String::with_capacity(text.len().saturating_add(body.len()));
            output.push_str(body);
            output.push_str(text);
            output
        }
        InstructionOverlayEdit::Replace { text } => {
            if text.len() > MAX_INSTRUCTION_OVERLAY_BODY_BYTES {
                return Err(InstructionOverlayError::BodyTooLarge);
            }
            text.clone()
        }
        InstructionOverlayEdit::ReplaceExact {
            from,
            to,
            expected_matches,
        } => replace_exact(body, from, to, *expected_matches)?,
        InstructionOverlayEdit::RemoveExact {
            text,
            expected_matches,
        } => replace_exact(body, text, "", *expected_matches)?,
    };
    Ok(output)
}

fn replace_exact(
    body: &str,
    from: &str,
    to: &str,
    expected_matches: usize,
) -> Result<String, InstructionOverlayError> {
    let actual = body.match_indices(from).count();
    if actual != expected_matches {
        return Err(InstructionOverlayError::MatchCountMismatch {
            expected: expected_matches,
            actual,
        });
    }
    let removed_bytes = from.len().saturating_mul(actual);
    let inserted_bytes = to.len().saturating_mul(actual);
    let output_len = body
        .len()
        .saturating_sub(removed_bytes)
        .saturating_add(inserted_bytes);
    if output_len > MAX_INSTRUCTION_OVERLAY_BODY_BYTES {
        return Err(InstructionOverlayError::BodyTooLarge);
    }
    Ok(body.replace(from, to))
}

fn check_output_length(current: usize, added: usize) -> Result<(), InstructionOverlayError> {
    if current.saturating_add(added) > MAX_INSTRUCTION_OVERLAY_BODY_BYTES {
        return Err(InstructionOverlayError::BodyTooLarge);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(edits: Vec<InstructionOverlayEdit>) -> InstructionOverlayComponentToml {
        InstructionOverlayComponentToml {
            on_absent: InstructionOverlayOnAbsent::Error,
            selection: InstructionOverlaySelection::Unique,
            edits,
        }
    }

    #[test]
    fn invalid_overlay_keys_are_redacted_at_typed_boundary() {
        let err =
            toml::from_str::<InstructionOverlaysToml>("private_component_key = { edits = [] }")
                .expect_err("unknown overlay key is rejected");
        assert!(!err.to_string().contains("private_component_key"));
        assert!(
            err.to_string()
                .contains("invalid instruction overlay configuration")
        );
    }

    #[test]
    fn selection_defaults_to_unique_and_accepts_only_closed_values() {
        let unique = toml::from_str::<InstructionOverlaysToml>(
            "[developer_guidance]\nedits = [{ op = 'append', text = 'x' }]",
        )
        .expect("selection defaults");
        assert_eq!(
            unique.developer_guidance.expect("component").selection,
            InstructionOverlaySelection::Unique
        );
        let all = toml::from_str::<InstructionOverlaysToml>(
            "[developer_guidance]\nselection = 'all_classified'\nedits = [{ op = 'append', text = 'x' }]",
        )
        .expect("closed opt-in selection");
        assert_eq!(
            all.developer_guidance.expect("component").selection,
            InstructionOverlaySelection::AllClassified
        );
        let err = toml::from_str::<InstructionOverlaysToml>(
            "[developer_guidance]\nselection = 'PRIVATE_SELECTION_SENTINEL'",
        )
        .expect_err("unknown selection is rejected");
        assert!(!err.to_string().contains("PRIVATE_SELECTION_SENTINEL"));
    }

    #[test]
    fn replace_exact_bounds_each_literal_independently() {
        let plan = InstructionOverlaysToml {
            developer_guidance: Some(component(vec![InstructionOverlayEdit::ReplaceExact {
                from: "a".repeat(MAX_INSTRUCTION_OVERLAY_LITERAL_BYTES),
                to: "b".repeat(MAX_INSTRUCTION_OVERLAY_LITERAL_BYTES),
                expected_matches: 1,
            }])),
            ..Default::default()
        };
        assert!(plan.validate_all().is_ok());
    }

    #[test]
    fn applies_ordered_literal_edits_verbatim_and_redacts_debug() {
        let plan = InstructionOverlaysToml {
            model_baseline: Some(component(vec![
                InstructionOverlayEdit::Prepend {
                    text: "π\n".to_string(),
                },
                InstructionOverlayEdit::ReplaceExact {
                    from: "old".to_string(),
                    to: "new".to_string(),
                    expected_matches: 1,
                },
                InstructionOverlayEdit::Append {
                    text: "\nend".to_string(),
                },
            ])),
            ..Default::default()
        };
        assert_eq!(
            plan.apply_component(
                InstructionOverlayComponent::ModelBaseline,
                /*body*/ Some("old"),
            )
            .expect("bounded edit"),
            Some("π\nnew\nend".to_string())
        );
        let debug = format!("{plan:?}");
        assert!(!debug.contains("old"));
        assert!(!debug.contains("new"));
        assert!(!debug.contains("π"));
    }

    #[test]
    fn replaces_removes_and_handles_absent_targets_explicitly() {
        let plan = InstructionOverlaysToml {
            developer_guidance: Some(component(vec![InstructionOverlayEdit::Replace {
                text: String::new(),
            }])),
            multi_agent_role: Some(InstructionOverlayComponentToml {
                on_absent: InstructionOverlayOnAbsent::Skip,
                selection: InstructionOverlaySelection::Unique,
                edits: vec![InstructionOverlayEdit::RemoveExact {
                    text: "optional".to_string(),
                    expected_matches: 1,
                }],
            }),
            ..Default::default()
        };
        assert_eq!(
            plan.apply_component(
                InstructionOverlayComponent::DeveloperGuidance,
                /*body*/ Some("value"),
            )
            .expect("replace"),
            Some(String::new())
        );
        assert_eq!(
            plan.apply_component(
                InstructionOverlayComponent::DeveloperGuidance,
                /*body*/ Some(""),
            )
            .expect("selected empty value"),
            Some(String::new())
        );
        assert_eq!(
            plan.apply_component(
                InstructionOverlayComponent::DeveloperGuidance,
                /*body*/ None,
            )
            .expect_err("default on_absent rejects missing target"),
            InstructionOverlayError::TargetAbsent
        );
        assert_eq!(
            plan.apply_component(
                InstructionOverlayComponent::MultiAgentRole,
                /*body*/ None,
            )
            .expect("optional absence"),
            None
        );
        assert!(matches!(
            plan.apply_component(
                InstructionOverlayComponent::MultiAgentRole,
                /*body*/ Some("optional"),
            ),
            Ok(Some(value)) if value.is_empty()
        ));
    }

    #[test]
    fn rejects_invalid_counts_and_limits_without_returning_partial_text() {
        let plan = InstructionOverlaysToml {
            model_baseline: Some(component(vec![InstructionOverlayEdit::ReplaceExact {
                from: "needle".to_string(),
                to: "changed".to_string(),
                expected_matches: 2,
            }])),
            ..Default::default()
        };
        let error = plan
            .apply_component(
                InstructionOverlayComponent::ModelBaseline,
                /*body*/ Some("prefix needle"),
            )
            .expect_err("count mismatch");
        assert_eq!(
            error,
            InstructionOverlayError::MatchCountMismatch {
                expected: 2,
                actual: 1
            }
        );
        assert!(!error.to_string().contains("needle"));
        assert!(!error.to_string().contains("changed"));

        let empty_search = InstructionOverlaysToml {
            model_baseline: Some(component(vec![InstructionOverlayEdit::RemoveExact {
                text: String::new(),
                expected_matches: 1,
            }])),
            ..Default::default()
        };
        assert_eq!(
            empty_search
                .validate_components(&[InstructionOverlayComponent::ModelBaseline])
                .expect_err("empty search"),
            InstructionOverlayError::EmptySearch
        );
    }

    #[test]
    fn rejects_too_large_literals_and_final_bodies() {
        let literal = "x".repeat(MAX_INSTRUCTION_OVERLAY_LITERAL_BYTES + 1);
        let plan = InstructionOverlaysToml {
            model_baseline: Some(component(vec![InstructionOverlayEdit::Append {
                text: literal,
            }])),
            ..Default::default()
        };
        assert_eq!(
            plan.validate_components(&[InstructionOverlayComponent::ModelBaseline])
                .expect_err("large literal"),
            InstructionOverlayError::LiteralTooLarge
        );

        let body = "x".repeat(MAX_INSTRUCTION_OVERLAY_BODY_BYTES);
        let plan = InstructionOverlaysToml {
            model_baseline: Some(component(vec![InstructionOverlayEdit::Append {
                text: "y".to_string(),
            }])),
            ..Default::default()
        };
        assert_eq!(
            plan.apply_component(
                InstructionOverlayComponent::ModelBaseline,
                /*body*/ Some(&body),
            )
            .expect_err("oversized output"),
            InstructionOverlayError::BodyTooLarge
        );
    }

    #[test]
    fn large_source_can_be_shrunk_and_growth_or_count_failures_preflight() {
        let source = "large source".repeat(MAX_INSTRUCTION_OVERLAY_BODY_BYTES / 12 + 100);
        let shrink = InstructionOverlaysToml {
            developer_guidance: Some(component(vec![InstructionOverlayEdit::Replace {
                text: "small".to_string(),
            }])),
            ..Default::default()
        };
        assert_eq!(
            shrink
                .apply_component(
                    InstructionOverlayComponent::DeveloperGuidance,
                    /*body*/ Some(&source),
                )
                .expect("replace can shrink a large input source"),
            Some("small".to_string())
        );

        let too_large = "x".repeat(MAX_INSTRUCTION_OVERLAY_BODY_BYTES);
        for edit in [
            InstructionOverlayEdit::Prepend {
                text: "y".to_string(),
            },
            InstructionOverlayEdit::Append {
                text: "y".to_string(),
            },
        ] {
            let plan = InstructionOverlaysToml {
                developer_guidance: Some(component(vec![edit])),
                ..Default::default()
            };
            assert_eq!(
                plan.apply_component(
                    InstructionOverlayComponent::DeveloperGuidance,
                    /*body*/ Some(&too_large),
                )
                .expect_err("growth over the component cap is rejected before allocation"),
                InstructionOverlayError::BodyTooLarge
            );
        }

        for edit in [
            InstructionOverlayEdit::ReplaceExact {
                from: "absent".to_string(),
                to: "replace".to_string(),
                expected_matches: 1,
            },
            InstructionOverlayEdit::RemoveExact {
                text: "absent".to_string(),
                expected_matches: 1,
            },
        ] {
            let plan = InstructionOverlaysToml {
                developer_guidance: Some(component(vec![edit])),
                ..Default::default()
            };
            assert!(matches!(
                plan.apply_component(
                    InstructionOverlayComponent::DeveloperGuidance,
                    /*body*/ Some(&source),
                ),
                Err(InstructionOverlayError::MatchCountMismatch {
                    expected: 1,
                    actual: 0
                })
            ));
        }
    }

    #[test]
    fn deserialization_is_closed_and_does_not_echo_unknown_operation_values() {
        let secret = "PRIVATE_OPERATION_VALUE_MUST_NOT_ECHO";
        let source =
            format!("[model_baseline]\nedits = [{{ op = '{secret}', text = 'private text' }}]");
        let error = toml::from_str::<InstructionOverlaysToml>(&source)
            .expect_err("unknown overlay operation rejected");
        assert!(!error.to_string().contains(secret));
        assert!(!error.to_string().contains("private text"));

        let unknown_on_absent = toml::from_str::<InstructionOverlaysToml>(
            "[model_baseline]\non_absent = 'PRIVATE_ABSENCE_VALUE'\nedits = [{ op = 'replace', text = 'x' }]",
        )
        .expect_err("unknown absence policy rejected");
        assert!(
            !unknown_on_absent
                .to_string()
                .contains("PRIVATE_ABSENCE_VALUE")
        );
    }

    #[test]
    fn empty_edits_table_disables_a_component_and_default_absence_is_error() {
        let parsed: InstructionOverlaysToml =
            toml::from_str("[model_baseline]\nedits = []").expect("closed component config");
        let component = parsed.model_baseline.as_ref().expect("selected component");
        assert_eq!(component.on_absent, InstructionOverlayOnAbsent::Error);
        assert_eq!(component.edits.len(), 0);
        assert_eq!(
            parsed
                .apply_component(
                    InstructionOverlayComponent::ModelBaseline,
                    /*body*/ Some("raw"),
                )
                .expect("empty table disables edits"),
            Some("raw".to_string())
        );
    }
}
