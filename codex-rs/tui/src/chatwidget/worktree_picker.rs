//! Worktree choices for local, feature-enabled session commands.

use super::*;
use crate::app_event::ManagedWorktreeMode;

impl ChatWidget {
    pub(super) fn managed_worktree_available(&self) -> bool {
        self.config.features.enabled(Feature::Worktrees)
            && self.local_worktree_operations
            && get_git_repo_root(self.config.cwd.as_path()).is_some()
    }

    pub(super) fn show_session_checkout_picker(
        &mut self,
        mode: ManagedWorktreeMode,
        name: Option<String>,
    ) {
        if !self.managed_worktree_available() {
            match mode {
                ManagedWorktreeMode::New => {
                    self.app_event_tx.send(AppEvent::NewSession { name });
                }
                ManagedWorktreeMode::Fork => {
                    self.app_event_tx
                        .send(AppEvent::ForkCurrentSession { name });
                }
            }
            return;
        }

        let title = match mode {
            ManagedWorktreeMode::New => "Where should the new conversation run?",
            ManagedWorktreeMode::Fork => "Where should the forked conversation run?",
        };
        let current_name = name.clone();
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some(title.to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items: vec![
                SelectionItem {
                    name: "Current checkout".to_string(),
                    description: Some("Keep using the current working directory".to_string()),
                    actions: vec![Box::new(move |tx| match mode {
                        ManagedWorktreeMode::New => {
                            tx.send(AppEvent::NewSession {
                                name: current_name.clone(),
                            });
                        }
                        ManagedWorktreeMode::Fork => {
                            tx.send(AppEvent::ForkCurrentSession {
                                name: current_name.clone(),
                            });
                        }
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "New worktree".to_string(),
                    description: Some("Create an isolated managed checkout".to_string()),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::StartManagedWorktree {
                            mode,
                            name: name.clone(),
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        self.request_redraw();
    }

    pub(super) fn show_managed_worktree_picker(&mut self) {
        if !self.config.features.enabled(Feature::Worktrees) {
            self.add_error_message(
                "Enable worktrees in /experimental to create a worktree.".to_string(),
            );
            return;
        }
        if !self.managed_worktree_available() {
            self.add_error_message("Managed worktrees require a local Git repository.".to_string());
            return;
        }

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Create a new worktree".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items: vec![
                SelectionItem {
                    name: "Continue current conversation".to_string(),
                    description: Some("Preserve this conversation in the new checkout".to_string()),
                    actions: vec![Box::new(|tx| {
                        tx.send(AppEvent::StartManagedWorktree {
                            mode: ManagedWorktreeMode::Fork,
                            name: None,
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Start new conversation".to_string(),
                    description: Some("Open a fresh conversation in the new checkout".to_string()),
                    actions: vec![Box::new(|tx| {
                        tx.send(AppEvent::StartManagedWorktree {
                            mode: ManagedWorktreeMode::New,
                            name: None,
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        self.request_redraw();
    }
}
