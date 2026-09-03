//! Dashboard for inspecting and managing the TUI's retained daemon tasks.

#[path = "agents_overview_render.rs"]
mod render;

use super::agents_overview::AGENTS_OVERVIEW_VIEW_ID;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::BottomPaneView;
use crate::bottom_pane::ViewCompletion;
use crate::key_hint::KeyBindingListExt;
use crate::key_hint::ShortcutHint;
use crate::key_hint::is_plain_text_key_event;
use crate::keymap::AgentsKeymap;
use crate::keymap::KeymapContext;
use crate::keymap::KeymapContextSet;
use crate::keymap::ListAction;
use crate::keymap::ListKeymap;
use crate::keymap::RuntimeKeymap;
use crate::render::renderable::Renderable;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadActiveFlag;
use codex_app_server_protocol::ThreadStatus;
use codex_protocol::ThreadId;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Margin;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum AgentsOverviewGroup {
    NeedsYou,
    Working,
    Ready,
    Finished,
}

impl AgentsOverviewGroup {
    pub(super) fn for_status(status: &ThreadStatus) -> Self {
        match status {
            ThreadStatus::Active { active_flags }
                if active_flags.contains(&ThreadActiveFlag::WaitingOnApproval)
                    || active_flags.contains(&ThreadActiveFlag::WaitingOnUserInput) =>
            {
                Self::NeedsYou
            }
            ThreadStatus::Active { .. } => Self::Working,
            ThreadStatus::Idle => Self::Ready,
            ThreadStatus::SystemError => Self::NeedsYou,
            ThreadStatus::NotLoaded => Self::Finished,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::NeedsYou => "Needs input",
            Self::Working => "Working",
            Self::Ready => "Ready",
            Self::Finished => "Finished",
        }
    }
}

#[derive(Clone)]
pub(super) struct AgentsOverviewRow {
    pub(super) thread: Thread,
    pub(super) thread_id: ThreadId,
    pub(super) group: AgentsOverviewGroup,
    pub(super) is_current: bool,
}

#[derive(Clone, Default)]
pub(super) struct AgentsOverviewViewState {
    pub(super) input: String,
    pub(super) connection_notice: Option<&'static str>,
    search: String,
    searching: bool,
    pub(super) status_grouping: bool,
    pub(super) renaming: bool,
    // The picker can finish this retained view when it selects the already active session.
    pub(super) completion: Option<ViewCompletion>,
}

pub(super) struct AgentsOverviewView {
    pub(super) rows: Vec<AgentsOverviewRow>,
    selected: usize,
    state: Arc<Mutex<AgentsOverviewViewState>>,
    exit_on_cancel: bool,
    app_event_tx: AppEventSender,
    keymap: ListKeymap,
    agents_keymap: AgentsKeymap,
}

impl AgentsOverviewView {
    pub(super) fn new(
        rows: Vec<AgentsOverviewRow>,
        selected_thread_id: Option<ThreadId>,
        exit_on_cancel: bool,
        app_event_tx: AppEventSender,
        keymap: RuntimeKeymap,
        state: Arc<Mutex<AgentsOverviewViewState>>,
    ) -> Self {
        let selected = selected_thread_id
            .and_then(|thread_id| rows.iter().position(|row| row.thread_id == thread_id))
            .or_else(|| rows.iter().position(|row| row.is_current))
            .unwrap_or(0);
        let mut view = Self {
            rows,
            selected,
            state,
            exit_on_cancel,
            app_event_tx,
            keymap: keymap.list,
            agents_keymap: keymap.agents,
        };
        view.state().completion = None;
        let visible = view.visible_indices();
        if !visible.contains(&view.selected) {
            view.selected = visible.first().copied().unwrap_or(usize::MAX);
        }
        view
    }

    pub(super) fn thread_ids(&self) -> Vec<ThreadId> {
        self.rows.iter().map(|row| row.thread_id).collect()
    }

