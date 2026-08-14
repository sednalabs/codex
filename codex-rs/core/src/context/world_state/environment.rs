use super::PreviousSectionState;
use super::WorldStateSection;
use crate::context::ContextualUserFragment;
use crate::context::environment_context::FileSystemContext;
use crate::context::environment_context::NetworkContext;
use crate::context::environment_context::push_xml_escaped_text;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

pub(crate) const SUBAGENT_CONTEXT_MAX_ROWS: usize = 16;
pub(crate) const SUBAGENT_CONTEXT_MAX_RENDERED_BYTES: usize = 4 * 1024;
const SUBAGENT_CONTEXT_FIELD_SCAN_CHARS: usize = 1_024;
const SUBAGENT_REFERENCE_MAX_ESCAPED_BYTES: usize = 192;
const SUBAGENT_NICKNAME_MAX_ESCAPED_BYTES: usize = 96;
const SUBAGENT_CONTEXT_FIXED_RENDERED_BYTES: usize =
    "  <subagents>\n".len() + "  </subagents>\n".len();
const SUBAGENT_CONTEXT_LINE_OVERHEAD_BYTES: usize = 5;
const SUBAGENT_CONTEXT_OMITTED_LINE_MAX_BYTES: usize = SUBAGENT_CONTEXT_LINE_OVERHEAD_BYTES
    + "<omitted count=\"\" />".len()
    + std::mem::size_of::<usize>() * 3;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SubagentContext {
    rendered: String,
}

impl SubagentContext {
    #[cfg(test)]
    pub(crate) fn as_str(&self) -> &str {
        self.rendered.as_str()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SubagentContextRow {
    rendered: String,
}

impl SubagentContextRow {
    pub(crate) fn new(reference: &str, nickname: Option<&str>) -> Self {
        let reference = bounded_xml_text(reference, SUBAGENT_REFERENCE_MAX_ESCAPED_BYTES);
        let rendered = match nickname.filter(|nickname| !nickname.is_empty()) {
            Some(nickname) => format!(
                "- {reference}: {}",
                bounded_xml_text(nickname, SUBAGENT_NICKNAME_MAX_ESCAPED_BYTES)
            ),
            None => format!("- {reference}"),
        };
        Self { rendered }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SubagentContextBuilder {
    rows: Vec<SubagentContextRow>,
    rendered_bytes: usize,
    omitted_count: usize,
}

impl Default for SubagentContextBuilder {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            rendered_bytes: SUBAGENT_CONTEXT_FIXED_RENDERED_BYTES,
            omitted_count: 0,
        }
    }
}

impl SubagentContextBuilder {
    pub(crate) fn has_row_capacity(&self) -> bool {
        self.rows.len() < SUBAGENT_CONTEXT_MAX_ROWS
    }

    pub(crate) fn push(&mut self, row: SubagentContextRow) -> bool {
        if !self.has_row_capacity() {
            return false;
        }
        let row_bytes = SUBAGENT_CONTEXT_LINE_OVERHEAD_BYTES + row.rendered.len();
        if self
            .rendered_bytes
            .saturating_add(row_bytes)
            .saturating_add(SUBAGENT_CONTEXT_OMITTED_LINE_MAX_BYTES)
            > SUBAGENT_CONTEXT_MAX_RENDERED_BYTES
        {
            return false;
        }
        self.rendered_bytes += row_bytes;
        self.rows.push(row);
        true
    }

    pub(crate) fn note_omitted(&mut self, count: usize) {
        self.omitted_count = self.omitted_count.saturating_add(count);
    }

    pub(crate) fn finish(self) -> SubagentContext {
        if self.rows.is_empty() && self.omitted_count == 0 {
            return SubagentContext::default();
        }
        let mut rows = self
            .rows
            .into_iter()
            .map(|row| row.rendered)
            .collect::<Vec<_>>();
        if self.omitted_count > 0 {
            rows.push(format!("<omitted count=\"{}\" />", self.omitted_count));
        }
        let rendered = rows.join("\n");
        debug_assert!(
            subagent_context_rendered_bytes(rendered.as_str())
                <= SUBAGENT_CONTEXT_MAX_RENDERED_BYTES
        );
        SubagentContext { rendered }
    }
}

fn subagent_context_rendered_bytes(rendered: &str) -> usize {
    SUBAGENT_CONTEXT_FIXED_RENDERED_BYTES
        + rendered
            .lines()
            .map(|line| SUBAGENT_CONTEXT_LINE_OVERHEAD_BYTES + line.len())
            .sum::<usize>()
}

fn bounded_xml_text(value: &str, max_escaped_bytes: usize) -> String {
    const ELLIPSIS: &str = "...";
    let content_limit = max_escaped_bytes.saturating_sub(ELLIPSIS.len());
    let mut rendered = String::new();
    let mut pending_space = false;
    let mut truncated = false;
    let mut chars = value.chars();

    for _ in 0..SUBAGENT_CONTEXT_FIELD_SCAN_CHARS {
        let Some(ch) = chars.next() else {
            break;
        };
        let ch = if is_xml_1_0_char(ch) { ch } else { '\u{FFFD}' };
        if ch.is_whitespace() {
            pending_space = !rendered.is_empty();
            continue;
        }
        let escaped = match ch {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&apos;".to_string(),
            _ => ch.to_string(),
        };
        let separator_bytes = usize::from(pending_space);
        if rendered
            .len()
            .saturating_add(separator_bytes)
            .saturating_add(escaped.len())
            > content_limit
        {
            truncated = true;
            break;
        }
        if pending_space {
            rendered.push(' ');
        }
        rendered.push_str(&escaped);
        pending_space = false;
    }
    if !truncated && chars.next().is_some() {
        truncated = true;
    }
    if truncated {
        rendered.push_str(ELLIPSIS);
    }
    rendered
}

fn is_xml_1_0_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{9}'
            | '\u{A}'
            | '\u{D}'
            | '\u{20}'..='\u{D7FF}'
            | '\u{E000}'..='\u{FFFD}'
            | '\u{10000}'..='\u{10FFFF}'
    )
}

