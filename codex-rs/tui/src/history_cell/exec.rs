//! Background terminal interaction and process-summary history cells.

use super::*;
use codex_app_server_protocol::TerminalWaitInfo;
use codex_app_server_protocol::TerminalWaitPrimitive;

#[derive(Debug)]
pub(crate) struct UnifiedExecInteractionCell {
    command_display: Option<String>,
    stdin: String,
}

impl UnifiedExecInteractionCell {
    pub(crate) fn new(command_display: Option<String>, stdin: String) -> Self {
        Self {
            command_display,
            stdin,
        }
    }
}

impl HistoryCell for UnifiedExecInteractionCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }
        let wrap_width = width as usize;
        let waited_only = self.stdin.is_empty();

        let mut header_spans = if waited_only {
            vec!["• Waited for background terminal".bold()]
        } else {
            vec!["↳ ".dim(), "Interacted with background terminal".bold()]
        };
        if let Some(command) = &self.command_display
            && !command.is_empty()
        {
            header_spans.push(" · ".dim());
            header_spans.push(command.clone().dim());
        }
        let header = Line::from(header_spans);

        let mut out: Vec<Line<'static>> = Vec::new();
        let header_wrapped = adaptive_wrap_line(&header, RtOptions::new(wrap_width));
        push_owned_lines(&header_wrapped, &mut out);

        if waited_only {
            return out;
        }

        let input_lines: Vec<Line<'static>> = self
            .stdin
            .lines()
            .map(|line| Line::from(line.to_string()))
            .collect();

        let input_wrapped = adaptive_wrap_lines(
            input_lines,
            RtOptions::new(wrap_width)
                .initial_indent(Line::from("  └ ".dim()))
                .subsequent_indent(Line::from("    ".dim())),
        );
        out.extend(input_wrapped);
        out
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if self.stdin.is_empty() {
            if let Some(command) = self
                .command_display
                .as_ref()
                .filter(|command| !command.is_empty())
            {
                out.push(Line::from(format!(
                    "Waited for background terminal: {command}"
                )));
            } else {
                out.push(Line::from("Waited for background terminal"));
            }
            return out;
        }

        if let Some(command) = self
            .command_display
            .as_ref()
            .filter(|command| !command.is_empty())
        {
            out.push(Line::from(format!(
                "Interacted with background terminal: {command}"
            )));
        } else {
            out.push(Line::from("Interacted with background terminal"));
        }
        out.extend(raw_lines_from_source(&self.stdin));
        out
    }
}

pub(crate) fn new_unified_exec_interaction(
    command_display: Option<String>,
    stdin: String,
) -> UnifiedExecInteractionCell {
    UnifiedExecInteractionCell::new(command_display, stdin)
}

#[derive(Debug)]
pub(crate) struct WaitPrimitiveCell {
    kind: WaitPrimitiveKind,
    detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WaitPrimitiveKind {
    Terminal {
        primitive: TerminalWaitPrimitive,
        max_wait_ms: Option<u64>,
        heartbeat_interval_ms: Option<u64>,
    },
    #[cfg_attr(debug_assertions, allow(dead_code))]
    BackgroundTerminal,
}

impl WaitPrimitiveKind {
    fn label(&self) -> &'static str {
        match self {
            WaitPrimitiveKind::Terminal { primitive, .. } => {
                terminal_wait_primitive_label(primitive)
            }
            WaitPrimitiveKind::BackgroundTerminal => "background terminal",
        }
    }

    fn wait_details(&self) -> Option<String> {
        match self {
            WaitPrimitiveKind::Terminal {
                max_wait_ms,
                heartbeat_interval_ms,
                ..
            } => terminal_wait_detail(*max_wait_ms, *heartbeat_interval_ms),
            WaitPrimitiveKind::BackgroundTerminal => None,
        }
    }
}

impl WaitPrimitiveCell {
    pub(crate) fn terminal(
        terminal_wait: TerminalWaitInfo,
        command_display: Option<String>,
    ) -> Self {
        Self {
            kind: WaitPrimitiveKind::Terminal {
                primitive: terminal_wait.primitive,
                max_wait_ms: terminal_wait.max_wait_ms,
                heartbeat_interval_ms: terminal_wait.heartbeat_interval_ms,
            },
            detail: command_display.filter(|command| !command.is_empty()),
        }
    }

