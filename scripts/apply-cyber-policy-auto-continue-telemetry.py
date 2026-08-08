#!/usr/bin/env python3
from __future__ import annotations

import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def read(path: str) -> str:
    return (ROOT / path).read_text()


def write(path: str, text: str) -> None:
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text)


def replace_once(path: str, old: str, new: str) -> None:
    text = read(path)
    count = text.count(old)
    if count == 0 and new in text:
        return
    if count != 1:
        raise RuntimeError(f"{path}: expected one replacement target, found {count}")
    write(path, text.replace(old, new, 1))


def git_blob(path: str) -> str:
    return subprocess.check_output(
        ["git", "hash-object", path], cwd=ROOT, text=True
    ).strip()


# AppCommand carries the ordinary app-server client message id without changing model input.
replace_once(
    "codex-rs/tui/src/app_command.rs",
    """    UserTurn {\n        items: Vec<UserInput>,\n        cwd: PathBuf,\n""",
    """    UserTurn {\n        items: Vec<UserInput>,\n        #[serde(skip_serializing_if = \"Option::is_none\")]\n        client_user_message_id: Option<String>,\n        cwd: PathBuf,\n""",
)
replace_once(
    "codex-rs/tui/src/app_command.rs",
    """    pub(crate) fn user_turn(\n        items: Vec<UserInput>,\n        cwd: PathBuf,\n""",
    """    pub(crate) fn user_turn(\n        items: Vec<UserInput>,\n        client_user_message_id: Option<String>,\n        cwd: PathBuf,\n""",
)
replace_once(
    "codex-rs/tui/src/app_command.rs",
    """        Self::UserTurn {\n            items,\n            cwd,\n""",
    """        Self::UserTurn {\n            items,\n            client_user_message_id,\n            cwd,\n""",
)

# ChatWidget keeps ordinary submissions unchanged and exposes one explicit generated-turn path.
replace_once(
    "codex-rs/tui/src/chatwidget/input_submission.rs",
    """        self.submit_user_message_with_history_and_shell_escape_policy(\n            user_message,\n            history_record,\n            ShellEscapePolicy::Allow,\n        )\n        .0\n    }\n\n    pub(super) fn submit_user_message_with_shell_escape_policy(\n""",
    """        self.submit_user_message_with_history_and_shell_escape_policy(\n            user_message,\n            history_record,\n            ShellEscapePolicy::Allow,\n            /*client_user_message_id*/ None,\n        )\n        .0\n    }\n\n    pub(super) fn submit_user_message_with_history_record_and_client_id(\n        &mut self,\n        user_message: UserMessage,\n        history_record: UserMessageHistoryRecord,\n        client_user_message_id: String,\n    ) -> bool {\n        self.submit_user_message_with_history_and_shell_escape_policy(\n            user_message,\n            history_record,\n            ShellEscapePolicy::Allow,\n            Some(client_user_message_id),\n        )\n        .0\n    }\n\n    pub(super) fn submit_user_message_with_shell_escape_policy(\n""",
)
replace_once(
    "codex-rs/tui/src/chatwidget/input_submission.rs",
    """        self.submit_user_message_with_history_and_shell_escape_policy(\n            user_message,\n            UserMessageHistoryRecord::UserMessageText,\n            shell_escape_policy,\n        )\n        .1\n    }\n\n    fn submit_user_message_with_history_and_shell_escape_policy(\n        &mut self,\n        user_message: UserMessage,\n        history_record: UserMessageHistoryRecord,\n        shell_escape_policy: ShellEscapePolicy,\n    ) -> (bool, Option<AppCommand>) {\n        if !self.is_session_configured() {\n            tracing::warn!(\"cannot submit user message before session is configured; queueing\");\n""",
    """        self.submit_user_message_with_history_and_shell_escape_policy(\n            user_message,\n            UserMessageHistoryRecord::UserMessageText,\n            shell_escape_policy,\n            /*client_user_message_id*/ None,\n        )\n        .1\n    }\n\n    fn submit_user_message_with_history_and_shell_escape_policy(\n        &mut self,\n        user_message: UserMessage,\n        history_record: UserMessageHistoryRecord,\n        shell_escape_policy: ShellEscapePolicy,\n        client_user_message_id: Option<String>,\n    ) -> (bool, Option<AppCommand>) {\n        if !self.is_session_configured() {\n            if client_user_message_id.is_some() {\n                tracing::warn!(\n                    \"cannot submit generated user message with provenance before session is configured\"\n                );\n                return (false, None);\n            }\n            tracing::warn!(\"cannot submit user message before session is configured; queueing\");\n""",
)
replace_once(
    "codex-rs/tui/src/chatwidget/input_submission.rs",
    """        let op = AppCommand::user_turn(\n            items,\n            self.config.cwd.to_path_buf(),\n""",
    """        let op = AppCommand::user_turn(\n            items,\n            client_user_message_id,\n            self.config.cwd.to_path_buf(),\n""",
)