/// Environment values visible to the model.
#[derive(Clone, Debug, Default)]
pub(crate) struct EnvironmentsState {
    environments: BTreeMap<String, EnvironmentState>,
    current_date: Option<String>,
    timezone: Option<String>,
    network: Option<NetworkContext>,
    filesystem: Option<FileSystemContext>,
    subagents: Option<String>,
}

impl EnvironmentsState {
    pub(crate) fn from_turn_context_with_environments(
        turn_context: &TurnContext,
        environments: &TurnEnvironmentSnapshot,
    ) -> Self {
        let workspace_roots = environments
            .primary()
            .map(TurnEnvironment::workspace_roots)
            .unwrap_or_default();
        Self {
            environments: environment_states(environments),
            current_date: turn_context.current_date.clone(),
            timezone: turn_context.timezone.clone(),
            network: network_from_turn_context(turn_context),
            filesystem: Some(FileSystemContext::from_permission_profile(
                turn_context.config.permissions.permission_profile(),
                workspace_roots,
            )),
            subagents: None,
        }
    }

    pub(crate) fn with_subagents(mut self, subagents: SubagentContext) -> Self {
        if !subagents.rendered.is_empty() {
            self.subagents = Some(subagents.rendered);
        }
        self
    }

    fn rendered_full(&self) -> RenderedEnvironments {
        RenderedEnvironments {
            updates: self
                .environments
                .iter()
                .map(|(id, environment)| {
                    (id.clone(), EnvironmentUpdate::Current(environment.clone()))
                })
                .collect(),
            legacy_single: is_legacy_single(&self.environments),
            current_date: self.current_date.clone(),
            timezone: self.timezone.clone(),
            network: self.network.clone(),
            filesystem: self.filesystem.clone(),
            subagents: self.subagents.clone(),
        }
    }
}

impl WorldStateSection for EnvironmentsState {
    const ID: &'static str = "environments";
    type Snapshot = EnvironmentsSnapshot;

