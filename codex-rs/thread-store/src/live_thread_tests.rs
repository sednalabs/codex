#![allow(clippy::expect_used)]

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_protocol::ThreadId;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_utils_absolute_path::test_support::PathExt;
use tempfile::TempDir;
use tokio::sync::Notify;

use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::CreateThreadParams;
use crate::DeleteThreadParams;
use crate::ListThreadsParams;
use crate::LiveThread;
use crate::LoadThreadHistoryParams;
use crate::LocalThreadStore;
use crate::LocalThreadStoreConfig;
use crate::ReadThreadByRolloutPathParams;
use crate::ReadThreadParams;
use crate::ResumeThreadParams;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::ThreadPage;
use crate::ThreadPersistenceMetadata;
use crate::ThreadStore;
use crate::ThreadStoreFuture;
use crate::UpdateThreadMetadataParams;

const FIRST_MODEL: &str = "first-model";
const SECOND_MODEL: &str = "second-model";

struct GatedThreadStore {
    inner: Arc<LocalThreadStore>,
    gated_append_index: usize,
    append_count: AtomicUsize,
    metadata_update_count: AtomicUsize,
    gated_append_persisted: Notify,
    next_append_persisted: Notify,
    release_gated_append: Notify,
    persist_completed: Notify,
    second_metadata_applied: Notify,
}

impl GatedThreadStore {
    fn new(inner: Arc<LocalThreadStore>, gated_append_index: usize) -> Self {
        Self {
            inner,
            gated_append_index,
            append_count: AtomicUsize::new(0),
            metadata_update_count: AtomicUsize::new(0),
            gated_append_persisted: Notify::new(),
            next_append_persisted: Notify::new(),
            release_gated_append: Notify::new(),
            persist_completed: Notify::new(),
            second_metadata_applied: Notify::new(),
        }
    }
}

