//! Extension points for downstream TUI surfaces that recur in fork syncs.
//!
//! These slots keep small, typed UI decisions out of central orchestration
//! files. The default contributor installed in this fork preserves the current
//! downstream behavior; an upstream-shaped tree can keep the same calls with no
//! registered contributors.

use std::borrow::Cow;

use crate::computer_use_display::ComputerUseDisplayState;
use crate::history_cell::HistoryCell;
use crate::status_indicator_widget::STATUS_DETAILS_DEFAULT_MAX_LINES;
use crate::status_indicator_widget::StatusDetailsCapitalization;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StatusIndicatorContribution {
    pub(crate) header: String,
    pub(crate) details: Option<String>,
    pub(crate) details_capitalization: StatusDetailsCapitalization,
    pub(crate) details_max_lines: usize,
}

impl StatusIndicatorContribution {
    pub(crate) fn new(
        header: String,
        details: Option<String>,
        details_capitalization: StatusDetailsCapitalization,
        details_max_lines: usize,
    ) -> Self {
        Self {
            header,
            details,
            details_capitalization,
            details_max_lines: details_max_lines.max(1),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BottomPaneSpacerPlacement {
    BetweenStatusAndInlinePreviews,
    BeforeComposerAfterStatusOrFooter,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct BottomPaneLayoutContext {
    pub(crate) has_status_or_footer: bool,
    pub(crate) has_inline_previews: bool,
}

#[cfg(any(not(debug_assertions), test))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UpdatePromptContribution {
    pub(crate) title: String,
    pub(crate) version_label: String,
    pub(crate) release_notes_label: String,
    pub(crate) update_now_label: String,
    pub(crate) skip_label: String,
    pub(crate) skip_until_next_version_label: String,
    pub(crate) continue_hint: String,
}

#[cfg(any(not(debug_assertions), test))]
impl UpdatePromptContribution {
    pub(crate) fn new(current_version: &str, latest_version: &str, update_command: &str) -> Self {
        Self {
            title: String::from("Update available!"),
            version_label: format!("{current_version} -> {latest_version}"),
            release_notes_label: String::from("Release notes: "),
            update_now_label: format!("Update now (runs `{update_command}`)"),
            skip_label: String::from("Skip"),
            skip_until_next_version_label: String::from("Skip until next version"),
            continue_hint: String::from("to continue"),
        }
    }
}

pub(crate) trait TuiContributor: Sync {
    fn status_indicator(&self, _status: &mut StatusIndicatorContribution) {}

    fn bottom_pane_spacer(
        &self,
        _placement: BottomPaneSpacerPlacement,
        _context: BottomPaneLayoutContext,
    ) -> bool {
        false
    }

    fn history_cell(&self, cell: Box<dyn HistoryCell>) -> Box<dyn HistoryCell> {
        cell
    }

    #[cfg(any(not(debug_assertions), test))]
    fn update_prompt(&self, _prompt: &mut UpdatePromptContribution) {}

    fn computer_use_surface_name(
        &self,
        _adapter: &str,
        _state: ComputerUseDisplayState,
    ) -> Option<Cow<'static, str>> {
        None
    }
}

pub(crate) fn contribute_status_indicator(
    mut status: StatusIndicatorContribution,
) -> StatusIndicatorContribution {
    for contributor in tui_contributors() {
        contributor.status_indicator(&mut status);
    }
    if status.details_max_lines == 0 {
        status.details_max_lines = STATUS_DETAILS_DEFAULT_MAX_LINES;
    }
    status
}

pub(crate) fn should_insert_bottom_pane_spacer(
    placement: BottomPaneSpacerPlacement,
    context: BottomPaneLayoutContext,
) -> bool {
    tui_contributors()
        .iter()
        .any(|contributor| contributor.bottom_pane_spacer(placement, context))
}

pub(crate) fn contribute_history_cell(mut cell: Box<dyn HistoryCell>) -> Box<dyn HistoryCell> {
    for contributor in tui_contributors() {
        cell = contributor.history_cell(cell);
    }
    cell
}

#[cfg(any(not(debug_assertions), test))]
pub(crate) fn contribute_update_prompt(
    mut prompt: UpdatePromptContribution,
) -> UpdatePromptContribution {
    for contributor in tui_contributors() {
        contributor.update_prompt(&mut prompt);
    }
    prompt
}

pub(crate) fn contribute_computer_use_surface_name(
    adapter: &str,
    state: ComputerUseDisplayState,
) -> Option<Cow<'static, str>> {
    tui_contributors()
        .iter()
        .find_map(|contributor| contributor.computer_use_surface_name(adapter, state))
}

fn tui_contributors() -> &'static [&'static dyn TuiContributor] {
    static DOWNSTREAM_TUI_CONTRIBUTOR: DownstreamTuiContributor = DownstreamTuiContributor;
    static CONTRIBUTORS: [&dyn TuiContributor; 1] = [&DOWNSTREAM_TUI_CONTRIBUTOR];
    &CONTRIBUTORS
}

