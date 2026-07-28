# Generated upstream gaps

> `status: generated` · `authority: evidence` · `captured: 2026-07-28T13:37:40Z`
>
> This is a bounded harvest queue, not a claim that every upstream change is
> safe to apply to the downstream fork.

## Current gap shape

At the captured refs, `upstream/main` is 60 commits ahead of `origin/main`.
The fork is intentionally carrying substantially more downstream work (2131
commits ahead by the same comparison). The full commit set belongs in the
machine-readable audit output; this page records only high-signal candidates
for the next harvest.

| Candidate | Current decision | Why |
| --- | --- | --- |
| `829f5b6b59` — separate app and exec RPC ownership | `track` | Protocol ownership may affect the downstream app-server carry; inspect with the next app-server code harvest before changing docs or schemas. |
| `0a0d09ad21` — clarify docs-folder guidance in `AGENTS.md` | `port concept` | The governance rules in [`../sedna-docs-governance.md`](../sedna-docs-governance.md) capture the relevant authority/freshness distinction without importing upstream repository policy wholesale. |
| `67849d950d` — remove local docs and specs | `ignore` | Downstream carry documentation is an intentional maintenance surface; removal would erase the evidence needed to operate the fork. |
| Upstream PR #30866 — reconcile loaded thread history on resume | `track` | Resume identity and fork lineage are adjacent to the usage/session work, but this docs train does not change app-server behavior. |
| Upstream PR #31487 — installed runtime snapshot API | `track` | Potentially useful for release-channel and runtime documentation; wait for a code-level harvest decision. |
| Upstream PR #31515 — client-only web-search result metadata | `track` | App-server protocol surface may change; do not pre-adopt generated schema text. |

## Separate priority-0 gap

The current downstream `app-server-daemon` sources still contain upstream
installer/update-channel references. That is a runtime/code remediation and is
not silently fixed by this documentation-only train. See the explicit scope
note in [`../sedna-docs-governance.md`](../sedna-docs-governance.md); update the
installer guidance only after the corresponding code change has landed and
hosted validation proves the release channel.

## Regeneration boundary

Recompute this page from the live `upstream/main` and `origin/main` refs after
each harvest. Keep exact commit IDs and a dated adoption decision in
[`../upstream-sync/2026-07-28.md`](../upstream-sync/2026-07-28.md), then move
resolved entries into the next dated snapshot rather than rewriting history.