# Generate exact provenance before finalize_turn clears turn-scoped lifecycle state.
replace_once(
    "codex-rs/tui/src/chatwidget/turn_runtime.rs",
    "use super::*;\n",
    "use super::*;\nuse codex_protocol::automatic_turn::AutomaticTurnProvenance;\n",
)
old_cyber = """    pub(super) fn on_cyber_policy_error(&mut self, from_replay: bool) {\n        // Policy enforcement remains server-side. These opt-in retries preserve the original\n        // thread and context and use the normal submission path; do not rewrite or drop context,\n        // switch models, or otherwise route around the policy decision here.\n        let should_auto_continue = !from_replay\n            && !self.blocks_direct_input\n            && self\n                .config\n                .notices\n                .auto_continue_on_cyber_policy\n                .unwrap_or(false)\n            && self.cyber_policy_auto_continue_attempts < CYBER_POLICY_AUTO_CONTINUE_MAX_ATTEMPTS;\n        if should_auto_continue {\n            self.cyber_policy_auto_continue_attempts += 1;\n        }\n        self.input_queue.submit_pending_steers_after_interrupt = false;\n        self.finalize_turn();\n        self.add_to_history(history_cell::new_cyber_policy_error_event());\n        self.request_redraw();\n\n        // Keep the generated follow-up visible in the transcript without adding it to the user's\n        // cross-session composer history.\n        if should_auto_continue\n            && self.submit_user_message_with_history_record(\n                UserMessage::from(CYBER_POLICY_AUTO_CONTINUE_PROMPT),\n                UserMessageHistoryRecord::Override(UserMessageHistoryOverride {\n                    text: String::new(),\n                    text_elements: Vec::new(),\n                }),\n            )\n        {\n            return;\n        }\n\n        // The bounded retry chain is over. A later user-initiated turn receives a fresh allowance.\n        self.cyber_policy_auto_continue_attempts = 0;\n        // After an error ends the turn, try sending the next queued input.\n        self.maybe_send_next_queued_input();\n    }\n"""
new_cyber = """    pub(super) fn on_cyber_policy_error(&mut self, from_replay: bool) {\n        // Policy enforcement remains server-side. These opt-in retries preserve the original\n        // thread and context and use the normal submission path; do not rewrite or drop context,\n        // switch models, or otherwise route around the policy decision here.\n        let should_auto_continue = !from_replay\n            && !self.blocks_direct_input\n            && self\n                .config\n                .notices\n                .auto_continue_on_cyber_policy\n                .unwrap_or(false)\n            && self.cyber_policy_auto_continue_attempts < CYBER_POLICY_AUTO_CONTINUE_MAX_ATTEMPTS;\n        let client_user_message_id = if should_auto_continue {\n            self.cyber_policy_auto_continue_attempts += 1;\n            match (self.thread_id(), self.turn_lifecycle.last_turn_id.as_deref()) {\n                (Some(thread_id), Some(trigger_turn_id)) => {\n                    AutomaticTurnProvenance::cyber_policy_auto_continue(\n                        thread_id,\n                        trigger_turn_id,\n                        self.cyber_policy_auto_continue_attempts,\n                        CYBER_POLICY_AUTO_CONTINUE_MAX_ATTEMPTS,\n                    )\n                    .and_then(|provenance| provenance.to_client_user_message_id())\n                }\n                _ => None,\n            }\n        } else {\n            None\n        };\n        if should_auto_continue && client_user_message_id.is_none() {\n            tracing::warn!(\n                \"cyber-policy auto-continue provenance unavailable; preserving bounded retry behavior\"\n            );\n        }\n        self.input_queue.submit_pending_steers_after_interrupt = false;\n        self.finalize_turn();\n        self.add_to_history(history_cell::new_cyber_policy_error_event());\n        self.request_redraw();\n\n        // Keep the generated follow-up visible in the transcript without adding it to the user's\n        // cross-session composer history. The client id carries model-opaque provenance into the\n        // canonical rollout and usage ledger; if it is unavailable, preserve the existing retry.\n        if should_auto_continue {\n            let history_record = UserMessageHistoryRecord::Override(UserMessageHistoryOverride {\n                text: String::new(),\n                text_elements: Vec::new(),\n            });\n            let submitted = match client_user_message_id {\n                Some(client_user_message_id) => self\n                    .submit_user_message_with_history_record_and_client_id(\n                        UserMessage::from(CYBER_POLICY_AUTO_CONTINUE_PROMPT),\n                        history_record,\n                        client_user_message_id,\n                    ),\n                None => self.submit_user_message_with_history_record(\n                    UserMessage::from(CYBER_POLICY_AUTO_CONTINUE_PROMPT),\n                    history_record,\n                ),\n            };\n            if submitted {\n                return;\n            }\n        }\n\n        // The bounded retry chain is over. A later user-initiated turn receives a fresh allowance.\n        self.cyber_policy_auto_continue_attempts = 0;\n        // After an error ends the turn, try sending the next queued input.\n        self.maybe_send_next_queued_input();\n    }\n"""
replace_once("codex-rs/tui/src/chatwidget/turn_runtime.rs", old_cyber, new_cyber)

