# Rust/codex-rs

## Fork Branch Policy

Use `origin/main` as the maintained branch and public PR target. Keep `origin/upstream-main` an exact, fast-forward-only mirror of `upstream/main`; never push feature work there. Sync upstream through `sedna-sync-upstream`, then merge `upstream-main` into `main` (do not rebase shared branches). Avoid force-pushing `main`; exceptional repairs require `--force-with-lease`. Completed work is committed, pushed, and raised as a PR to `origin/main` before handoff. Keep each local branch tracking its matching `origin` branch.

## Upstream Integration Philosophy

Prefer upstream behavior and narrow internal extension seams over repeated patches to high-churn core files. During sync, preserve intentional realtime, voice, and realtime-text behavior; do not invent broad dynamic plugin systems or drop downstream behavior merely to simplify a merge.

## Downstream Carry Documentation

Document every introduced or materially changed downstream carry in the same branch: update `docs/divergences/index.yaml` with its behavior, files, owner, upstream status, guardrails, tests, and sync disposition. For live carries, also update `docs/carry-divergence-ledger.md` and `docs/downstream-regression-matrix.md`; update the relevant domain doc for public or agent-facing changes. Missing carry documentation is incomplete work.

In the codex-rs folder where the rust code lives:

- Crate names are prefixed with `codex-`; `core` is `codex-core`.
- Inline `format!` arguments. Prefer private modules, explicit public exports, exhaustive matches, method references, and APIs without ambiguous boolean/`Option` positional arguments.
- Do not add or modify code for `CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR`, `CODEX_SANDBOX_ENV_VAR`, or `CODEX_SANDBOX=seatbelt`; sandbox- and Seatbelt-aware tests depend on these markers.
- For positional opaque literals, follow `argument_comment_lint`: use an exact `/*param_name*/` comment where required. Do not add comments to string/char literals without a clarity benefit.
- New traits need role/usage docs. Prefer RPITIT traits with explicit `Send` bounds over `#[async_trait]` or `#[allow(async_fn_in_trait)]`.
- Tests should compare whole objects where useful; do not test static values or removed logic. Put new test modules in sibling `*_tests.rs` files and avoid test-only production helpers.
- Do not add general product docs under `docs/`; upstream documentation is maintained elsewhere. App-server API docs are the exception.
- Keep native computer-use runtimes behind provider seams: Codex owns tool schemas, events, app-server/TUI projection, rollout, and native image output; backends own sessions, capture, UI digests, and input execution. Route browser integrations through the provider interfaces, not hot app-server/core paths. Successful visual observe/step responses require model-visible `inputImage` content. For behavior changes, update the focused native-computer-use, surface, regression, divergence, and recipe docs/tests.
- If you change `ConfigToml` or nested config types, run `just write-config-schema` to update `codex-rs/core/config.schema.json`.
- When working with MCP tool calls, prefer using `codex-rs/codex-mcp/src/mcp_connection_manager.rs` to handle mutation of tools and tool calls. Aim to minimize the footprint of changes and leverage existing abstractions rather than plumbing code through multiple levels of function calls.
- Do not call `reset_client_session` unnecessarily; let the incremental check logic decide whether to reuse the previous request.
- When changing dependencies, run `just bazel-lock-update` and include `MODULE.bazel.lock`; CI checks drift. Add compile-time read files (`include_str!`, `include_bytes!`, migrations, etc.) to the crate's Bazel data.
- Instrument async definitions, not call-site futures; avoid duplicate instrumentation and one-use helpers. Keep high-churn modules focused and move substantial additions with tests/docs; avoid extending `codex-rs/tui/src/chatwidget.rs` except for trivial changes.

## Build and validation

Use approved hosted runners for builds, tests, lint, formatting, snapshots, codegen, and other substantive compute; helper availability does not authorize local execution. Finish fixes/formatting before affected validation, then rerun affected checks after behavioral, fixture, generated, or observer changes on the exact final candidate. Reuse only individually proven unaffected evidence for mechanical changes; formatting-only changes do not require a blanket full-suite rerun. Preserve required protected and final-artifact checks.

