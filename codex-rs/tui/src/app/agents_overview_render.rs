//! Layout rendering and cursor placement for the agent dashboard.

use super::*;

impl Renderable for AgentsOverviewView {
    fn desired_height(&self, _width: u16) -> u16 {
        24
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let state = self.state().clone();
        let (label, input) = if state.searching {
            ("  Search › ", &state.search)
        } else if state.renaming {
            ("  Rename › ", &state.input)
        } else {
            ("  New task › ", &state.input)
        };
        let x = area
            .x
            .saturating_add((label.width() + input.width()) as u16)
            .min(area.right().saturating_sub(3));
        Some((x, area.bottom().saturating_sub(2)))
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 12 || area.height < 8 {
            return;
        }
        Clear.render(area, buf);
        let [header, summary, divider, body, prompt, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        let inset =
            |rect: Rect| rect.inner(Margin::new(/*horizontal*/ 2, /*vertical*/ 0));
        Line::from("Agent command center".bold()).render(inset(header), buf);
        let (needs_you, working, ready) = self.rows.iter().fold((0, 0, 0), |counts, row| {
            let (needs_you, working, ready) = counts;
            match row.group {
                AgentsOverviewGroup::NeedsYou => (needs_you + 1, working, ready),
                AgentsOverviewGroup::Working => (needs_you, working + 1, ready),
                AgentsOverviewGroup::Ready => (needs_you, working, ready + 1),
                AgentsOverviewGroup::Finished => counts,
            }
        });
        let attention = format!("{needs_you} need input");
        if let Some(notice) = self.state().connection_notice {
            Line::from(notice.cyan()).render(inset(summary), buf);
        } else {
            Line::from(format!("{attention}   {working} working   {ready} ready").dim())
                .render(inset(summary), buf);
        }
        Line::from("─".repeat(usize::from(area.width.saturating_sub(4))).dim())
            .render(inset(divider), buf);
        let body = inset(body);
        if body.width >= 90 {
            let [list, gap, details] = Layout::horizontal([
                Constraint::Min(46),
                Constraint::Length(3),
                Constraint::Length(38),
            ])
            .areas(body);
            for y in gap.y..gap.bottom() {
                buf[(gap.x + 1, y)]
                    .set_symbol("│")
                    .set_style(Style::new().dim());
            }
            self.render_rows(list, buf);
            self.render_details(details, buf);
        } else {
            self.render_rows(body, buf);
        }
        let state = self.state().clone();
        let (label, input) = if state.searching {
            ("Search › ", &state.search)
        } else if state.renaming {
            ("Rename › ", &state.input)
        } else {
            ("New task › ", &state.input)
        };
        let placeholder = if input.is_empty() && !state.searching && !state.renaming {
            "Describe a task and press enter to dispatch it"
        } else {
            ""
        };
        let available_width = usize::from(inset(prompt).width)
            .saturating_sub(label.width())
            .saturating_sub(1);
        let mut visible_start = input.len();
        let mut visible_width = 0;
        for (index, character) in input.char_indices().rev() {
            let width = character.width().unwrap_or(0);
            if visible_width + width > available_width {
                break;
            }
            visible_width += width;
            visible_start = index;
        }
        let input = &input[visible_start..];
        Line::from(vec![label.cyan().bold(), input.into(), placeholder.dim()])
            .render(inset(prompt), buf);
        if state.connection_notice.is_some() {
            Line::from("ctrl+c quit · actions paused until the list is refreshed".dim())
                .render(inset(footer), buf);
            return;
        }
        let list_hint = |action| {
            self.keymap.primary_hint(action).filter(|hint| {
                !matches!(hint, ShortcutHint::Single(binding)
                if is_plain_text_key_event(KeyEvent::new(
                    binding.parts().0,
                    binding.parts().1,
                )) || [
                    &self.agents_keymap.search,
                    &self.agents_keymap.new_task,
                    &self.agents_keymap.rename,
                    &self.agents_keymap.stop,
                    &self.agents_keymap.toggle_grouping,
                ]
                .into_iter()
                .any(|bindings| bindings.contains(binding)))
            })
        };
        let navigation_hints = [ListAction::MoveUp, ListAction::MoveDown]
            .into_iter()
            .filter_map(list_hint)
            .map(|hint| hint.display_label().replace(" + ", "+"))
            .collect::<Vec<_>>();
        let navigation_hint = navigation_hints.join(
            if navigation_hints
                .iter()
                .all(|hint| hint.chars().count() == 1)
            {
                ""
            } else {
                " "
            },
        );
        let mut footer_spans = Vec::new();
        if !navigation_hint.is_empty() {
            footer_spans.extend([navigation_hint.bold(), " navigate  ".dim()]);
        }
        let mut add_hint = |hint: Option<ShortcutHint>, label: &'static str, enabled: bool| {
            if let Some(hint) = hint {
                let key = hint.display_label().replace(" + ", "+");
                footer_spans.push(if enabled { key.bold() } else { key.dim() });
                footer_spans.push(format!(" {label}  ").dim());
            }
        };
        add_hint(list_hint(ListAction::Accept), "open", true);
        add_hint(
            self.agents_keymap
                .primary_hint("search", &self.agents_keymap.search),
            "search",
            true,
        );
        add_hint(
            self.agents_keymap
                .primary_hint("toggle_grouping", &self.agents_keymap.toggle_grouping),
            "group",
            true,
        );
        add_hint(
            self.agents_keymap
                .primary_hint("rename", &self.agents_keymap.rename),
            "rename",
            true,
        );
        add_hint(
            self.agents_keymap
                .primary_hint("stop", &self.agents_keymap.stop),
            "stop",
            self.selected_row()
                .is_some_and(|row| matches!(row.thread.status, ThreadStatus::Active { .. })),
        );
        add_hint(list_hint(ListAction::Cancel), "back", true);
        let footer_area = inset(footer);
        let mut footer_line: Line = footer_spans.into();
        if footer_line.width() > usize::from(footer_area.width) {
            for span in &mut footer_line.spans {
                if span.content.ends_with("  ") {
                    span.content.to_mut().pop();
                }
            }
        }
        footer_line.render(footer_area, buf);
    }
}
