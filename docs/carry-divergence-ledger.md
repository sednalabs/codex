# Carry Divergence Ledger

This document records the current live divergences of the downstream branch
(historically `carry/main`, now `main`) from `upstream/main`.

It is an audit ledger, not a changelog. Ahead-count alone is not evidence of a
live divergence.

The snapshot below intentionally anchors to the audited code tree before the
docs-only refresh commit that records this snapshot.

## Audit Baseline

- Audited on: `2026-07-21`
- downstream integration code tree: `bf736ed0f822fdf0c65563ccd18e9f18f1fb068c`
- comparison basis: `upstream/main`
- mirror branch `upstream-main` (`origin/upstream-main`): `c9ef7eff005c3299a5a5f0004c34c6a3eedf2564`
- `upstream/main`: `c9ef7eff005c3299a5a5f0004c34c6a3eedf2564`
- downstream branch vs `upstream/main`: `1913` downstream ahead, `0` upstream ahead
- Mirror vs `upstream/main`: `0` ahead, `0` behind (`exact`)
- Downstream-only non-merge commits at audit time: `1641` unique, `0` patch-equivalent

## Audit Rules

- Count a live divergence only when the maintained downstream branch tree
  (historically `carry/main`, now `main`) still differs from `upstream/main`.
- Count generated schemas, snapshots, and inline test-module moves as
  derivative churn, not as standalone divergence items.
- Keep canonical config schema generation in `codex-config`; `codex-core`
  delegates to that implementation so Cargo and Bazel cannot normalize the
  same fixture through different code paths.
- Track exact-subject upstream matches separately as historical carry history.
- Treat the exact-subject upstream match list as a lower bound for "already
  upstreamed" history, not a complete semantic-duplicate detector.

## Latest Upstream-Owned Integration

### Hook Additional-Context Spill Limits

- The 2026-07-21 sync adopts upstream commit `e4836f998d`, which adds a
  per-command-hook `additionalContextLimit` for events that can emit
  `additionalContext`.
- Preserve upstream's exact semantics: an unset limit retains the approximate
  2,500-token default, `0` disables spilling for that hook, and each hook is
  evaluated independently before its context reaches the model.
- Preserve the field through JSON and TOML discovery, hook hashing, app-server
  config requirements and hook-list responses, generated schemas, and the TUI
  hook browser. Unsupported events ignore the setting with a warning.
- The generated app-server schemas are a shared sync seam. Future regeneration
  must retain this upstream field alongside downstream dynamic-tool and native
  computer-use protocol additions rather than selecting either parent schema.
- Exact-head Bazel run `29789793659` exposed a pre-existing command-runner race:
  a successful hook can emit stdout and exit without reading stdin, after which
  the parent's write observes `BrokenPipe` and previously discarded that valid
  output. Signed downstream commit `932cbceeb8` treats only `BrokenPipe` as a
  benign closed-input signal, then preserves the child's actual output and exit
  status; every other stdin write error retains the existing kill-and-report
  path.
- `hook_can_exit_successfully_without_reading_stdin` forces the closed-pipe path
  with a 2 MiB input on every supported host. The original
  `session_start_hooks_apply_additional_context_limits_individually` integration
  test remains enabled under Wine and guards the user-visible spill behavior.
  Drop this production-and-test carry together when upstream has equivalent
  successful-early-exit handling and regression coverage.

### Mid-Turn Compaction Hook Ordering

- The sync adopts upstream commit `8c41ed33ce`, which drains pending
  `SessionStart` hooks immediately after a successful mid-turn auto-compaction
  and before the next sampling request.
- A hook stop request ends the turn instead of allowing continuation. Otherwise,
  the hook's additional context is included in the immediately following
  sample, and repeated compactions in one turn repeat the same ordering.
- Preserve this ordering alongside downstream realtime and permission-context
  reconstruction. The upstream regressions
  `mid_turn_auto_compact_session_start_hooks_run_before_each_continuation` and
  `mid_turn_auto_compact_session_start_hook_stop_blocks_continuation` are the
  focused guardrails.

### Approval Rejection-Reason Propagation

- The sync adopts upstream commit `e52c35b000`, which changes protocol denials
  from a unit value to `Denied { rejection: String }` and serializes them as a
  structured `denied.rejection` object.
- Preserve the specific reason across command, patch, network, MCP, delegated,
  automatic-review, and shell-escalation paths. Invalid approval responses
  remain distinguishable from user declines, and model-visible rejection text
  remains bounded by upstream truncation.
- Conflict resolution kept upstream's structured denial schema and direct
  reason propagation while retaining the fork's independent `timed_out`
  semantics, `GuardianUserAuthorization` module path, dynamic-tool schema, and
  native computer-use schema additions.
- The shared-history assertion repair in `796d4248c5` only adapts a stale audio
  history test to the upstream copy-on-write `raw_items()` accessor. It does not
  introduce runtime carry.

### History And Hook Test API Convergence

- Upstream commit `ec3140db12` adopts the same `raw_items()` audio-history
  assertion and initializes the Windows command-hook fixture's
  `additional_context_limit` field.
- The earlier signed downstream test repair `796d4248c5` is now semantically
  upstream-owned and must not be counted as live carry.

### Paginated Rollout Lineage Resolution

- Upstream commit `b7e39aa316` adds the canonical bounded resolver for ordered
  paginated rollout segments, archived ancestors, and explicit history
  positions.
- Cycles, missing or mismatched source rollouts, non-paginated sources, and
  invalid cutoff bounds fail closed. Downstream resume, state, and usage
  overlays must use this lineage rather than inventing a competing traversal.

### Threadless MCP Connections

- Upstream commit `19940967bd` makes the MCP connection-manager event sender
  optional for threadless callers.
- Resource reads, status snapshots, and connector discovery skip session
  startup notifications, decline interactive elicitations, and continue
  non-interactive work. Downstream MCP pagination, OAuth, blocking waits, and
  runtime-snapshot ownership remain independent live carry.

### Linux `/proc` Preflight Isolation

- Upstream commit `44481a1c45` moves the bubblewrap `/proc` probe to a
  temporary minimal read-only filesystem view while preserving the requested
  network namespace mode.
- Conflict resolution retains downstream's constrained-host network-namespace
  fallback without restoring the command filesystem or working directory to
  the probe. `proc_mount_preflight_does_not_bind_the_full_filesystem` and
  `network_preflight_preserves_proc_mount_fallback` guard both halves.

### Absolute Test SQLite Paths

- Upstream commit `81e89fa5af` makes `SqliteConfig::new_for_testing` accept an
  `AbsolutePathBuf` directly. All downstream-only state fixtures use the same
  checked `.abs()` conversion; there is no production database-location or MCP
  behavior divergence.

### CSV Agent-Job Retirement And Migration Collision Repair

- Upstream commit `687f05cb94` removes `spawn_agents_on_csv`,
  `report_agent_job_result`, their coordinator/runtime state, and the
  `agent_jobs` / `agent_job_items` tables. The legacy feature and agent-config
  keys remain accepted as no-ops.
- Ordinary `spawn_agent`, MultiAgentV2, child model/reasoning selection, role
  skills, inventory, wait joins, dynamic tools, and native computer use are not
  part of the deleted CSV subsystem and remain intact.
- Preserve upstream `0042_drop_agent_jobs.sql` byte-for-byte. Move the
  already-shipped downstream `0042_external_agent_config_imports.sql` to
  `0047_external_agent_config_imports.sql`, then checksum-repair exact legacy
  records from `42` to `47` before SQLx applies upstream `42`.
- The production-path regression reconstructs exact `origin/main` through
  version `45`, simultaneously repairs remote-control-enabled `41 -> 46` and
  external-agent imports `42 -> 47` through `StateRuntime::init`, removes only
  the two job tables, and preserves thread rows, spawn edges, and the complete
  external-import record.
- The table drop intentionally discards unfinished CSV-job coordinator data.
  After both repairs and upstream migrations run, a pre-sync binary cannot
  reopen the database because it knows the former `41` and `42` checksums;
  rollback requires a pre-upgrade database copy.
- Hosted mirror run `29780661853` fast-forwarded `origin/upstream-main` to
  exact upstream `687f05cb94`. The sync job passed; the audit's exit `4` is the
  expected pre-promotion result while `origin/main` still points at the old
  downstream tree. Audit artifact `8476471215` has SHA-256
  `d03dd1c3b7ad0ad3bf4a1d6a3eca8e122d4cf5d385d9eb7e4688b53898dd432e`.

### Completed Hook Warning Headers

- Upstream commit `cf821e8ec8` renders the first line of a completed hook
  warning in the hook header after `says:` and indents any remaining lines.
  Warning entries are not repeated in the output body; hooks without warnings
  retain their existing headers.
- Downstream's compact transcript renderer shares the same completed-hook
  semantics while continuing to collapse only context entries. The holistic
  `verbose_transcript_preserves_hook_context_while_compact_collapses_it`
  regression protects rich, compact, and raw output together.
- Hosted mirror run `29781569004` fast-forwarded `origin/upstream-main` to
  exact upstream `cf821e8ec8`. Sync job `88483784931` passed. Audit job
  `88483884485` returned the expected pre-promotion exit `4` with
  `mirror=exact`; artifact `8476818381` has SHA-256
  `c9a3b5415b61aadc19153316fb9d3a9d0622ad63ab2940e16ca9378a178778e3`.
- Hosted generation run `29783168837` passed at temporary workflow head
  `321991090e` against exact integration source `74f7c82f02`. Artifact
  `8477717271` has archive SHA-256
  `81d9a3c91629578ac257281815c17e55b928c1a9c4fe50910204a07200ef9131`;
  its embedded 12-file patch has SHA-256
  `b13a993db6a856488fe060996de53641843125dc9c79dd8929faa558a8f69a68`.
  The run regenerated app-server and config schemas, the hook-browser snapshot,
  Rust formatting, and downstream table formatting, then passed both exact
  completed-hook regressions. Signed commit `3f15d104d7` records the verified
  artifact output without the temporary workflow.

### App-Read Connector Metadata And Auth Routing

- Upstream commit `60272096bc` enriches experimental `app/read` connector
  metadata with dark-icon URL aliases, distribution channel, install URL, and
  plugin display names. Plugin display names are derived from loaded plugin
  declarations without starting MCP servers.
- Downstream retains auth-dependent app/MCP routing. `app/read` synchronizes the
  shared `PluginsManager` from the same live ChatGPT auth snapshot used for the
  metadata request before it projects plugin names. This prevents an external
  auth transition or embedding-owned auth update from leaving the projection
  on an older API-key mode.
- `app_read_resynchronizes_plugin_auth_after_external_login_without_starting_mcp`
  covers an API-key-to-external-ChatGPT transition, sorted plugin names, and
  zero MCP startup requests. The complete `suite::v2::app_read::` group is part
  of `app-server-v2-contract-targeted`.
- Hosted mirror run `29784292249` fast-forwarded `origin/upstream-main` to exact
  upstream `60272096bc`; sync job `88492334147` passed and audit job
  `88492430174` returned the expected pre-promotion exit `4`. Artifact
  `8477855931` has SHA-256
  `a4a207bbae70b4f269b1ca605dd34622ddbdd268b1e50acbffe0ddf7d06e5c2c`.
- Hosted generation run `29784486643` passed all schema and focused app-read
  tests but failed closed because its disposable packager incorrectly required
  a non-empty patch. Corrected run `29785333279` passed at workflow head
  `ddbdf48e675460e04cd1b5238b00d87ac9b3b818` against source
  `4df3033de87cf8d3b2a716e3d01df75924106bcf`. Artifact `8478473413` has
  archive SHA-256
  `68685c9ab347da89c7145972fc504cacbbb9d46330231fe27d395bd2a59cfa1d`;
  its manifest records `changed_file_count=0` and the canonical empty-patch
  SHA-256 `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`.

### Exec-Server Windows Native Spawning

- Upstream commit `35c2278dd5` introduces the shared
  `codex_sandboxing::spawn_process` seam and routes executor-native pipe, PTY,
  inherited-file-descriptor, and Windows sandbox launches through it. The exec
  server now carries permission profiles, workspace roots, proxy state,
  filesystem overrides, sandbox level, and private-desktop selection into the
  canonical Windows session spawner.
- The only merge conflict was the obsolete downstream direct Windows launch
  block in `core/src/unified_exec/process_manager.rs`. Resolution adopts the
  upstream shared launcher and drops that duplicate block while preserving the
  downstream pre-spawn override preparation, proxy-aware effective-backend
  policy, metrics, telemetry, and unrelated bounded final-output carry.
- Hosted mirror run `29786347719` fast-forwarded `origin/upstream-main` to exact
  upstream `35c2278dd5`; sync job `88498617036` passed and audit job
  `88498704999` returned the expected pre-promotion exit `4`. Artifact
  `8478603020` has SHA-256
  `7eab307e0b280aca8b56b15cefce15e5f7b3af706b8af77c98d9c2ab0693b925`.
- Upstream's launcher API added inherited file descriptors but missed the Wine
  PTY regression call site. Signed downstream commit `932cbceeb8` passes the
  same empty descriptor slice used by the superseded wrapper, restoring the
  upstream Bazel target without changing runtime behavior. The same commit adds
  the exact optional-argument comment required by the fork's stricter Windows
  argument-comment lint. Drop each validation-only hunk independently when its
  upstream equivalent lands.

### Shared Skill Model Ownership

- Upstream commit `56c11cf658` moves host and environment skill metadata,
  policy, dependencies, interface, and configuration-rule models into the
  lower-level `codex-skills` crate. `codex-core-skills` keeps compatibility
  re-exports, so existing consumers retain type identity rather than adding a
  downstream adapter.
