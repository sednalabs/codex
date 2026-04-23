use schemars::JsonSchema;
use schemars::Schema;
use schemars::SchemaGenerator;
use schemars::generate::SchemaSettings;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use std::borrow::Cow;
use std::path::Path;
use std::path::PathBuf;

const GENERATED_DIR: &str = "generated";
const POST_TOOL_USE_INPUT_FIXTURE: &str = "post-tool-use.command.input.schema.json";
const POST_TOOL_USE_OUTPUT_FIXTURE: &str = "post-tool-use.command.output.schema.json";
const PERMISSION_REQUEST_INPUT_FIXTURE: &str = "permission-request.command.input.schema.json";
const PERMISSION_REQUEST_OUTPUT_FIXTURE: &str = "permission-request.command.output.schema.json";
const PRE_TOOL_USE_INPUT_FIXTURE: &str = "pre-tool-use.command.input.schema.json";
const PRE_TOOL_USE_OUTPUT_FIXTURE: &str = "pre-tool-use.command.output.schema.json";
const SESSION_START_INPUT_FIXTURE: &str = "session-start.command.input.schema.json";
const SESSION_START_OUTPUT_FIXTURE: &str = "session-start.command.output.schema.json";
const USER_PROMPT_SUBMIT_INPUT_FIXTURE: &str = "user-prompt-submit.command.input.schema.json";
const USER_PROMPT_SUBMIT_OUTPUT_FIXTURE: &str = "user-prompt-submit.command.output.schema.json";
const STOP_INPUT_FIXTURE: &str = "stop.command.input.schema.json";
const STOP_OUTPUT_FIXTURE: &str = "stop.command.output.schema.json";

#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub(crate) struct NullableString(Option<String>);

impl NullableString {
    pub(crate) fn from_path(path: Option<PathBuf>) -> Self {
        Self(path.map(|path| path.display().to_string()))
    }

    pub(crate) fn from_string(value: Option<String>) -> Self {
        Self(value)
    }
}