struct DownstreamTuiContributor;

impl TuiContributor for DownstreamTuiContributor {
    fn bottom_pane_spacer(
        &self,
        placement: BottomPaneSpacerPlacement,
        context: BottomPaneLayoutContext,
    ) -> bool {
        match placement {
            BottomPaneSpacerPlacement::BetweenStatusAndInlinePreviews => {
                context.has_status_or_footer && context.has_inline_previews
            }
            BottomPaneSpacerPlacement::BeforeComposerAfterStatusOrFooter => {
                !context.has_inline_previews && context.has_status_or_footer
            }
        }
    }

    fn computer_use_surface_name(
        &self,
        adapter: &str,
        _state: ComputerUseDisplayState,
    ) -> Option<Cow<'static, str>> {
        match adapter {
            "android" | "android_emulator" | "android-emulator" => {
                Some(Cow::Borrowed("Android emulator"))
            }
            "browser" => Some(Cow::Borrowed("browser")),
            "desktop" | "computer" => Some(Cow::Borrowed("computer")),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::text::Line;

    use super::*;
    use crate::history_cell::PlainHistoryCell;

    #[test]
    fn downstream_contributor_registers_bottom_pane_spacer_policy() {
        let context = BottomPaneLayoutContext {
            has_status_or_footer: true,
            has_inline_previews: true,
        };
        assert!(should_insert_bottom_pane_spacer(
            BottomPaneSpacerPlacement::BetweenStatusAndInlinePreviews,
            context
        ));
        assert!(!should_insert_bottom_pane_spacer(
            BottomPaneSpacerPlacement::BeforeComposerAfterStatusOrFooter,
            context
        ));

        let footer_only_context = BottomPaneLayoutContext {
            has_status_or_footer: true,
            has_inline_previews: false,
        };
        assert!(should_insert_bottom_pane_spacer(
            BottomPaneSpacerPlacement::BeforeComposerAfterStatusOrFooter,
            footer_only_context
        ));
    }

    #[test]
    fn contributor_slots_preserve_status_history_and_update_prompt_defaults() {
        let status = contribute_status_indicator(StatusIndicatorContribution::new(
            "Working".to_string(),
            Some("checking".to_string()),
            StatusDetailsCapitalization::Preserve,
            /*details_max_lines*/ 2,
        ));
        assert_eq!(status.header, "Working");
        assert_eq!(status.details.as_deref(), Some("checking"));
        assert_eq!(
            status.details_capitalization,
            StatusDetailsCapitalization::Preserve
        );
        assert_eq!(status.details_max_lines, 2);

        let prompt = contribute_update_prompt(UpdatePromptContribution::new(
            "1.2.3",
            "2.0.0",
            "npm install -g @openai/codex",
        ));
        assert_eq!(prompt.title, "Update available!");
        assert_eq!(prompt.version_label, "1.2.3 -> 2.0.0");
        assert_eq!(
            prompt.update_now_label,
            "Update now (runs `npm install -g @openai/codex`)"
        );

        let cell = contribute_history_cell(Box::new(PlainHistoryCell::new(vec![Line::from(
            "visible history",
        )])));
        assert_eq!(cell.raw_lines(), vec![Line::from("visible history")]);
    }

    #[test]
    fn downstream_contributor_names_native_computer_use_surfaces() {
        assert_eq!(
            contribute_computer_use_surface_name("android", ComputerUseDisplayState::Completed),
            Some(Cow::Borrowed("Android emulator"))
        );
        assert_eq!(
            contribute_computer_use_surface_name("browser", ComputerUseDisplayState::InProgress),
            Some(Cow::Borrowed("browser"))
        );
        assert_eq!(
            contribute_computer_use_surface_name("desktop", ComputerUseDisplayState::Failed),
            Some(Cow::Borrowed("computer"))
        );
        assert_eq!(
            contribute_computer_use_surface_name("future_adapter", ComputerUseDisplayState::Failed),
            None
        );
    }
}
