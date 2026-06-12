# Memory Differences and Architectural Review: Sedna Fork vs OpenAI Upstream

This artifact compares the current Sedna downstream tree against the current
OpenAI upstream tree for Codex memory functionality and memory handling.

Note: this review records the pre-implementation audit baseline listed below.
Its attestation recommendations were used to shape the follow-up
`f81972fc08` phase-2 memory attestation hardening commit.

It is intentionally evidence-based: it records live tree differences, reviews
the value and quality of each difference, calls out dormant or stale carry
surfaces, and separates changes worth keeping from changes that should be
revived in a different shape or retired.

## Comparison Baseline

- Audited on: 2026-05-24
- Worktree commit inspected: `ec78346e12ebaaa96ab131bc9a6d43a515aaa22d`
- Downstream ref: `origin/main`
- Downstream commit: `ec78346e12ebaaa96ab131bc9a6d43a515aaa22d`
- Upstream ref: `upstream/main`
- Upstream commit: `7d47056ea42636271ac020b86347fbbef49490aa`
- Mirror ref: `origin/upstream-main`
- Mirror commit: `7d47056ea42636271ac020b86347fbbef49490aa`
- Merge base: `c83ba22359f4140e44fc43500d2bedbb882d7211`
- Divergence count: `origin/main...upstream/main` is `1007 36`
  (`1007` downstream-only commits, `36` upstream-only commits).

Refs were refreshed with:

```text
git fetch origin --prune
git fetch upstream --prune
git switch --detach origin/main
```

Primary comparison commands used:

```text
git diff --name-status upstream/main..origin/main -- codex-rs/memories/read codex-rs/memories/write codex-rs/memories/mcp codex-rs/ext/memories codex-rs/state/src/runtime/memories.rs codex-rs/state/src/model/memories.rs codex-rs/state/migrations/0006_memories.sql codex-rs/state/migrations/0018_phase2_selection_snapshot.sql codex-rs/state/migrations/0024_phase2_attestation_roots.sql codex-rs/state/src/runtime/phase2_attestation.rs docs/memories.md
git diff --name-status -G'memor(y|ies)|ThreadMemoryMode|MemoryCitation|raw_memories|memory_mode|phase2' upstream/main..origin/main
git log --oneline --decorate --grep='memory\|memories\|phase2\|attestation' -i upstream/main..origin/main --
git log --oneline --decorate --grep='memory\|memories\|phase2\|attestation' -i origin/main..upstream/main --
git log --oneline --decorate origin/main..upstream/main -- codex-rs/memories codex-rs/ext/memories codex-rs/state docs/memories.md
git diff --quiet upstream/main..origin/main -- codex-rs/memories/read codex-rs/memories/write/src/phase1.rs codex-rs/memories/write/src/phase2.rs codex-rs/memories/write/src/runtime.rs codex-rs/memories/write/src/start.rs codex-rs/memories/write/src/storage.rs codex-rs/memories/write/src/workspace.rs codex-rs/state/src/runtime/memories.rs codex-rs/state/src/model/memories.rs codex-rs/state/migrations/0006_memories.sql codex-rs/state/migrations/0018_phase2_selection_snapshot.sql
git show 28248cffb0:codex-rs/core/src/memories/phase2.rs
git diff 28248cffb0..HEAD -- codex-rs/core/src/memories/phase2.rs codex-rs/memories/write/src/phase2.rs
```

## Executive Summary

The current live memory delta is much smaller than the historical downstream
carry docs imply.

Downstream differs from upstream in four memory-relevant areas:

1. Memory MCP and memory extension tool schema generation was adapted for the
   downstream `schemars = "1.2.1"` dependency, while upstream still uses
   `schemars = "0.8.22"`.
2. Downstream adds state DB storage and helper methods for per-memory-root
   phase-2 attestation requirements.
3. Downstream carries an added phase-2 attestation test file, but that file is
   currently not registered as a Rust test module and appears stale against the
   current `codex-rs/memories/write/src/phase2.rs` implementation.
4. Downstream adds memory documentation and validation metadata, but some of
   those docs and guardrails are stale relative to the current source layout
   and model defaults.

The active phase-1 extraction implementation, active phase-2 consolidation
implementation, memory read/prompt injection implementation, app-server memory
mode APIs, TUI memory settings UI, memory citation protocol, and SQLite
`memories` runtime are otherwise identical between the compared trees.

