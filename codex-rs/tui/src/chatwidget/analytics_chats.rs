//! Account-scoped chat usage selection and paging.

use super::*;
use crate::analytics_chats::AccountChats;
use crate::analytics_chats::Chats;
use crate::analytics_chats::ConsumerChats;
use crate::analytics_chats::LocalUsage;
use crate::chatwidget::usage::ANALYTICS_CHATS_VIEW_ID;

#[cfg(test)]
#[path = "analytics_chats_tests.rs"]
mod tests;

impl ChatWidget {
    pub(crate) fn show_analytics_chats_loading(&mut self) -> u64 {
        let request_id = self.take_next_rate_limit_reset_request_id();
        self.pending_analytics_chats_request_id = Some(request_id);
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(ANALYTICS_CHATS_VIEW_ID),
            title: Some("Analytics Chats".to_string()),
            subtitle: Some("Loading indexed chats…".to_string()),
            items: vec![SelectionItem {
                name: "Loading…".to_string(),
                is_disabled: true,
                ..Default::default()
            }],
            ..Default::default()
        });
        self.request_redraw();
        request_id
    }

    pub(crate) fn finish_analytics_chats(
        &mut self,
        request_id: u64,
        cursor: Option<String>,
        mut previous: Vec<Option<String>>,
        result: Result<AccountChats, String>,
    ) {
        if self.pending_analytics_chats_request_id != Some(request_id) {
            return;
        }
        self.pending_analytics_chats_request_id = None;
        let (mut items, next_cursor, subtitle) = match result {
            Ok(AccountChats::Workspace(page)) => workspace_rows(page),
            Ok(AccountChats::Consumer(page)) => consumer_rows(page),
            Err(error) => {
                let retry_cursor = cursor.clone();
                let retry_previous = previous.clone();
                (
                    vec![
                        SelectionItem {
                            name: error,
                            is_disabled: true,
                            ..Default::default()
                        },
                        SelectionItem {
                            name: "Retry".to_string(),
                            actions: vec![Box::new(move |tx| {
                                tx.send(AppEvent::OpenAnalyticsChats {
                                    cursor: retry_cursor.clone(),
                                    previous: retry_previous.clone(),
                                });
                            })],
                            dismiss_on_select: true,
                            ..Default::default()
                        },
                    ],
                    None,
                    "Unable to load chat usage.".to_string(),
                )
            }
        };
        if let Some(next_cursor) = next_cursor {
            let mut history = previous.clone();
            history.push(cursor);
            items.push(SelectionItem {
                name: "Older chats".to_string(),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::OpenAnalyticsChats {
                        cursor: Some(next_cursor.clone()),
                        previous: history.clone(),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        if let Some(prior_cursor) = previous.pop() {
            items.push(SelectionItem {
                name: "Newer chats".to_string(),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::OpenAnalyticsChats {
                        cursor: prior_cursor.clone(),
                        previous: previous.clone(),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        let params = SelectionViewParams {
            view_id: Some(ANALYTICS_CHATS_VIEW_ID),
            title: Some("Analytics Chats".to_string()),
            subtitle: Some(subtitle),
            items,
            ..Default::default()
        };
        if self
            .bottom_pane
            .replace_selection_view_if_present(ANALYTICS_CHATS_VIEW_ID, params)
        {
            self.request_redraw();
        }
    }
}

fn workspace_rows(page: Chats) -> (Vec<SelectionItem>, Option<String>, String) {
    let mut items = Vec::new();
    for chat in page.rows {
        let title = chat.display_title().to_string();
        let local_usage = chat.local_usage;
        if let Some(usage) = chat.usage {
            let mut estimated = format!(
                "Estimated credits: {}",
                format_micros(usage.estimated_usage_credits_micros)
            );
            if let Some(usd) = usage.estimated_usage_usd_micros {
                estimated.push_str(&format!(" · Estimated USD: {}", format_micros(usd)));
            }
            items.push(SelectionItem {
                name: title,
                description: Some(estimated),
                ..Default::default()
            });
            for group in usage.groups {
                let model = group.model.as_deref().unwrap_or("Model not reported");
                let effort = group
                    .reasoning_effort
                    .as_deref()
                    .unwrap_or("effort not reported");
                let speed = group.speed.as_deref().unwrap_or("speed not reported");
                items.push(SelectionItem {
                    name: format!("  {model} · {effort} · {speed}"),
                    description: Some(format!(
                        "Estimated credits: {}",
                        format_micros(group.estimated_usage_credits_micros)
                    )),
                    ..Default::default()
                });
            }
        } else {
            items.push(SelectionItem {
                name: title,
                description: Some("Estimate unavailable".to_string()),
                ..Default::default()
            });
        }
        append_local_usage(&mut items, local_usage);
    }
    if items.is_empty() {
        items.push(SelectionItem {
            name: "No chats in this page".to_string(),
            is_disabled: true,
            ..Default::default()
        });
    }
    let subtitle = match (page.partial_history, page.partial_estimates) {
        (true, true) => {
            "30d partial; page rank; estimates missing; local may be partial or overcount."
        }
        (true, false) => "30d index partial; page rank; local may be partial or overcount.",
        (false, true) => "Estimates unavailable; local may be partial or overcount.",
        (false, false) => "Page rank; local may be partial or overcount.",
    };
    (items, page.next_cursor, subtitle.to_string())
}

fn consumer_rows(page: ConsumerChats) -> (Vec<SelectionItem>, Option<String>, String) {
    let mut items = Vec::new();
    for chat in page.rows {
        let title = chat.display_title().to_string();
        let local_usage = chat.local_usage;
        if let Some(usage) = chat
            .usage
            .filter(|usage| usage.data_status != codex_backend_client::TaskUsageStatus::Unavailable)
        {
            let amounts = &usage.amounts;
            let weekly = amounts
                .weekly_limit_percent
                .map_or_else(|| "unavailable".into(), |value| format!("{value:.1}%"));
            let five_hour = amounts
                .five_hour_limit_percent
                .map_or_else(|| "unavailable".into(), |value| format!("{value:.1}%"));
            let balance = amounts
                .balance_usage_credits
                .as_ref()
                .map_or("unavailable", |value| value.as_str());
            items.push(SelectionItem {
                name: title,
                description: Some(format!(
                    "Weekly: {weekly} · 5-hour: {five_hour} · Balance credits: {balance}{}",
                    if usage.data_status == codex_backend_client::TaskUsageStatus::Partial {
                        " · partial"
                    } else {
                        ""
                    }
                )),
                ..Default::default()
            });
            for group in usage.groups {
                let model = group.model.as_deref().unwrap_or("Model not reported");
                let weekly = group
                    .amounts
                    .weekly_limit_percent
                    .map_or_else(|| "unavailable".into(), |value| format!("{value:.1}%"));
                let five_hour = group
                    .amounts
                    .five_hour_limit_percent
                    .map_or_else(|| "unavailable".into(), |value| format!("{value:.1}%"));
                let balance = group
                    .amounts
                    .balance_usage_credits
                    .as_ref()
                    .map_or("unavailable", |value| value.as_str());
                items.push(SelectionItem {
                    name: format!("  {model}"),
                    description: Some(format!(
                        "Weekly: {weekly} · 5-hour: {five_hour} · Balance credits: {balance}"
                    )),
                    ..Default::default()
                });
            }
        } else {
            items.push(SelectionItem {
                name: title,
                description: Some("Usage unavailable".to_string()),
                ..Default::default()
            });
        }
        append_local_usage(&mut items, local_usage);
    }
    if items.is_empty() {
        items.push(SelectionItem {
            name: "No chats in this page".to_string(),
            is_disabled: true,
            ..Default::default()
        });
    }
    let subtitle = match (page.partial_history, page.partial_usage) {
        (true, true) => "30d index partial; usage missing; local may be partial or overcount.",
        (true, false) => "30d index partial; local may be partial or overcount.",
        (false, true) => "Some task usage unavailable; local may be partial or overcount.",
        (false, false) => "Local usage may be partial or overcount.",
    };
    (items, page.next_cursor, subtitle.to_string())
}

fn append_local_usage(items: &mut Vec<SelectionItem>, usage: Option<LocalUsage>) {
    let Some(usage) = usage else {
        items.push(SelectionItem {
            name: "  Local root/subagent detail unavailable".to_string(),
            is_disabled: true,
            ..Default::default()
        });
        return;
    };
    for thread in usage.threads {
        let credits = thread
            .estimated_credits
            .filter(|value| value.is_finite())
            .map_or_else(|| "unavailable".to_string(), |value| format!("{value:.6}"));
        let model = thread.models.as_deref().unwrap_or("model unavailable");
        items.push(SelectionItem {
            name: format!("  Local {} · {model}", thread.kind),
            description: Some(format!(
                "Estimated thread credits (may overcount): {credits}{}",
                if thread.partial { " · partial" } else { "" }
            )),
            ..Default::default()
        });
    }
    if usage.truncated {
        items.push(SelectionItem {
            name: "  Additional local threads omitted".to_string(),
            is_disabled: true,
            ..Default::default()
        });
    }
}

fn format_micros(value: i64) -> String {
    let value = i128::from(value);
    let sign = if value < 0 { "-" } else { "" };
    let magnitude = value.abs();
    format!(
        "{sign}{}.{:06}",
        magnitude / 1_000_000,
        magnitude % 1_000_000
    )
}