    fn snapshot(&self) -> Self::Snapshot {
        EnvironmentsSnapshot {
            environments: self
                .environments
                .iter()
                .map(|(id, environment)| {
                    (
                        id.clone(),
                        EnvironmentSnapshot {
                            cwd: environment.cwd.inferred_native_path_string(),
                            status: environment.status,
                            shell: environment.shell.clone(),
                        },
                    )
                })
                .collect(),
            current_date: self.current_date.clone(),
            timezone: self.timezone.clone(),
            network: self.network.as_ref().map(NetworkContext::render),
            filesystem: self.filesystem.as_ref().map(FileSystemContext::render),
            subagents: self.subagents.clone(),
        }
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        let current = self.snapshot();
        let empty = EnvironmentsSnapshot::default();
        let previous = match previous {
            PreviousSectionState::Known(previous) => previous,
            PreviousSectionState::Absent | PreviousSectionState::Unknown => &empty,
        };
        let turn_context_values_changed = current.current_date != previous.current_date
            || current.timezone != previous.timezone
            || current.network != previous.network
            || current.filesystem != previous.filesystem;
        let mut updates = self
            .environments
            .iter()
            .filter(|(id, _)| {
                let environment = &current.environments[*id];
                previous
                    .environments
                    .get(*id)
                    .is_none_or(|previous| !environment.has_same_diff_value(previous))
            })
            .map(|(id, environment)| (id.clone(), EnvironmentUpdate::Current(environment.clone())))
            .collect::<BTreeMap<_, _>>();
        updates.extend(
            previous
                .environments
                .keys()
                .filter(|id| !self.environments.contains_key(*id))
                .map(|id| (id.clone(), EnvironmentUpdate::Unavailable)),
        );
        let legacy_single = is_legacy_single(&self.environments)
            && updates
                .values()
                .all(|update| matches!(update, EnvironmentUpdate::Current(_)));
        (!updates.is_empty() || turn_context_values_changed).then(|| {
            Box::new(RenderedEnvironments {
                updates,
                legacy_single,
                current_date: self.current_date.clone(),
                timezone: self.timezone.clone(),
                network: self.network.clone(),
                filesystem: self.filesystem.clone(),
                subagents: self.subagents.clone(),
            }) as Box<dyn ContextualUserFragment>
        })
    }
}

impl ContextualUserFragment for EnvironmentsState {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        environment_context_markers()
    }

    fn body(&self) -> String {
        self.rendered_full().body()
    }
}

struct RenderedEnvironments {
    updates: BTreeMap<String, EnvironmentUpdate>,
    legacy_single: bool,
    current_date: Option<String>,
    timezone: Option<String>,
    network: Option<NetworkContext>,
    filesystem: Option<FileSystemContext>,
    subagents: Option<String>,
}

enum EnvironmentUpdate {
    Current(EnvironmentState),
    Unavailable,
}

impl ContextualUserFragment for RenderedEnvironments {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        environment_context_markers()
    }

    fn body(&self) -> String {
        let mut rendered = "\n".to_string();
        if self.legacy_single {
            if let Some(EnvironmentUpdate::Current(environment)) = self.updates.values().next() {
                push_environment_values(&mut rendered, environment, "  ");
            }
        } else if !self.updates.is_empty() {
            rendered.push_str("  <environments>\n");
            for (id, update) in &self.updates {
                match update {
                    EnvironmentUpdate::Current(environment) => {
                        rendered.push_str("    <environment id=\"");
                        push_xml_escaped_text(&mut rendered, id);
                        rendered.push('"');
                        rendered.push_str(">\n");
                        push_environment_values(&mut rendered, environment, "      ");
                        rendered.push_str("    </environment>\n");
                    }
                    EnvironmentUpdate::Unavailable => {
                        rendered.push_str("    <environment id=\"");
                        push_xml_escaped_text(&mut rendered, id);
                        rendered.push_str("\" status=\"unavailable\" />\n");
                    }
                }
            }
            rendered.push_str("  </environments>\n");
        }
        push_optional_element(&mut rendered, "current_date", self.current_date.as_deref());
        push_optional_element(&mut rendered, "timezone", self.timezone.as_deref());
        if let Some(network) = &self.network {
            rendered.push_str("  ");
            rendered.push_str(&network.render());
            rendered.push('\n');
        }
        if let Some(filesystem) = &self.filesystem {
            rendered.push_str("  ");
            rendered.push_str(&filesystem.render());
            rendered.push('\n');
        }
        if let Some(subagents) = &self.subagents {
            rendered.push_str("  <subagents>\n");
            for line in subagents.lines() {
                rendered.push_str("    ");
                rendered.push_str(line);
                rendered.push('\n');
            }
            rendered.push_str("  </subagents>\n");
        }
        rendered
    }
}