    #[cfg_attr(debug_assertions, allow(dead_code))]
    pub(crate) fn background_terminal(command_display: Option<String>) -> Self {
        Self {
            kind: WaitPrimitiveKind::BackgroundTerminal,
            detail: command_display.filter(|display| !display.is_empty()),
        }
    }

    pub(crate) fn update_detail(&mut self, command_display: Option<String>) {
        if self.detail.is_some() {
            return;
        }
        self.detail = command_display.filter(|display| !display.is_empty());
    }

    pub(crate) fn is_waiting_cell(&self) -> bool {
        true
    }
}

impl HistoryCell for WaitPrimitiveCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }
        let mut spans = vec![
            "• Waiting".bold(),
            " · primitive: ".dim(),
            self.kind.label().to_string().cyan(),
        ];
        if let Some(detail) = &self.detail {
            spans.push(" · ".dim());
            spans.push(detail.clone().dim());
        }
        let header = Line::from(spans);
        let mut out = Vec::new();
        let wrapped = adaptive_wrap_line(&header, RtOptions::new(width as usize));
        push_owned_lines(&wrapped, &mut out);
        if let Some(wait_details) = self.kind.wait_details() {
            let wait_details = Line::from(wait_details.dim());
            let wrapped = adaptive_wrap_line(
                &wait_details,
                RtOptions::new(width as usize)
                    .initial_indent(Line::from("  └ ".dim()))
                    .subsequent_indent(Line::from("    ".dim())),
            );
            push_owned_lines(&wrapped, &mut out);
        }
        out
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut text = format!("Waiting · primitive: {}", self.kind.label());
        if let Some(detail) = &self.detail {
            text.push_str(" · ");
            text.push_str(detail);
        }
        if let Some(wait_details) = self.kind.wait_details() {
            text.push_str(" · ");
            text.push_str(&wait_details);
        }
        vec![Line::from(text)]
    }
}

pub(crate) fn new_terminal_wait_primitive(
    terminal_wait: TerminalWaitInfo,
    command_display: Option<String>,
) -> WaitPrimitiveCell {
    WaitPrimitiveCell::terminal(terminal_wait, command_display)
}

#[cfg_attr(debug_assertions, allow(dead_code))]
pub(crate) fn new_background_terminal_wait(command_display: Option<String>) -> WaitPrimitiveCell {
    WaitPrimitiveCell::background_terminal(command_display)
}

pub(crate) fn terminal_wait_primitive_label(primitive: &TerminalWaitPrimitive) -> &'static str {
    match primitive {
        TerminalWaitPrimitive::ExecCommandWaitUntilTerminal => {
            "exec_command(wait_until_terminal=true)"
        }
        TerminalWaitPrimitive::WriteStdinWaitUntilTerminal => {
            "write_stdin(wait_until_terminal=true)"
        }
        TerminalWaitPrimitive::WriteStdinEmptyPoll => "write_stdin(empty stdin poll)",
    }
}