impl ThreadStore for GatedThreadStore {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreFuture<'_, ()> {
        ThreadStore::create_thread(self.inner.as_ref(), params)
    }

    fn resume_thread(&self, params: ResumeThreadParams) -> ThreadStoreFuture<'_, ()> {
        ThreadStore::resume_thread(self.inner.as_ref(), params)
    }

    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let append_index = self.append_count.fetch_add(1, Ordering::SeqCst);
            ThreadStore::append_items(self.inner.as_ref(), params).await?;
            if append_index == self.gated_append_index {
                self.gated_append_persisted.notify_one();
                self.release_gated_append.notified().await;
            } else if append_index == self.gated_append_index + 1 {
                self.next_append_persisted.notify_one();
            }
            Ok(())
        })
    }

    fn persist_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            ThreadStore::persist_thread(self.inner.as_ref(), thread_id).await?;
            self.persist_completed.notify_one();
            Ok(())
        })
    }

    fn flush_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        ThreadStore::flush_thread(self.inner.as_ref(), thread_id)
    }

    fn shutdown_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        ThreadStore::shutdown_thread(self.inner.as_ref(), thread_id)
    }

    fn discard_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        ThreadStore::discard_thread(self.inner.as_ref(), thread_id)
    }

    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory> {
        ThreadStore::load_history(self.inner.as_ref(), params)
    }

    fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        ThreadStore::read_thread(self.inner.as_ref(), params)
    }

    fn read_thread_by_rollout_path(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        ThreadStore::read_thread_by_rollout_path(self.inner.as_ref(), params)
    }

    fn list_threads(&self, params: ListThreadsParams) -> ThreadStoreFuture<'_, ThreadPage> {
        ThreadStore::list_threads(self.inner.as_ref(), params)
    }

    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move {
            let applies_second_settings = params.patch.model.as_deref() == Some(SECOND_MODEL);
            let thread = ThreadStore::update_thread_metadata(self.inner.as_ref(), params).await?;
            self.metadata_update_count.fetch_add(1, Ordering::SeqCst);
            if applies_second_settings {
                self.second_metadata_applied.notify_one();
            }
            Ok(thread)
        })
    }

    fn archive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, ()> {
        ThreadStore::archive_thread(self.inner.as_ref(), params)
    }

    fn unarchive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        ThreadStore::unarchive_thread(self.inner.as_ref(), params)
    }

    fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreFuture<'_, ()> {
        ThreadStore::delete_thread(self.inner.as_ref(), params)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_appends_keep_sqlite_metadata_in_canonical_history_order() {
    let home = TempDir::new().expect("temp dir");
    let config = LocalThreadStoreConfig {
        codex_home: home.path().to_path_buf(),
        sqlite: codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        default_model_provider_id: "test-provider".to_string(),
    };
    let runtime = codex_state::StateRuntime::init(
        config.sqlite.home().to_path_buf(),
        config.default_model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    let local_store = Arc::new(LocalThreadStore::new(config, Some(runtime.clone())));
    let gated_store = Arc::new(GatedThreadStore::new(
        local_store,
        /*gated_append_index*/ 0,
    ));
    let thread_id = ThreadId::new();
    let live_thread = LiveThread::create(
        gated_store.clone(),
        create_thread_params(thread_id, home.path()),
    )
    .await
    .expect("create live thread");

    let first_live_thread = live_thread.clone();
    let first_cwd = home.path().to_path_buf();
    let first_append = tokio::spawn(async move {
        first_live_thread
            .append_items(&[thread_settings_item(
                FIRST_MODEL,
                ReasoningEffort::High,
                "first-provider",
                first_cwd.as_path(),
            )])
            .await
    });
    tokio::time::timeout(
        Duration::from_secs(5),
        gated_store.gated_append_persisted.notified(),
    )
    .await
    .expect("first append should reach the persistence gate");

    let second_live_thread = live_thread.clone();
    let second_cwd = std::env::current_dir().expect("current directory");
    let expected_second_cwd = second_cwd.clone();
    let second_append = tokio::spawn(async move {
        second_live_thread
            .append_items(&[thread_settings_item(
                SECOND_MODEL,
                ReasoningEffort::Ultra,
                "second-provider",
                second_cwd.as_path(),
            )])
            .await
    });

    let second_overtook_first = tokio::time::timeout(
        Duration::from_secs(1),
        gated_store.next_append_persisted.notified(),
    )
    .await
    .is_ok();
    if second_overtook_first {
        tokio::time::timeout(
            Duration::from_secs(5),
            gated_store.second_metadata_applied.notified(),
        )
        .await
        .expect("overtaking append should apply metadata");
    }
    gated_store.release_gated_append.notify_one();

    first_append
        .await
        .expect("first append task")
        .expect("first append");
    second_append
        .await
        .expect("second append task")
        .expect("second append");
    live_thread.flush().await.expect("flush live thread");

    let history = live_thread
        .load_history(/*include_archived*/ true)
        .await
        .expect("load canonical history");
    let persisted_settings = history
        .items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event)) => Some((
                event.thread_settings.model.clone(),
                event.thread_settings.model_provider_id.clone(),
                event.thread_settings.reasoning_effort.clone(),
                event.thread_settings.cwd.as_path().to_path_buf(),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    let metadata = runtime
        .get_thread(thread_id)
        .await
        .expect("sqlite metadata read")
        .expect("sqlite metadata");
    let metadata_cwd = metadata
        .cwd
        .canonicalize()
        .expect("canonicalize sqlite metadata cwd");
    let expected_metadata_cwd = expected_second_cwd
        .canonicalize()
        .expect("canonicalize expected sqlite metadata cwd");

    assert_eq!(
        (
            persisted_settings,
            metadata.model,
            metadata.reasoning_effort,
            metadata.model_provider,
            metadata_cwd,
        ),
        (
            vec![
                (
                    FIRST_MODEL.to_string(),
                    "first-provider".to_string(),
                    Some(ReasoningEffort::High),
                    home.path().to_path_buf(),
                ),
                (
                    SECOND_MODEL.to_string(),
                    "second-provider".to_string(),
                    Some(ReasoningEffort::Ultra),
                    expected_second_cwd.clone(),
                ),
            ],
            Some(SECOND_MODEL.to_string()),
            Some(ReasoningEffort::Ultra),
            "second-provider".to_string(),
            expected_metadata_cwd,
        )
    );
    assert!(
        !second_overtook_first,
        "later append reached persistence before the first append transaction completed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persist_waits_for_append_observation_before_flushing_pending_metadata() {
    let home = TempDir::new().expect("temp dir");
    let config = LocalThreadStoreConfig {
        codex_home: home.path().to_path_buf(),
        sqlite: codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        default_model_provider_id: "test-provider".to_string(),
    };
    let runtime = codex_state::StateRuntime::init(
        config.sqlite.home().to_path_buf(),
        config.default_model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    let local_store = Arc::new(LocalThreadStore::new(config, Some(runtime)));
    let gated_store = Arc::new(GatedThreadStore::new(
        local_store,
        /*gated_append_index*/ 1,
    ));
    let thread_id = ThreadId::new();
    let live_thread = LiveThread::create(
        gated_store.clone(),
        create_thread_params(thread_id, home.path()),
    )
    .await
    .expect("create live thread");

    live_thread
        .append_items(&[compacted_item()])
        .await
        .expect("append initial metadata touch");

    let append_live_thread = live_thread.clone();
    let append =
        tokio::spawn(async move { append_live_thread.append_items(&[compacted_item()]).await });
    tokio::time::timeout(
        Duration::from_secs(5),
        gated_store.gated_append_persisted.notified(),
    )
    .await
    .expect("second append should reach the persistence gate");

    let persist_live_thread = live_thread.clone();
    let persist = tokio::spawn(async move { persist_live_thread.persist().await });
    let persist_overtook_append = tokio::time::timeout(
        Duration::from_secs(1),
        gated_store.persist_completed.notified(),
    )
    .await
    .is_ok();
    gated_store.release_gated_append.notify_one();

    append
        .await
        .expect("append task")
        .expect("append coalesced metadata touch");
    persist
        .await
        .expect("persist task")
        .expect("persist thread");

    assert_eq!(gated_store.metadata_update_count.load(Ordering::SeqCst), 2);
    assert!(
        !persist_overtook_append,
        "persist reached the store before append observation completed"
    );
}

fn create_thread_params(thread_id: ThreadId, cwd: &std::path::Path) -> CreateThreadParams {
    CreateThreadParams {
        session_id: thread_id.into(),
        thread_id,
        extra_config: None,
        forked_from_id: None,
        parent_thread_id: None,
        source: SessionSource::Exec,
        thread_source: None,
        originator: "test_originator".to_string(),
        base_instructions: BaseInstructions::default(),
        dynamic_tools: Vec::new(),
        selected_capability_roots: Vec::new(),
        multi_agent_version: None,
        history_mode: ThreadHistoryMode::Legacy,
        subagent_history_start_ordinal: None,
        initial_window_id: uuid::Uuid::now_v7().to_string(),
        metadata: ThreadPersistenceMetadata {
            cwd: Some(cwd.to_path_buf()),
            model_provider: "test-provider".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    }
}

fn thread_settings_item(
    model: &str,
    reasoning_effort: ReasoningEffort,
    model_provider_id: &str,
    cwd: &std::path::Path,
) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
        ThreadSettingsAppliedEvent {
            thread_settings: ThreadSettingsSnapshot {
                model: model.to_string(),
                model_provider_id: model_provider_id.to_string(),
                service_tier: None,
                approval_policy: AskForApproval::Never,
                approvals_reviewer: ApprovalsReviewer::User,
                permission_profile: PermissionProfile::workspace_write(),
                active_permission_profile: None,
                cwd: cwd.to_path_buf().try_into().expect("absolute settings cwd"),
                reasoning_effort: Some(reasoning_effort.clone()),
                reasoning_summary: None,
                personality: None,
                collaboration_mode: CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: model.to_string(),
                        reasoning_effort: Some(reasoning_effort),
                        developer_instructions: None,
                    },
                },
            },
        },
    ))
}

fn compacted_item() -> RolloutItem {
    RolloutItem::Compacted(CompactedItem {
        message: "compacted".to_string(),
        replacement_history: None,
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
    })
}