impl JsonSchema for NullableString {
    fn schema_name() -> Cow<'static, str> {
        "NullableString".into()
    }

    fn json_schema(_gen: &mut SchemaGenerator) -> Schema {
        schema_from_json_literal(
            json!({
                "type": ["string", "null"]
            }),
            "nullable string schema",
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub(crate) struct HookUniversalOutputWire {
    #[serde(default = "default_continue")]
    pub r#continue: bool,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub suppress_output: bool,
    #[serde(default)]
    pub system_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub(crate) enum HookEventNameWire {
    #[serde(rename = "PreToolUse")]
    PreToolUse,
    #[serde(rename = "PermissionRequest")]
    PermissionRequest,
    #[serde(rename = "PostToolUse")]
    PostToolUse,
    #[serde(rename = "SessionStart")]
    SessionStart,
    #[serde(rename = "UserPromptSubmit")]
    UserPromptSubmit,
    #[serde(rename = "Stop")]
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
#[schemars(rename = "pre-tool-use.command.output")]
pub(crate) struct PreToolUseCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub decision: Option<PreToolUseDecisionWire>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub hook_specific_output: Option<PreToolUseHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
#[schemars(rename = "post-tool-use.command.output")]
pub(crate) struct PostToolUseCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub decision: Option<BlockDecisionWire>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub hook_specific_output: Option<PostToolUseHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
#[schemars(rename = "permission-request.command.output")]
pub(crate) struct PermissionRequestCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub hook_specific_output: Option<PermissionRequestHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub(crate) struct PermissionRequestHookSpecificOutputWire {
    pub hook_event_name: HookEventNameWire,
    #[serde(default)]
    pub decision: Option<PermissionRequestDecisionWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub(crate) struct PermissionRequestDecisionWire {
    pub behavior: PermissionRequestBehaviorWire,
    /// Reserved for a future input-rewrite capability.
    ///
    /// PermissionRequest hooks currently fail closed if this field is present.
    #[serde(default)]
    pub updated_input: Option<Value>,
    /// Reserved for a future permission-rewrite capability.
    ///
    /// PermissionRequest hooks currently fail closed if this field is present.
    #[serde(default)]
    pub updated_permissions: Option<Value>,
    #[serde(default)]
    pub message: Option<String>,
    /// Reserved for future short-circuiting semantics.
    ///
    /// PermissionRequest hooks currently fail closed if this field is `true`.
    #[serde(default)]
    pub interrupt: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub(crate) enum PermissionRequestBehaviorWire {
    #[serde(rename = "allow")]
    Allow,
    #[serde(rename = "deny")]
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub(crate) struct PostToolUseHookSpecificOutputWire {
    pub hook_event_name: HookEventNameWire,
    #[serde(default)]
    pub additional_context: Option<String>,
    #[serde(default)]
    #[serde(rename = "updatedMCPToolOutput")]
    pub updated_mcp_tool_output: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub(crate) struct PreToolUseHookSpecificOutputWire {
    pub hook_event_name: HookEventNameWire,
    #[serde(default)]
    pub permission_decision: Option<PreToolUsePermissionDecisionWire>,
    #[serde(default)]
    pub permission_decision_reason: Option<String>,
    #[serde(default)]
    pub updated_input: Option<Value>,
    #[serde(default)]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub(crate) enum PreToolUsePermissionDecisionWire {
    #[serde(rename = "allow")]
    Allow,
    #[serde(rename = "deny")]
    Deny,
    #[serde(rename = "ask")]
    Ask,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub(crate) enum PreToolUseDecisionWire {
    #[serde(rename = "approve")]
    Approve,
    #[serde(rename = "block")]
    Block,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "pre-tool-use.command.input")]
pub(crate) struct PreToolUseCommandInput {
    pub session_id: String,
    /// Codex extension: expose the active turn id to internal turn-scoped hooks.
    pub turn_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "pre_tool_use_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_use_id: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "permission-request.command.input")]
pub(crate) struct PermissionRequestCommandInput {
    pub session_id: String,
    /// Codex extension: expose the active turn id to internal turn-scoped hooks.
    pub turn_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "permission_request_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub tool_name: String,
    pub tool_input: Value,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "post-tool-use.command.input")]
pub(crate) struct PostToolUseCommandInput {
    pub session_id: String,
    /// Codex extension: expose the active turn id to internal turn-scoped hooks.
    pub turn_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "post_tool_use_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_response: Value,
    pub tool_use_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
#[schemars(rename = "session-start.command.output")]
pub(crate) struct SessionStartCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub hook_specific_output: Option<SessionStartHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionStartHookSpecificOutputWire {
    pub hook_event_name: HookEventNameWire,
    #[serde(default)]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
#[schemars(rename = "user-prompt-submit.command.output")]
pub(crate) struct UserPromptSubmitCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub decision: Option<BlockDecisionWire>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub hook_specific_output: Option<UserPromptSubmitHookSpecificOutputWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub(crate) struct UserPromptSubmitHookSpecificOutputWire {
    pub hook_event_name: HookEventNameWire,
    #[serde(default)]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
#[schemars(rename = "stop.command.output")]
pub(crate) struct StopCommandOutputWire {
    #[serde(flatten)]
    pub universal: HookUniversalOutputWire,
    #[serde(default)]
    pub decision: Option<BlockDecisionWire>,
    /// Claude requires `reason` when `decision` is `block`; we enforce that
    /// semantic rule during output parsing rather than in the JSON schema.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub(crate) enum BlockDecisionWire {
    #[serde(rename = "block")]
    Block,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "session-start.command.input")]
pub(crate) struct SessionStartCommandInput {
    pub session_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "session_start_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    #[schemars(schema_with = "session_start_source_schema")]
    pub source: String,
}

impl SessionStartCommandInput {
    pub(crate) fn new(
        session_id: impl Into<String>,
        transcript_path: Option<PathBuf>,
        cwd: impl Into<String>,
        model: impl Into<String>,
        permission_mode: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            transcript_path: NullableString::from_path(transcript_path),
            cwd: cwd.into(),
            hook_event_name: "SessionStart".to_string(),
            model: model.into(),
            permission_mode: permission_mode.into(),
            source: source.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "user-prompt-submit.command.input")]
pub(crate) struct UserPromptSubmitCommandInput {
    pub session_id: String,
    /// Codex extension: expose the active turn id to internal turn-scoped hooks.
    pub turn_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "user_prompt_submit_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "stop.command.input")]
pub(crate) struct StopCommandInput {
    pub session_id: String,
    /// Codex extension: expose the active turn id to internal turn-scoped hooks.
    pub turn_id: String,
    pub transcript_path: NullableString,
    pub cwd: String,
    #[schemars(schema_with = "stop_hook_event_name_schema")]
    pub hook_event_name: String,
    pub model: String,
    #[schemars(schema_with = "permission_mode_schema")]
    pub permission_mode: String,
    pub stop_hook_active: bool,
    pub last_assistant_message: NullableString,
}

pub fn write_schema_fixtures(schema_root: &Path) -> anyhow::Result<()> {
    let generated_dir = schema_root.join(GENERATED_DIR);
    ensure_empty_dir(&generated_dir)?;

    write_schema(
        &generated_dir.join(POST_TOOL_USE_INPUT_FIXTURE),
        schema_json::<PostToolUseCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(POST_TOOL_USE_OUTPUT_FIXTURE),
        schema_json::<PostToolUseCommandOutputWire>()?,
    )?;
    write_schema(
        &generated_dir.join(PERMISSION_REQUEST_INPUT_FIXTURE),
        schema_json::<PermissionRequestCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(PERMISSION_REQUEST_OUTPUT_FIXTURE),
        schema_json::<PermissionRequestCommandOutputWire>()?,
    )?;
    write_schema(
        &generated_dir.join(PRE_TOOL_USE_INPUT_FIXTURE),
        schema_json::<PreToolUseCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(PRE_TOOL_USE_OUTPUT_FIXTURE),
        schema_json::<PreToolUseCommandOutputWire>()?,
    )?;
    write_schema(
        &generated_dir.join(SESSION_START_INPUT_FIXTURE),
        schema_json::<SessionStartCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(SESSION_START_OUTPUT_FIXTURE),
        schema_json::<SessionStartCommandOutputWire>()?,
    )?;
    write_schema(
        &generated_dir.join(USER_PROMPT_SUBMIT_INPUT_FIXTURE),
        schema_json::<UserPromptSubmitCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(USER_PROMPT_SUBMIT_OUTPUT_FIXTURE),
        schema_json::<UserPromptSubmitCommandOutputWire>()?,
    )?;
    write_schema(
        &generated_dir.join(STOP_INPUT_FIXTURE),
        schema_json::<StopCommandInput>()?,
    )?;
    write_schema(
        &generated_dir.join(STOP_OUTPUT_FIXTURE),
        schema_json::<StopCommandOutputWire>()?,
    )?;

    Ok(())
}

fn write_schema(path: &Path, json: Vec<u8>) -> anyhow::Result<()> {
    std::fs::write(path, json)?;
    Ok(())
}

fn ensure_empty_dir(dir: &Path) -> anyhow::Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir)?;
    Ok(())
}

fn schema_json<T>() -> anyhow::Result<Vec<u8>>
where
    T: JsonSchema,
{
    let schema = schema_for_type::<T>();
    let mut value = serde_json::to_value(schema)?;
    normalize_legacy_option_schema(&mut value);
    let value = canonicalize_json(&value);
    Ok(serde_json::to_vec_pretty(&value)?)
}

fn schema_for_type<T>() -> Schema
where
    T: JsonSchema,
{
    SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<T>()
}

fn canonicalize_json(value: &Value) -> Value {
    canonicalize_json_with_key(/*key*/ None, value)
}

fn canonicalize_json_with_key(key: Option<&str>, value: &Value) -> Value {
    match value {
        Value::String(text) if key == Some("description") => {
            Value::String(text.split_whitespace().collect::<Vec<_>>().join(" "))
        }
        Value::Array(items) => {
            let mut canonical_items: Vec<_> = items
                .iter()
                .map(|item| canonicalize_json_with_key(/*key*/ None, item))
                .collect();
            if key == Some("required") {
                canonical_items.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
            }
            Value::Array(canonical_items)
        }
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            let mut sorted = Map::with_capacity(map.len());
            for (key, child) in entries {
                sorted.insert(
                    key.clone(),
                    canonicalize_json_with_key(Some(key.as_str()), child),
                );
            }
            Value::Object(sorted)
        }
        _ => value.clone(),
    }
}

fn session_start_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("SessionStart")
}

fn post_tool_use_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("PostToolUse")
}

fn pre_tool_use_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("PreToolUse")
}

fn permission_request_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("PermissionRequest")
}

fn user_prompt_submit_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("UserPromptSubmit")
}

fn stop_hook_event_name_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_const_schema("Stop")
}

fn permission_mode_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_enum_schema(&[
        "default",
        "acceptEdits",
        "plan",
        "dontAsk",
        "bypassPermissions",
    ])
}

fn session_start_source_schema(_gen: &mut SchemaGenerator) -> Schema {
    string_enum_schema(&["startup", "resume", "clear"])
}

fn schema_from_json_literal(value: Value, description: &'static str) -> Schema {
    match value.try_into() {
        Ok(schema) => schema,
        Err(err) => unreachable!("internal {description} was invalid: {err}"),
    }
}

fn string_const_schema(value: &str) -> Schema {
    schema_from_json_literal(
        json!({
            "type": "string",
            "const": value,
        }),
        "string const schema",
    )
}

fn string_enum_schema(values: &[&str]) -> Schema {
    schema_from_json_literal(
        json!({
            "type": "string",
            "enum": values,
        }),
        "string enum schema",
    )
}

fn normalize_legacy_option_schema(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                normalize_legacy_option_schema(item);
            }
        }
        Value::Object(map) => {
            for child in map.values_mut() {
                normalize_legacy_option_schema(child);
            }

            let default_is_null = map.get("default").is_some_and(Value::is_null);
            if !default_is_null {
                return;
            }

            if let Some(Value::Array(types)) = map.get_mut("type") {
                types.retain(|ty| ty != "null");
                if types.len() == 1
                    && let Some(single_type) = types.pop()
                {
                    map.insert("type".to_string(), single_type);
                }
            }

            let Some(any_of) = map.remove("anyOf") else {
                return;
            };
            let Value::Array(variants) = any_of else {
                map.insert("anyOf".to_string(), any_of);
                return;
            };

            let mut non_null_variants = Vec::new();
            let mut removed_null_variant = false;
            for variant in variants {
                if is_null_schema_value(&variant) {
                    removed_null_variant = true;
                } else {
                    non_null_variants.push(variant);
                }
            }

            if removed_null_variant && !non_null_variants.is_empty() {
                map.insert("allOf".to_string(), Value::Array(non_null_variants));
            } else {
                map.insert("anyOf".to_string(), Value::Array(non_null_variants));
            }
        }
        _ => {}
    }
}

