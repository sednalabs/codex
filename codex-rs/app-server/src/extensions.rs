use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::Turn;
use codex_core::AuthManager;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core::file_watcher::WatchPath;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

/// Internal app-server extension seam.
///
/// This is intentionally small and static: we want a no-op upstream-shaped
/// surface plus a downstream implementation, not a dynamic plugin system.
pub(crate) trait AppServerHooks: Send + Sync + 'static {
    /// Lifecycle hook for app-server startup.
    fn on_app_server_start(
        &self,
        _thread_manager: &Arc<ThreadManager>,
        _config: &Arc<Config>,
        _auth_manager: Arc<AuthManager>,
    ) {
    }

    /// Policy describing what follow-up work should happen after a config mutation.
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
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ConfigMutationFollowUp {
    pub(crate) clear_plugin_related_caches: bool,
    pub(crate) maybe_start_plugin_startup_tasks_for_latest_config: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    ) {
        thread_manager
            .plugins_manager()
            .maybe_start_plugin_startup_tasks_for_config(config, auth_manager);
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
}

fn nearest_existing_watch_ancestor(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .skip(1)
        .find(|ancestor| ancestor.exists())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_core::file_watcher::WatchPath;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

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
}