    fn state(&self) -> MutexGuard<'_, AgentsOverviewViewState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn selected_row(&self) -> Option<&AgentsOverviewRow> {
        self.rows
            .get(self.selected)
            .filter(|_| self.visible_indices().contains(&self.selected))
    }

    fn visible_indices(&self) -> Vec<usize> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let search = state.search.to_lowercase();
        let mut visible = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                let searchable = format!(
                    "{} {} {}",
                    row.thread.name.as_deref().unwrap_or_default(),
                    row.thread.preview,
                    row.thread.cwd.display(),
                )
                .to_lowercase();
                (search.is_empty() || searchable.contains(&search)).then_some(index)
            })
            .collect::<Vec<_>>();
        if !state.status_grouping {
            visible.sort_by_key(|index| {
                (
                    &self.rows[*index].thread.cwd,
                    std::cmp::Reverse(self.rows[*index].thread.updated_at),
                )
            });
        }
        visible
    }

    fn move_selection(&mut self, forward: bool) {
        if self.state().renaming {
            return;
        }
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        let current = visible
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        self.selected = if forward {
            visible[(current + 1) % visible.len()]
        } else {
            visible[current.checked_sub(1).unwrap_or(visible.len() - 1)]
        };
    }

    fn activate(&mut self) {
        let state = self.state().clone();
        let input = state.input.clone();
        if !state.searching && !input.is_empty() && input.trim().is_empty() {
            return;
        }
        if !state.searching && !input.trim().is_empty() {
            if state.renaming {
                if let Some(row) = self.selected_row() {
                    self.app_event_tx
                        .send(AppEvent::RenameAgentsOverviewThread {
                            thread_id: row.thread_id,
                            name: input.trim().to_string(),
                        });
                }
                self.state().renaming = false;
            } else {
                self.app_event_tx
                    .send(AppEvent::DispatchAgentsOverviewTask {
                        prompt: input,
                        cwd: (!state.status_grouping)
                            .then(|| self.selected_row().map(|row| row.thread.cwd.clone()))
                            .flatten(),
                    });
            }
            self.state().input.clear();
        } else if let Some(row) = self.selected_row().filter(|_| !state.renaming) {
            self.app_event_tx
                .send(AppEvent::SelectAgentsOverviewThread {
                    thread_id: row.thread_id,
                });
            if state.searching {
                let mut state = self.state();
                state.search.clear();
                state.searching = false;
            }
            self.state().completion = Some(ViewCompletion::Accepted);
        }
    }

    fn edit_input(&mut self, edit: impl FnOnce(&mut String)) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let searching = state.searching;
        edit(if searching {
            &mut state.search
        } else {
            &mut state.input
        });
        drop(state);
        if searching {
            self.selected = self
                .visible_indices()
                .first()
                .copied()
                .unwrap_or(usize::MAX);
        }
        true
    }

    fn status(row: &AgentsOverviewRow) -> (&'static str, Span<'static>) {
        match row.group {
            AgentsOverviewGroup::NeedsYou => ("Needs input", "●".red()),
            AgentsOverviewGroup::Working => ("Working", "●".green()),
            AgentsOverviewGroup::Ready => ("Ready", "○".cyan()),
            AgentsOverviewGroup::Finished => ("Finished", "✓".dim()),
        }
    }

    fn render_rows(&self, area: Rect, buf: &mut Buffer) {
        let mut offset = 0;
        let mut previous_group: Option<String> = None;
        let project_grouping = !self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .status_grouping;
        let visible = self.visible_indices();
        let mut first = visible
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or_default();
        let mut height = 2;
        while first > 0 {
            let previous = &self.rows[visible[first - 1]];
            let current = &self.rows[visible[first]];
            let group_changed = if project_grouping {
                previous.thread.cwd != current.thread.cwd
            } else {
                previous.group != current.group
            };
            let added_height = 1 + 2 * u16::from(group_changed);
            if height + added_height > area.height {
                break;
            }
            height += added_height;
            first -= 1;
        }
        for index in visible.into_iter().skip(first) {
            if offset >= area.height {
                break;
            }
            let row = &self.rows[index];
            let group = if project_grouping {
                row.thread.cwd.display().to_string()
            } else {
                row.group.label().to_string()
            };
            if previous_group.as_deref() != Some(group.as_str()) {
                offset += u16::from(previous_group.is_some());
                if offset >= area.height {
                    break;
                }
                let count = self
                    .rows
                    .iter()
                    .filter(|candidate| {
                        if project_grouping {
                            candidate.thread.cwd == row.thread.cwd
                        } else {
                            candidate.group == row.group
                        }
                    })
                    .count();
                Line::from(vec![group.clone().bold(), format!("  {count}").dim()])
                    .render(Rect::new(area.x, area.y + offset, area.width, 1), buf);
                offset += 1;
                previous_group = Some(group);
            }
            if offset >= area.height {
                break;
            }
            let marker = if self.selected == index {
                "›".cyan().bold()
            } else {
                " ".into()
            };
            let title = row
                .thread
                .name
                .as_deref()
                .or_else(|| (!row.thread.preview.is_empty()).then_some(row.thread.preview.as_str()))
                .unwrap_or("Untitled task");
            let (status, dot) = Self::status(row);
            let current = if row.is_current { "  current" } else { "" };
            let mut spans = vec![
                marker,
                " ".into(),
                dot,
                " ".into(),
                title.into(),
                current.dim(),
            ];
            if project_grouping {
                spans.extend(["  ".into(), status.dim()]);
            }
            Line::from(spans).render(Rect::new(area.x, area.y + offset, area.width, 1), buf);
            offset += 1;
        }
    }

    fn render_details(&self, area: Rect, buf: &mut Buffer) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let (status, dot) = Self::status(row);
        let mut lines = vec![
            Line::from("Task details".bold()),
            Line::default(),
            Line::from(row.thread.name.as_deref().unwrap_or("Untitled task").bold()),
            Line::from(vec![dot, " ".into(), status.into()]),
            Line::default(),
            Line::from("Project".dim()),
            Line::from(row.thread.cwd.display().to_string()),
        ];
        if let Some(branch) = row
            .thread
            .git_info
            .as_ref()
            .and_then(|git| git.branch.as_ref())
        {
            lines.push(Line::default());
            lines.push("Branch".dim().into());
            lines.push(branch.clone().into());
        }
        let preview = crate::text_formatting::truncate_text(
            &row.thread.preview,
            usize::from(area.width) * usize::from(area.height),
        );
        lines.extend([
            Line::default(),
            Line::from("Latest activity".dim()),
            Line::from(match preview.as_str() {
                "" => "No activity yet.",
                preview => preview,
            }),
        ]);
        Paragraph::new(crate::wrapping::word_wrap_lines(
            lines,
            usize::from(area.width),
        ))
        .render(area, buf);
    }
}