- Implicit invocation still defaults to allowed unless explicitly disabled.
  Empty product restrictions remain unrestricted, while a non-empty product
  list still requires a matching product. Host and environment metadata now
  share that product-restriction implementation.
- The sole merge conflict was the import block in the downstream-expanded
  `core-plugins/src/manager_tests.rs`. Resolution keeps the still-used
  `PluginSkillSnapshots` compatibility import, drops two unused imports, and
  adopts upstream's canonical `codex_skills::SkillConfigRules` owner.
- Downstream preferred-user skill precedence, plugin auth projection, dynamic
  tools, and no-MCP-start app metadata behavior remain separate live carry.
  The relocation does not create a new downstream divergence.

### Remote Compaction History Efficiency

- Upstream commit `fd3c1dc13d` estimates remote-compaction history items once,
  updates the unclamped token total as trailing tool outputs are rewritten,
  and installs the replacement history only after the selected rewrites are
  complete. It also snapshots compaction input only when rollout tracing is
  enabled and reuses the v2 prompt input instead of cloning it.
- The compaction files were upstream-exact before this merge and merged without
  conflict. Downstream realtime world-state reconstruction, compaction hooks,
  capacity retry, turn metadata, and dynamic-media accounting remain on their
  existing seams and are not reclassified as part of this optimization.
- Hosted mirror run `29788160229` fast-forwarded `origin/upstream-main` to
  exact upstream `fd3c1dc13d`; sync job `88504053569` passed and audit job
  `88504128184` returned the expected pre-promotion exit `4`. Artifact
  `8479246296` was independently downloaded with SHA-256
  `65100c7f559bdb708290142ae7ff729b9f409e433841dbe660f5ef271b7bfdb1`;
  its embedded report records `mirror=upstream=fd3c1dc13d` and exact mirror
  health.
- Hosted exact-head proof run `29788805449` passed at disposable workflow head
  `bf26fbc18d` against source `c86d398de2`. Jobs `88505945020`,
  `88505945036`, and `88505945060` respectively passed the remote-compaction,
  shared-skill/plugin-auth, and formatting/generation fanout. Artifact
  `8479620777` has archive SHA-256
  `153794fd6d68a47b0a5fa7738fd21373b435e8909a25fc8a3019d841f1334fc6`;
  its allowlisted two-file patch has SHA-256
  `f467c47a91d4e6d278892d2f80a7347af550d7c6b1afae0b0cbd7e3e8adae491`
  and was applied exactly in signed commit `b350254be3`. The temporary remote
  branch, worktree, and local branch were then removed.

### Catalog Messages For Non-Request Approval Policies

- Upstream commit `2be7d3bcd9` adds model-catalog approval messages for `never`
  and `unless_trusted`, selects the variant matching the active policy, and
  retains the built-in text when that catalog key is absent. An explicitly
  empty value suppresses only the built-in approval section, matching existing
  `on_request` semantics.
- The model metadata and prompt changes merged without conflict. The
  downstream Schemars 1.2 adapter remains the only local shape difference in
  `openai_models.rs`; the new fields and their deserialization behavior are
  otherwise upstream-exact. This changes model-visible instructions only, not
  approval enforcement, sandbox policy, or escalation authorization.

### Explicit Outbound Proxy Route Resolution

- Upstream commit `c9ef7eff00` resolves system-proxy failure into an explicit
  environment-proxy or direct route, carries `NO_PROXY` through HTTP and
  WebSocket connections, uses cached decisions before platform lookup, and
  provides an async resolver that serializes blocking Windows/macOS discovery
  outside Tokio workers. The current production WebSocket connector still uses
  the synchronous resolver, so this sync does not claim that live call path has
  moved off Tokio workers yet.
- The sole merge conflict was the proxy-cache digest renderer. Resolution takes
  upstream's complete route design and keeps the existing sha2 0.11-compatible
  explicit hexadecimal encoder. No proxy policy or credential-bearing value is
  changed or exposed by that compatibility hunk. Drop the encoder only when the
  downstream digest line again supports upstream's formatter or upstream adopts
  an equivalent explicit encoder that passes the locked Cargo and Bazel graphs.
- Hosted mirror run `29791336871` advanced `origin/upstream-main` to exact
  `c9ef7eff00`; sync job `88513605376` passed and audit job `88513679966`
  returned the expected pre-promotion exit `4`. Artifact `8480383914` has
  GitHub SHA-256
  `66c768664ada195eb2d39c23e2d3eb2e04ab34e8b01d6f61175b1f4392f08158`
  and records exact mirror health. Signed two-parent merge `bf736ed0f8`
  preserves `c9ef7eff00` as its second parent.

## Current Live Divergences

### Fork Workflow And Validation Policy

- `main` is now the default PR and integration branch, while `upstream-main`
  is the exact upstream mirror.
- Downstream sync policy is merge-based, not rebase-based.
- Upstream now groups PR-blocking checks through reusable leaf workflows called
  by `blocking-ci.yml`. Downstream preserves that upstream topology and carries
  only the wrapper entrypoint expansion for `merge_group` and `upstream-main`
  pushes, instead of reintroducing direct triggers on every child workflow.
- Hosted Rust archive builders reclaim common Linux runner disk headroom before
  `cargo nextest archive`, build the `ci-test` archive payload with debug info
  disabled and symbols stripped, stay archive-only, and leave test execution
  to the archive-consuming `tests` and `remote_tests` jobs. Those replay jobs also
  install `bubblewrap` and reclaim hosted disk before archive extraction so
  sandbox and remote replay failures are not artifacts of runner packaging or
  disk pressure. The `remote_tests` replay job builds its Docker remote-env
  Codex binary in an isolated temporary Cargo target directory and removes
  those host-side build artifacts before replaying the shared nextest archive,
  so remote-env setup does not consume the extraction headroom needed by the
  227-binary archive. The archive uses default Cargo features, matching upstream
  and the non-sandbox V8 release artifact; explicit sandbox coverage remains in
  `v8-canary`. The `remote_tests` replay job keeps a 45-minute hosted budget so
  long archive download and remote-environment setup time does not masquerade
  as a product failure. Remote replay skips host-only compact/resume, hook, and
  forced-`rm` shell-safety/approval fixtures, while Guardian's local proxy
  fixtures use a host-native cwd. The large-output summary remains host-only
  until exec-server replay preserves
  bounded head, tail, and omission metadata before core subscribes. The
  `codex.skill-loader-fixture-hermeticity-targeted` lane pins the two
  skill-loader fixture assertions that suppress or ignore ambient parent
  project layers, so hosted-runner repository markers cannot alter the result.
  The rust-ci-full summary parser records final nextest
  retry statuses so `TRY 1 FAIL` followed by `TRY 2 PASS` does not block, while
  persistent `TRY 2 FAIL` / `TRY 2 TIMEOUT` lines still appear in structured
  harvest artifacts. Validation-lab Rust batches reclaim target artifacts before
  the first lane and between later lanes when hosted disk falls below the safety
  floor, the link-heavy native computer-use tool-registry lane is weighted as a
  singleton batch, archive jobs skip sccache, and validation-lab Rust batches
  retry once on narrow Cargo registry transport failures such as crates.io
  HTTP/2 or EOF download flakes, and argument-comment lint retries once on the
  same narrow Cargo metadata/fetch failure class before reporting a lint blocker.
  Large validation-lab plans keep their resolved metadata in a runner-temp
  JSON file for per-field parsing. The live workflow feeds that file to the
  fingerprint helper through stdin instead of granting the helper a
  caller-selected path or placing the complete plan in argv or the process
  environment. The helper retains environment input only for legacy direct
  callers; workflow wiring must preserve stdin until those callers are migrated.
  This keeps `full`, `broad`, and Frontier Max dispatches below host exec limits
  without widening the helper's path authority.
  CodeQL's compare-API diff discovery stops when a pull request reaches 300
  changed files. For large upstream-sync pull requests, downstream restores the
  same diff-informed query restriction after CodeQL initialization by deriving
  the complete added/modified line ranges from the runner's checked-out Git
  history. This avoids treating unchanged base alerts as new without suppressing
  queries, dismissing the base backlog, or weakening normal pull-request scans.
  Runtime
  permission policy keeps the configured `codex_linux_sandbox_exe` readable
  under restricted filesystem profiles so GitHub-hosted archived nextest runs
  can re-enter the sandbox helper from extracted test binaries; the Linux bwrap
  launch path also adds the helper directory and `:minimal` system runtime roots
  to the outer bootstrap filesystem view before re-entering the inner seccomp
  stage. The
  workspace JWT dependency uses `jsonwebtoken` with the
  `aws_lc_rs` provider so hosted Cargo/Bazel `--locked` runs avoid pulling the
  RustCrypto RSA graph. Downstream dependency-policy validation preserves
  upstream's current `quick-xml` advisory shape: the direct workspace
  dependency is on the fixed line, and the remaining trusted transitive
  `plist`/`syntect` and `wayland-scanner`/`arboard` paths keep synchronized
  RustSec exceptions in `deny.toml` and `.cargo/audit.toml` until those
  upstream crates can use `quick-xml >=0.41.0`. The reqwest ownership ratchet
  names `codex-android-computer-use` as temporary downstream migration debt
  until its MCP transport moves behind `codex-http-client`; other new direct
  reqwest owners remain denied. Hosted macOS V8 staging, Bazel
  clippy, and Bazel
  release-build verification keep fanout below runner process/thread ceilings.
  Python SDK runtime-package staging rejects archive traversal, links, and
  special entries before writing ordinary package files beneath the staging
  root, without an unfiltered compatibility fallback on older Python runtimes.
  Hosted frontier argument-comment lint uses the prebuilt linter package so
  cold validation-lab runs do not spend the lane compiling V8/ICU before
  linting ordinary Rust call sites; V8 proof-of-concept buildability remains
  covered by build/test workflows. Hosted `rust-ci` callers pass `GH_TOKEN`
  through to the composite action so DotSlash can use authenticated
  `gh release download` fallback on Windows. Direct-runtime permission profiles
  stay on the bubblewrap/seccomp
  enforcement path when legacy Landlock is configured so sandbox validation
  fails safely instead of weakening policy. TUI carry smoke uses the same
  hosted test stack floor as core carry smoke so frontier/checkpoint validation
  can stay on GitHub hosted compute instead of falling back to local compute.
  Remote executor sweeps skip host-local managed-network approval and denial
  fixtures until the remote harness provides a proxy endpoint reachable from the
  target process; environment-specific approval scoping remains covered by unit
  tests and host-local integration. Compact/resume rollback fixtures remain
  host-only, wait for the thread-idle lifecycle before rollback admission, and
  correlate typed rollback errors to the exact submission under a short
  phase-labelled deadline. This isolates the fixtures from the terminal-event
  visibility window without claiming the production persist/clear/deliver
  ordering issue is resolved.
- The Bazel crate macro accepts and forwards optional unit-test arguments so
  upstream's serialized exec-server unit-test declaration remains analyzable
  until equivalent macro support lands upstream.
- Windows hosted setup prefers a real Dev Drive but falls back to an existing
  secondary or system volume when the runner image lacks Dev Drive formatting,
  so validation does not fail before the requested command starts.
- Windows Bazel shards serialize local test actions because sandbox identities,
  ACLs, and firewall rules are host-global; concurrent policy tests can
  otherwise invalidate one another and create false allow/deny results. The
  Bazel CI wrapper applies its selected rc config before explicit caller flags,
  so the workflow's `--local_test_jobs=1` limit cannot be silently reset by the
  Windows cross-build config.
- Helper-backed local validation and release flows may be used when configured,
  but those presets are not a tracked repository contract.
- Divergence regression ownership is tracked in
  [`downstream-regression-matrix.md`](downstream-regression-matrix.md).
- Field-level native tool-surface deltas are summarized in
  [`downstream-tool-surface-matrix.md`](downstream-tool-surface-matrix.md).
- Future registry-plus-generation maintenance direction is captured in
  [`downstream-divergence-tracking.md`](downstream-divergence-tracking.md).
- Downstream guidance prefers MCP tool surfaces with blocking wait
  semantics over transcript-driven polling when the tool contract supports it.
- Primary files:
  - `.github/scripts/run_validation_lane_batch.py`
  - `.github/scripts/prepare_codeql_diff_ranges.py`
  - `.github/scripts/rusty_v8_bazel.py`
  - `.github/scripts/test_ci_planners.py`
  - `.github/workflows/blocking-ci.yml`
  - `.github/workflows/bazel.yml`
  - `.github/workflows/codeql.yml`
  - `.github/workflows/rust-ci-full.yml`
  - `.github/workflows/v8-canary.yml`
  - `defs.bzl`
  - `codex-rs/core/src/config/mod.rs`
  - `codex-rs/core/src/config/permissions.rs`
  - `codex-rs/core/src/config/permissions_tests.rs`
  - `codex-rs/linux-sandbox/src/linux_run_main.rs`
  - `codex-rs/Cargo.toml`
  - `codex-rs/Cargo.lock`
  - `docs/contributing.md`
  - `docs/downstream.md`

### Windows Filesystem Boundary Compatibility

- The 2026-07-16 sync adopts upstream commit `5a85351dfe`, which retires the
  `GeneratedDefault` provenance and its setup-time metadata normalization.
  Downstream no longer claims that either Windows backend can reserve absent
  `.git`, `.agents`, or `.codex` child names without filesystem interception.
- The 2026-07-21 sync adopts upstream commit `bd92b056dd`, which distinguishes
  explicit from inherited allow ACEs when deciding whether a write root needs
  repair. Preserve that distinction: `SET_ACCESS` cannot replace an inherited
  `FILE_DELETE_CHILD` grant, so inherited stale rights must not trigger a
  refresh loop that cannot converge.