No upstream-only memory commit or upstream-only memory path delta was found in
the `origin/main..upstream/main` range.

Architectural verdict:

- Keep the memory tool schema null-stripping carry. Its purpose is clear: it
  preserves the existing "optional inputs are omitted, not explicit null"
  contract after the downstream `schemars` 1.x dependency upgrade.
- Do not treat the current attestation carry as active protection. The state
  table, helper API, docs, lane metadata, and unregistered tests describe an
  intended fail-closed behavior that the live phase-2 runtime no longer calls.
- The attestation idea still has architectural value, but the old implementation
  should not be restored wholesale. It was designed around a previous
  reuse-existing-artifacts path. Current phase 2 is now a git-baseline workflow:
  sync inputs, diff the memory root, run an internal consolidation agent only
  when the workspace changed, then reset the baseline after agent completion.
- If attestation is revived, it should become a small post-agent commit gate for
  the current git-baseline architecture, not a large reinserted block inside
  `phase2.rs`.

## Architectural Review Findings

### Finding 1: Schema Carry Is Valuable Compatibility Glue

Purpose:

- Keep downstream memory tool schemas compatible with `schemars = "1.2.1"`.
- Preserve the upstream semantic contract that optional tool input fields should
  be omitted rather than sent as explicit JSON `null`.

Value:

- High. Without this adaptation, the schema contract can drift from what memory
  tool callers and validators expect.
- The change is narrow: it affects memory extension and memory MCP schema
  generation, not memory read/write business logic.

Quality:

- Medium. The implementation is direct and understandable, but the same
  sanitizer is duplicated in the extension schema helper and the MCP schema
  helper.
- There is no focused test proving that input schemas reject null admissions
  while output schemas preserve them.

Disposition:

- Keep it.
- Improve it if touched again by extracting a shared helper or by adding small
  schema-shape tests in both crates. The current duplication is acceptable debt
  for a narrow dependency-compatibility patch, but it is not elegant enough to
  grow.

### Finding 2: Current Attestation Carry Is Dormant Debt

Purpose:

- The historical purpose was good: prevent phase-2 memory consolidation from
  silently accepting stale, missing, tampered, or wrong-input artifacts after a
  successful attested run.

Value:

- The underlying invariant is valuable because memory is injected into future
  prompts. Bad durable memory can create long-lived behavioral drift.
- The old specific mechanism has lower value in the current tree because the
  active phase-2 flow no longer has the same "unchanged selection, reuse old
  artifacts" shape that the attestation code was built to police.

Quality:

- Current quality is poor because it is half-carried:
  - the DB migration and `StateRuntime` methods exist,
  - the active phase-2 runtime does not call them,
  - the attestation tests are tracked but not registered,
  - the tests reference symbols that no longer exist,
  - docs and validation lanes still present the behavior as active.

Disposition:

- Do not wire the dormant API back into phase 2 as-is.
- Either retire the stale docs/lane claims, or revive attestation in a
  current-architecture shape described below.

### Finding 3: Attestation Is Worth Reviving Only As A Smaller Commit Gate

The old code should not be resurrected wholesale. It added a large amount of
logic to the central phase-2 module, mixed secure file handling, tree hashing,
selection fingerprinting, model/prompt/sandbox fingerprinting, sidecar I/O,
state DB updates, test hooks, and agent completion handling in one place.

The valuable part is not the old shape; it is the commit-gate invariant:

- The prepared phase-2 inputs should not change between prompt construction and
  successful completion.
- The agent should leave valid, non-empty required outputs when the prompt says
  outputs are required.
- The recorded successful baseline should be tied to the exact input selection,
  prompt/model/sandbox contract, and output artifact tree.
- A missing or invalid attestation should not silently reopen a bootstrap path
  after a root has previously completed an attested run.

In the current git-baseline architecture, a revived design should:

1. Add a private memory-write module such as
   `codex-rs/memories/write/src/phase2_attestation.rs` or
   `codex-rs/memories/write/src/provenance.rs`.
2. Capture a prepared-input fingerprint after `sync_phase2_workspace_inputs` and
   `write_workspace_diff`, before spawning the consolidation agent.
3. Include all prompt-relevant prepared inputs in that fingerprint:
   `raw_memories.md`, `rollout_summaries/**`, `extensions/**` where present,
   and the generated `phase2_workspace_diff.md`.
