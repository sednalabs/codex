//! Convenience sender for app events and common outbound TUI commands.
//!
//! This wraps the raw channel so call sites can submit typed `AppCommand`s
//! without duplicating event construction or session logging behavior.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use crate::app_command::AppCommand;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::FileChangeApprovalDecision;
use codex_app_server_protocol::McpServerElicitationAction;
use codex_app_server_protocol::RequestId as AppServerRequestId;
use codex_app_server_protocol::ReviewTarget;
use codex_app_server_protocol::ThreadRealtimeAudioChunk;
use codex_app_server_protocol::ToolRequestUserInputResponse;
use codex_protocol::ThreadId;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use tokio::sync::mpsc::UnboundedSender;

use crate::app_event::AppEvent;
use crate::session_log;

#[derive(Clone, Debug)]
pub(crate) struct AppEventSender {
    pub app_event_tx: UnboundedSender<AppEvent>,
    thread_lifecycle_generation: Arc<AtomicU64>,
}

impl AppEventSender {
    pub(crate) fn new(app_event_tx: UnboundedSender<AppEvent>) -> Self {
        Self {
            app_event_tx,
            thread_lifecycle_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Send an event to the app event channel. If it fails, we swallow the
    /// error and log it.
    pub(crate) fn send(&self, event: AppEvent) {
        // Record inbound events for high-fidelity session replay.
        // Avoid double-logging Ops; those are logged at the point of submission.
        if !matches!(event, AppEvent::CodexOp(_)) {
            session_log::log_inbound_app_event(&event);
        }
        if let Err(e) = self.app_event_tx.send(event) {
            tracing::error!("failed to send event: {e}");
        }
    }

    pub(crate) fn set_thread_lifecycle_generation(&self, generation: u64) {
        self.thread_lifecycle_generation
            .store(generation, Ordering::Release);
    }

    pub(crate) fn thread_lifecycle_generation(&self) -> u64 {
        self.thread_lifecycle_generation.load(Ordering::Acquire)
    }

    /// Returns a sender permanently scoped to one captured thread lifecycle.
    ///
    /// Interactive prompts may be displayed while another thread owns the visible widget. Those
    /// prompts must retain their target lifecycle rather than reading the widget's later global
    /// generation when the user answers them.
    pub(crate) fn for_thread_lifecycle_generation(&self, generation: u64) -> Self {
        Self {
            app_event_tx: self.app_event_tx.clone(),
            thread_lifecycle_generation: Arc::new(AtomicU64::new(generation)),
        }
    }

    pub(crate) fn interrupt(&self) {
        self.send(AppEvent::CodexOp(AppCommand::interrupt()));
    }

    pub(crate) fn compact(&self) {
        self.send(AppEvent::CodexOp(AppCommand::compact()));
    }

    pub(crate) fn set_thread_name(&self, name: String) {
        self.send(AppEvent::CodexOp(AppCommand::set_thread_name(name)));
    }

    pub(crate) fn review(&self, target: ReviewTarget) {
        self.send(AppEvent::CodexOp(AppCommand::review(target)));
    }

    pub(crate) fn list_skills(&self, cwds: Vec<PathBuf>, force_reload: bool) {
        self.send(AppEvent::CodexOp(AppCommand::list_skills(
            cwds,
            force_reload,
        )));
    }

    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub(crate) fn realtime_conversation_audio(&self, audio: ThreadRealtimeAudioChunk) {
        self.send(AppEvent::CodexOp(AppCommand::realtime_conversation_audio(
            audio,
        )));
    }

    pub(crate) fn user_input_answer(&self, id: String, response: ToolRequestUserInputResponse) {
        self.send(AppEvent::CodexOp(AppCommand::user_input_answer(
            id, response,
        )));
    }

    pub(crate) fn exec_approval(
        &self,
        thread_id: ThreadId,
        id: String,
        decision: CommandExecutionApprovalDecision,
    ) {
        self.send(AppEvent::SubmitThreadOp {
            thread_id,
            lifecycle_generation: self.thread_lifecycle_generation(),
            op: AppCommand::exec_approval(id, /*turn_id*/ None, decision),
        });
    }

    pub(crate) fn request_permissions_response(
        &self,
        thread_id: ThreadId,
        id: String,
        response: RequestPermissionsResponse,
    ) {
        self.send(AppEvent::SubmitThreadOp {
            thread_id,
            lifecycle_generation: self.thread_lifecycle_generation(),
            op: AppCommand::request_permissions_response(id, response),
        });
    }

    pub(crate) fn patch_approval(
        &self,
        thread_id: ThreadId,
        id: String,
        decision: FileChangeApprovalDecision,
    ) {
        self.send(AppEvent::SubmitThreadOp {
            thread_id,
            lifecycle_generation: self.thread_lifecycle_generation(),
            op: AppCommand::patch_approval(id, decision),
        });
    }

    pub(crate) fn resolve_elicitation(
        &self,
        thread_id: ThreadId,
        server_name: String,
        request_id: AppServerRequestId,
        decision: McpServerElicitationAction,
        content: Option<serde_json::Value>,
        meta: Option<serde_json::Value>,
    ) {
        self.send(AppEvent::SubmitThreadOp {
            thread_id,
            lifecycle_generation: self.thread_lifecycle_generation(),
            op: AppCommand::resolve_elicitation(server_name, request_id, decision, content, meta),
        });
    }

    pub(crate) fn lookup_message_history_entry(
        &self,
        thread_id: ThreadId,
        offset: usize,
        log_id: u64,
    ) {
        self.send(AppEvent::LookupMessageHistoryEntry {
            thread_id,
            lifecycle_generation: self.thread_lifecycle_generation(),
            offset,
            log_id,
        });
    }

    pub(crate) fn lookup_message_history_batch(
        &self,
        thread_id: ThreadId,
        cursor: crate::app_event::HistoryBatchCursor,
        log_id: u64,
    ) {
        self.send(AppEvent::LookupMessageHistoryBatch {
            thread_id,
            lifecycle_generation: self.thread_lifecycle_generation(),
            cursor,
            log_id,
        });
    }
}
