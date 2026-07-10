# Downstream Native Tool Surface Matrix

This matrix compares the product-native tool surface on the downstream branch
(historically `carry/main`, now `main`) against `upstream/main`.

It intentionally excludes session-only developer wrappers such as
`multi_tool_use.parallel`; those are runtime conveniences, not fork
divergences.

Last reviewed: pending the current integration sync.

| Surface | `upstream/main` | `main` | Live divergence? | Guardrails |
| --- | --- | --- | --- | --- |
| `exec_command` | PTY execution and standard process fields | Adds `wait_until_terminal`, `max_wait_ms`, and `heartbeat_interval_ms` through the runtime capability provider. | yes | `codex.blocking-waits-unified-exec-targeted`; `codex.blocking-waits-core-targeted` |
| `write_stdin` | Standard interactive-session fields | Adds the same bounded terminal-wait contract for empty input. | yes | `codex.blocking-waits-unified-exec-targeted` |
| Bare Android, browser, and desktop tools | Ordinary client-supplied dynamic tools | Promoted to canonical `ComputerUse` handlers with Codex-owned schemas, adapter dispatch, and mutating classification. | yes | `codex.native-computer-use-tool-registry-targeted`; `codex.app-server-computer-use-targeted` |
| Namespaced native-like tools | Ordinary namespaced dynamic tools | Remain ordinary dynamic tools; Android, browser, and desktop names are promoted only when bare. | no | `canonical_android_dynamic_tool_ignores_namespaced_android_names`; `canonical_browser_dynamic_tool_ignores_namespaced_browser_names`; `namespaced_desktop_tools_are_not_promoted` |
| `dynamicTools` input representation | Tagged function and namespace input with legacy flat ingestion | Retains flat function objects with optional `namespace`, `persistOnResume`, and `capability`; a lossless tagged migration is deferred. | yes | `codex.app-server-protocol-test`; `codex.app-server-v2-contract-targeted` |
| App-server computer-use bridge | No computer-use-specific v2 request | Projects live `ComputerUseCall` start and completion items and routes `item/computerUse/call` to capable clients. | yes | `codex.app-server-computer-use-targeted`; `codex.app-server-protocol-test` |
| Native image output | Dynamic-tool output already supports `inputImage` | Adds computer-use-specific request, provider, and live projection plumbing while preserving model-facing native images. | yes | `codex.tui-native-computer-use-targeted`; `codex.native-computer-use-tool-registry-targeted` |
| Computer-use history | No native computer-use thread contract | Live app-server and TUI projection only; events remain transient in every history mode and do not replay from snapshots. | yes | `computer_use_started_and_completed_translate_to_thread_events`; `live_computer_use_call_is_visible_while_active_and_after_completion` |
| `spawn_agent` response | v1 `agent_id` and `nickname`; v2 task metadata | Keeps v1 shape; v2 conditionally exposes identity plus requested/effective model and reasoning metadata. A missing resolved reasoning value is explicit `null`. | yes | `codex.core-subagent-surface-targeted`; `codex.core-subagent-model-pinning-targeted` |
| `list_agents` | Feature-gated live v2 inventory | Exposes the upstream live handler across the downstream collab surface and adds active-descendant hints through runtime capabilities. | yes | `multi_agent_v2_list_agents_returns_completed_status_without_encrypted_spawn_preview`; `multi_agent_v2_list_agents_filters_by_relative_path_prefix`; `multi_agent_v2_list_agents_omits_closed_agents` |
| `inspect_agent_tree` | Absent | Compact nested inspection with `live`, `stale`, or `all` scope, branch filters, and bounded depth/row limits. | yes | `codex.core-subagent-inspect-tree-fallback-targeted` |
| `wait_agent` arguments and output | Optional `timeout_ms`, mailbox/user-steering wakeups, and message plus timeout output | Adds optional `targets` and `return_when`, plus `requested_ids`, `pending_ids`, and `completion_reason`. Outcome fields are tool-output-only; canonical transcript items retain only identities and status snapshots. | yes | `completion_rule_distinguishes_any_from_all`; `multi_agent_v2_wait_agent_returns_summary_for_mailbox_activity`; `wait_agent_output_omits_capability_owned_fields_without_provider` |
| `apply_patch` | Freeform patch grammar | Same freeform grammar. | no | Apply-patch handler tests |
| `js_repl` | Feature-gated freeform JavaScript grammar | Same grammar. | no | `docs/js_repl.md`; `js_repl_*` tests |

Notes:

- `main` keeps operator surfaces where the tool contract absorbs bounded waiting
  or inventory inspection instead of forcing transcript polling.
- The flat `dynamicTools` representation remains the compatibility authority
  until app-server input, persisted rows, resume filtering, and provider
  registries can migrate together without loss.
- Provider artifact paths are diagnostic evidence only. Screenshots and browser
  viewport captures are model-facing only when returned as native image content.