# Existing app-server request methods are left untouched. A narrow descendant module mirrors only
# turn/start and turn/steer when a generated client id is present.
replace_once(
    "codex-rs/tui/src/app_server_session/fs.rs",
    "use super::AppServerSession;\n",
    "mod automatic_turn;\n\nuse super::AppServerSession;\n",
)
write(
    "codex-rs/tui/src/app_server_session/fs/automatic_turn.rs",
    '''use super::super::AppServerSession;\nuse super::super::TurnPermissionsOverride;\nuse super::super::turn_permissions_overrides;\nuse codex_app_server_client::TypedRequestError;\nuse codex_app_server_protocol::AskForApproval;\nuse codex_app_server_protocol::ClientRequest;\nuse codex_app_server_protocol::TurnStartParams;\nuse codex_app_server_protocol::TurnStartResponse;\nuse codex_app_server_protocol::TurnSteerParams;\nuse codex_app_server_protocol::TurnSteerResponse;\nuse codex_app_server_protocol::UserInput;\nuse codex_config::types::ApprovalsReviewer;\nuse codex_protocol::config_types::CollaborationMode;\nuse codex_protocol::config_types::Personality;\nuse codex_protocol::config_types::ReasoningSummary;\nuse codex_protocol::openai_models::ReasoningEffort;\nuse codex_utils_absolute_path::AbsolutePathBuf;\nuse color_eyre::eyre::Result;\nuse color_eyre::eyre::WrapErr;\nuse std::path::PathBuf;\n\nimpl AppServerSession {\n    #[allow(clippy::too_many_arguments)]\n    pub(crate) async fn turn_start_with_client_user_message_id(\n        &mut self,\n        thread_id: codex_protocol::ThreadId,\n        items: Vec<UserInput>,\n        client_user_message_id: Option<String>,\n        cwd: PathBuf,\n        approval_policy: AskForApproval,\n        approvals_reviewer: ApprovalsReviewer,\n        permissions_override: TurnPermissionsOverride,\n        workspace_roots: &[AbsolutePathBuf],\n        model: String,\n        effort: Option<ReasoningEffort>,\n        summary: Option<ReasoningSummary>,\n        service_tier: Option<Option<String>>,\n        collaboration_mode: Option<CollaborationMode>,\n        personality: Option<Personality>,\n        output_schema: Option<serde_json::Value>,\n    ) -> Result<TurnStartResponse> {\n        let request_id = self.next_request_id();\n        let (sandbox_policy, permissions) =\n            turn_permissions_overrides(permissions_override, cwd.as_path());\n        self.client\n            .request_typed(ClientRequest::TurnStart {\n                request_id,\n                params: TurnStartParams {\n                    thread_id: thread_id.to_string(),\n                    client_user_message_id,\n                    input: items,\n                    responsesapi_client_metadata: None,\n                    additional_context: None,\n                    environments: None,\n                    cwd: Some(cwd),\n                    runtime_workspace_roots: Some(workspace_roots.to_vec()),\n                    approval_policy: Some(approval_policy),\n                    approvals_reviewer: Some(approvals_reviewer.into()),\n                    sandbox_policy,\n                    permissions,\n                    model: Some(model),\n                    service_tier,\n                    effort,\n                    summary,\n                    personality,\n                    output_schema,\n                    collaboration_mode,\n                    multi_agent_mode: None,\n                },\n            })\n            .await\n            .wrap_err("turn/start failed in TUI")\n    }\n\n    pub(crate) async fn turn_steer_with_client_user_message_id(\n        &mut self,\n        thread_id: codex_protocol::ThreadId,\n        turn_id: String,\n        items: Vec<UserInput>,\n        client_user_message_id: Option<String>,\n    ) -> std::result::Result<TurnSteerResponse, TypedRequestError> {\n        let request_id = self.next_request_id();\n        self.client\n            .request_typed(ClientRequest::TurnSteer {\n                request_id,\n                params: TurnSteerParams {\n                    thread_id: thread_id.to_string(),\n                    client_user_message_id,\n                    input: items,\n                    responsesapi_client_metadata: None,\n                    additional_context: None,\n                    expected_turn_id: turn_id,\n                },\n            })\n            .await\n    }\n}\n''',
)

