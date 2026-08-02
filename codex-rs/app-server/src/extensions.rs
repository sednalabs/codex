use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use codex_analytics::AnalyticsEventsClient;
use codex_app_server_protocol::MarketplaceAddResponse;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadGoal;
use codex_app_server_protocol::ThreadGoalUpdatedNotification;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::WarningNotification;
use codex_core::NewThread;
use codex_core::StartThreadOptions;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core_plugins::EffectivePluginsChange;
use codex_exec_server::EnvironmentManager;
use codex_extension_api::AgentSpawnFuture;
use codex_extension_api::AgentSpawner;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ExtensionWarning;
use codex_file_watcher::WatchPath;
use codex_goal_extension::GoalService;
use codex_http_client::HttpClientFactory;
use codex_login::AuthManager;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_rollout::state_db::StateDbHandle;
use codex_thread_store::ThreadStore;
use codex_utils_absolute_path::AbsolutePathBuf;

use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;
use crate::outgoing_message::ThreadScopedOutgoingMessageSender;
use crate::thread_state::ThreadListenerCommand;
use crate::thread_state::ThreadStateManager;

pub(crate) struct ThreadExtensionDependencies {
    pub(crate) event_sink: Arc<dyn ExtensionEventSink>,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) state_db: Option<StateDbHandle>,
    pub(crate) analytics_events_client: AnalyticsEventsClient,
    pub(crate) thread_manager: Weak<ThreadManager>,
    pub(crate) goal_service: Arc<GoalService>,
    pub(crate) environment_manager: Arc<EnvironmentManager>,
    pub(crate) executor_skill_provider: Arc<dyn codex_skills_extension::SkillProvider>,
    pub(crate) git_attribution_base_url: String,
    pub(crate) http_client_factory: HttpClientFactory,
    /// Process-scoped persistence backend for extensions that need stored thread history.
    pub(crate) thread_store: Arc<dyn ThreadStore>,
}

