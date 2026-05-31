//! Computer-use history cells for browser, desktop, Android, and future
//! native visual adapters.

use super::*;
use crate::computer_use_display::ComputerUseDisplayState;
use crate::computer_use_display::computer_use_action_label;
use codex_app_server_protocol::ComputerUseCallOutputContentItem;
use codex_app_server_protocol::ComputerUseCallStatus;

#[derive(Debug, Clone)]
pub(crate) struct ComputerUseInvocation {
    pub(crate) adapter: String,
    pub(crate) tool: String,
    pub(crate) arguments: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub(crate) struct ComputerUseCallOutcome {
    pub(crate) status: ComputerUseCallStatus,
    pub(crate) content_items: Option<Vec<ComputerUseCallOutputContentItem>>,
    pub(crate) success: Option<bool>,
    pub(crate) error: Option<String>,
    pub(crate) duration: Option<Duration>,
}

#[derive(Debug)]
pub(crate) struct ComputerUseCallCell {
    call_id: String,
    invocation: ComputerUseInvocation,
    start_time: Instant,
    outcome: Option<ComputerUseCallOutcome>,
    animations_enabled: bool,
}

impl ComputerUseCallCell {
    pub(crate) fn new(
        call_id: String,
        invocation: ComputerUseInvocation,
        animations_enabled: bool,
    ) -> Self {
        Self {
            call_id,
            invocation,
            start_time: Instant::now(),
            outcome: None,
            animations_enabled,
        }
    }

    pub(crate) fn call_id(&self) -> &str {
        &self.call_id
    }

    pub(crate) fn complete(&mut self, outcome: ComputerUseCallOutcome) {
        self.outcome = Some(outcome);
    }

    fn display_state(&self) -> ComputerUseDisplayState {
        let Some(outcome) = self.outcome.as_ref() else {
            return ComputerUseDisplayState::InProgress;
        };
        match &outcome.status {
            ComputerUseCallStatus::InProgress => ComputerUseDisplayState::InProgress,
            ComputerUseCallStatus::Completed if outcome.success != Some(false) => {
                ComputerUseDisplayState::Completed
            }
            ComputerUseCallStatus::Completed | ComputerUseCallStatus::Failed => {
                ComputerUseDisplayState::Failed
            }
        }
    }

    fn detail_source(&self) -> String {
        let mut details = Vec::new();
        details.push(self.invocation.tool.clone());

        if let Some(args) = self.invocation.arguments.as_ref()
            && !args.is_null()
            && args != &serde_json::json!({})
        {
            details.push(format!("arguments: {args}"));
        }

        if let Some(outcome) = self.outcome.as_ref() {
            if let Some(duration) = outcome.duration {
                details.push(format!("duration: {}ms", duration.as_millis()));
            }
            if let Some(error) = outcome.error.as_ref()
                && !error.trim().is_empty()
            {
                details.push(format!("error: {error}"));
            }
            if let Some(content_items) = outcome.content_items.as_ref() {
                details.extend(content_items.iter().filter_map(content_item_preview));
            }
        }

        details.join("\n")
    }
}

impl HistoryCell for ComputerUseCallCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let state = self.display_state();
        let bullet = match state {
            ComputerUseDisplayState::Completed => "•".green().bold(),
            ComputerUseDisplayState::Failed => "•".red().bold(),
            ComputerUseDisplayState::InProgress => activity_indicator(
                Some(self.start_time),
                MotionMode::from_animations_enabled(self.animations_enabled),
                ReducedMotionIndicator::StaticBullet,
            )
            .unwrap_or_else(|| "•".dim()),
        };
        let mut lines: Vec<Line<'static>> = vec![
            vec![
                bullet,
                " ".into(),
                computer_use_action_label(&self.invocation.adapter, state)
                    .bold()
                    .into(),
            ]
            .into(),
        ];

        let detail_width = (width as usize).saturating_sub(4).max(1);
        let detail = format_and_truncate_tool_result(
            &self.detail_source(),
            TOOL_CALL_MAX_LINES,
            detail_width,
        );
        if detail.trim().is_empty() {
            return lines;
        }

        let mut detail_lines = Vec::new();
        for segment in detail.lines() {
            let line = Line::from(segment.to_string().dim());
            let wrapped = adaptive_wrap_line(
                &line,
                RtOptions::new(detail_width)
                    .initial_indent("".into())
                    .subsequent_indent("    ".into()),
            );
            detail_lines.extend(wrapped.iter().map(line_to_static));
        }
        lines.extend(prefix_lines(detail_lines, "  └ ".dim(), "    ".into()));
        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let state = self.display_state();
        let mut lines = vec![Line::from(computer_use_action_label(
            &self.invocation.adapter,
            state,
        ))];
        let detail = format_and_truncate_tool_result(
            &self.detail_source(),
            TOOL_CALL_MAX_LINES,
            RAW_TOOL_OUTPUT_WIDTH,
        );
        lines.extend(raw_lines_from_source(&detail));
        lines
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        if !self.animations_enabled || self.outcome.is_some() {
            return None;
        }
        Some((self.start_time.elapsed().as_millis() / 50) as u64)
    }
}

pub(crate) fn new_active_computer_use_call(
    call_id: String,
    invocation: ComputerUseInvocation,
    animations_enabled: bool,
) -> ComputerUseCallCell {
    ComputerUseCallCell::new(call_id, invocation, animations_enabled)
}

fn content_item_preview(item: &ComputerUseCallOutputContentItem) -> Option<String> {
    match item {
        ComputerUseCallOutputContentItem::InputText { text } if !text.trim().is_empty() => {
            Some(text.clone())
        }
        ComputerUseCallOutputContentItem::InputText { .. } => None,
        ComputerUseCallOutputContentItem::InputImage { .. } => {
            Some("<native screenshot>".to_string())
        }
    }
}
