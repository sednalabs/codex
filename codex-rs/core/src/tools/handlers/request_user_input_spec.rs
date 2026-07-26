use codex_protocol::config_types::ModeKind;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputWaitMode;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub const REQUEST_USER_INPUT_TOOL_NAME: &str = "request_user_input";
pub const MIN_AUTO_RESOLUTION_MS: u64 = 60_000;
pub const MAX_AUTO_RESOLUTION_MS: u64 = 240_000;

pub fn create_request_user_input_tool(description: String) -> ToolSpec {
    let option_props = BTreeMap::from([
        (
            "label".to_string(),
            JsonSchema::string(Some("User-facing label (1-5 words).".to_string())),
        ),
        (
            "description".to_string(),
            JsonSchema::string(Some(
                "One short sentence explaining impact/tradeoff if selected.".to_string(),
            )),
        ),
    ]);

    let options_schema = JsonSchema::array(JsonSchema::object(
            option_props,
            Some(vec!["label".to_string(), "description".to_string()]),
            Some(false.into()),
        ), Some(
            "Provide 2-3 mutually exclusive choices. Put the recommended option first and suffix its label with \"(Recommended)\". Do not include an \"Other\" option in this list; the client will add a free-form \"Other\" option automatically."
                .to_string(),
        ));

    let question_props = BTreeMap::from([
        (
            "id".to_string(),
            JsonSchema::string(Some(
                "Stable identifier for mapping answers (snake_case).".to_string(),
            )),
        ),
        (
            "header".to_string(),
            JsonSchema::string(Some(
                "Short header label shown in the UI (12 or fewer chars).".to_string(),
            )),
        ),
        (
            "question".to_string(),
            JsonSchema::string(Some(
                "Single-sentence prompt shown to the user.".to_string(),
            )),
        ),
        ("options".to_string(), options_schema),
    ]);

    let questions_schema = JsonSchema::array(
        JsonSchema::object(
            question_props,
            Some(vec![
                "id".to_string(),
                "header".to_string(),
                "question".to_string(),
                "options".to_string(),
            ]),
            Some(false.into()),
        ),
        Some("Questions to show the user. Prefer 1 and do not exceed 3".to_string()),
    );

    let auto_resolution_ms_schema = JsonSchema::number(Some(format!(
        "Required for advisory waitMode and forbidden for blocking waitMode. Sets the visible auto-resolution countdown in milliseconds, from {MIN_AUTO_RESOLUTION_MS} to {MAX_AUTO_RESOLUTION_MS}. Use {MIN_AUTO_RESOLUTION_MS} for lightly helpful context and up to {MAX_AUTO_RESOLUTION_MS} when the answer would materially unblock better work."
    )));

    let properties = BTreeMap::from([
        ("questions".to_string(), questions_schema),
        (
            "waitMode".to_string(),
            JsonSchema::string_enum(
                vec![json!("blocking"), json!("advisory")],
                Some(
                    "Wait behavior. blocking is the default and waits indefinitely; advisory requires autoResolutionMs and may continue with best judgment when the countdown expires. For compatibility, older payloads that omit waitMode map to advisory only when autoResolutionMs is present."
                        .to_string(),
                ),
            ),
        ),
        ("autoResolutionMs".to_string(), auto_resolution_ms_schema),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: REQUEST_USER_INPUT_TOOL_NAME.to_string(),
        description,
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["questions".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn request_user_input_unavailable_message(
    mode: ModeKind,
    available_modes: &[ModeKind],
) -> Option<String> {
    if available_modes.contains(&mode) {
        None
    } else {
        let mode_name = mode.display_name();
        Some(format!(
            "request_user_input is unavailable in {mode_name} mode"
        ))
    }
}

pub fn normalize_request_user_input_args(
    mut args: RequestUserInputArgs,
) -> Result<RequestUserInputArgs, String> {
    let missing_options = args
        .questions
        .iter()
        .any(|question| question.options.as_ref().is_none_or(Vec::is_empty));
    if missing_options {
        return Err("request_user_input requires non-empty options for every question".to_string());
    }

    for question in &mut args.questions {
        question.is_other = true;
    }

    args.wait_mode = Some(match (args.wait_mode, args.auto_resolution_ms) {
        (None, None) | (Some(RequestUserInputWaitMode::Blocking), None) => {
            RequestUserInputWaitMode::Blocking
        }
        (None, Some(_)) | (Some(RequestUserInputWaitMode::Advisory), Some(_)) => {
            RequestUserInputWaitMode::Advisory
        }
        (Some(RequestUserInputWaitMode::Blocking), Some(_)) => {
            return Err(
                "request_user_input waitMode blocking forbids autoResolutionMs; omit the duration or use advisory"
                    .to_string(),
            );
        }
        (Some(RequestUserInputWaitMode::Advisory), None) => {
            return Err(
                "request_user_input waitMode advisory requires autoResolutionMs".to_string(),
            );
        }
    });

    if let Some(auto_resolution_ms) = args.auto_resolution_ms {
        let clamped_auto_resolution_ms =
            auto_resolution_ms.clamp(MIN_AUTO_RESOLUTION_MS, MAX_AUTO_RESOLUTION_MS);
        if clamped_auto_resolution_ms != auto_resolution_ms {
            tracing::warn!(
                auto_resolution_ms,
                clamped_auto_resolution_ms,
                "clamped request_user_input autoResolutionMs to supported range"
            );
            args.auto_resolution_ms = Some(clamped_auto_resolution_ms);
        }
    }

    Ok(args)
}

pub fn request_user_input_tool_description(available_modes: &[ModeKind]) -> String {
    let allowed_modes = format_allowed_modes(available_modes);
    format!(
        "Request user input for one to three short questions and wait for the response. Use waitMode blocking (the default) without autoResolutionMs when explicit user input is required; it waits indefinitely. Use waitMode advisory with autoResolutionMs from {MIN_AUTO_RESOLUTION_MS} to {MAX_AUTO_RESOLUTION_MS} only when continuing with best judgment is acceptable if the user does not answer. This tool is only available in {allowed_modes}."
    )
}

fn format_allowed_modes(available_modes: &[ModeKind]) -> String {
    let mode_names: Vec<&str> = available_modes
        .iter()
        .map(|mode| mode.display_name())
        .collect();

    match mode_names.as_slice() {
        [] => "no modes".to_string(),
        [mode] => format!("{mode} mode"),
        [first, second] => format!("{first} or {second} mode"),
        [..] => format!("modes: {}", mode_names.join(",")),
    }
}

#[cfg(test)]
#[path = "request_user_input_spec_tests.rs"]
mod tests;