- The `/tmp` special root remains Unix-only. Treating root-relative `/tmp` as a
  Windows drive path would create a split writable-root policy that the legacy
  restricted-token backend cannot enforce.
- Explicit read-only carveouts and pre-existing protected metadata remain the
  enforceable boundary. Keep those assertions holistic rather than restoring
  removed setup-side-effect tests or persistent sentinel directories.
- The compatible legacy token keeps Everyone on its default DACL for IPC, but
  excludes Everyone from the restricting SID set and retains `WRITE_RESTRICTED`
  so existing launch and runtime dependencies remain readable. This preserves
  the pre-existing backend behavior; it does not make capability-SID deny ACEs
  authoritative for standalone `DELETE` or `FILE_DELETE_CHILD` checks.
- Hosted comparison run `29628256459` proved the remaining limitation:
  `WRITE_RESTRICTED` started successfully but deleted both normalized hostile
  targets, while full restriction without Everyone could not start the process.
  Full restriction with Everyone passed that fixture and the complete Windows
  sandbox unit target in run `29628486469`, but review found that any path which
  grants Everyone write access would also satisfy the restricting check. The
  merged-head run `29636964666` additionally proved that full restriction breaks
  PowerShell and gnullvm runtime loading without explicit capability-SID read
  access. Those runs are diagnostic evidence, not proof that either incomplete
  full-restriction shape is safe to ship.
- A durable full-restriction design must provision the exact active capability
  SIDs with synchronous read/execute access to bounded launch and runtime
  dependencies, must not derive those roots from ambient `PATH`, and must avoid
  stale capability ACEs widening later policies. Until then, the normalized
  deletion fixture deliberately characterizes the legacy limitation rather than
  claiming containment.
- Hosted Windows guardrails:
  `slash_tmp_permission_path_is_unix_only`,
  `filesystem_policy_blocks_protected_metadata_path_writes_by_default`,
  `missing_symbolic_metadata_carveouts_need_direct_runtime_enforcement`,
  `windows_restricted_token_supports_full_read_split_write_read_carveouts`, and
  `legacy_write_restricted_deletion_limitation_is_explicit`, plus
  `restricted_sids_exclude_everyone` and
  `default_dacl_keeps_everyone_for_ipc_compatibility`.
- The deletion characterization uses PowerShell 7, which adjacent
  restricted-token tests prove can initialize under the compatible token.
  Bazel's gnullvm test executable can still fail during helper re-entry with
  exact status `0xc0000142`; that status proves the wrapper was reached but
  does not replace the read and denied-write assertions on MSVC and other
  release-shaped targets.
- Primary files:
  - `codex-rs/protocol/src/permissions.rs`
  - `codex-rs/sandboxing/src/policy_transforms.rs`
  - `codex-rs/sandboxing/src/policy_transforms_tests.rs`
  - `codex-rs/sandboxing/src/windows.rs`
  - `codex-rs/core/src/exec_tests.rs`
  - `codex-rs/windows-sandbox-rs/src/token.rs`
  - `codex-rs/windows-sandbox-rs/src/token_tests.rs`
  - `codex-rs/windows-sandbox-rs/src/unified_exec/tests.rs`
  - `codex-rs/exec-server/tests/file_system_windows.rs`

### Windows Proxy-Aware Backend Selection

- A managed network proxy is an effective elevated-backend requirement on
  Windows, regardless of whether the configured sandbox level is elevated or
  restricted token.
- Direct exec and unified exec share the same backend selector and filesystem
  override resolver. The prepared deny-read/write and split-root overrides
  must reach the canonical Windows session spawner unchanged.
- PowerShell `-NoProfile` startup, spawn-failure metrics, tool telemetry, and
  turn metadata follow the effective backend, not only the configured level.
- The direct-loopback denial fixture runs native `curl.exe` with explicit direct
  routing and connect/overall deadlines. Because a WFP-blocked connect can
  outlive curl's own deadline, the fixture makes curl its first potentially
  blocking operation, observes the bounded unified-exec yield, and terminates
  only the registered background session through Codex's process manager. The
  unique non-proxy endpoint must record zero direct requests. Distinct
  connected, expected-denial, and probe-error outcomes prevent a missing or
  broken probe from passing.
- Hosted guardrails:
  `windows_proxy_enforcement_uses_elevated_backend`,
  `windows_spawn_failure_metric_uses_effective_backend`,
  `proxy_enforced_restricted_token_uses_windows_elevated_tag`,
  `proxy_enforced_windows_sandbox_prepares_elevated_filesystem_overrides`,
  `inserts_no_profile_for_proxy_selected_elevated_windows_sandbox`, and
  `unified_exec_proxy_blocks_direct_loopback_bypass_on_windows`.
- Upstream provenance: `4bc2c723ef` introduced proxy-selected elevation for
  direct exec, and `35c2278dd5` now owns the shared native launcher used by
  unified exec and exec-server process spawning. The obsolete downstream
  direct launch block is retired. Preserve the remaining downstream completion
  carry for override preparation, PowerShell startup, metrics, telemetry, and
  effective-backend attribution until those adjacent policies are equivalent
  upstream.
- Primary files:
  - `codex-rs/windows-sandbox-rs/src/lib.rs`
  - `codex-rs/sandboxing/src/spawn.rs`
  - `codex-rs/sandboxing/src/windows.rs`
  - `codex-rs/core/src/sandboxing/mod.rs`
  - `codex-rs/core/src/exec.rs`
  - `codex-rs/core/src/sandbox_tags.rs`
  - `codex-rs/core/src/unified_exec/process_manager.rs`
  - `codex-rs/windows-sandbox-rs/src/unified_exec/mod.rs`

### App-Server Command-Cwd Windows Sandbox Mode

- `command/exec` with `permissionProfile` reloads the trusted project selected
  by the command `cwd`; its Windows sandbox mode travels with its permission
  profile, workspace roots, and network policy.
- Do not replace that reloaded mode with the app-server process's global mode.
  A global disabled mode must not silently disable a command project's explicit
  unelevated sandbox.
- Hosted guardrails:
  `command_exec_permission_profile_project_roots_use_command_cwd` and
  `command_exec_permission_profile_uses_command_cwd_windows_sandbox_mode`.
- Upstream provenance: `8e8fd94c60` introduced the command-cwd reload behavior;
  `4bc2c723ef` accidentally dropped the selected level while changing proxy
  handling. Keep the restoration until upstream restores it.
- Primary files:
  - `codex-rs/app-server/src/request_processors/command_exec_processor.rs`
  - `codex-rs/app-server/tests/suite/v2/command_exec.rs`
  - `codex-rs/app-server/README.md`

### App-Read Plugin Auth Synchronization

- Upstream owns the enriched experimental `app/read` metadata contract through
  `60272096bc`, including plugin display-name projection without MCP startup.
- Downstream plugin app declarations remain gated by the current auth mode.
  Before reading plugins, `app/read` copies the live auth snapshot into
  `PluginsManager`, matching the synchronization already used by neighboring
  plugin list, installed, and read RPCs.
- Ordinary serialized login completion already refreshes the manager before the
  next request on the standard transport. The request-boundary synchronization
  additionally covers embedding-owned or externally updated auth managers and
  keeps the metadata and plugin projections on one snapshot.
- Hosted guardrail:
  `app_read_resynchronizes_plugin_auth_after_external_login_without_starting_mcp`
  in `codex.app-server-v2-contract-targeted`, together with the complete
  `suite::v2::app_read::` group.
- Primary files:
  - `codex-rs/app-server/src/request_processors/apps_processor.rs`
  - `codex-rs/app-server/tests/suite/v2/app_read.rs`
  - `justfile`

### Python Code Quality Corrections

- Downstream carries three upstreamable Python maintenance corrections so
  GitHub Code Quality can verify the corresponding findings are closed on
  `main`.
- The Windows timeout security smoke exceeds the harness deadline, records its
  expected `subprocess.TimeoutExpired`, and allows unexpected failures to
  surface before checking that outside writes remain denied.
- The Unix `just` shell launcher declares its `os.execvp` path as non-returning,
  and the issue-digest collector no longer retains an unused derived local.
- The same maintenance change removes an unused list from the downstream
  divergence audit; that script remains covered by the existing branch-policy
  carry rather than this upstream-fix record.
- Preserve these corrections during upstream syncs until equivalent control
  flow lands upstream. Once it does, drop the redundant patch and this carry
  record.
- Primary files:
  - `codex-rs/windows-sandbox-rs/sandbox_smoketests.py`
  - `scripts/just-shell.py`
  - `.codex/skills/codex-issue-digest/scripts/collect_issue_digest.py`

### First-Party Usage Ledger Ownership

- Downstream keeps usage-ledger ownership in this repo.
- Billing-turn canonicalization and historical AUD reporting remain downstream
  requirements, and the canonical local ledger implementation lives in
  `usage.sqlite` rather than an external sibling repository.
- `codex-rs/state/src/runtime/usage.rs` and
  `codex-rs/state/usage_migrations/0001_usage_tables.sql` do not currently
  have upstream counterparts, so future sync passes should treat them as
  downstream-owned behavior to preserve rather than as stale carry to delete.
- Usage-ledger ownership stays here: any upstream-native reimplementation must
  reproduce the downstream per-turn ledger, provider/token metadata, and
  billing-turn reporting semantics before the canonical source of truth can
  move out of this repository.
- The 2026-07-18 sync adopts upstream's `SqliteConfig` as the single connection
  factory for state, logs, goals, memories, and the downstream usage database.
  Downstream extension migrations, generalized state-migration repair, usage
  telemetry, storage mapping, and failure-path pool cleanup remain additive
  behavior around that upstream-owned connection seam.
- Primary files:
  - `codex-rs/core/src/session/session.rs`
  - `codex-rs/state/src/runtime.rs`
  - `codex-rs/state/src/runtime/usage.rs`
  - `codex-rs/state/usage_migrations/0001_usage_tables.sql`
  - `docs/downstream.md`

### Usage Event Logging And Metadata Capture

- Sessions record a downstream-only usage log database that tracks token,
  provider, tool, and spawn metadata per thread so downstream reporting can
  export a full list of agents, tool calls, weighting/effort metadata, rate-
  limit snapshots, and completion/forging regions for downstream billing and
  audit workflows.
- The new `usage` SQLite DB stores `usage_threads`, `usage_provider_calls`,
  `usage_tool_calls`, `usage_quota_snapshots`, `usage_spawn_requests`, and
  `usage_fork_snapshots`, capturing per-turn requested model/provider hints,
  tool invocation lifecycles, rate-limit snapshots, and parent/child thread
  relationships for spawn requests.
- `usage_provider_calls` also stores provider-confirmed `final_model` and
  `model_snapshot` values when turn completion reports them, preserving the
  downstream distinction between requested/configured model, historical
  `actual_model_used`, and final provider identity. Core turn completion
  captures terminal `ResponseEvent::ServerModelIdentity` values so app-server,
  TUI, and usage-ledger consumers receive provider-confirmed identity instead
  of falling back to `None`.
- Provider calls and fork snapshots retain upstream's prompt-cache write token
  count as a distinct accounting dimension. The existing non-cached input
  total remains input minus cache reads, matching upstream telemetry semantics;
  cache-write usage is stored alongside it rather than silently collapsed.
- Completed thread/list/read and TUI status surfaces prefer thread-local
  provider identity evidence from turn completion or the usage ledger before
  falling back to configured session metadata; active/running threads keep the
  live effective model first so sub-agent status does not regress to the
  parent/session model.
- The 2026-07-18 sync adopts upstream `tui.resume_cwd`, including remembered
  current/session directory choices, explicit `--cd` precedence, remote
  workspace rejection, and persistence-error handling. The same
  `session_resume` module retains downstream model and reasoning-effort
  recovery; future syncs must preserve both behaviors rather than selecting
  one side of that additive seam.
- Primary files:
  - `codex-rs/core/src/session/turn.rs`
  - `codex-rs/core/src/session/turn_context.rs`
  - `codex-rs/core/src/tasks/mod.rs`
  - `codex-rs/app-server/src/request_processors/thread_processor.rs`
  - `codex-rs/core/src/state/service.rs`
  - `codex-rs/protocol/src/protocol.rs`
  - `codex-rs/state/src/lib.rs`
  - `codex-rs/state/src/migrations.rs`
  - `codex-rs/state/src/runtime.rs`
  - `codex-rs/state/src/runtime/usage.rs`
  - `codex-rs/state/usage_migrations/0001_usage_tables.sql`
  - `codex-rs/state/usage_migrations/0003_usage_provider_call_model_identity.sql`
  - `codex-rs/state/usage_migrations/0004_usage_cache_write_tokens.sql`
  - `codex-rs/tui/src/chatwidget/status_surfaces.rs`
  - `codex-rs/tui/src/session_resume.rs`
  - `codex-rs/state/Cargo.toml`

### Side Chat Persistence And Usage Ledger Tracking

- `/side` conversations are persisted as side-tagged fork threads instead of
  pathless ephemeral forks, so they keep rollout transcripts, remain resumable,
  and can be forked by thread id or rollout path.
- Default history/list/search surfaces hide side chats; explicit
  `threadSources: ["side"]` requests expose the side-chat history class.
- Usage-ledger lineage records side forks in `usage_threads` and
  `usage_fork_snapshots`, marks `usage_threads.thread_source = "side"`, and
  writes normal provider-call rows for side turns.
- The upstream-shaped fork presentation is the single TUI authority for the
  fork shape: regular presentation maps to `ThreadSource::User`, while side
  presentation maps to `ThreadSource::Side`, excludes inherited response
  turns, and skips the redundant parent-title lookup. The `/side` caller keeps
  the fork persisted and seeds navigation before delayed `thread/started`
  delivery so selection does not race a second liveness read.