fn terminal_wait_detail(
    max_wait_ms: Option<u64>,
    heartbeat_interval_ms: Option<u64>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(max_wait_ms) = max_wait_ms {
        parts.push(format!("max_wait_ms={max_wait_ms}"));
    }
    if let Some(heartbeat_interval_ms) = heartbeat_interval_ms {
        parts.push(format!("heartbeat_interval_ms={heartbeat_interval_ms}"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[derive(Debug)]
struct UnifiedExecProcessesCell {
    processes: Vec<UnifiedExecProcessDetails>,
}

impl UnifiedExecProcessesCell {
    fn new(processes: Vec<UnifiedExecProcessDetails>) -> Self {
        Self { processes }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct UnifiedExecProcessDetails {
    pub(crate) command_display: String,
    pub(crate) recent_chunks: Vec<String>,
}

impl HistoryCell for UnifiedExecProcessesCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }

        let wrap_width = width as usize;
        let max_processes = 16usize;
        let mut out: Vec<Line<'static>> = Vec::new();
        out.push(vec!["Background terminals".bold()].into());
        out.push("".into());

        if self.processes.is_empty() {
            out.push("  • No background terminals running.".italic().into());
            return out;
        }

        let prefix = "  • ";
        let prefix_width = UnicodeWidthStr::width(prefix);
        let truncation_suffix = " [...]";
        let truncation_suffix_width = UnicodeWidthStr::width(truncation_suffix);
        let mut shown = 0usize;
        for process in &self.processes {
            if shown >= max_processes {
                break;
            }
            let command = &process.command_display;
            let (snippet, snippet_truncated) = {
                let (first_line, has_more_lines) = match command.split_once('\n') {
                    Some((first, _)) => (first, true),
                    None => (command.as_str(), false),
                };
                let max_graphemes = 80;
                let mut graphemes = first_line.grapheme_indices(true);
                if let Some((byte_index, _)) = graphemes.nth(max_graphemes) {
                    (first_line[..byte_index].to_string(), true)
                } else {
                    (first_line.to_string(), has_more_lines)
                }
            };
            if wrap_width <= prefix_width {
                out.push(Line::from(prefix.dim()));
                shown += 1;
                continue;
            }
            let budget = wrap_width.saturating_sub(prefix_width);
            let mut needs_suffix = snippet_truncated;
            if !needs_suffix {
                let (_, remainder, _) = take_prefix_by_width(&snippet, budget);
                if !remainder.is_empty() {
                    needs_suffix = true;
                }
            }
            if needs_suffix && budget > truncation_suffix_width {
                let available = budget.saturating_sub(truncation_suffix_width);
                let (truncated, _, _) = take_prefix_by_width(&snippet, available);
                out.push(vec![prefix.dim(), truncated.cyan(), truncation_suffix.dim()].into());
            } else {
                let (truncated, _, _) = take_prefix_by_width(&snippet, budget);
                out.push(vec![prefix.dim(), truncated.cyan()].into());
            }

            let chunk_prefix_first = "    ↳ ";
            let chunk_prefix_next = "      ";
            for (idx, chunk) in process.recent_chunks.iter().enumerate() {
                let chunk_prefix = if idx == 0 {
                    chunk_prefix_first
                } else {
                    chunk_prefix_next
                };
                let chunk_prefix_width = UnicodeWidthStr::width(chunk_prefix);
                if wrap_width <= chunk_prefix_width {
                    out.push(Line::from(chunk_prefix.dim()));
                    continue;
                }
                let budget = wrap_width.saturating_sub(chunk_prefix_width);
                let (truncated, remainder, _) = take_prefix_by_width(chunk, budget);
                if !remainder.is_empty() && budget > truncation_suffix_width {
                    let available = budget.saturating_sub(truncation_suffix_width);
                    let (shorter, _, _) = take_prefix_by_width(chunk, available);
                    out.push(
                        vec![chunk_prefix.dim(), shorter.dim(), truncation_suffix.dim()].into(),
                    );
                } else {
                    out.push(vec![chunk_prefix.dim(), truncated.dim()].into());
                }
            }
            shown += 1;
        }

        let remaining = self.processes.len().saturating_sub(shown);
        if remaining > 0 {
            let more_text = format!("... and {remaining} more running");
            if wrap_width <= prefix_width {
                out.push(Line::from(prefix.dim()));
            } else {
                let budget = wrap_width.saturating_sub(prefix_width);
                let (truncated, _, _) = take_prefix_by_width(&more_text, budget);
                out.push(vec![prefix.dim(), truncated.dim()].into());
            }
        }

        out
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(self.display_lines(u16::MAX))
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.display_lines(width).len() as u16
    }
}

pub(crate) fn new_unified_exec_processes_output(
    processes: Vec<UnifiedExecProcessDetails>,
) -> CompositeHistoryCell {
    let command = PlainHistoryCell::new(vec!["/ps".magenta().into()]);
    let summary = UnifiedExecProcessesCell::new(processes);
    CompositeHistoryCell::new(vec![Box::new(command), Box::new(summary)])
}