fn is_null_schema_value(value: &Value) -> bool {
    match value {
        Value::Object(map) => match map.get("type") {
            Some(Value::String(kind)) => kind == "null",
            Some(Value::Array(types)) => !types.is_empty() && types.iter().all(|ty| ty == "null"),
            _ => map.get("const").is_some_and(Value::is_null),
        },
        _ => false,
    }
}

fn default_continue() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::PERMISSION_REQUEST_INPUT_FIXTURE;
    use super::PERMISSION_REQUEST_OUTPUT_FIXTURE;
    use super::POST_TOOL_USE_INPUT_FIXTURE;
    use super::POST_TOOL_USE_OUTPUT_FIXTURE;
    use super::PRE_TOOL_USE_INPUT_FIXTURE;
    use super::PRE_TOOL_USE_OUTPUT_FIXTURE;
    use super::PermissionRequestCommandInput;
    use super::PostToolUseCommandInput;
    use super::PreToolUseCommandInput;
    use super::SESSION_START_INPUT_FIXTURE;
    use super::SESSION_START_OUTPUT_FIXTURE;
    use super::STOP_INPUT_FIXTURE;
    use super::STOP_OUTPUT_FIXTURE;
    use super::StopCommandInput;
    use super::USER_PROMPT_SUBMIT_INPUT_FIXTURE;
    use super::USER_PROMPT_SUBMIT_OUTPUT_FIXTURE;
    use super::UserPromptSubmitCommandInput;
    use super::schema_json;
    use super::write_schema_fixtures;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use tempfile::TempDir;

    fn expected_fixture(name: &str) -> &'static str {
        match name {
            POST_TOOL_USE_INPUT_FIXTURE => {
                include_str!("../schema/generated/post-tool-use.command.input.schema.json")
            }
            POST_TOOL_USE_OUTPUT_FIXTURE => {
                include_str!("../schema/generated/post-tool-use.command.output.schema.json")
            }
            PERMISSION_REQUEST_INPUT_FIXTURE => {
                include_str!("../schema/generated/permission-request.command.input.schema.json")
            }
            PERMISSION_REQUEST_OUTPUT_FIXTURE => {
                include_str!("../schema/generated/permission-request.command.output.schema.json")
            }
            PRE_TOOL_USE_INPUT_FIXTURE => {
                include_str!("../schema/generated/pre-tool-use.command.input.schema.json")
            }
            PRE_TOOL_USE_OUTPUT_FIXTURE => {
                include_str!("../schema/generated/pre-tool-use.command.output.schema.json")
            }
            SESSION_START_INPUT_FIXTURE => {
                include_str!("../schema/generated/session-start.command.input.schema.json")
            }
            SESSION_START_OUTPUT_FIXTURE => {
                include_str!("../schema/generated/session-start.command.output.schema.json")
            }
            USER_PROMPT_SUBMIT_INPUT_FIXTURE => {
                include_str!("../schema/generated/user-prompt-submit.command.input.schema.json")
            }
            USER_PROMPT_SUBMIT_OUTPUT_FIXTURE => {
                include_str!("../schema/generated/user-prompt-submit.command.output.schema.json")
            }
            STOP_INPUT_FIXTURE => {
                include_str!("../schema/generated/stop.command.input.schema.json")
            }
            STOP_OUTPUT_FIXTURE => {
                include_str!("../schema/generated/stop.command.output.schema.json")
            }
            _ => panic!("unexpected fixture name: {name}"),
        }
    }

    fn normalize_newlines(value: &str) -> String {
        value.replace("\r\n", "\n")
    }

    #[test]
    fn generated_hook_schemas_match_fixtures() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let schema_root = temp_dir.path().join("schema");
        write_schema_fixtures(&schema_root).expect("write generated hook schemas");

        for fixture in [
            POST_TOOL_USE_INPUT_FIXTURE,
            POST_TOOL_USE_OUTPUT_FIXTURE,
            PERMISSION_REQUEST_INPUT_FIXTURE,
            PERMISSION_REQUEST_OUTPUT_FIXTURE,
            PRE_TOOL_USE_INPUT_FIXTURE,
            PRE_TOOL_USE_OUTPUT_FIXTURE,
            SESSION_START_INPUT_FIXTURE,
            SESSION_START_OUTPUT_FIXTURE,
            USER_PROMPT_SUBMIT_INPUT_FIXTURE,
            USER_PROMPT_SUBMIT_OUTPUT_FIXTURE,
            STOP_INPUT_FIXTURE,
            STOP_OUTPUT_FIXTURE,
        ] {
            let expected = normalize_newlines(expected_fixture(fixture));
            let actual = std::fs::read_to_string(schema_root.join("generated").join(fixture))
                .unwrap_or_else(|err| panic!("read generated schema {fixture}: {err}"));
            let actual = normalize_newlines(&actual);
            assert_eq!(expected, actual, "fixture should match generated schema");
        }
    }

    #[test]
    fn turn_scoped_hook_inputs_include_codex_turn_id_extension() {
        // Codex intentionally diverges from Claude's public hook docs here so
        // internal hook consumers can key off the active turn.
        let pre_tool_use: Value = serde_json::from_slice(
            &schema_json::<PreToolUseCommandInput>().expect("serialize pre tool use input schema"),
        )
        .expect("parse pre tool use input schema");
        let post_tool_use: Value = serde_json::from_slice(
            &schema_json::<PostToolUseCommandInput>()
                .expect("serialize post tool use input schema"),
        )
        .expect("parse post tool use input schema");
        let permission_request: Value = serde_json::from_slice(
            &schema_json::<PermissionRequestCommandInput>()
                .expect("serialize permission request input schema"),
        )
        .expect("parse permission request input schema");
        let user_prompt_submit: Value = serde_json::from_slice(
            &schema_json::<UserPromptSubmitCommandInput>()
                .expect("serialize user prompt submit input schema"),
        )
        .expect("parse user prompt submit input schema");
        let stop: Value = serde_json::from_slice(
            &schema_json::<StopCommandInput>().expect("serialize stop input schema"),
        )
        .expect("parse stop input schema");

        for schema in [
            &pre_tool_use,
            &permission_request,
            &post_tool_use,
            &user_prompt_submit,
            &stop,
        ] {
            assert_eq!(schema["properties"]["turn_id"]["type"], "string");
            assert!(
                schema["required"]
                    .as_array()
                    .expect("schema required fields")
                    .contains(&Value::String("turn_id".to_string()))
            );
        }
    }
}