- Fork and fresh-session lifecycle paths adopt upstream's metadata-read
  de-duplication. Resume and picker backfill reuse discovered server status,
  while locally live descendant channels remain authoritative and downstream
  V1 writable versus V2 parent-owned input behavior remains unchanged.
- Forks created from an existing side conversation inherit the side
  `thread_source` unless the caller explicitly supplies a different source,
  keeping nested side-chat forks hidden from default history surfaces and
  marked in usage-ledger lineage.
- `scripts/codex-resume-recent.sh` skips side chats by default, with
  `--include-side` available when an operator deliberately wants side-chat
  resume candidates.
- Primary files:
  - `codex-rs/tui/src/app/side.rs`
  - `codex-rs/tui/src/app/agent_navigation.rs`
  - `codex-rs/tui/src/app/loaded_threads.rs`
  - `codex-rs/tui/src/app/session_lifecycle.rs`
  - `codex-rs/tui/src/app/tests/session_lifecycle_requests.rs`
  - `codex-rs/tui/src/app_server_session.rs`
  - `codex-rs/app-server/src/request_processors/thread_processor.rs`
  - `codex-rs/app-server/src/filters.rs`
  - `codex-rs/state/src/runtime/usage.rs`
  - `codex-rs/state/usage_migrations/0002_usage_thread_source.sql`
  - `scripts/codex-resume-recent.sh`

### App-server Thread Source, History Mode, And Name Compatibility

- Preserve downstream `thread_source` provenance alongside upstream
  `history_mode` metadata in thread listing, summary, resume, persisted
  metadata, and generated protocol schemas.
- These fields are independent dimensions: history storage mode must not erase
  whether a thread came from a side conversation, sub-agent, or another
  attributed source.
- Upstream paginated-thread compatibility views are adopted: full read/resume
  history, `initialTurnsPage`, and item/turn listing hydrate the stored
  `ThreadItem` projection rather than rebuilding paginated context through the
  legacy event-history path. That upstream ownership must continue to preserve
  downstream `thread_source` alongside `history_mode`.
- Upstream state-backed thread names are likewise additive metadata. Named
  paginated threads must preserve `thread_source`, `history_mode`, and `name`
  together across read, state-only list, and metadata-only resume responses.
- Upstream paginated Git metadata updates and paginated memory eligibility are
  also adopted. SQLite-only Git patches and memory-mode reconciliation must
  remain additive to downstream `thread_source`; none may coerce a paginated
  thread back to legacy history or erase its name or provenance.
- Primary files:
  - `codex-rs/protocol/`
  - `codex-rs/rollout/`
  - `codex-rs/state/`
  - `codex-rs/thread-store/`
  - `codex-rs/app-server-protocol/`
  - `codex-rs/app-server/tests/suite/conversation_summary.rs`
  - `codex-rs/app-server/tests/suite/v2/thread_read.rs`

### Private Configured Thread Identity Provenance Contract

- Keep configured-identity provenance storage out of generic `ThreadMetadata`.
  SQLite owns the private tri-state fact, while `codex-state` exposes only the
  typed `Unknown`, `KnownAbsent`, or `Present` enum plus three narrow
  `StateRuntime` read/transition methods. `Unknown` must never be interpreted
  as evidence of absence.
- Migration `0045` defaults existing rows and old-binary-shaped inserts to
  `Unknown`. StateRuntime permits only atomic forward transitions:
  `Unknown -> KnownAbsent`, `Unknown -> Present`, and
  `KnownAbsent -> Present`; mutation methods report a missing thread row
  explicitly rather than treating it as an idempotent transition.
- Generic thread-metadata inserts and upserts deliberately omit the private
  column, so unrelated metadata writes cannot reset or fabricate provenance.
- Provenance and collision-repair fixtures use upstream's on-disk
  `SqliteConfig` test topology. The writer-slot guard holds `BEGIN IMMEDIATE`
  through a writable pool while generalized repair reads through a separate
  read-only pool, proving a no-op repair does not need the writer slot.
- This stage does not classify rollout events, reconstruct history, store
  configured identity values, enforce precedence, or synchronize live state.
- Primary files:
  - `codex-rs/state/migrations/0045_threads_configured_identity_provenance.sql`
  - `codex-rs/state/src/migrations_tests.rs`
  - `codex-rs/state/src/runtime/configured_identity_provenance.rs`
  - `codex-rs/state/src/runtime/configured_identity_provenance_tests.rs`

### Phase-2 Memory Attestation And Prepared-Input Fingerprinting

- Downstream phase-2 memory consolidation remains fail-closed once attestation
  support has been initialized for a memory root.
- Consolidated memory artifacts are fingerprinted against the prepared input
  tree, the effective consolidator contract, and the output tree, then recorded
  in runtime state so unchanged workspaces can safely reuse existing outputs
  while drifted or tampered artifacts are rejected after bootstrap.
- This is an intentional downstream carry, not derivative test churn: losing
  the attestation runtime while keeping the attestation tests is a regression.
- Because these downstream state migrations occupy slots that upstream did not
  have at the time they were introduced, later upstream migrations may need to
  be replayed into the next free downstream migration version while preserving
  their SQL content. Current examples include upstream's device-key binding
  table (`0028_device_key_bindings.sql` upstream, `0031_device_key_bindings.sql`
  downstream) and upstream's thread-goals table (`0029_thread_goals.sql`
  upstream, `0032_thread_goals.sql` downstream), avoiding collisions with the
  already-shipped downstream `0028` through `0031` migration versions. When an
  upstream sync collides with already-shipped downstream state migration
  versions, keep upstream migration numbers when possible and move downstream
  additive carry to the next free version with checksum-gated runtime repair
  for databases that already recorded the old version. The current example is
  preserving upstream `0040_threads_history_mode.sql` while moving downstream
  visible-thread sort indexes to `0044_threads_visible_sort_indexes.sql`.
  The 2026-07-20 sync likewise preserves upstream
  `0041_threads_name.sql` and moves the previously renumbered
  remote-control-enabled migration from downstream slot `0041` to
  `0046_remote_control_enrollments_enabled.sql`; checksum-gated repair moves
  already-applied downstream `0041` records before the upstream migration runs.
  The same sync preserves upstream `0042_drop_agent_jobs.sql` byte-for-byte and
  moves the already-shipped downstream external-agent import ledger from slot
  `0042` to `0047_external_agent_config_imports.sql`. The runtime repairs exact
  known downstream checksums for both `0041` and `0042` before SQLx applies the
  upstream migrations. The `0042` upgrade deliberately removes only the
  retired CSV coordinator's `agent_jobs` and `agent_job_items` tables; ordinary
  threads, spawn edges, and external-agent import records remain intact.
  Once that repair and upstream `0041` have run, a pre-sync binary cannot reopen
  the same database because it knows the old `0041` and `0042` checksums;
  rollback therefore requires a pre-upgrade database copy rather than only
  replacing the binary.
- Upstream `thread_history_migrations/0002_thread_items_item_type.sql` belongs
  to the separate rebuildable `thread_history_1.sqlite` migrator. It does not
  collide with downstream state migration `0045`, and must not trigger state
  migration renumbering or checksum repair.
- Primary files:
  - `codex-rs/memories/write/src/phase2.rs`
  - `codex-rs/memories/write/src/phase2_attestation.rs`
  - `codex-rs/memories/write/src/phase2_attestation_tests.rs`
  - `codex-rs/memories/write/src/startup_tests.rs`
  - `codex-rs/state/src/migrations.rs`
  - `codex-rs/state/src/migrations_tests.rs`
  - `codex-rs/state/src/runtime/migration_repair.rs`
  - `codex-rs/state/src/runtime/phase2_attestation.rs`
  - `codex-rs/state/migrations/0024_phase2_attestation_roots.sql`
  - `codex-rs/state/migrations/0038_phase2_attested_baselines.sql`
  - `codex-rs/state/migrations/0031_device_key_bindings.sql`
  - `codex-rs/state/migrations/0032_thread_goals.sql`
  - `codex-rs/state/migrations/0044_threads_visible_sort_indexes.sql`
  - `codex-rs/state/migrations/0046_remote_control_enrollments_enabled.sql`
  - `codex-rs/state/migrations/0047_external_agent_config_imports.sql`
  - `docs/memories.md`

### Release Metadata And Rebuild Triggers

- Release builds embed canonical release identity plus compact provenance
  metadata.
- Version metadata rebuilds when git state changes, including shared worktree
  git state.
- Primary files:
  - `codex-rs/utils/version/build.rs`
  - `codex-rs/utils/version/src/lib.rs`
  - `codex-rs/cli/src/main.rs`

### Sub-agent selection compatibility, inventory metadata, and wait joins

- Upstream owns configured default and explicit
  `spawn_agent(model=..., reasoning_effort=...)` child selection, applies role
  settings after that selection, and validates the final effective model and
  reasoning pair.
- The 2026-07-16 sync adopts upstream's unified `[agents]` configuration,
  canonical `max_concurrent_threads_per_session` key, legacy `max_threads`
  normalization, flattened role declarations, and conditional `agent_type`
  exposure when roles are configured. Configured default sub-agent model and
  reasoning settings are now active upstream behavior rather than downstream
  carry.
- Downstream's Schemars 1.2 adapter intentionally leaves schema-only
  `deny_unknown_fields` off the flattened `AgentsToml` object so arbitrary
  `[agents.<role>]` keys remain validated as `AgentRoleToml`. A semantic schema
  regression locks that role-valued `additionalProperties` contract instead of
  relying only on generated-fixture equality.
- Spawn-agent tool guidance should follow upstream's authorization wording that
  a user request or applicable `AGENTS.md`/skill instruction can authorize
  delegation, and should keep upstream's warning that `model` overrides are
  exceptional. Downstream additionally keeps the guardrail that requests for
  depth, thoroughness, research, investigation, or detailed codebase analysis
  do not by themselves authorize spawning.
- Keep downstream itineraries that explicitly request a child model/economy
  aligned with the upstream selection and role precedence pipeline.
- Roles remain authoritative when they set `model`, `model_provider`,
  `model_reasoning_effort`, or `model_verbosity`.
- The remaining role carry preserves the resolved runtime provider object plus
  `model_reasoning_summary` and verbosity when a role does not replace them.
  Downstream also treats an empty supported-reasoning list as unknown rather
  than rejecting an otherwise selectable model.
- Models whose MultiAgentV2 backend assignment is unspecified remain selectable;
  a known, different backend still fails closed. The targeted role regression
  proves the provider object, reasoning summary, and verbosity together rather
  than checking only the selected model slug.
- `gpt-5.6-luna` is the narrow exception: downstream advertises and accepts it
  as a V2 child while the upstream-synced catalog still selects V1 for Luna
  top-level sessions. Remove this compatibility rule once upstream metadata
  marks Luna V2-compatible; do not widen it to other v1-tagged models.
- `core/src/agent/role.rs` stays on the upstream-native layered reload shape;
  downstream no longer carries a duplicate model/reasoning selection pipeline.
- The live tool-contract schema in
  `codex-rs/core/src/tools/handlers/multi_agents_spec.rs` and
  `codex-rs/core/src/tools/spec_plan.rs`, plus the regression suite in
  `codex-rs/core/src/tools/handlers/multi_agents_tests.rs`, are already back
  on upstream-native shape; the remaining carry is
  concentrated in role application, descendant inventory, spawn result
  metadata, wait summaries, and `agent/control.rs`.
- The historical `spawn_approval` argument was unused by both spawn handlers;
  the upstream removal is retained rather than carried as a phantom contract.
- MultiAgentV2 keeps the configurable `agents` namespace and the shared usage
  hint that distinguishes direct agent tool calls from `functions.exec` tools.
  This is intentional downstream routing behavior, not a reason to retain old
  handler implementations. Description tests derive the effective namespace
  from configuration rather than hard-coding either upstream or downstream
  defaults. Namespace-sensitive upstream fixtures must likewise use the
  effective downstream namespace rather than assuming upstream's default.
- A delegate whose cancellation token is already cancelled returns
  `TurnAborted` before allocating channels or spawning a child session; the
  subsequent cancellation-aware spawn remains responsible for races after
  that initial boundary.
- The 2026-07-15 sync adopts upstream's removal of `last_task_message` and
  `last_task_message_preview` from `list_agents` and `inspect_agent_tree`.
  Descendant counts, status, role, nickname, and live/stale structure remain,
  but inventory output does not expose instruction content.
- The v1 spawn result retains upstream `agent_id`/`nickname`. The v2 result exposes canonical `task_name`, conditionally visible `agent_id`/`nickname`, and the requested/effective model and reasoning fields after role application. Role, status, identity source, provider ID, and reasoning summary remain inventory or internal metadata rather than spawn-result fields.
- V2 requires `task_name`; when no effective reasoning effort is known it
  serializes `null` rather than manufacturing a `medium` value. Wait completion
  derives pending target ids from refreshed status snapshots, not the original
  requested-id list.