impl BottomPaneView for AgentsOverviewView {
    fn view_id(&self) -> Option<&'static str> {
        Some(AGENTS_OVERVIEW_VIEW_ID)
    }

    fn selected_index(&self) -> Option<usize> {
        Some(self.selected)
    }

    fn keymap_contexts(&self) -> KeymapContextSet {
        KeymapContextSet::new(KeymapContext::List).with(KeymapContext::Agents)
    }

    fn completion(&self) -> Option<ViewCompletion> {
        self.state().completion
    }

    fn is_complete(&self) -> bool {
        self.completion().is_some()
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        true
    }

    fn handle_paste(&mut self, pasted: String) -> bool {
        self.edit_input(|input| {
            input.push_str(&crate::history_cell::sanitize_user_text(pasted.into()))
        })
    }

    fn handle_key_event(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Backspace
            && key.modifiers.is_empty()
            && self.keymap.action_for(key).is_none()
        {
            self.edit_input(|input| {
                input.pop();
            });
            return;
        }
        if is_plain_text_key_event(key)
            && let KeyCode::Char(character) = key.code
        {
            self.edit_input(|input| input.push(character));
            return;
        }

        if self.agents_keymap.search.is_pressed(key) || {
            let state = self.state();
            state.connection_notice.is_some()
                && state.searching
                && self.keymap.action_for(key) == Some(ListAction::Cancel)
        } {
            let mut state = self.state();
            if !state.renaming {
                state.searching = !state.searching;
                if !state.searching {
                    state.search.clear();
                }
            }
            return;
        }

        if self.state().connection_notice.is_some() {
            match self.keymap.action_for(key) {
                Some(ListAction::MoveUp) => self.move_selection(/*forward*/ false),
                Some(ListAction::MoveDown) => self.move_selection(/*forward*/ true),
                _ => {}
            }
            return;
        }

        if self.agents_keymap.resume.is_pressed(key) {
            self.app_event_tx.send(AppEvent::OpenResumePicker);
            return;
        }
        if self.agents_keymap.toggle_grouping.is_pressed(key) {
            let mut state = self.state();
            state.status_grouping = !state.status_grouping;
            return;
        }
        if self.agents_keymap.new_task.is_pressed(key) {
            let mut state = self.state();
            state.search.clear();
            state.searching = false;
            state.renaming = false;
            state.input.clear();
            return;
        }
        if self.agents_keymap.rename.is_pressed(key) {
            if let Some(row) = self.selected_row() {
                let mut state = self.state();
                if state.input.is_empty() {
                    state.input = row.thread.name.clone().unwrap_or_default();
                    state.search.clear();
                    state.searching = false;
                    state.renaming = true;
                }
            }
            return;
        }
        if self.agents_keymap.stop.is_pressed(key) {
            if let Some(row) = self.selected_row()
                && matches!(row.thread.status, ThreadStatus::Active { .. })
            {
                self.app_event_tx.send(AppEvent::StopAgentsOverviewThread {
                    thread_id: row.thread_id,
                });
            }
            return;
        }

        if let Some(action) = self.keymap.action_for(key) {
            if self.state().renaming
                && matches!(action, ListAction::JumpTop | ListAction::JumpBottom)
            {
                return;
            }
            match action {
                ListAction::MoveUp => self.move_selection(/*forward*/ false),
                ListAction::MoveDown => self.move_selection(/*forward*/ true),
                ListAction::JumpTop => {
                    self.selected = self.visible_indices().first().copied().unwrap_or(0);
                }
                ListAction::JumpBottom => {
                    self.selected = self.visible_indices().last().copied().unwrap_or(0);
                }
                ListAction::Accept => self.activate(),
                ListAction::Cancel => {
                    let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                    if state.searching {
                        state.search.clear();
                        state.searching = false;
                        self.selected = 0;
                    } else if !state.input.is_empty() || state.renaming {
                        state.input.clear();
                        state.renaming = false;
                    } else {
                        if self.exit_on_cancel {
                            self.app_event_tx
                                .send(AppEvent::Exit(crate::app::ExitMode::Immediate));
                        }
                        state.completion = Some(ViewCompletion::Cancelled);
                    }
                }
                ListAction::PageUp | ListAction::PageDown => {
                    for _ in 0..5 {
                        self.move_selection(action == ListAction::PageDown);
                    }
                }
                ListAction::MoveLeft | ListAction::MoveRight => {}
            }
        } else if key.code == KeyCode::Backspace {
            self.edit_input(|input| {
                input.pop();
            });
        }
    }
}