4. Capture a consolidator fingerprint covering model provider, model, reasoning
   effort, prompt hash, approval policy, sandbox policy, and disabled recursive
   tool surfaces.
5. After the agent completes, before `reset_memory_workspace_baseline`, validate
   that required outputs exist and are non-empty, the prepared-input fingerprint
   still matches, and the output tree fingerprint is recorded.
6. Persist the attestation in a baseline-scoped form. A root-only boolean is too
   coarse for the current design; the durable record should identify the memory
   root plus the attested output tree or git baseline identity.
7. On the no-workspace-change fast path, validate an existing durable
   attestation when one exists. If no attestation exists yet, either treat the
   root as pre-attestation bootstrap or schedule a re-attestation run explicitly.
8. Register tests in `codex-memories-write`, not `codex-core`, and update
   `justfile`, `.github/workflows/sedna-heavy-tests.yml`, and
   `.github/validation-lanes.json` accordingly.

This preserves the value of the carry while fitting the new upstream-shaped
phase-2 flow.

### Finding 4: Documentation Currently Overstates Runtime Behavior

Purpose:

- The downstream docs and validation metadata are trying to preserve operator
  knowledge about intentional memory carry.

Value:

- Medium. The docs are useful as carry inventory, but only if they distinguish
  active behavior from intended or historical behavior.

Quality:

- Low in the attestation area. They mention old paths, stale model defaults, and
  an attestation flow that is not wired into active code.

Disposition:

- Refresh docs and lane metadata after the attestation decision.
- If attestation is not revived immediately, label it as dormant/historical in
  divergence docs rather than active behavior.

## Live File-Level Differences

The targeted memory-path diff resolves to these live differences:

```text
M  codex-rs/ext/memories/src/schema.rs
M  codex-rs/memories/mcp/src/schema.rs
A  codex-rs/memories/write/src/phase2_attestation_tests.rs
A  codex-rs/state/migrations/0024_phase2_attestation_roots.sql
A  codex-rs/state/src/runtime/phase2_attestation.rs
A  docs/memories.md
```

Additional documentation and guardrail surfaces with memory-specific changed
lines:

```text
M  docs/example-config.md
A  docs/carry-divergence-ledger.md
A  docs/divergences/index.yaml
A  docs/downstream-regression-matrix.md
M  justfile
A  .github/workflows/sedna-heavy-tests.yml
A  .github/validation-lanes.json
```

The generated app-server schema files also contain memory-related names because
they aggregate the whole protocol schema, but the source protocol memory types
and memory-specific app-server/TUI code are not changed by this fork delta.

## Difference 1: Memory Tool Input Schema Null Handling

Files:

- `codex-rs/ext/memories/src/schema.rs`
- `codex-rs/memories/mcp/src/schema.rs`
- `codex-rs/Cargo.toml`

Upstream behavior:

- Uses `schemars::r#gen::SchemaSettings`.
- Sets `settings.inline_subschemas = true`.
- Sets `settings.option_add_null_type = option_add_null_type`.
- Depends on `schemars = "0.8.22"`.

Downstream behavior:

- Uses `schemars::generate::SchemaSettings`.
- Sets `settings.inline_subschemas = true`.
- Does not set `option_add_null_type` directly.
- Depends on `schemars = "1.2.1"`.
- When building memory tool input schemas, recursively strips JSON null
  admissions after schema generation.
- When building memory tool output schemas, leaves explicit null admissions in
  place.

The downstream sanitizer removes null-only forms from memory tool input
schemas:

- `type: "null"`
- `"null"` members inside `type` arrays
- `const: null`
- `null` members inside `enum`
- null-only branches inside `anyOf` and `oneOf`

Functional meaning:

- Both sides intend memory tool inputs to model optional fields by omission,
  not by explicit JSON null.
- Downstream preserves that contract after the Schemars 1.x upgrade by
  rewriting the generated schema tree.
- Downstream memory tool outputs still allow explicit null for optional fields.
- The downstream implementation is more defensive about nested/null-only schema
  forms than the upstream 0.8-era direct setting.

Operational implication:

- Memory extension and memory MCP clients that validate input schemas should
  not rely on sending explicit `null` for optional inputs in downstream.
- The change is scoped to the two memory schema helpers. It does not imply a
  fork-only change to memory search/list/read business logic.