- `list_agents` is a first-class inventory tool on `carry/main`: the live handler is already on the upstream `multi_agents_v2` path, and the stale downstream `multi_agents/list_agents.rs` copy was dead carry rather than active behavior.
- The remaining inventory divergence is therefore not a separate handler path; it is the extra descendant and persisted edge-status plumbing available from `agent/control.rs`, which still needs to be re-homed onto the upstream-native v2 inventory shape rather than dropped.
- Downstream policy is to preserve the intent of the live carry while keeping the tree as close to upstream as possible; we explicitly carry the always-on, cheap live `list_agents` surface (including `has_active_subagents`/`active_subagent_count` and nested visibility/status metadata) to keep nested-agent live visibility intact, pair it with a richer, potentially stale `inspect_agent_tree` surface for deeper inventory sweeps, and welcome upstream-native reimplementation whenever it preserves these behaviors with less divergence.
- `inspect_agent_tree` now surfaces the richer tree inspection contract: it can toggle `live` vs `stale` descendant visibility, focus on selected `agent_roots`, and returns compact depth/row-limited tree rows so downstream observability stays explicit without replaying bulky historical snapshots.
- `wait_agent` adds `return_when=any|all` plus `requested_ids`, `pending_ids`,
  `completion_reason`, and `timed_out` so downstream joins happen on explicit
  tool contracts rather than transcript polling. These completion fields are
  public tool-output-only; canonical transcript items retain target identities
  and agent-state snapshots without duplicating timeout, mailbox, or pending
  outcome state. The v2 schema also permits omitting `targets` when the caller
  intentionally wants a current-turn input-activity wait, including mailbox
  delivery or user steering, or timeout.
- Full-history forks preserve conversation and agent identity while accepting
  configured or explicit child model/reasoning selection; only `agent_type`
  remains invalid for that fork shape.
- Cold V2 descendant reloads preserve the child's indexed agent path, model,
  provider, and reasoning effort rather than inheriting the resumed root's
  selection. Rollout previews may supply history and display context, but
  cannot overwrite the complete indexed identity used to reload the child.
  Legacy rows with no
  indexed model identity retain their rollout model and effort, while a
  populated indexed model makes an absent indexed effort an intentional clear.
- Capacity-triggered V2 residency eviction first materializes and shuts down an
  unloadable quiescent thread, then removes only the exact `Arc<CodexThread>` it
  examined. A concurrently installed replacement is never removed by a stale
  eviction attempt.
- The evicted identity retains only a bounded `Completed`, `Errored`, or
  `Interrupted` status behind the current registry generation. Status queries
  and newly created subscriptions expose that cold snapshot, failed reloads
  keep it, and a successful reload clears it. Bridging an already-open cold
  subscription across reload remains separate follow-up work.
  This is a manual port of the useful mechanism from draft upstream PR #30154
  onto the current tree, extended to keep interrupted identities observable and
  reloadable.
- Each registered agent generation now owns one serialized lifecycle authority
  for V2 unload, reload, message delivery, and explicit close. Queue-only mail
  sent to an unloaded agent remains in a registry-owned FIFO without starting a
  runtime. Reloads serialize on a gate that eviction never acquires; residency
  reservation releases the mailbox lock and rechecks the registry generation
  before reload. A triggering follow-up transfers the FIFO first and retains it
  if reload fails.
- Residency eviction may move pending queue-only mail out of a completed,
  errored, or interrupted runtime instead of pinning that runtime indefinitely.
  Triggering mail remains live and blocks eviction; failed shutdown restores
  the exact drained FIFO, identity-checked replacement races receive the FIFO,
  and explicit close discards cold mail under the same lifecycle authority.
  Preserve this direct fix for upstream issue #32353 until upstream provides an
  equivalent generation-safe lifecycle and mailbox-transfer contract.
- Eviction fails closed for ephemeral V2 sessions without a durable rollout;
  capacity pressure leaves that runtime resident rather than manufacturing a
  cold identity that persisted-history reload cannot restore.
- The built-in downstream awaiter profile also raises its default background timeout and prefers longer blocking waits plus `list_agents` snapshots over repeated short polling from the model layer. The built-in `terminal-babysitter` role deliberately locks `gpt-5.4-mini` with low reasoning for bounded monitored-wait seams.
- Primary files:
  - `codex-rs/core/src/agent/builtins/awaiter.toml`
  - `codex-rs/core/src/agent/builtins/terminal-babysitter.toml`
  - `codex-rs/core/src/agent/control.rs`
  - `codex-rs/core/src/agent/control/legacy.rs`
  - `codex-rs/core/src/agent/control/lifecycle.rs`
  - `codex-rs/core/src/agent/control/residency.rs`
  - `codex-rs/core/src/agent/control/residency_tests.rs`
  - `codex-rs/core/src/agent/control/spawn.rs`
  - `codex-rs/core/src/agent/control_tests.rs`
  - `codex-rs/core/src/agent/lifecycle.rs`
  - `codex-rs/core/src/agent/mod.rs`
  - `codex-rs/core/src/agent/registry.rs`
  - `codex-rs/core/src/agent/registry_tests.rs`
  - `codex-rs/core/src/agent/role.rs`
  - `codex-rs/core/src/agent/role_tests.rs`
  - `codex-rs/config/src/config_toml.rs`
  - `codex-rs/core/config.schema.json`
  - `codex-rs/core/src/codex_delegate.rs`
  - `codex-rs/core/src/config/mod.rs`
  - `codex-rs/core/src/config/schema_tests.rs`
  - `codex-rs/core/src/session/input_queue.rs`
  - `codex-rs/core/src/session/mod.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents_v2/list_agents.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents_v2/message_tool.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents/spawn.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents/wait.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents_common.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents_tests.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents_spec.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents_spec_tests.rs`
  - `codex-rs/core/src/tools/spec_plan.rs`
  - `codex-rs/core/src/tools/tool_runtime_capabilities.rs`
  - `codex-rs/core/src/thread_manager.rs`
  - `codex-rs/core/tests/suite/agent_execution.rs`
  - `codex-rs/core/tests/suite/spawn_agent_description.rs`
  - `codex-rs/core/tests/suite/multi_agent_resume.rs`
  - `codex-rs/core/tests/suite/subagent_notifications.rs`
  - `codex-rs/thread-store/src/local/read_thread.rs`
  - `.github/scripts/test_ci_planners.py`
  - `justfile`
  - `docs/config.md`
  - `docs/downstream-tool-surface-matrix.md`

### Dead-Cwd Absolute Path Handling

- `AbsolutePathBuf::from_absolute_path()` avoids consulting process cwd for
  already-absolute inputs.
- This preserves path resolution after cwd disappears.
- Primary files:
  - `codex-rs/utils/absolute-path/src/lib.rs`
  - `codex-rs/utils/absolute-path/tests/dead_cwd.rs`

### Session Environment And Thread-Tail State

- Session environment updates validate duplicate and unknown environment ids
  before mutating stored session state.
- When a session cwd/environment update changes the legacy fallback cwd,
  the current upstream explicit-environment cwd and workspace-root semantics
  are authoritative. The superseded fallback-cwd rewrite implementation is not
  carried merely because it previously conflicted.
- Default turns refresh runtime `ThreadEnvironments` from stored selections so
  explicit empty or non-fallback stored environments are honored.
- Mailbox deferral must not overtake explicit steered user input, while
  response-only queued items may still defer after an answer boundary.
- Legacy active turns that only contain `UserMessageEvent` tails are still
  treated as mid-turn so replay/fork state does not discard the active start.
- Upstream `PermissionsState` owns permission-instruction diffing and retained
  fragment matching. Downstream custom realtime-start instructions remain the
  earlier world-state contribution relative to permissions, including remote
  compaction and resume reconstruction, so adopting upstream ownership does not
  silently replace the configured realtime wording.
- Upstream V3 `initialItems` seed the realtime backend's initial history
  independently. A nonempty seed must not replace, reorder, or suppress the
  downstream custom realtime-start world-state instruction on the next Codex
  request.
- Primary files:
  - `codex-rs/core/src/context/world_state/permissions.rs`
  - `codex-rs/core/src/session/input_queue.rs`
  - `codex-rs/core/src/session/mod.rs`
  - `codex-rs/core/src/session/session.rs`
  - `codex-rs/core/src/session/turn_context.rs`
  - `codex-rs/core/src/session/world_state.rs`
  - `codex-rs/core/tests/suite/compact_remote.rs`
  - `codex-rs/core/src/thread_manager.rs`

### Blocking Unified-Exec Waits And Compaction-Aware Turn Completion

- `exec_command` and `write_stdin` support blocking wait semantics via
  `wait_until_terminal`, `max_wait_ms`, and `heartbeat_interval_ms`.
- `write_stdin` still requires an empty `chars` payload when
  `wait_until_terminal=true`.
- Spec and regression guardrails also cover the surfaced wait fields, reject
  invalid `wait_until_terminal` types, and enforce the empty-`chars`
  requirement for blocking `write_stdin`.
- Timeout notes are appended to returned `raw_output`.
- Canonical command-execution items retain optional live `terminal_wait`
  metadata so upstream item-lifecycle adapters reproduce it on legacy begin
  and end events and app-server v2 projects it into command items. History
  reconstruction uses `None` when older persisted items do not contain that
  live-only detail.
- TUI command cells use `duration` rather than output presence as completion
  authority, so streamed output deltas remain active. Interrupting an
  unfinished command preserves its streamed output while marking it failed,
  and downstream `terminal_wait` metadata still drives the active wait label.
- The downstream intent is to absorb long-running shell waits in the tool layer
  instead of spending model turns on repeated short-poll status checks.
- Code-mode nested `exec_command` output follows the same model-policy bounded
  unified-exec summary shape before JavaScript observes `result.output`; do not
  restore raw large-output preservation expectations in code-mode tests when
  the tool response already carries truncation warning headers.
- Remote unified-exec command resolution must keep the session/user shell for
  commands that omit `shell`, while matching explicit aliases such as
  `powershell` reuse the selected environment shell instead of resolving that
  alias on the host running Codex.
- Code mode may expose the read-only `get_context_remaining` helper so scripts
  can inspect remaining budget, but interactive direct-model-only tools such as
  `request_user_input` remain hidden from nested execution.
- In local downstream workflows, this composes with existing blocking
  coordination primitives such as `wait_agent` and helper-backed `*_and_wait`
  calls so joins happen on state transitions rather than transcript churn.
- This blocking MCP tool pattern was carried downstream before task support was
  fully operational.
- `TurnCompleteEvent` retains upstream's optional structured terminal `error`
  payload alongside downstream `compaction_events_in_turn`, `final_model`, and
  `model_snapshot` metadata. Future conflict resolution must keep this additive
  union rather than choosing either field set.
- Token-count events also carry provider and model context in downstream flow.
- Sub-agent delegate forwarding should continue to surface `TokenCount` events
  back to the parent session; preserve this behavior even when re-homing the
  delegate code onto newer upstream structure.
- Provider `ServerOverloaded` responses use cancellable capacity-retry backoff
  for sampling, inline compaction, and remote compaction instead of terminating
  the turn immediately. Preserve the `capacity_retry` loops in
  `session/turn.rs`, `compact.rs`, and `compact_remote.rs` during upstream
  syncs.
- Primary files:
  - `codex-rs/core/src/capacity_retry.rs`
  - `codex-rs/core/src/session/turn.rs`
  - `codex-rs/core/src/compact.rs`
  - `codex-rs/core/src/compact_remote.rs`
  - `codex-rs/core/src/tools/spec_plan.rs`
  - `codex-rs/core/src/tools/handlers/unified_exec.rs`
  - `codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs`
  - `codex-rs/core/src/tools/events.rs`
  - `codex-rs/app-server/src/bespoke_event_handling.rs`
  - `codex-rs/app-server-protocol/src/protocol/v2/item.rs`
  - `codex-rs/core/tests/suite/code_mode.rs`
  - `codex-rs/core/tests/suite/remote_env.rs`
  - `codex-rs/core/tests/suite/unified_exec.rs`
  - `codex-rs/protocol/src/items.rs`
  - `codex-rs/protocol/src/legacy_events.rs`
  - `codex-rs/protocol/src/protocol.rs`
  - `codex-rs/core/src/session/mod.rs`
  - `codex-rs/tui/src/chatwidget/command_lifecycle.rs`
  - `codex-rs/tui/src/chatwidget/tests/app_server.rs`
  - `codex-rs/tui/src/exec_cell/model.rs`
  - `codex-rs/tui/src/exec_cell/render.rs`
  - `docs/downstream.md`
  - `docs/downstream-regression-matrix.md`

### Source-Owned Unified-Exec Final Transcript

- Unified exec records each source output chunk in a bounded, non-draining
  final transcript before offering it to the drainable response buffer or the
  best-effort delta broadcast.
- Normal command completion waits for the local stdout/stderr sources or the
  exec-server event source to close, then drains already-published delta
  messages before emitting `ExecCommandEnd`.
- Source draining is bounded by the established exec I/O drain timeout so a
  daemonized descendant or broken backend cannot suppress the final event.
  Chunks recorded before that deadline remain authoritative; output produced
  only after the deadline is intentionally outside the completed transcript.
- Exec-server exit events start that bounded drain immediately even when
  inherited descriptors delay the later source-closed event.
- The bounded transcript intentionally retains a head and tail and represents
  discarded middle bytes with an omission marker; this carry prevents delta
  lag from silently losing final output or corrupting that accounting.
- Drop this direct upstream fix when upstream provides equivalent source-owned
  bounded final aggregation, bounded source-close ordering, and regression
  coverage.
- Primary files:
  - `codex-rs/core/src/unified_exec/process.rs`
  - `codex-rs/core/src/unified_exec/async_watcher.rs`
  - `codex-rs/core/src/unified_exec/process_manager.rs`
  - `codex-rs/core/src/unified_exec/process_manager_tests.rs`
  - `codex-rs/core/src/unified_exec/process_output_tests.rs`
  - `codex-rs/core/tests/suite/unified_exec.rs`
  - `justfile`
  - `.github/validation-lanes.json`

### App-Server Remote Control Account Wake

- Remote-control enrollment waits must wake when cached ChatGPT auth changes
  the account id, not only when refresh-token material changes.
- `AuthManager::auth_change_receiver()` is therefore account-scoped for
  request recovery, while `auth_changed_for_refresh` remains the narrower
  token-refresh decision.
