# Carry divergence ledger

> `status: current` · `authority: evidence` · `candidate: external exact-delivery receipt`

The historical source-composition receipt is `77968332f63d31490d71ce462a868a7e224b75f1`
with tree `867e02cec96c67e5afaf363f965204ab28c9cfb2`; the final candidate is
bound only by the current Ops handoff and hosted proof so this document never
self-references a stale commit hash.

This ledger records the accepted P1-P6 carry and the P7 composition boundary.
It is deliberately independent of the unrelated rewritten `origin/main`
lineage. Generated counts and exact identities are recorded in
[`generated/upstream-status.md`](generated/upstream-status.md).

## Frozen train

- Product repository: `sednalabs/codex`
- Accepted P6 base/head: `repair/w14078-p6-tui-realtime` at
  `0d0f73e7fd44ec0d5bc2dc0ba5625a8559ed8a9c`
- Accepted P6 tree: `6be24ec81e116584c248c937302e0dacb0949c94`
- Frozen upstream cut: `openai/codex` at
  `392f56a611c412b9b2eb1d9d4e59a3b42bee483a`
- Frozen upstream tree: `ac81df5b0332908409b60a0addc7e967eed868ea`
- Current `origin/main` (unrelated sanitation lineage):
  `c338b65e0a037eaa31e370d287805d757469a876`
- Current `upstream/main` (`b19cebecc0169097bda7539af03c886e03bdeafe`) is
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
configured-identity provenance, dynamic-tool persistence, and browser
computer-use routing. P1-P6 are accepted evidence and are not reopened absent
a concrete candidate defect.

## P7-owned composition

P7 may change only shared workflows, validation configuration, the permitted
lockfiles, and the divergence/status evidence pages listed in the work-item
contract. The bounded upstream harvest classified the following items:

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

## Acceptance boundary

Acceptance requires an exact final candidate SHA/tree and complete path
manifest, standard public GitHub-hosted proof on that exact product head,
decision-complete managed Luna-high review, resolved applicable conversations,
and an accepted Ops handoff to root. A source commit, branch, PR, workflow
dispatch, partial green run, or review-in-progress is not terminal evidence.

Windows material remains hosted-proof work only; this ledger makes no
provisional P5 Windows acceptance claim.