fn push_environment_values(rendered: &mut String, environment: &EnvironmentState, indent: &str) {
    rendered.push_str(indent);
    rendered.push_str("<cwd>");
    push_xml_escaped_text(rendered, &environment.cwd.inferred_native_path_string());
    rendered.push_str("</cwd>\n");
    if environment.status == EnvironmentStatus::Starting {
        rendered.push_str(indent);
        rendered.push_str("<status>starting</status>\n");
    }
    if let Some(shell) = &environment.shell {
        rendered.push_str(indent);
        rendered.push_str("<shell>");
        push_xml_escaped_text(rendered, shell);
        rendered.push_str("</shell>\n");
    }
}

fn push_optional_element(rendered: &mut String, name: &str, value: Option<&str>) {
    let Some(value) = value else {
        return;
    };
    rendered.push_str("  <");
    rendered.push_str(name);
    rendered.push('>');
    push_xml_escaped_text(rendered, value);
    rendered.push_str("</");
    rendered.push_str(name);
    rendered.push_str(">\n");
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EnvironmentState {
    cwd: PathUri,
    status: EnvironmentStatus,
    shell: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct EnvironmentsSnapshot {
    environments: BTreeMap<String, EnvironmentSnapshot>,
    current_date: Option<String>,
    timezone: Option<String>,
    network: Option<String>,
    filesystem: Option<String>,
    subagents: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct EnvironmentSnapshot {
    cwd: String,
    status: EnvironmentStatus,
    shell: Option<String>,
}

impl EnvironmentSnapshot {
    fn has_same_diff_value(&self, other: &Self) -> bool {
        self.cwd == other.cwd
            && self.status == other.status
            && self
                .shell
                .as_ref()
                .zip(other.shell.as_ref())
                .is_none_or(|(current, previous)| current == previous)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EnvironmentStatus {
    Starting,
    Available,
}

fn environment_states(snapshot: &TurnEnvironmentSnapshot) -> BTreeMap<String, EnvironmentState> {
    let mut environments = snapshot
        .turn_environments()
        .map(|environment| {
            (
                environment.environment_id.clone(),
                EnvironmentState {
                    cwd: environment.cwd().clone(),
                    status: EnvironmentStatus::Available,
                    shell: environment
                        .shell
                        .as_ref()
                        .map(|shell| shell.name().to_string()),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    for environment in snapshot.starting() {
        environments
            .entry(environment.selection.environment_id.clone())
            .or_insert_with(|| EnvironmentState {
                cwd: environment.selection.cwd.clone(),
                status: EnvironmentStatus::Starting,
                shell: None,
            });
    }
    environments
}

fn is_legacy_single(environments: &BTreeMap<String, EnvironmentState>) -> bool {
    environments.len() == 1
        && environments
            .values()
            .all(|environment| environment.status == EnvironmentStatus::Available)
}

fn environment_context_markers() -> (&'static str, &'static str) {
    (
        codex_protocol::protocol::ENVIRONMENT_CONTEXT_OPEN_TAG,
        codex_protocol::protocol::ENVIRONMENT_CONTEXT_CLOSE_TAG,
    )
}

fn network_from_turn_context(turn_context: &TurnContext) -> Option<NetworkContext> {
    let network = turn_context
        .config
        .config_layer_stack
        .requirements()
        .network
        .as_ref()?;

    Some(NetworkContext::new(
        network
            .domains
            .as_ref()
            .and_then(codex_config::NetworkDomainPermissionsToml::allowed_domains)
            .unwrap_or_default(),
        network
            .domains
            .as_ref()
            .and_then(codex_config::NetworkDomainPermissionsToml::denied_domains)
            .unwrap_or_default(),
    ))
}

#[cfg(test)]
#[path = "environment_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "environment_render_tests.rs"]
mod render_tests;