- This prevents remote control from sleeping until the retry interval after an
  account-id-only reload, and keeps `UnauthorizedRecovery` aligned with the
  fresh auth state before reconnect/enroll attempts.
- Primary files:
  - `codex-rs/app-server-transport/src/transport/remote_control/websocket.rs`
  - `codex-rs/login/src/auth/manager.rs`

### Native Computer-Use Adapter Bridge

- Downstream promotes bare `android_observe`, `android_step`,
  `android_install_build_from_run`, `browser_observe`, `browser_step`,
  `desktop_observe`, and `desktop_step` dynamic tools into first-party native
  computer-use function tools with Codex-owned schemas.
- The Android reference bridge exposes atomic two-to-five-pointer
  `multi_touch` input through `android.input.multi_touch`, validates the whole
  gesture before dispatch, and reports an explicit unsupported-capability
  failure instead of synthesizing sequential single-touch input.
- Namespaced Android-like, browser-like, or desktop-like tools remain ordinary dynamic tools
  so app-specific providers can keep their own tool surfaces without taking
  over the native Codex contract.
- App-server `dynamicTools` accepts a deferred bare native tool so it can be
  discovered through `tool_search`; the same bare deferred shape remains
  invalid for ordinary dynamic tools. A capability-bearing native tool forces
  a loaded-thread resume reload because its provider contract may have changed.
- This sync intentionally retains the flat `DynamicToolSpec` compatibility
  shape (`namespace` plus function metadata) because app-server requests,
  persisted SQLite rows, provider registries, and resume filtering still share
  that representation. Upstream's tagged function/namespace model should land
  only through the tracked Dynamic Tools alignment work with lossless legacy
  ingestion and state migration, not as an incidental conflict resolution.
- `codex-core` owns `ComputerUseCallRequest` and
  `ComputerUseCallResponse` events, pending response registration, timeout
  cleanup, success/error projection, adapter selection, mutating
  classification, install-specific timeout selection, and hook payload
  formatting.
- App-server API v2 owns `item/computerUse/call`, response forwarding, and
  `ThreadItem::ComputerUseCall` start/completion projection.
- Dispatch resolves the primary environment from the refreshed per-step
  snapshot, not the frozen turn-start snapshot, so a provider that becomes
  ready after turn creation is immediately available to native computer-use.
- The active TUI session renders native computer-use items from live protocol
  events. Computer-use events are transient, so thread history and snapshots do
  not replay them after resume. The TUI provider registry handles Android and
  routes browser calls to either a configured provider command or the built-in
  Playwright provider for `backend=auto`; when that browser provider is
  configured, CLI/TUI thread start, resume, and fork requests advertise
  `browser_observe` and `browser_step` automatically. The Playwright bridge
  supports accessibility-oriented selectors plus human-like mouse and keyboard
  primitives, defaults to per-thread browser profile isolation for concurrent
  sidecars, can return visible-control metadata and selector candidates for UX
  loops, can save redacted audit artifacts, can use locally configured
  service-account navigation headers for allowed hosts, and can still be
  configured for shared, environment-scoped, or per-call profiles when that
  lifecycle is intentional. Thread-spawned agents
  inherit the parent thread's native dynamic tools, so browser-capable sidecars
  receive the native browser surface rather than silently dropping to a
  compatibility adapter.
- The Android adapter is retained as the reference MCP-backed runtime provider:
  reuse `android-emulator-mcp` or a successor when it exposes the current
  Android MCP contract, and adapt harness-specific behavior provider-side
  rather than in hot Codex core paths.
- The desktop adapter is the cleanroom provider seam for macOS Screen
  Recording/Accessibility-style runtimes and future native desktop providers.
  TUI dispatch stays behind an operator-configured command provider.
- `codex doctor` includes read-only native provider diagnostics for browser
  provider configuration, headed display/Chrome fields, and Android provider
  endpoint/credential shape without launching browsers, connecting to profiles,
  or starting emulator sessions.
- Android screenshots and browser viewport captures are expected to reach the
  model as native image content items. Provider artifact paths are kept for
  diagnostics, audit, or replay; they are not the normal model-facing visual
  channel.
- Nested code-mode calls retain native computer-use responses as a typed
  `{ content, success }` object. Callers forward an `input_image` item with
  `image(...)` and never serialize the complete image-bearing result to text.
  Failed provider responses preserve the same typed content contract, including
  diagnostic text and any screenshot returned with the failure.
- Upstream dynamic-tool and code-mode audio output is adopted as
  `InputAudio { audio_url }` and the `audio(...)` helper. Upstream now also owns
  audio preparation, duration-aware context accounting, output truncation,
  compaction, and replay through user and tool history. Those paths are
  additive to the downstream optional `detail` carried on dynamic
  `InputImage`; app-server events, stored thread items, generated schemas, and
  JSONL previews preserve both. The two upstream dynamic-tool audio fixtures
  use downstream's flat `DynamicToolSpec` compatibility record without
  restoring the tagged namespace shape. Regenerate Cargo and Bazel locks from
  the merged dependency graph rather than unioning parent entries: upstream's
  Symphonia lock can name a `bitflags` version that the downstream graph has
  already superseded. Native computer-use responses remain text/image-only and
  keep their fail-loud screenshot contract.
- Computer-use events remain transient in every history mode; live rollout
  tracing maps them to tool-runtime start/end boundaries without writing them
  into thread snapshots.
- Occurrence search continues to index only user messages and final agent
  messages. Its exhaustive `ThreadItem` classification explicitly excludes
  `ComputerUseCall` alongside other tool-call items, preserving the upstream
  search contract without dropping downstream enum coverage at compile time.
- Runtime providers own Android sessions, browser sessions, screenshots,
  viewport capture, UI digests, input execution, and provider-side build
  installation. Solar Gravity Lab is a proving and consumer app, not the
  generic owner of Codex computer-use tooling.
- The built-in Playwright browser provider clears Chromium tab-session restore
  artifacts before opening a persistent browser context, so stale restored tabs
  cannot hit an old localhost target before an explicit requested navigation.
  Provider-managed `state.json`, cookies, local storage, and other profile data
  are preserved.
- Plugin app declarations are validated on the authenticated ChatGPT app
  projection. The unauthenticated plugin projection may intentionally omit apps,
  so future syncs should not treat app absence from unauthenticated manager
  tests as evidence that downstream app/plugin carry is removable.
- Primary files:
  - `codex-rs/protocol/src/computer_use.rs`
  - `codex-rs/protocol/src/protocol.rs`
  - `codex-rs/tools/src/android_tool.rs`
  - `codex-rs/tools/src/browser_tool.rs`
  - `codex-rs/tools/src/computer_use_tool.rs`
  - `codex-rs/tools/src/desktop_tool.rs`
  - `codex-rs/core-plugins/src/lib.rs`
  - `codex-rs/core/src/tools/handlers/computer_use.rs`
  - `codex-rs/core/src/tools/handlers/computer_use_code_mode.rs`
  - `codex-rs/code-mode-protocol/src/description.rs`
  - `codex-rs/core/tests/suite/code_mode.rs`
  - `.github/validation-lanes.json`
  - `.github/scripts/test_ci_planners.py`
  - `justfile`
  - `codex-rs/tools/src/tool_search.rs`
  - `codex-rs/app-server/src/computer_use.rs`
  - `codex-rs/app-server/src/bespoke_event_handling.rs`
  - `codex-rs/app-server-protocol/src/protocol/common.rs`
  - `codex-rs/app-server-protocol/src/protocol/v2/item.rs`
  - `codex-rs/app-server-protocol/src/protocol/thread_history.rs`
  - `codex-rs/thread-store/src/local/thread_history/search.rs`
  - `codex-rs/tui/src/android_computer_use_provider.rs`
  - `codex-rs/browser-computer-use/src/lib.rs`
  - `codex-rs/browser-computer-use/src/browser_playwright_provider.mjs`
  - `codex-rs/tui/src/browser_computer_use_provider.rs`
  - `codex-rs/tui/src/computer_use_provider.rs`
  - `codex-rs/tui/src/desktop_computer_use_provider.rs`
  - `codex-rs/exec/src/lib.rs`
  - `codex-rs/tui/src/app/app_server_requests.rs`
  - `codex-rs/tui/src/app/app_server_events.rs`
  - `codex-rs/tui/src/chatwidget.rs`
  - `codex-rs/tui/src/chatwidget/interrupts.rs`
  - `codex-rs/tui/src/history_cell/computer_use.rs`
  - `codex-rs/rollout/src/policy.rs`
  - `codex-rs/rollout-trace/src/protocol_event.rs`
  - `codex-rs/app-server/tests/suite/v2/computer_use.rs`
  - `codex-rs/tools/src/android_tool_tests.rs`
  - `codex-rs/tools/src/browser_tool_tests.rs`
  - `codex-rs/tools/src/computer_use_tool_tests.rs`
  - `docs/native-computer-use.md`
  - `docs/native-computer-use-cleanroom.md`

### Review And History Accounting Alignment

- Review and history token summaries use a stable unavailable fallback string
  instead of ad hoc formatting.
- Review-mode accounting remains aligned with live runtime state rather than
  stale defaults.
- Primary files:
  - `codex-rs/protocol/src/protocol.rs`
  - `codex-rs/core/src/guardian/review_session.rs`
  - `codex-rs/core/src/context_manager/history.rs`
  - `docs/downstream.md`

### Direct And App-Backed MCP Tool Catalog Reconciliation

- When a direct MCP server and an app connector expose the same logical
  callable, Codex compares the complete model-visible contract before reducing
  the catalog to one route.
- Exact matches prefer the direct MCP route and make that routing decision
  visible in the namespace description. Direct-only and app-only environments
  remain unchanged, so the app route is still a working fallback.
- A difference in descriptions, input or output schemas, safety annotations,
  or task execution keeps both routes visible, labels both namespaces, and
  emits a structured warning. Routing names, connector metadata, icons, and
  server scheduling policy are provenance rather than callable-contract fields.
- Primary files:
  - `codex-rs/core/src/mcp_tool_exposure.rs`
  - `codex-rs/core/src/mcp_tool_exposure_test.rs`
  - `.github/scripts/test_ci_planners.py`
  - `.github/validation-lanes.json`
  - `.github/workflows/sedna-heavy-tests.yml`
  - `justfile`
  - `docs/downstream-tool-surface-matrix.md`

### Complete MCP Tool Catalogue Collection And Refresh

- MCP pagination and host-side deferred loading are separate contracts. Codex
  drains every non-null opaque `tools/list` cursor, including the empty string,
  before publishing a catalogue for ordinary exposure or deferred tool search.
- Collection fails closed on cursor cycles, duplicate tool names, more than 64
  pages, or more than 10,000 tools. A partial walk is never published.
- Tool-list change notifications advance a generation. Codex discards and
  retries a walk that crosses generations, atomically swaps only a complete
  replacement, and retains the last complete snapshot when refresh fails.
- The 2026-07-15 sync re-homes this contract on upstream's
  `connector_runtime` and `tool_catalog_cache` ownership. Startup, hard-refresh,
  and list-changed fetches publish the raw complete catalogue to the applicable
  shared cache before per-client filtering; the removed `codex_apps_cache.rs`
  implementation is not carried.
- Upstream commits `3307ea8b63` and `1bbdb32789` are adopted as complementary
  cache safety: a server can disable shared tool-catalog caching, remotely
  sourced environment variables bypass that cache, and cached definitions do
  not substitute stale process metadata or output for the live connection.
  These guarantees narrow the carry but do not replace downstream's bounded
  pagination and atomic `list_changed` refresh contract.
- The 2026-07-18 sync also adopts upstream commit `f24e695470`'s thread-level
  `McpRuntime` ownership. `McpRuntime` is the sole owner of live connections;
  immutable `McpRuntimeSnapshot` values pair projected config with the
  connection set used by an in-flight step, so refresh must not eagerly shut
  down the previous snapshot. Downstream catalogue pagination, OAuth backend,
  safety policy, and environment-scoped projection continue through this
  upstream-owned runtime rather than a second manager mirror.
- Centralized ownership does not by itself unload quiescent threads retained by
  `ThreadManager`. Capacity-triggered V2 residency eviction now calls
  `shutdown_and_wait()` before generation-fenced, thread-instance-checked
  removal so cloned MCP resource clients do not survive that eviction path.
  Timed idle eviction remains follow-up work and must retain the capacity path's
  existing active-turn and pending-mailbox guards; idle timestamps,
  configuration, and operator observability remain outside this bounded slice.
- The Streamable HTTP regression performs deferred `tool_search` for a tool
  supplied only on page two, invokes that tool, and verifies its output.
- Preserve this carry until upstream issue #26094 is resolved by behavior that
  covers the complete bounded snapshot and refresh contract, not only a basic
  happy-path page walk.
- Primary files:
  - `codex-rs/rmcp-client/src/rmcp_client.rs`
  - `codex-rs/connectors/src/connector_runtime/mod.rs`
  - `codex-rs/codex-mcp/src/connection_manager.rs`
  - `codex-rs/codex-mcp/src/runtime.rs`
  - `codex-rs/codex-mcp/src/resource_client.rs`
  - `codex-rs/codex-mcp/src/rmcp_client.rs`
  - `codex-rs/codex-mcp/src/rmcp_client_tests.rs`
  - `codex-rs/codex-mcp/src/tool_catalog_cache.rs`
  - `codex-rs/app-server/tests/suite/v2/mcp_server_status.rs`
  - `codex-rs/core/tests/suite/mcp_tool_cache.rs`
  - `codex-rs/core/tests/suite/rmcp_client.rs`
  - `codex-rs/core/src/session/mcp.rs`
  - `codex-rs/core/src/state/service.rs`

### MCP Server Safety Policy Extensions