/// Internal app-server extension seam.
///
/// This keeps fork-specific behavior behind a narrow, upstream-shaped hook
/// surface rather than spreading it through request processors and hot loops.
pub(crate) trait AppServerHooks: Send + Sync + 'static {
    /// Lifecycle hook for app-server startup.
    #[allow(dead_code)]
    fn on_app_server_start(
        &self,
        _thread_manager: &Arc<ThreadManager>,
        _config: &Arc<Config>,
        _auth_manager: Arc<AuthManager>,
        _on_effective_plugins_changed: Option<Arc<dyn Fn(EffectivePluginsChange) + Send + Sync>>,
    ) {
    }

    /// Policy describing what follow-up work should happen after a config mutation.
    #[allow(dead_code)]
    fn config_mutation_follow_up(&self, _kind: ConfigMutationKind) -> ConfigMutationFollowUp {
        ConfigMutationFollowUp::default()
    }

    /// Opportunity to overlay live runtime context onto a thread/read result.
    fn augment_thread_read(
        &self,
        _thread: &mut Thread,
        _active_turn: Option<&Turn>,
        _has_live_in_progress_turn: bool,
    ) {
    }

    /// Opportunity to overlay live runtime context onto a thread/resume result.
    fn augment_thread_resume(
        &self,
        _thread: &mut Thread,
        _active_turn: Option<&Turn>,
        _has_live_in_progress_turn: bool,
    ) {
    }

    /// Delivery policy for selected best-effort notifications.
    fn notification_dispatch_mode(
        &self,
        _kind: NotificationDispatchKind,
    ) -> NotificationDispatchMode {
        NotificationDispatchMode::AwaitWriteCompletion
    }

    /// Filesystem watch registration policy.
    fn fs_watch_paths_for_target(&self, path: &AbsolutePathBuf) -> Vec<WatchPath> {
        vec![WatchPath {
            path: path.to_path_buf(),
            recursive: false,
        }]
    }

    /// Filesystem watch event mapping policy.
    fn fs_changed_path_for_watch_target(
        &self,
        _watch_target: &AbsolutePathBuf,
        event_path: AbsolutePathBuf,
    ) -> Option<AbsolutePathBuf> {
        Some(event_path)
    }

    /// Whether mapped fs/changed batches should be deduplicated before sending.
    fn dedupe_fs_changed_paths(&self) -> bool {
        false
    }

    /// Opportunity to overlay marketplace/add state before it reaches clients.
    fn augment_marketplace_add_response(&self, _response: &mut MarketplaceAddResponse) {}
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) struct ConfigMutationFollowUp {
    pub(crate) clear_plugin_related_caches: bool,
    pub(crate) maybe_start_plugin_startup_tasks_for_latest_config: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum ConfigMutationKind {
    ValueWrite,
    BatchWrite,
    ExperimentalFeatureEnablementSet,
    SkillsConfigWrite,
    PluginInstall,
    PluginUninstall,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NotificationDispatchKind {
    CommandExecOutputDelta,
    FsChanged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NotificationDispatchMode {
    AwaitWriteCompletion,
    EnqueueOnly,
}

pub(crate) fn app_server_hooks() -> &'static dyn AppServerHooks {
    &SEDNA_APP_SERVER_HOOKS
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn noop_app_server_hooks() -> &'static dyn AppServerHooks {
    &NOOP_APP_SERVER_HOOKS
}

pub(crate) async fn dispatch_notification_to_connection(
    outgoing: &OutgoingMessageSender,
    connection_id: ConnectionId,
    kind: NotificationDispatchKind,
    notification: ServerNotification,
) {
    match app_server_hooks().notification_dispatch_mode(kind) {
        NotificationDispatchMode::AwaitWriteCompletion => {
            outgoing
                .send_server_notification_to_connection_and_wait(connection_id, notification)
                .await;
        }
        NotificationDispatchMode::EnqueueOnly => {
            outgoing
                .send_server_notification_to_connection(connection_id, notification)
                .await;
        }
    }
}

pub(crate) fn thread_extensions<S>(
    guardian_agent_spawner: S,
    dependencies: ThreadExtensionDependencies,
) -> Arc<ExtensionRegistry<Config>>
where
    S: AgentSpawner<StartThreadOptions, Spawned = NewThread, Error = CodexErr> + 'static,
{
    let ThreadExtensionDependencies {
        event_sink,
        auth_manager,
        state_db,
        analytics_events_client,
        thread_manager,
        goal_service,
        environment_manager,
        executor_skill_provider,
        git_attribution_base_url,
        http_client_factory,
        thread_store: _thread_store,
    } = dependencies;
    let mut builder = ExtensionRegistryBuilder::<Config>::with_event_sink(event_sink);
    if let Some(state_db) = state_db {
        codex_goal_extension::install_with_backend(
            &mut builder,
            state_db,
            analytics_events_client,
            codex_otel::global(),
            thread_manager,
            goal_service,
            |config: &Config| config.features.enabled(codex_features::Feature::Goals),
        );
    }
    codex_git_attribution::install(
        &mut builder,
        auth_manager.clone(),
        git_attribution_base_url,
        http_client_factory,
    );
    codex_guardian::install(&mut builder, guardian_agent_spawner);
    codex_memories_extension::install(&mut builder, codex_otel::global());
    codex_mcp_extension::install(&mut builder);
    codex_mcp_extension::install_executor_plugins(&mut builder, environment_manager);
    codex_web_search_extension::install(&mut builder, auth_manager.clone());
    codex_image_generation_extension::install(&mut builder, auth_manager, |config: &Config| {
        Some(config.codex_home.clone())
    });
    let skill_providers = codex_skills_extension::SkillProviders::new()
        .with_executor_provider(executor_skill_provider)
        .with_orchestrator_provider(Arc::new(
            codex_skills_extension::OrchestratorSkillProvider::new(),
        ))
        .with_host_provider(Arc::new(codex_skills_extension::HostSkillProvider::new()));
    codex_skills_extension::install_with_providers_and_metrics(
        &mut builder,
        skill_providers,
        codex_otel::global(),
        |config: &Config| codex_skills_extension::SkillsExtensionConfig {
            include_instructions: config.include_skill_instructions,
            bundled_skills_enabled: config.bundled_skills_enabled(),
            orchestrator_skills_enabled: config.orchestrator_skills_enabled,
            shadow_selection_enabled: config
                .features
                .enabled(codex_features::Feature::SkillSearch),
        },
    );
    Arc::new(builder.build())
}

pub(crate) fn app_server_extension_event_sink(
    outgoing: Arc<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
) -> Arc<dyn ExtensionEventSink> {
    Arc::new(AppServerExtensionEventSink {
        outgoing,
        thread_state_manager,
    })
}

pub(crate) async fn send_thread_warning(
    outgoing: &Arc<OutgoingMessageSender>,
    thread_id: ThreadId,
    message: String,
) {
    let thread_subscriptions = outgoing
        .thread_subscription_targets_for_thread(thread_id)
        .await;
    let thread_outgoing = ThreadScopedOutgoingMessageSender::from_captured_thread_subscriptions(
        Arc::clone(outgoing),
        thread_subscriptions,
        thread_id,
    );
    thread_outgoing
        .send_server_notification(ServerNotification::Warning(WarningNotification {
            thread_id: Some(thread_id.to_string()),
            message,
        }))
        .await;
}

struct AppServerExtensionEventSink {
    outgoing: Arc<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
}

const MAX_EXTENSION_WARNING_BYTES: usize = 256;
const EXTENSION_WARNING_SUBSCRIBER_TIMEOUT: Duration = Duration::from_secs(10);

impl ExtensionEventSink for AppServerExtensionEventSink {
    fn emit(&self, event: Event) {
        match event.msg {
            EventMsg::ThreadGoalUpdated(thread_goal_event) => {
                let thread_id = thread_goal_event.thread_id;
                let turn_id = thread_goal_event.turn_id;
                let goal: ThreadGoal = thread_goal_event.goal.into();
                // `ExtensionEventSink::emit` is synchronous. Capture the
                // token map here, before queueing an ordered listener command
                // or spawning a fallback, so either path preserves this
                // event's original lifecycle identity.
                let thread_subscriptions = self
                    .outgoing
                    .thread_subscription_targets_for_thread_now(thread_id);
                if let Some(listener_command_tx) = self
                    .thread_state_manager
                    .current_listener_command_tx(thread_id)
                {
                    let command = ThreadListenerCommand::EmitThreadGoalUpdated {
                        turn_id: turn_id.clone(),
                        goal: goal.clone(),
                        thread_subscriptions: thread_subscriptions.clone(),
                    };
                    if listener_command_tx.send(command).is_ok() {
                        return;
                    }
                    tracing::warn!(
                        "failed to enqueue extension goal update for {thread_id}: listener command channel is closed"
                    );
                }
                let outgoing = Arc::clone(&self.outgoing);
                tokio::spawn(async move {
                    outgoing
                        .send_server_notification_to_thread_subscriptions(
                            &thread_subscriptions,
                            ServerNotification::ThreadGoalUpdated(ThreadGoalUpdatedNotification {
                                thread_id: thread_id.to_string(),
                                turn_id,
                                goal,
                            }),
                        )
                        .await;
                });
            }
            msg => {
                tracing::debug!(event_id = %event.id, ?msg, "dropping unsupported extension event");
            }
        }
    }

    fn emit_warning(&self, warning: ExtensionWarning) {
        let ExtensionWarning {
            thread_id,
            turn_id: _,
            message,
        } = warning;
        let Ok(thread_id) = ThreadId::from_string(&thread_id) else {
            tracing::warn!(
                %thread_id,
                "dropping extension warning with invalid thread id"
            );
            return;
        };
        let mut message = message;
        if message.len() > MAX_EXTENSION_WARNING_BYTES {
            let mut truncate_at = MAX_EXTENSION_WARNING_BYTES;
            while !message.is_char_boundary(truncate_at) {
                truncate_at -= 1;
            }
            message.truncate(truncate_at);
        }
        if let Some(listener_command_tx) = self
            .thread_state_manager
            .current_listener_command_tx(thread_id)
        {
            let command = ThreadListenerCommand::EmitWarning {
                message: message.clone(),
            };
            if listener_command_tx.send(command).is_ok() {
                return;
            }
            tracing::warn!(
                "failed to enqueue extension warning for {thread_id}: listener command channel is closed"
            );
        }
        let outgoing = Arc::clone(&self.outgoing);
        let thread_state_manager = self.thread_state_manager.clone();
        tokio::spawn(async move {
            if tokio::time::timeout(
                EXTENSION_WARNING_SUBSCRIBER_TIMEOUT,
                thread_state_manager.wait_for_thread_subscriber(thread_id),
            )
            .await
            .is_err()
            {
                tracing::warn!(
                    %thread_id,
                    timeout_secs = EXTENSION_WARNING_SUBSCRIBER_TIMEOUT.as_secs(),
                    "dropping extension warning after waiting for a thread subscriber"
                );
                return;
            }
            send_thread_warning(&outgoing, thread_id, message).await;
        });
    }
}

pub(crate) fn guardian_agent_spawner(
    thread_manager: Weak<ThreadManager>,
) -> impl AgentSpawner<StartThreadOptions, Spawned = NewThread, Error = CodexErr> {
    move |forked_from_thread_id: ThreadId,
          options: StartThreadOptions|
          -> AgentSpawnFuture<'static, NewThread, CodexErr> {
        let thread_manager = thread_manager.clone();
        Box::pin(async move {
            let thread_manager = thread_manager.upgrade().ok_or_else(|| {
                CodexErr::UnsupportedOperation("thread manager dropped".to_string())
            })?;
            thread_manager
                .spawn_subagent(forked_from_thread_id, options)
                .await
        })
    }
}

#[cfg_attr(not(test), allow(dead_code))]
struct NoopAppServerHooks;
#[cfg_attr(not(test), allow(dead_code))]
static NOOP_APP_SERVER_HOOKS: NoopAppServerHooks = NoopAppServerHooks;

impl AppServerHooks for NoopAppServerHooks {}

struct SednaAppServerHooks;
static SEDNA_APP_SERVER_HOOKS: SednaAppServerHooks = SednaAppServerHooks;

impl AppServerHooks for SednaAppServerHooks {
    fn on_app_server_start(
        &self,
        thread_manager: &Arc<ThreadManager>,
        config: &Arc<Config>,
        auth_manager: Arc<AuthManager>,
        on_effective_plugins_changed: Option<Arc<dyn Fn(EffectivePluginsChange) + Send + Sync>>,
    ) {
        thread_manager
            .plugins_manager()
            .maybe_start_plugin_startup_tasks_for_config(
                &config.plugins_config_input(),
                auth_manager,
                on_effective_plugins_changed,
            );
    }

    fn config_mutation_follow_up(&self, kind: ConfigMutationKind) -> ConfigMutationFollowUp {
        match kind {
            ConfigMutationKind::ValueWrite
            | ConfigMutationKind::BatchWrite
            | ConfigMutationKind::ExperimentalFeatureEnablementSet => ConfigMutationFollowUp {
                clear_plugin_related_caches: true,
                maybe_start_plugin_startup_tasks_for_latest_config: true,
            },
            ConfigMutationKind::SkillsConfigWrite
            | ConfigMutationKind::PluginInstall
            | ConfigMutationKind::PluginUninstall => ConfigMutationFollowUp {
                clear_plugin_related_caches: true,
                maybe_start_plugin_startup_tasks_for_latest_config: false,
            },
        }
    }

    fn notification_dispatch_mode(
        &self,
        kind: NotificationDispatchKind,
    ) -> NotificationDispatchMode {
        match kind {
            NotificationDispatchKind::CommandExecOutputDelta
            | NotificationDispatchKind::FsChanged => NotificationDispatchMode::EnqueueOnly,
        }
    }

    fn fs_watch_paths_for_target(&self, path: &AbsolutePathBuf) -> Vec<WatchPath> {
        let watch_path = path.to_path_buf();
        let mut watched_paths = vec![WatchPath {
            path: watch_path.clone(),
            recursive: watch_path.is_dir(),
        }];
        if !watch_path.exists()
            && let Some(existing_ancestor) = nearest_existing_watch_ancestor(&watch_path)
        {
            watched_paths.push(WatchPath {
                recursive: existing_ancestor.parent().is_some(),
                path: existing_ancestor,
            });
        }
        watched_paths
    }

    fn fs_changed_path_for_watch_target(
        &self,
        watch_target: &AbsolutePathBuf,
        event_path: AbsolutePathBuf,
    ) -> Option<AbsolutePathBuf> {
        let watch_target = watch_target.as_path();
        let event_path_ref = event_path.as_path();
        if event_path_ref == watch_target {
            return Some(event_path);
        }
        if watch_target.starts_with(event_path_ref) {
            return AbsolutePathBuf::try_from(watch_target.to_path_buf()).ok();
        }
        if event_path_ref.starts_with(watch_target) {
            return Some(event_path);
        }
        None
    }

    fn dedupe_fs_changed_paths(&self) -> bool {
        true
    }

    fn augment_marketplace_add_response(&self, _response: &mut MarketplaceAddResponse) {}
}

fn nearest_existing_watch_ancestor(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .skip(1)
        .find(|ancestor| ancestor.exists())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use codex_protocol::protocol::ThreadGoal as CoreThreadGoal;
    use codex_protocol::protocol::ThreadGoalStatus;
    use codex_protocol::protocol::ThreadGoalUpdatedEvent;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    use crate::outgoing_message::ConnectionId;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::outgoing_message::OutgoingMessage;
    use crate::thread_state::ConnectionCapabilities;

    use super::*;

    #[test]
    fn noop_hooks_default_to_upstreamish_behavior() {
        assert_eq!(
            noop_app_server_hooks()
                .notification_dispatch_mode(NotificationDispatchKind::CommandExecOutputDelta,),
            NotificationDispatchMode::AwaitWriteCompletion
        );
        assert_eq!(
            noop_app_server_hooks().config_mutation_follow_up(ConfigMutationKind::ValueWrite),
            ConfigMutationFollowUp::default()
        );
    }

    #[test]
    fn sedna_hooks_preserve_config_mutation_follow_ups() {
        assert_eq!(
            app_server_hooks().config_mutation_follow_up(ConfigMutationKind::ValueWrite),
            ConfigMutationFollowUp {
                clear_plugin_related_caches: true,
                maybe_start_plugin_startup_tasks_for_latest_config: true,
            }
        );
        assert_eq!(
            app_server_hooks().config_mutation_follow_up(ConfigMutationKind::PluginInstall),
            ConfigMutationFollowUp {
                clear_plugin_related_caches: true,
                maybe_start_plugin_startup_tasks_for_latest_config: false,
            }
        );
    }

    #[test]
    fn sedna_hooks_enable_non_blocking_notification_dispatch() {
        assert_eq!(
            app_server_hooks().notification_dispatch_mode(NotificationDispatchKind::FsChanged),
            NotificationDispatchMode::EnqueueOnly
        );
    }

    #[test]
    fn sedna_watch_paths_include_recursive_parent_for_watch_before_create() {
        let temp_dir = TempDir::new().expect("temp dir");
        let target = AbsolutePathBuf::try_from(temp_dir.path().join("missing/child.txt"))
            .expect("absolute target");
        let watch_paths = app_server_hooks().fs_watch_paths_for_target(&target);
        assert_eq!(watch_paths.len(), 2);
        assert_eq!(watch_paths[0].path, target.to_path_buf());
        assert!(!watch_paths[0].recursive);
        assert_eq!(
            watch_paths[1],
            WatchPath {
                path: temp_dir.path().to_path_buf(),
                recursive: true,
            }
        );
    }

    #[test]
    fn sedna_watch_mapping_normalizes_parent_events_back_to_watch_target() {
        let temp_dir = TempDir::new().expect("temp dir");
        let target = AbsolutePathBuf::try_from(temp_dir.path().join("missing/child.txt"))
            .expect("absolute target");
        let mapped = app_server_hooks().fs_changed_path_for_watch_target(
            &target,
            AbsolutePathBuf::try_from(temp_dir.path().to_path_buf()).expect("absolute root"),
        );
        assert_eq!(mapped, Some(target));
    }

    #[tokio::test]
    async fn app_server_event_sink_uses_listener_fifo_for_goal_updates_warnings_and_clears() {
        let (outgoing_tx, _outgoing_rx) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            AnalyticsEventsClient::disabled(),
        ));
        let thread_state_manager = ThreadStateManager::new();
        let thread_id = ThreadId::default();
        let (listener_command_tx, mut listener_command_rx) = mpsc::unbounded_channel();
        thread_state_manager.register_listener_command_tx(thread_id, listener_command_tx.clone());
        let sink = app_server_extension_event_sink(outgoing, thread_state_manager);

        sink.emit(thread_goal_updated_event(thread_id, "turn-1"));
        sink.emit_warning(ExtensionWarning {
            thread_id: thread_id.to_string(),
            turn_id: Some("turn-warning".to_string()),
            message: "catalog was shortened".to_string(),
        });
        sink.emit(thread_goal_updated_event(thread_id, "turn-2"));
        listener_command_tx
            .send(ThreadListenerCommand::EmitThreadGoalCleared {
                thread_subscriptions: Vec::new(),
            })
            .expect("listener command channel should be open");

        let mut observed = Vec::new();
        for _ in 0..4 {
            let command = timeout(Duration::from_secs(1), listener_command_rx.recv())
                .await
                .expect("timed out waiting for listener command")
                .expect("listener command channel closed unexpectedly");
            match command {
                ThreadListenerCommand::EmitThreadGoalUpdated { turn_id, .. } => {
                    observed.push(turn_id.expect("extension goal updates should include turn ids"));
                }
                ThreadListenerCommand::EmitWarning { message } => observed.push(message),
                ThreadListenerCommand::EmitThreadGoalCleared { .. } => {
                    observed.push("cleared".to_string())
                }
                _ => panic!("unexpected listener command"),
            }
        }

        assert_eq!(
            vec![
                "turn-1".to_string(),
                "catalog was shortened".to_string(),
                "turn-2".to_string(),
                "cleared".to_string()
            ],
            observed
        );
    }

    #[tokio::test]
    async fn app_server_event_sink_truncates_warning_before_listener_enqueue() {
        let (outgoing_tx, _outgoing_rx) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            AnalyticsEventsClient::disabled(),
        ));
        let thread_state_manager = ThreadStateManager::new();
        let thread_id = ThreadId::default();
        let (listener_command_tx, mut listener_command_rx) = mpsc::unbounded_channel();
        thread_state_manager.register_listener_command_tx(thread_id, listener_command_tx);
        let sink = app_server_extension_event_sink(outgoing, thread_state_manager);

        sink.emit_warning(ExtensionWarning {
            thread_id: thread_id.to_string(),
            turn_id: Some("turn-warning".to_string()),
            message: "🙂".repeat(65),
        });

        let command = timeout(Duration::from_secs(1), listener_command_rx.recv())
            .await
            .expect("timed out waiting for listener command")
            .expect("listener command channel closed unexpectedly");
        let ThreadListenerCommand::EmitWarning { message } = command else {
            panic!("expected warning listener command");
        };
        assert_eq!(message, "🙂".repeat(64));
    }

    #[tokio::test]
    async fn app_server_event_sink_targets_subscriber_without_listener() {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let subscribed_connection = ConnectionId(1);
        let unrelated_connection = ConnectionId(2);
        let thread_state_manager = ThreadStateManager::new();
        for connection_id in [subscribed_connection, unrelated_connection] {
            thread_state_manager
                .connection_initialized(connection_id, ConnectionCapabilities::default())
                .await;
        }
        thread_state_manager
            .try_ensure_connection_subscribed(
                thread_id,
                subscribed_connection,
                /*experimental_raw_events*/ false,
            )
            .await
            .expect("connection should be subscribed");
        let sink = app_server_extension_event_sink(outgoing, thread_state_manager);

        sink.emit_warning(ExtensionWarning {
            thread_id: thread_id.to_string(),
            turn_id: Some("turn-1".to_string()),
            message: "catalog was shortened".to_string(),
        });

        let envelope = timeout(Duration::from_secs(1), outgoing_rx.recv())
            .await
            .expect("timed out waiting for warning notification")
            .expect("outgoing channel closed unexpectedly");
        let OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            write_complete_tx: _,
        } = envelope
        else {
            panic!("expected connection-targeted warning notification");
        };
        assert_eq!(connection_id, subscribed_connection);
        let OutgoingMessage::ThreadScopedNotification(notification) = message else {
            panic!("expected tagged app-server warning notification");
        };
        let envelope = notification.envelope;
        let ServerNotification::Warning(notification) = envelope.notification else {
            panic!("expected warning notification");
        };
        assert_eq!(
            notification,
            WarningNotification {
                thread_id: Some(thread_id.to_string()),
                message: "catalog was shortened".to_string(),
            }
        );
        assert!(outgoing_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn app_server_event_sink_waits_for_subscriber_without_listener() {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let subscribed_connection = ConnectionId(1);
        let thread_state_manager = ThreadStateManager::new();
        thread_state_manager
            .connection_initialized(subscribed_connection, ConnectionCapabilities::default())
            .await;
        let sink = app_server_extension_event_sink(outgoing, thread_state_manager.clone());

        sink.emit_warning(ExtensionWarning {
            thread_id: thread_id.to_string(),
            turn_id: Some("turn-1".to_string()),
            message: "catalog was shortened".to_string(),
        });
        tokio::task::yield_now().await;
        thread_state_manager
            .try_ensure_connection_subscribed(
                thread_id,
                subscribed_connection,
                /*experimental_raw_events*/ false,
            )
            .await
            .expect("connection should be subscribed");

        let envelope = timeout(Duration::from_secs(1), outgoing_rx.recv())
            .await
            .expect("timed out waiting for warning notification")
            .expect("outgoing channel closed unexpectedly");
        let OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            write_complete_tx: _,
        } = envelope
        else {
            panic!("expected connection-targeted warning notification");
        };
        assert_eq!(connection_id, subscribed_connection);
        let OutgoingMessage::ThreadScopedNotification(notification) = message else {
            panic!("expected tagged app-server warning notification");
        };
        let envelope = notification.envelope;
        let ServerNotification::Warning(notification) = envelope.notification else {
            panic!("expected warning notification");
        };
        assert_eq!(
            notification,
            WarningNotification {
                thread_id: Some(thread_id.to_string()),
                message: "catalog was shortened".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn app_server_event_sink_targets_subscriber_after_listener_closes() {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let subscribed_connection = ConnectionId(1);
        let thread_state_manager = ThreadStateManager::new();
        thread_state_manager
            .connection_initialized(subscribed_connection, ConnectionCapabilities::default())
            .await;
        thread_state_manager
            .try_ensure_connection_subscribed(
                thread_id,
                subscribed_connection,
                /*experimental_raw_events*/ false,
            )
            .await
            .expect("connection should be subscribed");
        let (listener_command_tx, listener_command_rx) = mpsc::unbounded_channel();
        drop(listener_command_rx);
        thread_state_manager.register_listener_command_tx(thread_id, listener_command_tx);
        let sink = app_server_extension_event_sink(outgoing, thread_state_manager);

        sink.emit_warning(ExtensionWarning {
            thread_id: thread_id.to_string(),
            turn_id: Some("turn-1".to_string()),
            message: "catalog was shortened".to_string(),
        });

        let envelope = timeout(Duration::from_secs(1), outgoing_rx.recv())
            .await
            .expect("timed out waiting for warning notification")
            .expect("outgoing channel closed unexpectedly");
        let OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            write_complete_tx: _,
        } = envelope
        else {
            panic!("expected connection-targeted warning notification");
        };
        assert_eq!(connection_id, subscribed_connection);
        let OutgoingMessage::ThreadScopedNotification(notification) = message else {
            panic!("expected tagged app-server warning notification");
        };
        let envelope = notification.envelope;
        let ServerNotification::Warning(notification) = envelope.notification else {
            panic!("expected warning notification");
        };
        assert_eq!(
            notification,
            WarningNotification {
                thread_id: Some(thread_id.to_string()),
                message: "catalog was shortened".to_string(),
            }
        );
        assert!(outgoing_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn app_server_event_sink_drops_warning_with_invalid_thread_id() {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            AnalyticsEventsClient::disabled(),
        ));
        let sink = app_server_extension_event_sink(outgoing, ThreadStateManager::new());

        sink.emit_warning(ExtensionWarning {
            thread_id: "not-a-thread-id".to_string(),
            turn_id: Some("turn-1".to_string()),
            message: "catalog was shortened".to_string(),
        });

        assert!(outgoing_rx.try_recv().is_err());
    }

    fn thread_goal_updated_event(thread_id: ThreadId, turn_id: &str) -> Event {
        Event {
            id: turn_id.to_string(),
            msg: EventMsg::ThreadGoalUpdated(ThreadGoalUpdatedEvent {
                thread_id,
                turn_id: Some(turn_id.to_string()),
                goal: CoreThreadGoal {
                    thread_id,
                    objective: "wire extension events".to_string(),
                    status: ThreadGoalStatus::Active,
                    token_budget: Some(123),
                    tokens_used: 45,
                    time_used_seconds: 6,
                    created_at: 7,
                    updated_at: 8,
                },
            }),
        }
    }
}