Use `validation-lab.yml` for remote scratch/integration checks (`targeted` for one seam; `frontier` only with a recent trusted baseline). Public releases and native release verification cover Linux `x86_64` and Arm64 GNU. Intel macOS remains an explicit release mode; other release targets require an approved support-contract change and matching docs/workflows. Preview artifacts are non-release; only the protected Sedna release workflow publishes public releases.

## Bug investigations

For likely upstream/core regressions, check `upstream/main`, then related open `openai/codex` issues/PRs, then trace locally. Fork patches, branch policy, wrappers, and local environment glue are downstream-only.

## Compatibility and tests

Check compatibility when changing app-server APIs, raw response events (including experimental), CLI parameters, config loading, or rollout resume. Agent-logic changes need integration coverage for changed behavior; prefer existing `core/suite` helpers. Put new test modules in sibling `*_tests.rs` files and avoid test-only production helpers.

## TUI

Follow `codex-rs/tui/styles.md`; keep parallel behavior in `tui_app_server` aligned with `tui`.

Use ratatui Stylize helpers and `tui/src/wrapping.rs` for line wrapping; details live in the focused TUI guidance.

## Tests

Place new test modules in sibling `*_tests.rs` files; do not move existing inline test modules solely to follow this rule.

### Snapshot tests

User-visible UI changes require `insta` snapshot coverage. On an approved hosted runner, generate snapshots on the exact final candidate, inspect every `*.snap.new`, and accept only intended, reviewed updates; never accept snapshots automatically.

### Test assertions

Use `pretty_assertions::assert_eq` and compare whole objects where useful. Avoid mutating process environment; pass environment-derived dependencies explicitly. Cover meaningful negative behavior, not removed or statically defined values.

### Spawning workspace binaries in tests (Cargo vs Bazel)

Use `codex_utils_cargo_bin::cargo_bin` for first-party binaries and `find_resource!` for fixtures so they resolve under Bazel runfiles; avoid `env!("CARGO_MANIFEST_DIR")`.

### Integration tests

#### codex_core integration testing

Use `core_test_support::responses` and `TestCodexBuilder::build_with_auto_env()` for integration tests. Retain `ResponseMock`s and assert outbound requests through `ResponsesRequest` helpers; prefer one-shot mounts and event waits. See `$remote-tests` for foreign app/exec OS cases.

#### app-server integration testing

Test app-server through its public JSON-RPC API using `TestAppServer` with auto-environment setup; see `$remote-tests` for foreign app/exec OS cases.

## App-server API Development Best Practices

These guidelines apply to app-server protocol work in `codex-rs`, especially:

- `app-server-protocol/src/protocol/common.rs`
- `app-server-protocol/src/protocol/v2.rs`
- `app-server/README.md`

### Core Rules

Add API surface in v2, not v1. Use `*Params`, `*Response`, and `*Notification` types; keep Rust/TypeScript wire names and tags aligned (camelCase, except config keys), and export v2 types to `v2/`. Preserve structured request semantics: optional request fields use `Option` with `#[ts(optional = nullable)]`; do not omit v2 payload fields with `skip_serializing_if`. New list methods default to cursor pagination (`cursor`/`limit` request fields; `data`/`next_cursor` response fields). Keep experimental gating on experimental API elements.

### Development Workflow

- Update `app-server/README.md` when API behavior changes. For payload-shape changes, run `just write-app-server-schema` (add `--experimental` when needed) and validate with `just test -p codex-app-server-protocol`; prefer behavioral/schema coverage to marker-only tests.

## Platform Support

Follow the applicable component and workflow contracts for platform support. Preserve component portability and run all mandatory cross-platform checks; the absence of a Windows release target does not waive required Windows tests. Release targets and preview/executor modes are defined by their owning workflows and do not by themselves add or remove component test requirements.

Codex supports running connected app-server and exec-server on different operating systems. See the
`$remote-tests` skill for details about integration testing these configurations.