- Downstream retains per-server safety controls:
  - `enable_elicitation`
  - `read_only`
  - `strict_tool_classification`
  - `require_approval_for_mutating`
- These coexist with upstream `oauth_resource` support.
- Primary files:
  - `codex-rs/core/src/config/types.rs`
  - `codex-rs/core/src/config/edit.rs`
  - `docs/config.md`
  - `docs/downstream.md`

### MCP OAuth Fallback Hardening And Keyring Backend Preservation

- Downstream treats the MCP OAuth fallback credential file as best-effort
  recovery state: empty files are absent, corrupt files after keyring failure
  are warnings instead of hard startup failures, and writes use atomic temp-file
  replacement with explicit syncs.
- Cached expired tokens remain visibly expired when reloaded so the OAuth
  manager refreshes them before reconnecting instead of treating stale access
  tokens as usable.
- Proactive refresh failures such as missing refresh tokens are classified as
  authentication-required startup failures, so operators get the reauth path
  instead of a generic MCP startup failure.
- The selected keyring backend is intentional carry now that upstream supports
  encrypted local secrets storage. Syncs must preserve both upstream
  concrete-store pinning and `AuthKeyringBackendKind::Secrets` support, plus the
  downstream resolved-store refresh lock that prevents replaying stale rotating
  refresh tokens from a different backend.
- Normal requests stage access-only credentials in RMCP. Refresh material is
  installed only inside the serialized refresh transaction, and RMCP-derived
  persistence reacquires that same per-server lock, rereads the pinned store,
  and adopts newer durable credentials rather than overwriting or deleting them.
- Request-only staging strips `refresh_token` and derived `expires_in`; durable
  reconciliation restores omitted refresh material, scopes, and expiry fields.
  A matching `expires_at` makes a countdown-only `expires_in` difference
  non-conflicting, and a newly rotated refresh token is retained in memory
  before the durable write is attempted.
- Refresh-only credential staging deliberately omits granted scopes when
  handing credentials to RMCP so refresh requests do not broaden explicit
  persisted scopes with authorization-server-advertised `offline_access`; Codex
  preserves the durable stored scope set when the provider omits scopes.
- During future syncs, do not "simplify" the OAuth helpers by dropping either
  the secrets-backed keyring path or the downstream resolved-store reread/save
  discipline unless an upstream replacement covers those behaviors and the
  no-unrequested-`offline_access` refresh contract.
- Primary files:
  - `codex-rs/rmcp-client/src/oauth.rs`
  - `codex-rs/rmcp-client/src/oauth/resolved_store.rs`
  - `codex-rs/rmcp-client/src/oauth/store_lock.rs`
  - `codex-rs/rmcp-client/tests/streamable_http_oauth_startup.rs`
  - `codex-rs/rmcp-client/src/rmcp_client.rs`
  - `codex-rs/rmcp-client/src/startup_error.rs`
  - `codex-rs/codex-mcp/src/connection_manager.rs`

### MCP OAuth Device Login For Headless Servers

- `codex mcp login --device-auth <server>` lets an operator complete MCP OAuth
  login from SSH-only or browserless hosts through the OAuth Device
  Authorization Grant instead of relying on a local browser callback.
- Streamable HTTP OAuth discovery preserves `token_endpoint`,
  `device_authorization_endpoint`, `registration_endpoint`, and
  `grant_types_supported`, so the CLI can fail loudly when a server does not
  actually advertise device-login support or lacks both configured client id
  and dynamic registration support.
- The device-login flow uses a configured public MCP OAuth `client_id` when one
  is available. Otherwise, it performs standards-based dynamic client
  registration using the device grant shape, a public-client token endpoint
  auth method, optional requested scopes, and refresh-token registration only
  when server grant metadata permits or omits grant support.
- After client-id resolution, the flow uses PKCE, the identity-provider
  verification URL/user code, token-endpoint polling, and the existing MCP
  OAuth token cache.
- This is an intentional downstream carry for headless MCP server login until
  upstream ships an equivalent headless MCP OAuth login contract. During
  upstream syncs, preserve this behavior unless the upstream replacement covers
  the same discovery, grant-validation, dynamic-registration fallback,
  configured-client-id fast path, PKCE, polling, and token-cache path.
- Primary files:
  - `codex-rs/cli/src/mcp_cmd.rs`
  - `codex-rs/codex-mcp/src/mcp/auth.rs`
  - `codex-rs/rmcp-client/src/auth_status.rs`
  - `codex-rs/rmcp-client/src/perform_oauth_device_login.rs`
  - `codex-rs/rmcp-client/src/lib.rs`
  - `.github/scripts/test_ci_planners.py`
  - `.github/validation-lanes.json`
  - `.github/workflows/sedna-heavy-tests.yml`
  - `justfile`
  - `docs/downstream.md`
  - `docs/downstream-regression-matrix.md`

### TUI Session-State, Queue, Interrupt, And Usage Surfaces

- Per-thread approval/sandbox/reviewer overrides survive thread switches.
- Active-thread session state survives config refresh and fresh-session clones
  keep policy mutability before new-thread and fork flows.
- `/agent` picker rows expose per-thread used-token totals from cached thread
  usage without requiring a broader context-window plumbing pass.
- Combined session token totals remain visible across `/status` and
  footer/status-line surfaces without overwriting the active thread's own usage
  totals.
- Unavailable slash commands queue and replay after the current task instead of
  being rejected immediately.
- Active-turn runtime choice commands such as `/model`, `/permissions`, `/plan`,
  and model service-tier slash commands remain selectable while a task is
  running so their chosen settings apply to queued follow-up turns.
- Interrupt handling defaults to double-`Esc` confirmation, including status-row
  interrupts while a turn is running, and preserves queued follow-ups and
  queued model changes coherently.
- Active-turn status labels preserve downstream operator cues, including
  showing `Compacting context` while context compaction is running instead of
  falling back to generic `Working`.
- Selected-turn copy keeps `CopyStatus` success/error feedback in the
  transcript overlay footer, including clipboard failures and expiry/replacement
  behavior, so copy actions never fail silently. The copied payload remains
  `## User\n\n<prompt>\n\n## Assistant\n\n<markdown>` and takes the last
  finalized source-backed assistant or proposed-plan markdown before the next
  visible prompt. After upstream moved retry safety to source-preserving forks,
  this path intentionally reads canonical transcript cells and carries no
  ordinal cache, rollback truncation, or rollback-only error state.
- Finalized assistant Markdown and proposed plans may cache rich rendered lines
  by width, syntax-theme revision, terminal palette, and color level. Raw
  transcript and selected-turn copy remain source-backed, and visualization
  directives remain uncached because their filesystem-dependent result can
  change after insertion.
- Bottom-pane transient views run their pre-draw tick and completion path so
  request-user-input overlays and other timed active views can redraw,
  auto-resolve, and pop through the same active-view seam instead of stalling
  behind static composer redraws. Stacked selection popups can be replaced or
  dismissed by view id even when another view sits above them, `/resume` stays
  available during MCP startup while active-turn blocking remains in force, and
  `/status` history preserves cached status text while a refresh is pending.
- TUI realtime voice remains a downstream carry on non-Linux targets even
  though upstream removed that surface; Linux keeps explicit unavailable stubs,
  so syncs should preserve the platform split instead of deleting
  `audio_device.rs` or the Linux `voice` stub as stale code. The non-Linux
  split also depends on the isolated `codex-realtime-webrtc` crate, the
  target-scoped `cpal` entry in `codex-rs/tui/Cargo.toml`, and their Cargo and
  Bazel dependency graphs. The WebRTC crate is an intentional macOS transport
  boundary, not an orphan left behind by upstream's removal.
- Thread replay routing may omit only notifications that are already handled or
  ignored during replay. It must update active-turn and side-parent state first,
  retain replay-visible native computer-use completion items, and keep
  downstream realtime audio forwarding on its dedicated transport.
- Weekly status-line pacing keeps downstream stale handling and selectable
  render styles.
- Upgradeable legacy models stay visible in the model picker even when ordinary
  hidden presets are excluded.
- `/quit` and `/exit` inside an active `/side` conversation close only that side
  conversation and return to the parent session; the same commands in the main
  conversation remain application exits.
- Primary files:
  - `codex-rs/tui/src/app.rs`
  - `codex-rs/tui/src/app/thread_events.rs`
  - `codex-rs/tui/src/app/thread_routing.rs`
  - `codex-rs/tui/src/app/side.rs`
  - `codex-rs/tui/src/app/event_dispatch.rs`
  - `codex-rs/tui/src/app_backtrack.rs`
  - `codex-rs/tui/src/app_event.rs`
  - `codex-rs/tui/Cargo.toml`
  - `codex-rs/Cargo.lock`
  - `codex-rs/realtime-webrtc/BUILD.bazel`
  - `codex-rs/realtime-webrtc/Cargo.toml`
  - `codex-rs/realtime-webrtc/src/lib.rs`
  - `codex-rs/realtime-webrtc/src/native.rs`
  - `codex-rs/tui/src/audio_device.rs`
  - `codex-rs/tui/src/bottom_pane/mod.rs`
  - `codex-rs/tui/src/bottom_pane/textarea.rs`
  - `codex-rs/tui/src/chatwidget/slash_dispatch.rs`
  - `codex-rs/tui/src/multi_agents.rs`
  - `codex-rs/tui/src/voice.rs`
  - `codex-rs/tui/src/slash_command.rs`
  - `codex-rs/tui/src/bottom_pane/chat_composer.rs`
  - `codex-rs/tui/src/bottom_pane/slash_commands.rs`
  - `codex-rs/tui/src/bottom_pane/status_line_setup.rs`
  - `codex-rs/tui/src/chatwidget.rs`
  - `codex-rs/tui/src/chatwidget/interaction.rs`
  - `codex-rs/tui/src/chatwidget/transcript.rs`
  - `codex-rs/tui/src/chatwidget/slash_dispatch.rs`
  - `codex-rs/tui/src/chatwidget/protocol.rs`
  - `codex-rs/tui/src/chatwidget/tool_lifecycle.rs`
  - `codex-rs/tui/src/chatwidget/status_surfaces.rs`
  - `codex-rs/tui/src/history_cell/mod.rs`
  - `codex-rs/tui/src/history_cell/markdown_render_cache.rs`
  - `codex-rs/tui/src/history_cell/messages.rs`
  - `codex-rs/tui/src/history_cell/messages_tests.rs`
  - `codex-rs/tui/src/history_cell/plans.rs`
  - `codex-rs/tui/src/history_cell/plans_tests.rs`
  - `codex-rs/tui/src/pager_overlay.rs`
  - `codex-rs/tui/src/render/highlight.rs`
  - `codex-rs/tui/src/status/card.rs`
  - `codex-rs/tui/src/status/rate_limits.rs`
  - `docs/config.md`
  - `docs/tui-weekly-usage-pacing-status-line.md`
  - `docs/downstream.md`
  - `docs/downstream-regression-matrix.md`

### TUI Transcript Compact Detail Mode

- The `Ctrl+T` transcript overlay intentionally carries two detail modes:
  verbose mode for the complete transcript and compact mode for prompt/agent
  review without terminal/tool noise.
- Verbose mode remains the default audit surface. Compact mode keeps user
  prompts, assistant-facing responses, selected reasoning summaries, and
  important warning/error/stop hook output visible while collapsing injected
  session context, context-only hook entries, and other detail that belongs in
  verbose transcript inspection.
- The overlay footer exposes the active detail mode and the toggle key, and the
  overlay state preserves scroll, selected prompt, raw/rich render mode, and
  verbose/compact detail mode across close/reopen.
- The transcript header now names the active mode (`Transcript: verbose` or
  `Transcript: compact`), and the footer spells out the switch keys as
  `raw render` / `rich render` and `compact view` / `verbose view` when there
  is enough terminal width.
- `[tui].transcript_default_detail_mode = "verbose" | "compact"` chooses the
  default detail mode at startup and after active config refreshes; verbose
  remains the default so the complete audit transcript is preserved unless the
  user opts into quiet review. An already-open transcript keeps its current
  mode until it is closed.
- This is an intentional downstream TUI ergonomics carry. Preserve it during
  upstream syncs unless upstream lands equivalent transcript detail-mode
  behavior that keeps the same verbose audit fallback and compact prompt/agent
  review path.
- Primary files:
  - `codex-rs/config/src/profile_toml.rs`
  - `codex-rs/config/src/types.rs`
  - `codex-rs/core/src/config/mod.rs`
  - `codex-rs/core/config.schema.json`
  - `codex-rs/tui/src/pager_overlay.rs`
  - `codex-rs/tui/src/history_cell/mod.rs`
  - `codex-rs/tui/src/history_cell/hook_cell.rs`
  - `codex-rs/tui/src/history_cell/messages.rs`
  - `codex-rs/tui/src/chatwidget.rs`
  - `codex-rs/tui/src/chatwidget/transcript.rs`
  - `codex-rs/tui/src/app.rs`
  - `codex-rs/tui/src/app/input.rs`
  - `codex-rs/tui/src/app_backtrack.rs`
  - `codex-rs/tui/src/footer_hints.rs`
  - `codex-rs/tui/src/keymap.rs`
  - `codex-rs/tui/src/keymap_setup.rs`
  - `codex-rs/tui/src/resume_picker.rs`
  - `codex-rs/config/src/tui_keymap.rs`
  - `docs/downstream-regression-matrix.md`

### Custom Prompt Discovery And Review Prompt Flow

- Downstream restores a file-backed custom prompt catalogue under
  `$CODEX_HOME/prompts`, including optional frontmatter metadata for prompt
  descriptions and argument hints.
