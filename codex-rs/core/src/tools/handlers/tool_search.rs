use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::ToolSearchOutput;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::tool_search_spec::create_tool_search_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use bm25::Document;
use bm25::Language;
use bm25::SearchEngine;
use bm25::SearchEngineBuilder;
use codex_tools::LoadableToolSpec;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::TOOL_SEARCH_DEFAULT_LIMIT;
use codex_tools::TOOL_SEARCH_TOOL_NAME;
use codex_tools::ToolName;
use codex_tools::ToolSearchEntry;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSearchSourceInfo;
use codex_tools::ToolSpec;
use codex_tools::coalesce_loadable_tool_specs;

pub struct ToolSearchHandler {
    entries: Vec<ToolSearchEntry>,
    search_source_infos: Vec<ToolSearchSourceInfo>,
    search_engine: SearchEngine<usize>,
}

impl ToolSearchHandler {
    pub(crate) fn new(search_infos: Vec<ToolSearchInfo>) -> Self {
        let mut entries = Vec::with_capacity(search_infos.len());
        let mut search_source_infos = Vec::new();
        for search_info in search_infos {
            entries.push(search_info.entry);
            if let Some(source_info) = search_info.source_info {
                search_source_infos.push(source_info);
            }
        }
        let documents: Vec<Document<usize>> = entries
            .iter()
            .map(|entry| entry.search_text.clone())
            .enumerate()
            .map(|(idx, search_text)| Document::new(idx, search_text))
            .collect();
        let search_engine =
            SearchEngineBuilder::<usize>::with_documents(Language::English, documents).build();

        Self {
            entries,
            search_source_infos,
            search_engine,
        }
    }
}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for ToolSearchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_SEARCH_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_tool_search_tool(&self.search_source_infos, TOOL_SEARCH_DEFAULT_LIMIT)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation { payload, .. } = invocation;

        let args = match payload {
            ToolPayload::ToolSearch { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::Fatal(format!(
                    "{TOOL_SEARCH_TOOL_NAME} handler received unsupported payload"
                )));
            }
        };

        let query = args.query.trim();
        if query.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "query must not be empty".to_string(),
            ));
        }
        let limit = args.limit.unwrap_or(TOOL_SEARCH_DEFAULT_LIMIT);

        if limit == 0 {
            return Err(FunctionCallError::RespondToModel(
                "limit must be greater than zero".to_string(),
            ));
        }

        if self.entries.is_empty() {
            return Ok(boxed_tool_output(ToolSearchOutput { tools: Vec::new() }));
        }

        let tools = self.search(query, limit)?;

        Ok(boxed_tool_output(ToolSearchOutput { tools }))
    }
}

impl CoreToolRuntime for ToolSearchHandler {}

impl ToolSearchHandler {
    fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<LoadableToolSpec>, FunctionCallError> {
        let exact_result_ids = self.exact_identifier_match_ids(query, limit);
        if exact_result_ids.len() >= limit {
            let results = exact_result_ids
                .iter()
                .filter_map(|id| self.entries.get(*id));
            return self.search_output_tools(results);
        }

        if exact_result_ids.is_empty() {
            let results = self
                .search_engine
                .search(query, limit)
                .into_iter()
                .map(|result| result.document.id)
                .filter_map(|id| self.entries.get(id));
            return self.search_output_tools(results);
        }

        let exact_result_id_set = exact_result_ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        let results = exact_result_ids
            .iter()
            .filter_map(|id| self.entries.get(*id))
            .chain(
                self.search_engine
                    .search(query, limit)
                    .into_iter()
                    .map(|result| result.document.id)
                    .filter(|id| !exact_result_id_set.contains(id))
                    .filter_map(|id| self.entries.get(id)),
            );
        self.search_output_tools(results.take(limit))
    }

    fn exact_identifier_match_ids(&self, query: &str, limit: usize) -> Vec<usize> {
        let terms = exact_identifier_terms(query);
        if terms.is_empty() || limit == 0 {
            return Vec::new();
        }

        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry_matches_exact_identifier(entry, &terms))
            .map(|(idx, _)| idx)
            .take(limit)
            .collect()
    }

    fn search_output_tools<'a>(
        &self,
        results: impl IntoIterator<Item = &'a ToolSearchEntry>,
    ) -> Result<Vec<LoadableToolSpec>, FunctionCallError> {
        Ok(coalesce_loadable_tool_specs(
            results.into_iter().map(|entry| entry.output.clone()),
        ))
    }
}

fn exact_identifier_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|term| {
            term.trim_matches(|ch: char| {
                !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == ':' || ch == '/')
            })
        })
        .filter(|term| is_exact_identifier_term(term))
        .map(str::to_ascii_lowercase)
        .collect()
}

