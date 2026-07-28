# Sedna documentation governance

This page defines how downstream-divergence documentation stays trustworthy
while the fork is ahead of `upstream/main`.

## Authority and document classes

Every divergence document must make its authority and freshness visible near
the top of the page:

| Class | Meaning | Source of truth |
| --- | --- | --- |
| `current` | Maintained policy or contract | The document, with exact refs and `last_verified` metadata |
| `generated` | A reproducible projection of live repository state | The registry, refs, and generator command; the rendered page is a checked-in receipt |
| `historical` | A retained review, bundle, or decision record | The dated evidence named by the page; it must not be used as a current count |
| `draft` | Proposed wording or design | The linked review or decision; not an operational contract |
| `deprecated` | Retained only for migration or provenance | The superseding current or generated page |

Use `authority: normative` for behavior and policy, `authority: informative`
for explanation, and `authority: evidence` for snapshots and receipts. A page
that contains both policy and evidence must label the sections separately.

## Canonical sources

`docs/divergences/index.yaml` is the canonical registry for live downstream
code carries. It must cover every downstream-only code path reported by the
audit and must include the owner, boundary, extraction target, and guardrail
fields required by the audit runner. Narrative pages may explain why a carry
exists, but they must not invent divergence counts or silently replace a
registry entry.

The current-state projections are:

- [`generated/upstream-status.md`](generated/upstream-status.md) for exact refs,
  counts, and mirror health;
- [`generated/upstream-gaps.md`](generated/upstream-gaps.md) for bounded
  upstream-only work awaiting harvest; and
- [`generated/app-server-protocol-delta.md`](generated/app-server-protocol-delta.md)
  for the app-server protocol snapshot.

The carry ledger and regression matrix remain manually curated until a
generator is implemented. They are useful narrative and operational guides,
but their baseline must link to the generated status receipt.

## Refresh and review procedure

Run the following from a clean checkout after refreshing both remotes. Use the
live `upstream/main` ref when `origin/upstream-main` is stale:

```text
python3 scripts/downstream-divergence-audit.py --repo . \
  --downstream-ref origin/main --mirror-ref upstream/main \
  --upstream-remote upstream --upstream-branch main \
  --registry-path docs/divergences/index.yaml \
  --output-dir /tmp/sedna-divergence-audit-current \
  --format both --code-only --enforce-registry
just downstream-divergence-audit
python3 -m json.tool docs/divergences/index.yaml >/dev/null
git diff --check
```

Refresh the generated pages from the resulting JSON receipt, record the exact
capture time and refs, and review the dated entry in
[`upstream-sync/2026-07-28.md`](upstream-sync/2026-07-28.md). A non-zero audit
exit is a documentation defect to fix or explicitly record; it is not a reason
to publish a stale baseline.

During an Ops outage, an operator-held audit bundle may provide review context,
but it is not a public source of truth and must not be referenced with local
paths, mailbox identities, or other operator metadata. Preserve it separately
from the repository and publish only the redacted conclusions and reproducible
commands.

## Known limitations and next steps

- The mirror ref `origin/upstream-main` can lag the live upstream remote. The
  lag must be recorded rather than presented as an exact mirror.
- `docs/carry-divergence-ledger.md` and
  `docs/downstream-regression-matrix.md` still require intentional human
  maintenance.
- The next implementation step is a typed registry v2 plus generators for the
  ledger and regression projections, followed by a semantic audit of owners,
  extraction targets, and guardrail lanes.
- Daemon installer/update-channel correctness is a separate priority-0 code
  remediation. Documentation must point at the intended Sedna release channel
  only after that code change has landed; this documentation train records the
  gap without claiming the runtime fix is complete.