Architectural review:

- Purpose is clear and still current.
- Value is high enough to keep because it preserves a wire/tool contract across
  a dependency upgrade.
- Quality is serviceable but should not be copied further. The duplicated
  sanitizer should either stay frozen or be extracted/tested if this area grows.
- Recommended action: keep as-is for now; add focused schema tests before any
  future refactor.

## Difference 2: Phase-2 Attestation Root State

Files:

- `codex-rs/state/migrations/0024_phase2_attestation_roots.sql`
- `codex-rs/state/src/runtime/phase2_attestation.rs`
- `codex-rs/state/src/runtime.rs`
- `codex-rs/state/src/migrations.rs`

Downstream adds a state DB table:

```sql
CREATE TABLE IF NOT EXISTS phase2_attestation_roots (
    memory_root_key TEXT PRIMARY KEY,
    required_since INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
```

Downstream adds two `StateRuntime` methods:

- `global_phase2_attestation_required_for_root(memory_root_key)`
- `mark_global_phase2_attestation_required_for_root(memory_root_key)`

Intended meaning, based on comments and tests:

- A memory root starts in a bootstrap state where missing attestation can be
  tolerated.
- Once that root has completed the bootstrap path, the state DB records that
  attestation is required.
- Later reuse for that root should fail closed if attestation is missing.
- The requirement is scoped by `memory_root_key`, so one memory root must not
  leak attestation state into another.

Actual current production wiring:

- `git grep global_phase2_attestation origin/main -- codex-rs` finds only the
  new state runtime file and the added attestation test file.
- `codex-rs/memories/write/src/phase2.rs` is identical between downstream and
  upstream in the compared trees.
- `codex-rs/memories/write/src/phase2.rs` does not call the new
  `StateRuntime` attestation methods.
- `codex-rs/memories/write/src/phase2.rs` does not contain the attestation
  artifact readiness functions referenced by the added test file.

Functional conclusion:

- The downstream tree has a persistent state schema/API delta for memory
  attestation roots.
- In the current tree, that delta appears dormant from the active memory
  consolidation path.
- The active phase-2 consolidation behavior is not different from upstream
  based on the final-tree comparison.

Architectural review:

- The state table is not harmful by itself, but it is currently maintenance
  cost without runtime protection.
- The root-scoped boolean is too coarse for a revived git-baseline design. It
  can say "this root once required attestation", but it cannot identify which
  baseline, output tree, input set, prompt/model contract, or sandbox contract
  was attested.
- If attestation is revived, the existing table can be treated as a bootstrap
  compatibility marker, but a richer baseline-scoped record would be needed for
  strong provenance.
- Recommended action: do not expand callers to this API until the new
  attestation boundary is designed.

Migration implication:

- Downstream occupies state migration slot `0024` with
  `phase2_attestation_roots`.
- Upstream has no equivalent table in the compared tree.
- Later upstream state migrations are renumbered in downstream to avoid
  colliding with the downstream-only migration slot. That renumbering is an
  indirect state DB history difference, not a separate memory behavior.

## Difference 3: Added But Unregistered Attestation Tests

File:

- `codex-rs/memories/write/src/phase2_attestation_tests.rs`

Downstream adds tests for:

- Rejecting rollout-summary drift even when memory outputs are fresh.
- Rejecting missing attestation after durable DB requirement state is
  initialized, even if the support marker is deleted.
- Rejecting missing attestation when a state DB exists but no requirement row
  exists.

Current registration status:

- `rg phase2_attestation_tests codex-rs/memories/write/src` returns no module
  registration.
- `codex-rs/memories/write/src/lib.rs` registers `startup_tests`, but not
  `phase2_attestation_tests`.
- The added file is therefore not part of the current Rust test build.

Current symbol status:

- The added tests refer to symbols such as
  `agent::consolidation_artifacts_ready_with_expected_supporting_tree` and
  `test_write_consolidation_artifact_attestation_with_state_db`.
- `git grep` finds those names only in the added test file.
- If the file were registered as-is, it appears likely to fail compilation
  against the current memory write implementation.

Functional conclusion:

- The file is evidence of intended downstream attestation behavior.
- It is not current executable guardrail coverage.
- It should not be counted as proving an active fork-vs-upstream memory
  behavior difference.

Architectural review:

- The tests encode useful failure cases, especially input drift and deletion of
  sidecar/support markers.
- They are not a safe starting point for direct registration because they target
  stale helper names and an older phase-2 module shape.
- Recommended action: port the scenarios, not the file. New tests should live
  beside the current `codex-memories-write` phase-2/workspace tests and assert
  the revived commit-gate API directly.

## Difference 4: Downstream Memory Documentation

Files:

- `docs/memories.md`
- `docs/example-config.md`
- `docs/carry-divergence-ledger.md`
- `docs/divergences/index.yaml`
- `docs/downstream-regression-matrix.md`
- `justfile`
- `.github/workflows/sedna-heavy-tests.yml`
- `.github/validation-lanes.json`

Downstream adds a dedicated `docs/memories.md` page. Upstream does not have this
file. It documents:

- `~/.codex/memories`
- `raw_memories.md`
- `rollout_summaries/`
- `MEMORY.md`
- `memory_summary.md`
- `skills/`
- phase-1 and phase-2 startup behavior
- resume/refresh behavior
- retention caps
- reset/discard behavior
- `[memories]` config knobs

Downstream also adds a `[memories]` sample block to `docs/example-config.md`.

Current accuracy notes:

- `docs/memories.md` and `docs/example-config.md` state phase-1 defaults to
  `gpt-5.1-codex-mini` and phase-2 defaults to `gpt-5.3-codex`.
- Current source in `codex-rs/memories/write/src/lib.rs` sets phase 1 to
  `gpt-5.4-mini` with low reasoning and phase 2 to `gpt-5.4` with medium
  reasoning.
- `docs/memories.md` states phase 2 writes attestation sidecars and records a
  durable per-memory-root state DB requirement.
- Current active `codex-rs/memories/write/src/phase2.rs` does not contain the
  attestation sidecar/readiness flow and is identical to upstream for this
  comparison.

Downstream divergence metadata also mentions the attestation carry:

- `docs/carry-divergence-ledger.md`
- `docs/divergences/index.yaml`
- `docs/downstream-regression-matrix.md`

Current accuracy notes for that metadata:

- Some entries refer to old paths such as `codex-rs/core/src/memories/phase2.rs`.
- The current memory write implementation lives under `codex-rs/memories/write`.
- `.github/workflows/sedna-heavy-tests.yml` uses the old
  `codex-rs/core/src/memories/...` path matcher for the attestation lane.
- `justfile` defines `core-attestation-targeted` as:

```text
cargo test -p codex-core consolidation_artifacts_ready_rejects_ --lib -- --test-threads=1
cargo test -p codex-state global_phase2_attestation_requirement_is_root_scoped -- --exact --test-threads=1
```

That first command targets `codex-core`, while the added attestation test file
is under `codex-rs/memories/write` and is not registered.

Functional conclusion:

- Documentation and guardrail metadata are downstream-only differences.
- They currently overstate the active attestation behavior present in the code.
- Treat the current code comparison in this artifact as the authoritative state
  until those docs/lanes are refreshed.

Architectural review:

- The documentation is useful as an inventory of downstream intent, but it is
  not currently reliable operator guidance for phase-2 attestation.
- Stale model defaults are a simple correctness bug.
- Old `codex-rs/core/src/memories/...` paths make validation planning brittle
  because current memory write code lives in `codex-rs/memories/write`.
- Recommended action: refresh docs immediately if this artifact is promoted
  into tracked work. Do not wait for a full attestation implementation to fix
  stale defaults and paths.

## Unchanged Memory Functionality

The following memory surfaces are unchanged between the compared trees:

- Active phase-1 memory extraction:
  - `codex-rs/memories/write/src/phase1.rs`
- Active phase-2 memory consolidation:
  - `codex-rs/memories/write/src/phase2.rs`
- Memory startup runtime:
  - `codex-rs/memories/write/src/runtime.rs`
  - `codex-rs/memories/write/src/start.rs`
- Memory workspace/file handling:
  - `codex-rs/memories/write/src/storage.rs`
  - `codex-rs/memories/write/src/workspace.rs`
  - `codex-rs/memories/write/src/control.rs`
  - `codex-rs/memories/write/src/guard.rs`
- Memory read/prompt injection:
  - `codex-rs/memories/read/**`