# Forward the client id through the same start/steer routing without altering other turn semantics.
replace_once(
    "codex-rs/tui/src/app/thread_routing.rs",
    """            AppCommand::UserTurn {\n                items,\n                cwd,\n""",
    """            AppCommand::UserTurn {\n                items,\n                client_user_message_id,\n                cwd,\n""",
)
replace_once(
    "codex-rs/tui/src/app/thread_routing.rs",
    """                        match app_server\n                            .turn_steer(thread_id, steer_turn_id.clone(), items.to_vec())\n                            .await\n""",
    """                        match app_server\n                            .turn_steer_with_client_user_message_id(\n                                thread_id,\n                                steer_turn_id.clone(),\n                                items.to_vec(),\n                                client_user_message_id.clone(),\n                            )\n                            .await\n""",
)
replace_once(
    "codex-rs/tui/src/app/thread_routing.rs",
    """                    let response = app_server\n                        .turn_start(\n                            thread_id,\n                            items.to_vec(),\n                            cwd.clone(),\n""",
    """                    let response = app_server\n                        .turn_start_with_client_user_message_id(\n                            thread_id,\n                            items.to_vec(),\n                            client_user_message_id.clone(),\n                            cwd.clone(),\n""",
)

# Focused TUI regression coverage lives in the already-declared app_server test module.
write(
    "codex-rs/tui/src/chatwidget/tests/app_server.rs",
    '''use super::*;\n\n#[tokio::test]\nasync fn cyber_policy_auto_continue_carries_exact_model_opaque_provenance() {\n    let (mut chat, _rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;\n    chat.config.notices.auto_continue_on_cyber_policy = Some(true);\n    let thread_id = ThreadId::new();\n    chat.thread_id = Some(thread_id);\n\n    handle_turn_started(&mut chat, "turn-trigger");\n    handle_error(\n        &mut chat,\n        "server fallback message",\n        Some(CodexErrorInfo::CyberPolicy),\n    );\n\n    let Op::UserTurn {\n        items,\n        client_user_message_id,\n        ..\n    } = next_submit_op(&mut op_rx)\n    else {\n        panic!("expected automatic continue user turn");\n    };\n    assert_eq!(\n        items,\n        vec![UserInput::Text {\n            text: "continue".to_string(),\n            text_elements: Vec::new(),\n        }]\n    );\n    let client_id = client_user_message_id.expect("automatic continue should carry provenance");\n    let provenance =\n        codex_protocol::automatic_turn::AutomaticTurnProvenance::from_client_user_message_id(\n            &client_id,\n        )\n        .expect("automatic continue provenance should decode");\n    assert_eq!(provenance.thread_id, thread_id.to_string());\n    assert_eq!(provenance.trigger_turn_id, "turn-trigger");\n    assert_eq!(provenance.attempt, 1);\n    assert_eq!(provenance.max_attempts, 3);\n}\n\n#[tokio::test]\nasync fn ordinary_continue_has_no_automatic_turn_provenance() {\n    let (mut chat, _rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;\n    chat.thread_id = Some(ThreadId::new());\n    chat.submit_user_message(UserMessage::from("continue"));\n\n    let Op::UserTurn {\n        client_user_message_id,\n        ..\n    } = next_submit_op(&mut op_rx)\n    else {\n        panic!("expected ordinary user turn");\n    };\n    assert!(client_user_message_id.is_none());\n}\n''',
)