- Downstream also preserves the live ad hoc custom-review prompt entry point in
  the TUI review flow, so users can still open the dedicated custom prompt
  view from the review popup and submit review text without losing the standard
  review interaction.
- Today the runtime wiring is clearest for the ad hoc `ReviewTarget::Custom`
  review path. The separate file-backed prompt catalogue remains a carried
  downstream surface, but it should not be described as fully reconnected to a
  user-facing picker until that runtime wiring is verified or intentionally
  restored.
- Primary files:
  - `codex-rs/core/src/custom_prompts.rs`
  - `codex-rs/core/src/custom_prompts_tests.rs`
  - `codex-rs/protocol/src/custom_prompts.rs`
  - `codex-rs/core/src/lib.rs`
  - `codex-rs/protocol/src/lib.rs`
  - `codex-rs/tui/src/app.rs`
  - `codex-rs/tui/src/app_event.rs`
  - `codex-rs/tui/src/bottom_pane/custom_prompt_view.rs`
  - `codex-rs/tui/src/chatwidget.rs`

### Code-Mode Declaration Formatting

- `main` still emits imported tool declarations of the form:
  `import { tools } from "..."; declare function ...`
- `upstream/main` still emits the older inline
  `declare const tools: { ... }` example.
- This is a live carry-only divergence.
- `ToolRouter` preserves `ResponseItem::CustomToolCall.namespace` when
  constructing registry tool names, so namespaced MCP/app custom tools do not
  flatten into plain names before routing.
- Primary files:
  - `codex-rs/core/src/tools/code_mode_description.rs`
  - `codex-rs/core/src/tools/router.rs`

### External-Agent Session Import Compatibility

- Upstream consolidated the former downstream session-import crate into
  `codex-external-agent-migration::sessions`. Preserve the imported-session
  ledger, source-content SHA-256 identity, and single-pass record parsing while
  using the upstream module and package layout.
- The workspace uses `sha2` 0.11, so content hashes retain the downstream
  explicit hexadecimal encoder rather than relying on the older digest
  formatting implementation. Callers inside the consolidated module must
  resolve that helper through the `sessions` module, not the crate root.
- `codex.external-agent-session-migration-targeted` is the focused hosted
  guardrail for future upstream moves. The divergence registry deliberately
  lists only the five session files that remain different from upstream so
  adjacent upstream-owned migration code is not hidden from the audit.
- Primary files:
  - `codex-rs/external-agent-migration/src/sessions/export.rs`
  - `codex-rs/external-agent-migration/src/sessions/ledger.rs`
  - `codex-rs/external-agent-migration/src/sessions/ledger_tests.rs`
  - `codex-rs/external-agent-migration/src/sessions/mod.rs`
  - `codex-rs/external-agent-migration/src/sessions/records.rs`
  - `.github/validation-lanes.json`
  - `justfile`

### External-Agent Migration Path Containment

- Repository-scoped external-agent detection and import canonicalize the
  project selected by the trusted local app-server client before deriving
  migration paths. Static source and destination components under that root
  must not be symlinks, including settings leaves, MCP configuration leaves,
  hook scripts, and dangling target leaves.
- This fail-closed check applies to configuration, MCP settings, subagents,
  hooks, commands, skills, and instruction-file imports. Home-scoped imports
  retain their separate source-root behavior, while shared target-leaf checks
  avoid overwriting symlink entries in either scope. It does not claim
  handle-relative protection against concurrent replacement of an already
  checked repository path.
- Upstream MEMORY migration remains home-scoped, but its client-selected project
  keys must be exactly one normal path component. Before memory detection or
  import mutates the workspace, the memory root, extension/resource ancestors,
  project root, `scope.json`, Markdown resource leaves, and `instructions.md`
  are checked beneath `CODEX_HOME` and rejected when any existing component is
  a symlink. This preserves stale-project removal without allowing absolute or
  parent-relative selections to escape the extension workspace.
- A resource directory is managed only when it contains a regular, non-symlink
  `scope.json` marker. Ordinary files and unmarked metadata directories are
  ignored, but every directory entry and candidate marker is symlink-preflighted
  before its type is used, so stale project-root and marker symlinks fail closed.
- The marker rule affects future detection. A one-time backfill for unchanged
  imports that predate the marker is an explicit follow-up, not an automatic
  mutation performed by this sync.
- The focused lane covers both the extracted migration-crate denial tests and
  the app-server request boundary.
  `projects_needing_import_rejects_symlinked_stale_memory_project` proves stale
  owned projects are preflighted before detection offers them. In particular,
  `projects_needing_import_rejects_symlinked_stale_project_scope` proves a real
  project directory cannot redirect its ownership marker, while
  `external_agent_memory_import_rejects_stale_symlink_before_workspace_mutation`
  proves a rejected stale-project selection cannot initialize or rewrite the
  memory workspace before the containment error is reported.
- Recursive copy helpers skip symlink entries and refuse symlinked target
  directories. Empty-text target checks use `symlink_metadata`, so an empty or
  dangling symlink is protected rather than treated as an overwritable file.
- Upstream's closed draft containment change covered an empty leaf target but
  did not cover exact repository settings/MCP leaves, source roots, or
  destination-ancestor symlinks. Keep this carry until upstream has equivalent
  detection-and-import containment.
- Primary files:
  - `.github/scripts/test_ci_planners.py`
  - `.github/validation-lanes.json`
  - `.github/workflows/sedna-heavy-tests.yml`
  - `codex-rs/app-server/tests/suite/v2/external_agent_config.rs`
  - `codex-rs/external-agent-migration/Cargo.toml`
  - `codex-rs/external-agent-migration/src/detect/mod.rs`
  - `codex-rs/external-agent-migration/src/hooks_common.rs`
  - `codex-rs/external-agent-migration/src/lib_tests.rs`
  - `codex-rs/external-agent-migration/src/memory_import.rs`
  - `codex-rs/external-agent-migration/src/memory_import_tests.rs`
  - `codex-rs/external-agent-migration/src/migration_source.rs`
  - `codex-rs/external-agent-migration/src/scope.rs`
  - `codex-rs/external-agent-migration/src/scope_tests.rs`
  - `codex-rs/external-agent-migration/src/service.rs`
  - `codex-rs/external-agent-migration/src/service_tests.rs`
  - `codex-rs/external-agent-migration/src/service_tests/containment.rs`
  - `codex-rs/external-agent-migration/src/service_tests/general/config_import.rs`
  - `codex-rs/external-agent-migration/src/service_tests/general/detection.rs`
  - `codex-rs/external-agent-migration/src/service_tests/general/repo_import.rs`
  - `codex-rs/external-agent-migration/src/service_tests/plugins/basics.rs`
  - `codex-rs/external-agent-migration/src/service_tests/plugins/marketplaces.rs`
  - `codex-rs/external-agent-migration/src/source/cla.rs`
  - `codex-rs/external-agent-migration/src/source/cur.rs`
  - `codex-rs/external-agent-migration/src/subagents.rs`
  - `codex-rs/external-agent-migration/src/utils.rs`
  - `justfile`
- Hosted guardrails:
  - `codex.external-agent-migration-containment-targeted`
  - `rust-ci-full`
  - `CodeQL Advanced`

## Not Counted As Standalone Live Divergences

- Merge and sync history:
  - carry-only merge commits are sync history, not independent downstream
    behaviors; do not treat their count as a behavior inventory.
- Merge-repair and promotion-fix history:
  - examples include `Fix main core regressions after upstream sync`,
    `Fix main promotion follow-ups`, and
    `Fix hybrid merge API drift in core/tui tests`
- Generated and derivative churn:
  - schema outputs under `codex-rs/app-server-protocol/schema/`
  - generated SDK outputs under `sdk/python/`
  - TUI snapshot updates under `codex-rs/tui/src/**/snapshots/`
- Structural test-only churn in large modules:
  - `codex-rs/core/src/plugins/manager.rs`
  - startup plugin sync bounded wait and completion re-arm
  - `codex-rs/core/src/config/edit.rs`
  - `codex-rs/core/src/tools/spec_plan.rs`
  - `codex-rs/tui/src/test_support.rs` centralizes the existing 8 MiB test
    stack budget for stack-heavy app and app-server-session regressions; keep
    their behavioral assertions intact when importing upstream test changes
  - `codex-rs/app-server-daemon/src/backend/pid_tests.rs` removes the PID file
    while its reservation lock remains held, preventing test cleanup races;
    drop this carry when upstream adopts equivalent ordering
  - `codex-rs/tui/src/diff_render.rs` retains the downstream ratatui `0.30.1`
    `Widget::render` test adapter and tab-free gallery fixtures; production
    rendering follows upstream's borrowed-file-change implementation
  - `codex-rs/tui/src/history_cell/plans_tests.rs` asserts that transcript
    measurement primes the plan cache without pinning upstream's
    ratatui-version-specific row count; the sentinel equality still proves
    reuse, and this adaptation can drop when ratatui versions converge or
    upstream adopts an equivalent semantic assertion
  - `codex-rs/core/tests/suite/pending_input.rs` uses the interrupted sleep
    completion as the sole consequence barrier so an unrelated event wait
    cannot consume the completion before its assertion
  - `codex-rs/core/tests/suite/otel.rs` waits for orderly session shutdown
    before reading the trace buffer so retained response spans are closed;
    both metadata assertions remain unchanged
- Schema-generation adapters that preserve legacy wire deserialization while
  keeping generated app-server schemas on the current public shape, such as
  `#[schemars(!from)]` around `MultiAgentMode` wire aliases, belong with
  app-server/protocol maintenance rather than as standalone behavior.
- The removed config template-interpolation module is deliberately not carry.
  Effective config now follows upstream's authoritative layer-stack
  materialization; old interpolation helpers and their tests must not be
  resurrected during later syncs.

## Historical Carry Commits Now Upstream-Equivalent

The following carry commits have exact-subject matches on `upstream/main`. They
should not be treated as current fork-only behavior by title alone.

```text
027afb885 -> 3b1c78a5c | [skill-creator] Add forward-testing instructions (#13600)
8b3348530 -> 07c22d20f | Add code_mode output helpers for text and images (#14244)
e4bc35278 -> 8ac27b2a1 | Add ephemeral flag support to thread fork (#14248)
22d0aea5b -> 3d4628c9c | Add granular metrics for cloud requirements load (#14108)
052ec629b -> 180a5820f | Add keyboard based fast switching between agents in TUI (#13923)
e79155902 -> 3d41ff0b7 | Add model-controlled truncation for code mode results (#14258)
01e2c3b8d -> b7f8e9195 | Add OpenAI Docs skill (#13596)
c7e28cffa -> ee8f84153 | Add output schema to MCP tools and expose MCP tool results in code mode (#14236)
5b10b93ba -> 39c1bc1c6 | Add realtime start instructions config override (#14270)
816e447ea -> 12ee9eb6e | Add snippets annotated with types to tools when code mode enabled (#14284)
2895d3571 -> 91ca20c7c | Add spawn_agent model overrides (#14160)
18199d4e0 -> 83b22bb61 | Add store/load support for code mode (#14259)
bda9e55c7 -> f2d66fadd | add(core): arc_monitor (#13936)
15163050d -> d5694529c | app-server: propagate nested experimental gating for AskForApproval::Reject (#14191)
295b56bec -> c1a424691 | chore: add a separate reject-policy flag for skill approvals (#14271)
bf936fa0c -> ce1d9abf1 | Clarify close_agent tool description (#14269)
e52afd28b -> 00ea8aa7e | Expose strongly-typed result for exec_command (#14183)
de2a73cd9 -> 889b4796f | feat: Add additional macOS Sandbox Permissions for Launch Services, Contacts, Reminders (#14155)
2544bd02a -> d751e68f4 | feat: Allow sync with remote plugin status. (#14176)
9a501ddb0 -> 026cfde02 | Fix Linux tmux segfault in user shell lookup (#13900)
b90921eba -> 7144f84c6 | Fix release-mode integration test compiler failure (#13603)
78280f872 -> f385199cc | fix(arc_monitor): api path (#14290)
44bfd2f12 -> b1dddcb76 | Increase sdk workflow timeout to 15 minutes (#14252)
b73228722 -> a67660da2 | Load agent metadata from role files (#14177)
e4edafe1a -> f9cba5cb1 | Log ChatGPT user ID for feedback tags (#13901)
566897d42 -> 31bf1dbe6 | Make unified exec session_id numeric (#14279)
b33edebd6 -> 4ac604285 | Mark incomplete resumed turns interrupted when idle (#14125)
f8ef154a6 -> 2621ba17e | Pass more params to compaction (#14247)
24b8d443b -> 01792a4c6 | Prefix code mode output with success or failure message and include error stack (#14272)
16daab66d -> e77b2fd92 | prompt changes to guardian (#14263)
37f51382f -> 8a099b3df | Rename code mode tool to exec (#14254)
cec211cab -> da74da668 | render local file links from target paths (#13857)
2cfa10609 -> fd4a67352 | Responses: set x-client-request-id as convesration_id when talking to responses (#14312)
46e6661d4 -> c4d35084f | Reuse McpToolOutput in McpHandler (#14229)
8af97ce4b -> 7f2232938 | Revert "Pass more params to compaction" (#14298)
567ad7faf -> 285b3a514 | Show spawned agent model and effort in TUI (#14273)
cc417c39a -> a4d884c76 | Split spawn_csv from multi_agent (#14282)
f6e966e64 -> 9b5078d3e | Stabilize pipe process stdin round-trip test (#14013)
77a02909a -> 52a7f4b68 | Stabilize split PTY output on Windows (#14003)
3f7cb0304 -> c8446d7cf | Stabilize websocket response.failed error delivery (#14017)
28934762d -> 722e8f08e | unifying all image saves to /tmp to bug-proof (#14149)
```
