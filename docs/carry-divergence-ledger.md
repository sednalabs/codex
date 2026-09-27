# Carry divergence ledger

> `status: current` · `authority: evidence` · `candidate: external exact-delivery receipt`

The accepted native-wait composition source is
`e5c9b3e870f861af898ac7a8f7c704743d551d89` with tree
`83417286656d9a368ef0abfea32a125cfd5bb998`. The final cumulative candidate is
bound only by the current Ops handoff and hosted proof, so this document never
self-references a stale commit hash.

This ledger records the accepted P1-P6 carry and the P7 composition boundary.
It is deliberately independent of the unrelated rewritten `origin/main`
lineage. Generated counts and exact identities are recorded in
[`generated/upstream-status.md`](generated/upstream-status.md).

## Frozen train

- Product repository: `sednalabs/codex`
- Accepted cumulative composition source: `e5c9b3e870f861af898ac7a8f7c704743d551d89`
- Accepted composition tree: `83417286656d9a368ef0abfea32a125cfd5bb998`
- Frozen upstream cut: `openai/codex` at
  `392f56a611c412b9b2eb1d9d4e59a3b42bee483a`
- Frozen upstream tree: `ac81df5b0332908409b60a0addc7e967eed868ea`
- Current `origin/main` (unrelated sanitation lineage):
  `8e4b1a7eb6a21417a444e0752f749bc0d5a1cb0a`
- Current `upstream/main` (`67a709665ac7b50311b93e32612c9a8281684787`) is
  next-train information only and does not recut this train.

The P7 candidate remains upstream-rooted through the frozen cut and accepted
P1-P6 ancestry. A conventional merge with old `origin/main` is prohibited.
Root retains protected-main mutation, rollback, post-main acceptance, and
ancestry cutover.

## Accepted carry families

The machine-readable registry in [`divergences/index.yaml`](divergences/index.yaml)
records each family, its owner, guardrail lane, and retirement condition. The
families are retained as one cumulative contract: terminal completion input,
realtime continuity, usage provenance, phase-two memory attestation,
configured-identity provenance, dynamic-tool persistence, browser
computer-use routing, native multi-agent wait continuity, RMCP caller
cancellation, and the merge-queue CodeQL required gate. Accepted leaf evidence
is not reopened absent a concrete candidate defect.

## P7-owned composition

P7 composes the accepted downstream carry, the exact late-main deltas required
by the work-item contract, shared workflows, validation configuration, the
permitted lockfiles, and the divergence/status evidence pages. It does not
merge or replay the unrelated old-main lineage. The bounded harvest and
late-main reconciliation classified the following items:

| Candidate                                  | Source                              | Decision | Rationale                                                                                                                                   |
| ------------------------------------------ | ----------------------------------- | -------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `80a0c1da88dfde63e37d9f48bd66389fb8f83dbe` | upstream commit `#47693`            | `adopt`  | Shared CI setup can safely configure DotSlash curl retries without changing product semantics.                                              |
| `99d581e9ddf54becb80cec54052255f82d8e5477` | sednalabs workflow hardening `#871` | `adopt`  | Content-only adoption removes custom runner groups from Bazel, Rust CI, and SDK workflows; no unrelated `origin/main` ancestry is imported. |
| `f747d23d4bc8a167207fb1c411e221022391fdbb` | upstream commit `#46993`            | `track`  | The useful `justfile` wording is coupled to an out-of-scope shell-script change; do not create a partial lock-check contract.               |
| `a16381c4457e23191d4786968011434c37a04041` | upstream commit `#47748`            | `track`  | Cargo/Bazel debug defaults require `MODULE.bazel` and a patch outside the admitted P7 set.                                                  |
| `9c77996cd1c28f683fa800891d502d9bb017e692` | upstream commit `#47095`            | `track`  | Release workflow removal is release-high-consequence and not required for this cutline.                                                     |
| `6824dabe0393337a38cb257d5fe75ae5ca168470` | upstream commit `#47597`            | `track`  | Release-channel/canary behavior requires a separate release-governed outcome.                                                               |
| `1d87af5faa75c2c09785cd088353d3236333673f` | upstream commit `#47742`            | `ignore` | Its Cargo.lock delta is inseparable from out-of-scope source changes and does not prove the P7 acceptance boundary.                         |
| `f5960fcc22b918e658bc486e53a80031d64fd41e` | upstream commit `#47713`            | `ignore` | Shared-crate source and lock changes are outside P7; retain as next-train material.                                                         |
| `5babf441c179fa8f4f36ebabc5233d0630ce9658` | sednalabs PR #854                   | `track`  | Preserved exact head is based on old `main` and touches `.codex/**`, outside the P7 product scope.                                          |
| `3f888b0380b99d7629315ddcaddc15bd39ebce92` | accepted cancellation final         | `retain` | Apply its exact final-tree delta from `93800c2b`; preserves RMCP caller cancellation, recovery, and coupled hosted validation.             |
| `ee3d56ab613875319193852ce4be5159702d507a` | accepted usage final                | `retain` | Apply its exact final-tree delta from `93800c2b`; preserves usage migration compatibility and the current credit-rate rows.                |
| `226f8076bf240ec72f659f4f310e6284e734a5c5` | protected main PR #907              | `adopt`  | Product behavior already uses the package version; retain the stronger regression assertion without importing old-main ancestry.          |
| `c2e76173b1`                               | protected main PR #895              | `retain` | Port the reverse-ancestor native-wait guard onto the current wait architecture to prevent cyclic waits.                                    |
| `a98b85f89a7d0d0afa516bc23d0c3994c0f0dee9` | protected main PR #902              | `equivalent` | The accepted native-wait source already carries authoritative turn-reopen and delayed-activity suppression semantics.                   |
| `fe5cbe1e9ce7767336b1d24bb7511d7fbd407402` | protected main PR #903              | `equivalent` | The accepted native-wait source already carries exact-target mailbox wake causality.                                                     |
| `8e4b1a7eb6a21417a444e0752f749bc0d5a1cb0a` | protected main PR #912              | `retain` | Carry GPT-6 Sol/Luna catalog and selection coverage; reserve the current-main instruction-overlay join for final root reconciliation.       |
| `8e4b1a7eb6a21417a444e0752f749bc0d5a1cb0a` | current-main CodeQL producer        | `retain` | Carry the `merge_group`-capable `CodeQL required gate`, coupled scripts, query packs, and standard-hosted runner policy.                    |

No upstream work is silently deleted. `track` means preserved for the next
authorized train or a separately scoped successor.

## Replan repair dispositions

The exact hosted failures in run `36002658222` caused a bounded programme
replan. The focused harvest found no newer upstream equivalent for these seams;
the local repairs remain limited to the named acceptance contracts:

| Seam                                                | Upstream disposition     | P7 disposition                                                                                                         |
| --------------------------------------------------- | ------------------------ | ---------------------------------------------------------------------------------------------------------------------- |
| `x86_64-pc-windows-gnullvm` memory symlink removal  | `deferred-no-equivalent` | Repair by portable directory/file symlink removal with coupled Windows fixture coverage.                               |
| Unknown legacy rollout event                        | `deferred-no-equivalent` | Preserve forward-compatible `EventMsg::Unknown` consumption and correct the stale assertion.                           |
| `codex-rs/code-mode/Cargo.toml` feature exception   | `deferred-no-equivalent` | Remove the stale verifier exception; retain the legitimate V8 POC exception.                                           |
| Validation planner identity/runner/dispatch receipt | `downstream-governance`  | Enforce nested runner-group/label rejection, allowed labels, dispatch inputs, and exact hosted SHA/tree when supplied. |

The agent-action-provenance and background agent-picker heads remain preserved
outside this freeze unless they land before the final root cutoff. The current
instruction-overlay repair is likewise a reserved exact landed reconciliation,
not a duplicated implementation in this branch.

## Acceptance boundary

Acceptance requires an exact final candidate SHA/tree and complete path
manifest, standard public GitHub-hosted proof on that exact product head,
decision-complete managed Luna-high review, resolved applicable conversations,
and an accepted Ops handoff to root. A source commit, branch, PR, workflow
dispatch, partial green run, or review-in-progress is not terminal evidence.

Windows expansion is outside this cutline. Existing Windows evidence remains
preserved, but no new Windows repair or release work is admitted by this
ledger. Any active protected rule that still requires Windows must be satisfied
or changed under separate root authority; this document does not waive it.
