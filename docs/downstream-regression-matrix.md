# Downstream regression matrix

> `status: current` · `authority: informative` · `candidate: external exact-delivery receipt`

This matrix binds each accepted carry family to its proving surface. It does
not turn optional evidence into a prerequisite and it does not reopen accepted
P1-P6 work.

| Carry family                   | Invariant                                                                                                                                     | Exact proving surface                                                                                             | Disposition                                                                                             |
| ------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| Terminal completion input      | Completion is opt-in, metadata-only, identity-bound, and ordered after output drain.                                                          | Hosted core unified-exec and multi-agent targeted lanes.                                                          | Retain through P7; reopen only on candidate defect.                                                     |
| Realtime adapter               | V3 backend seed history does not replace downstream world-state instructions across resume/compaction.                                        | Hosted realtime targeted lane and exact protocol fixtures.                                                        | Retain through P7; no redesign.                                                                         |
| Usage provenance               | Pricing is gated on successful provider-observed usage and migrations remain readable.                                                        | Hosted state usage lane and Cargo lock verification.                                                              | Retain through P7.                                                                                      |
| Phase-two memory attestation   | Attestation and recovery namespaces remain collision-free and symlink-safe.                                                                   | Hosted memory/state targeted lane.                                                                                | Retain through P7.                                                                                      |
| Configured identity provenance | Requested and effective identities are not fabricated during replay or resume.                                                                | Hosted protocol/state identity lane.                                                                              | Retain through P7.                                                                                      |
| Dynamic-tool persistence       | Whole-call persistence is restart-safe and idempotent.                                                                                        | Hosted state/runtime targeted lane.                                                                               | Retain through P7.                                                                                      |
| Browser computer-use routing   | Requests route to the configured provider, preserve failure text, and keep generated protocol fixtures aligned.                               | Hosted app-server/TUI/browser targeted lane plus protocol fixture check.                                          | Retain through P7.                                                                                      |
| Native multi-agent waits       | Native waits remain event-driven, wake only for causal target events, preserve turn reopen state, and reject waits on self or an ancestor.    | Hosted TUI agent-picker, agent-tree, spawn-model-surface, and core orchestration targeted lanes.                  | Retain accepted leaf evidence; causally review only composed shared interactions.                       |
| RMCP request cancellation      | Dropping a caller cancels the in-flight request without leaking HTTP work, while streamable HTTP recovery remains intact.                     | Hosted RMCP client transport and consumer-check lanes.                                                            | Retain accepted cancellation final-tree delta.                                                          |
| GPT-6 catalog support          | GPT-6 Sol and Luna remain selectable and model identity/pinning is preserved across client, core, and TUI surfaces.                           | Hosted spawn-agent tool/description model-surface and core subagent-model-pinning lanes.                          | Retain; final instruction-overlay reconciliation is root-owned before freeze.                           |
| Provider User-Agent version    | Provider requests report the truthful package version without downstream-identifying metadata.                                                | Hosted provider client regression assertion.                                                                      | Upstream-equivalent behavior; retain the stronger regression test.                                      |
| Shared CI composition          | Required blocking lanes use the shared setup action, DotSlash retry configuration, and standard public runners.                               | Hosted `blocking-ci` dispatch on the exact head, with `repo-checks`, `rust-ci`, `bazel`, and `sdk` child results. | P7 composition proof; full/release lanes remain separately governed and are not required cutline proof. |
| CodeQL merge-queue gate        | Pull-request and merge-group events always report one trustworthy `CodeQL required gate`; custom packs and path classification remain joined. | Hosted CodeQL workflow on exact PR and merge-group heads, including classifier fixtures.                          | Retain through P7; never substitute it with an old PR-only green run.                                   |
| Ancestry cutover               | Candidate remains rooted at frozen upstream; old unrelated `origin/main` is preserved but never imported.                                     | Root-owned exact graph/protection/rollback readback after handoff.                                                | Root-owned, not performed by P7.                                                                        |

## Hosted proof boundary

Local compilation, Cargo, Bazel, Clippy, package, release, and substantive
tests are prohibited on the coordination host. A green old run or an overlay
run is not proof for the product candidate. All acceptance evidence must bind
the exact final product SHA and keep any proof overlay identity separate.

## Windows note

Windows expansion and release work are outside this cutline. Existing evidence
is preserved. A currently active protected Windows rule still has to be
satisfied or deliberately changed by the root under separate authority; this
matrix grants no waiver.
