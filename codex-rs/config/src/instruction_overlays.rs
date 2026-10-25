use schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructionOverlayOnAbsent {
    #[default]
    Error,
    Skip,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructionOverlaySelection {
    #[default]
    Unique,
    AllClassified,
}

#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct InstructionOverlayComponentToml {
    #[serde(default)]
    pub on_absent: InstructionOverlayOnAbsent,
    /// Exact model slugs for this target; omitted means the target applies to every model.
    #[serde(default)]
    pub models: Option<Vec<String>>,
    #[serde(default)]
    pub selection: InstructionOverlaySelection,
    #[serde(default)]
    pub edits: Vec<InstructionOverlayEdit>,
}

impl fmt::Debug for InstructionOverlayComponentToml {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstructionOverlayComponentToml")
            .field("on_absent", &self.on_absent)
            .field("models", &self.models)
            .field("selection", &self.selection)
            .field("edits", &self.edits)
            .finish()
    }
}

impl InstructionOverlayComponentToml {
    /// Returns whether this overlay target is enabled for the exact model slug.
    pub fn applies_to_model(&self, model_slug: &str) -> bool {
        self.models
            .as_ref()
            .is_none_or(|models| models.iter().any(|model| model == model_slug))
    }
}

#[derive(Clone, Serialize, PartialEq, Eq, JsonSchema)]
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

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum RawInstructionOverlayEdit {
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

impl<'de> Deserialize<'de> for InstructionOverlayEdit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let edit = RawInstructionOverlayEdit::deserialize(deserializer).map_err(|_| {
            <D::Error as serde::de::Error>::custom("invalid instruction overlay edit")
        })?;
        Ok(match edit {
            RawInstructionOverlayEdit::Prepend { text } => Self::Prepend { text },
            RawInstructionOverlayEdit::Append { text } => Self::Append { text },
            RawInstructionOverlayEdit::Replace { text } => Self::Replace { text },
            RawInstructionOverlayEdit::ReplaceExact {
                from,
                to,
                expected_matches,
            } => Self::ReplaceExact {
                from,
                to,
                expected_matches,
            },
            RawInstructionOverlayEdit::RemoveExact {
                text,
                expected_matches,
            } => Self::RemoveExact {
                text,
                expected_matches,
            },
        })
    }
}

impl fmt::Debug for InstructionOverlayEdit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let operation = match self {
            Self::Prepend { .. } => "prepend",
            Self::Append { .. } => "append",
            Self::Replace { .. } => "replace",
            Self::ReplaceExact { .. } => "replace_exact",
            Self::RemoveExact { .. } => "remove_exact",
        };
        f.debug_struct("InstructionOverlayEdit")
            .field("op", &operation)
            .field("text", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    InvalidModelSelectors,
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
            Self::InvalidModelSelectors => {
                f.write_str("instruction overlay models must contain nonempty exact model slugs")
            }
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
            if plan
                .models
                .as_ref()
                .is_some_and(|models| models.is_empty() || models.iter().any(String::is_empty))
            {
                return Err(InstructionOverlayError::InvalidModelSelectors);
            }
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
            let mut output = String::with_capacity(body.len().saturating_add(text.len()));
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

    #[test]
    fn parses_targeted_edits_applies_and_redacts_debug_text() {
        let plan: InstructionOverlaysToml = toml::from_str(
            "[developer_guidance]\nedits = [{ op = \"remove_exact\", text = \"sensitive prompt text\", expected_matches = 1 }]\n",
        )
        .expect("parse overlays");
        assert_eq!(
            plan.apply_component(
                InstructionOverlayComponent::DeveloperGuidance,
                Some("prefix sensitive prompt text suffix"),
            )
            .expect("apply edit")
            .as_deref(),
            Some("prefix  suffix")
        );
        assert!(!format!("{plan:?}").contains("sensitive prompt text"));
    }

    #[test]
    fn model_applicability_defaults_to_all_and_matches_exact_slugs() {
        let unscoped = InstructionOverlayComponentToml::default();
        assert!(unscoped.applies_to_model("gpt-6.1-sol"));

        let plan: InstructionOverlaysToml = toml::from_str(
            "[model_baseline]\nmodels = [\"gpt-6.1-sol\", \"gpt-6-luna\"]\nedits = [{ op = \"remove_exact\", text = \"target\", expected_matches = 1 }]\n",
        )
        .expect("parse model-scoped overlay");
        let model_baseline = plan
            .component(InstructionOverlayComponent::ModelBaseline)
            .expect("model baseline overlay");
        assert!(model_baseline.applies_to_model("gpt-6.1-sol"));
        assert!(model_baseline.applies_to_model("gpt-6-luna"));
        assert!(!model_baseline.applies_to_model("gpt-6.1-sol-preview"));
    }

    #[test]
    fn empty_model_applicability_list_is_invalid() {
        let plan = InstructionOverlaysToml {
            model_baseline: Some(InstructionOverlayComponentToml {
                models: Some(Vec::new()),
                edits: vec![InstructionOverlayEdit::RemoveExact {
                    text: "target".to_string(),
                    expected_matches: 1,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            plan.validate_all(),
            Err(InstructionOverlayError::InvalidModelSelectors)
        );
    }

    #[test]
    fn exact_match_mismatch_fails_instead_of_silently_nooping() {
        let plan = InstructionOverlaysToml {
            developer_guidance: Some(InstructionOverlayComponentToml {
                edits: vec![InstructionOverlayEdit::RemoveExact {
                    text: "expected developer instruction".to_string(),
                    expected_matches: 1,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(matches!(
            plan.apply_component(
                InstructionOverlayComponent::DeveloperGuidance,
                Some("changed upstream wording"),
            ),
            Err(InstructionOverlayError::MatchCountMismatch {
                expected: 1,
                actual: 0
            })
        ));
    }

    #[test]
    fn invalid_edit_config_error_does_not_echo_instruction_text() {
        let raw = "[instruction_overlays.developer_guidance]\nedits = [{ op = \"unknown\", text = \"private instruction\" }]\n";
        // Config loading first parses TOML into a value, then performs typed
        // deserialization. This avoids TOML's source-snippet formatter, which
        // would otherwise include a valid but rejected prompt literal.
        let value: toml::Value = toml::from_str(raw).expect("well-formed TOML");
        let error = value
            .try_into::<crate::config_toml::ConfigToml>()
            .expect_err("unknown edit should fail");
        assert!(!error.to_string().contains("private instruction"));
    }
}