- Memory state model/runtime:
  - `codex-rs/state/src/runtime/memories.rs`
  - `codex-rs/state/src/model/memories.rs`
  - `codex-rs/state/migrations/0006_memories.sql`
  - `codex-rs/state/migrations/0018_phase2_selection_snapshot.sql`
- App-server memory reset and memory-mode test surfaces:
  - `codex-rs/app-server/tests/suite/v2/memory_reset.rs`
  - `codex-rs/app-server/tests/suite/v2/thread_memory_mode_set.rs`
- TUI memory settings surfaces:
  - `codex-rs/tui/src/bottom_pane/memories_settings_view.rs`
  - memory settings/reset snapshots under `codex-rs/tui/src/chatwidget/snapshots`
- Memory citation protocol:
  - `codex-rs/protocol/src/memory_citation.rs`
  - generated `MemoryCitation` TypeScript files
- Thread memory mode protocol type:
  - generated `ThreadMemoryMode` TypeScript file

Important qualifier:

- `codex-rs/app-server-protocol/src/protocol/v2/thread.rs` does differ, but
  the changed source fields are about dynamic tools, raw-event serialization,
  and include-turn defaults. The only memory-looking phrase in that file is the
  ordinary phrase "loaded in memory", not the Codex Memories feature.

## Upstream-Only Memory Changes

No upstream-only memory changes were found in the current `origin/main..upstream/main`
range.

Evidence:

- `git log --grep='memory\|memories\|phase2\|attestation' -i origin/main..upstream/main --`
  returned no commits.
- `git log origin/main..upstream/main -- codex-rs/memories codex-rs/ext/memories codex-rs/state docs/memories.md`
  returned no commits.
- The reverse final-tree diff has no upstream-only memory subsystem addition
  beyond the inverse of the downstream-only files listed above.

## Noisy Hits Excluded From Live Memory Differences

The broad `-G` search for memory terms also returned files that are not live
Codex Memories behavior differences:

- Generated app-server schema aggregate JSON files. These include memory type
  names indirectly but are not backed by memory-specific source changes.
- `codex-rs/exec/*` files. Their matching lines use generic "in-memory" terms
  or changed tests unrelated to Codex Memories.
- `codex-rs/core/src/config/config_tests.rs`. It imports and asserts memory
  config structs, but the matched diff hunk is in a large config-test file with
  many unrelated downstream config changes.
- `justfile` memory mentions outside `core-attestation-targeted`, such as tests
  named `classifies_memory_excluded_fragments`; those are guardrail grouping
  choices, not active memory implementation deltas.

## Practical Takeaways

- For active runtime behavior, the only direct memory implementation delta is
  the memory tool schema null-stripping adaptation.
- For persisted state, downstream has an extra memory-attestation table/API
  that upstream does not have, but the active consolidation path currently does
  not call it.
- For validation, the current attestation guardrail is not reliable as written:
  its test file is not registered and its `justfile` command points at
  `codex-core`.
- For documentation, downstream has more memory docs than upstream, but the
  model defaults and attestation claims should be refreshed before being used
  as current operator guidance.
- For architecture, the right attestation question is not "restore the old
  sidecar code?" It is "should phase 2 have a post-agent commit gate before
  resetting the git baseline?" The answer is yes if downstream wants stronger
  durable-memory provenance than upstream; the implementation should be smaller
  and better factored than the old carry.

## Suggested Follow-Up Fixes

1. Refresh `docs/memories.md` and `docs/example-config.md` to match the current
   source defaults: phase 1 `gpt-5.4-mini`, phase 2 `gpt-5.4`.
2. Update path references from `codex-rs/core/src/memories/...` to the current
   `codex-rs/memories/write/...` layout wherever the attestation lane or
   divergence registry is retained.
3. Reclassify the current attestation carry in divergence docs as dormant unless
   code is revived in the same change.
4. If downstream wants stronger memory provenance, implement the smaller
   git-baseline commit gate described above. Do not reinsert the historical
   1000-line phase-2 block.
5. Replace the stale attestation test file with registered
   `codex-memories-write` tests covering:
   - prepared input drift between spawn and completion,
   - missing or empty required outputs after completion,
   - missing/invalid durable attestation after a root has an attested baseline,
   - no-change fast path behavior with and without an existing attestation.
6. Update `core-attestation-targeted` or rename it to a memory-write lane that
   actually runs the registered tests.