# Fix the state projection's borrowed error classification.
replace_once(
    "codex-rs/state/src/runtime/extension_storage/automatic_turns.rs",
    "matches!(error.codex_error_info, Some(CodexErrorInfo::CyberPolicy))",
    "matches!(error.codex_error_info.as_ref(), Some(CodexErrorInfo::CyberPolicy))",
)

expected = {
    "codex-rs/tui/src/app_command.rs": "252e3fc2bc74cc0b8340514af5d66fa4677b5f8a",
    "codex-rs/tui/src/chatwidget/input_submission.rs": "969a4febd1e016bbf0edb57c3afb5f2bc102fdd0",
    "codex-rs/tui/src/chatwidget/turn_runtime.rs": "28b53f233cfe17c28e6927c9102811e330b33cc4",
    "codex-rs/tui/src/app/thread_routing.rs": "b517f9a46a09b3237cead57aba50b0941ec236b4",
    "codex-rs/tui/src/app_server_session/fs.rs": "e1795e7555e29ba53526ea76e3fc8644369bda11",
    "codex-rs/tui/src/app_server_session/fs/automatic_turn.rs": "70c9c712b30ab9ac20c1f4bee490c2a6a148ee3a",
    "codex-rs/tui/src/chatwidget/tests/app_server.rs": "37bbde7de33aa473c99e6449fc455d40106c6559",
    "codex-rs/state/src/runtime/extension_storage/automatic_turns.rs": "cdfede48e19d9807f76ada4561598878cbc47d54",
}

bad = []
for path, wanted in expected.items():
    actual = git_blob(path)
    if actual != wanted:
        bad.append(f"{path}: expected {wanted}, got {actual}")
if bad:
    raise RuntimeError("post-patch blob verification failed:\n" + "\n".join(bad))

print("cyber-policy auto-continue telemetry patch verified")