fn is_exact_identifier_term(term: &str) -> bool {
    term.len() >= 3
        && (term.chars().any(|ch| matches!(ch, '_' | ':' | '/')) || term.matches('-').count() >= 2)
}

fn entry_matches_exact_identifier(entry: &ToolSearchEntry, terms: &[String]) -> bool {
    match &entry.output {
        LoadableToolSpec::Function(tool) => terms
            .iter()
            .any(|term| tool.name.eq_ignore_ascii_case(term)),
        LoadableToolSpec::Namespace(namespace) => namespace.tools.iter().any(|tool| {
            let ResponsesApiNamespaceTool::Function(tool) = tool;
            terms
                .iter()
                .any(|term| matches_namespaced_tool_identifier(term, &namespace.name, &tool.name))
        }),
    }
}

fn matches_namespaced_tool_identifier(term: &str, namespace: &str, tool_name: &str) -> bool {
    if tool_name.eq_ignore_ascii_case(term) {
        return true;
    }

    let namespace_prefix = namespace.trim_end_matches('_');
    if namespace_prefix.is_empty() {
        return false;
    }

    let namespace_len = namespace_prefix.len();
    let delimiter_len = "__".len();
    if term.len() <= namespace_len + delimiter_len {
        return false;
    }

    if !term.is_char_boundary(namespace_len)
        || !term.is_char_boundary(namespace_len + delimiter_len)
    {
        return false;
    }

    if !term[..namespace_len].eq_ignore_ascii_case(namespace_prefix) {
        return false;
    }

    if !term[namespace_len..].starts_with("__") {
        return false;
    }

    term[namespace_len + delimiter_len..].eq_ignore_ascii_case(tool_name.trim_start_matches('_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::handlers::DynamicToolHandler;
    use crate::tools::handlers::McpHandler;
    use codex_mcp::ToolInfo;
    use codex_protocol::dynamic_tools::DynamicToolSpec;
    use codex_tools::ResponsesApiNamespace;
    use codex_tools::ResponsesApiNamespaceTool;
    use codex_tools::ResponsesApiTool;
    use pretty_assertions::assert_eq;
    use rmcp::model::Tool;
    use std::sync::Arc;

    #[test]
    fn mixed_search_results_coalesce_mcp_namespaces() {
        let dynamic_tools = [DynamicToolSpec {
            namespace: Some("codex_app".to_string()),
            name: "automation_update".to_string(),
            description: "Create, update, view, or delete recurring automations.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "mode": { "type": "string" },
                },
                "required": ["mode"],
                "additionalProperties": false,
            }),
            defer_loading: true,
            persist_on_resume: true,
            capability: None,
        }];
        let mcp_tools = [
            tool_info("calendar", "create_event", "Create events"),
            tool_info("calendar", "list_events", "List events"),
        ];
        let mut search_infos = mcp_tools
            .iter()
            .map(|tool| {
                McpHandler::new(tool.clone())
                    .expect("MCP tool should convert")
                    .search_info()
                    .expect("MCP handler should return search info")
            })
            .collect::<Vec<_>>();
        search_infos.extend(dynamic_tools.iter().map(|tool| {
            DynamicToolHandler::new(tool)
                .expect("dynamic tool should convert")
                .search_info()
                .expect("dynamic handler should return search info")
        }));
        let handler = ToolSearchHandler::new(search_infos);
        let results = [
            &handler.entries[0],
            &handler.entries[2],
            &handler.entries[1],
        ];

        let tools = handler
            .search_output_tools(results)
            .expect("mixed search output should serialize");

        assert_eq!(
            tools,
            vec![
                LoadableToolSpec::Namespace(ResponsesApiNamespace {
                    name: "mcp__calendar".to_string(),
                    description: "Tools in the mcp__calendar namespace.".to_string(),
                    tools: vec![
                        ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                            name: "create_event".to_string(),
                            description: "Create events desktop tool".to_string(),
                            strict: false,
                            defer_loading: Some(true),
                            parameters: codex_tools::JsonSchema::object(
                                Default::default(),
                                /*required*/ None,
                                Some(false.into()),
                            ),
                            output_schema: None,
                        }),
                        ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                            name: "list_events".to_string(),
                            description: "List events desktop tool".to_string(),
                            strict: false,
                            defer_loading: Some(true),
                            parameters: codex_tools::JsonSchema::object(
                                Default::default(),
                                /*required*/ None,
                                Some(false.into()),
                            ),
                            output_schema: None,
                        }),
                    ],
                }),
                LoadableToolSpec::Namespace(ResponsesApiNamespace {
                    name: "codex_app".to_string(),
                    description: "Tools in the codex_app namespace.".to_string(),
                    tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                        name: "automation_update".to_string(),
                        description: "Create, update, view, or delete recurring automations."
                            .to_string(),
                        strict: false,
                        defer_loading: Some(true),
                        parameters: codex_tools::JsonSchema::object(
                            std::collections::BTreeMap::from([(
                                "mode".to_string(),
                                codex_tools::JsonSchema::string(/*description*/ None),
                            )]),
                            Some(vec!["mode".to_string()]),
                            Some(false.into()),
                        ),
                        output_schema: None,
                    })],
                }),
            ],
        );
    }

    #[test]
    fn mcp_search_prefers_read_only_d1_query_for_natural_phrasing() {
        let search_infos = cloudflare_like_tools()
            .into_iter()
            .map(|tool| {
                McpHandler::new(tool)
                    .expect("MCP tool should convert")
                    .search_info()
                    .expect("MCP handler should return search info")
            })
            .collect::<Vec<_>>();
        let handler = ToolSearchHandler::new(search_infos);

        for query in [
            "cloudflare d1 execute query",
            "Cloudflare D1 read only query execute SQL database",
        ] {
            let tools = handler
                .search(query, /*limit*/ 1)
                .expect("search should succeed");
            assert_eq!(namespace_tool_names(&tools), vec!["d1_query_read_only"]);
        }
    }

    #[test]
    fn mcp_search_surfaces_ops_queue_tools_for_exact_identifier_query() {
        let search_infos = [
            tool_info(
                "ops",
                "work_item_queue_add",
                "Add a work item to an Ops runner queue",
            ),
            tool_info("ops", "work_item_queue_read", "Read an Ops runner queue"),
            tool_info(
                "ops",
                "work_item_queue_remove",
                "Remove a work item from an Ops runner queue",
            ),
            tool_info(
                "ops",
                "work_item_queue_upsert",
                "Upsert an Ops runner queue",
            ),
            tool_info(
                "ops",
                "runner_checkpoint_append",
                "Append a runner checkpoint",
            ),
        ]
        .into_iter()
        .map(|tool| {
            McpHandler::new(tool)
                .expect("MCP tool should convert")
                .search_info()
                .expect("MCP handler should return search info")
        })
        .collect::<Vec<_>>();
        let handler = ToolSearchHandler::new(search_infos);

        let tools = handler
            .search(
                concat!(
                    "work_item_queue_add ",
                    "work_item_queue_read ",
                    "work_item_queue_upsert ",
                    "work_item_queue_remove",
                ),
                /*limit*/ 8,
            )
            .expect("search should succeed");

        assert_eq!(
            namespace_tool_names(&tools),
            vec![
                "work_item_queue_add",
                "work_item_queue_read",
                "work_item_queue_remove",
                "work_item_queue_upsert",
            ]
        );
    }

    #[test]
    fn mcp_search_surfaces_flattened_namespace_identifier_query() {
        let search_infos = [
            tool_info(
                "ops",
                "work_item_queue_add",
                "Add a work item to an Ops runner queue",
            ),
            tool_info("ops", "work_item_queue_read", "Read an Ops runner queue"),
        ]
        .into_iter()
        .map(|tool| {
            McpHandler::new(tool)
                .expect("MCP tool should convert")
                .search_info()
                .expect("MCP handler should return search info")
        })
        .collect::<Vec<_>>();
        let handler = ToolSearchHandler::new(search_infos);

        let tools = handler
            .search("mcp__ops__work_item_queue_read", /*limit*/ 1)
            .expect("search should succeed");

        assert_eq!(namespace_tool_names(&tools), vec!["work_item_queue_read"]);
    }

    #[test]
    fn exact_identifier_match_requires_callable_identifier_equality() {
        let search_infos = [
            tool_info(
                "ops",
                "work_item_queue_remove_all",
                "Remove all items from an Ops runner queue",
            ),
            tool_info(
                "ops",
                "work_item_queue_remove",
                "Remove one item from an Ops runner queue",
            ),
        ]
        .into_iter()
        .map(|tool| {
            McpHandler::new(tool)
                .expect("MCP tool should convert")
                .search_info()
                .expect("MCP handler should return search info")
        })
        .collect::<Vec<_>>();
        let handler = ToolSearchHandler::new(search_infos);

        let tools = handler
            .search("work_item_queue_remove", /*limit*/ 1)
            .expect("search should succeed");

        assert_eq!(namespace_tool_names(&tools), vec!["work_item_queue_remove"]);
    }

    #[test]
    fn exact_identifier_match_does_not_promote_schema_identifiers() {
        let search_infos = cloudflare_like_tools()
            .into_iter()
            .map(|tool| {
                McpHandler::new(tool)
                    .expect("MCP tool should convert")
                    .search_info()
                    .expect("MCP handler should return search info")
            })
            .collect::<Vec<_>>();
        let handler = ToolSearchHandler::new(search_infos);

        assert!(
            handler
                .exact_identifier_match_ids("database_id", /*limit*/ 8)
                .is_empty()
        );

        let tools = handler
            .search(
                "Cloudflare database_id read only query execute SQL database",
                /*limit*/ 1,
            )
            .expect("search should succeed");

        assert_eq!(namespace_tool_names(&tools), vec!["d1_query_read_only"]);
    }

    #[test]
    fn exact_identifier_terms_ignores_natural_language_words() {
        assert_eq!(
            exact_identifier_terms(concat!(
                "Find `work_item_queue_add`, ",
                "mcp__ops__work_item_queue_read and queue tools",
            )),
            vec![
                "work_item_queue_add".to_string(),
                "mcp__ops__work_item_queue_read".to_string(),
            ]
        );
    }

    fn namespace_tool_names(tools: &[LoadableToolSpec]) -> Vec<&str> {
        tools
            .iter()
            .flat_map(|tool| match tool {
                LoadableToolSpec::Namespace(namespace) => namespace
                    .tools
                    .iter()
                    .map(|tool| match tool {
                        ResponsesApiNamespaceTool::Function(tool) => tool.name.as_str(),
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect()
    }

    fn cloudflare_like_tools() -> Vec<ToolInfo> {
        vec![
            cloudflare_tool_info(
                "api_read",
                "Execute a read-only Cloudflare REST API GET operation.",
                &["operation_id", "query"],
                /*read_only*/ true,
            ),
            cloudflare_tool_info(
                "api_mutate",
                "Execute a mutating Cloudflare REST API operation.",
                &["operation_id", "body"],
                /*read_only*/ false,
            ),
            cloudflare_tool_info(
                "d1_validate_query",
                "Validate one read-only D1 SQL statement without executing it.",
                &["database_id", "sql"],
                /*read_only*/ true,
            ),
            cloudflare_tool_info(
                "d1_execute_write",
                "Execute one audited D1 row-write SQL statement.",
                &["database_id", "sql"],
                /*read_only*/ false,
            ),
            cloudflare_tool_info(
                "d1_query_read_only",
                "Run or execute one read-only D1 SQL SELECT query against a database and return rows.",
                &["database_id", "sql", "max_rows"],
                /*read_only*/ true,
            ),
        ]
    }

    fn cloudflare_tool_info(
        tool_name: &str,
        description: &str,
        properties: &[&str],
        read_only: bool,
    ) -> ToolInfo {
        let properties = properties
            .iter()
            .map(|property| {
                (
                    (*property).to_string(),
                    serde_json::json!({ "type": "string" }),
                )
            })
            .collect::<serde_json::Map<String, serde_json::Value>>();

        let mut annotations = rmcp::model::ToolAnnotations::default();
        annotations.read_only_hint = Some(read_only);

        ToolInfo {
            server_name: "cloudflare".to_string(),
            supports_parallel_tool_calls: false,
            server_origin: None,
            callable_name: tool_name.to_string(),
            callable_namespace: "mcp__cloudflare__".to_string(),
            namespace_description: Some("Cloudflare account and data tools.".to_string()),
            tool: Tool::new(
                tool_name.to_string(),
                description.to_string(),
                Arc::new(rmcp::model::object(serde_json::json!({
                    "type": "object",
                    "properties": properties,
                    "additionalProperties": false,
                }))),
            )
            .with_annotations(annotations),
            connector_id: None,
            connector_name: Some("Cloudflare".to_string()),
            plugin_display_names: Vec::new(),
        }
    }

    fn tool_info(server_name: &str, tool_name: &str, description_prefix: &str) -> ToolInfo {
        ToolInfo {
            server_name: server_name.to_string(),
            supports_parallel_tool_calls: false,
            server_origin: None,
            callable_name: tool_name.to_string(),
            callable_namespace: format!("mcp__{server_name}"),
            namespace_description: None,
            tool: Tool::new(
                tool_name.to_string(),
                format!("{description_prefix} desktop tool"),
                Arc::new(rmcp::model::object(serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }))),
            ),
            connector_id: None,
            connector_name: None,
            plugin_display_names: Vec::new(),
        }
    }
}
