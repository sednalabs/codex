# Generated upstream status

> `status: generated` · `authority: evidence` · `captured: 2026-07-28T13:37:40Z`
>
> This checked-in page is a current-state projection. Regenerate it after each
> upstream refresh; it is not a substitute for the registry or a release note.

## Snapshot

| Field                             | Value                                                                |
| --------------------------------- | -------------------------------------------------------------------- |
| Downstream ref                    | `origin/main` at `bff348fd68a99e1996d00dce1d46ba8ed9d37be3`          |
| Live upstream ref                 | `upstream/main` at `7cde2323f3712999e9ab98b16287e08b7735d52f`        |
| Maintained mirror ref             | `origin/upstream-main` at `3418498f01422f5f650ea645d4bd19e05c3a9616` |
| Merge base                        | `a4535884169be8da2f81b8a4debecbd4dc11aa97`                           |
| Downstream vs live upstream       | `2131` downstream-ahead, `60` upstream-ahead commits                 |
| Downstream-only non-merge commits | `1800` unique, `0` patch-equivalent                                  |
| Mirror vs live upstream           | `0` mirror-ahead, `14` upstream-ahead (`stale_ff_only`)              |

The mirror is therefore not an exact comparison baseline today. The current
projection uses the live upstream ref and records the mirror lag explicitly.

## Audit receipt

The registry-backed audit was run with:

```text
python3 scripts/downstream-divergence-audit.py --repo . \
  --downstream-ref origin/main --mirror-ref upstream/main \
  --upstream-remote upstream --upstream-branch main \
  --registry-path docs/divergences/index.yaml \
  --output-dir /tmp/sedna-divergence-audit-current \
  --format both --code-only --enforce-registry
```

Before this documentation update, the receipt identified one uncovered live
code path: `scripts/pyproject.toml`. The registry now assigns that path to the
existing `ci-workflow-automation` carry; rerun the command above to produce the
post-change receipt and confirm a zero uncovered-path count.

## Related projections

- [`upstream-gaps.md`](upstream-gaps.md) lists the bounded upstream-only work
  that needs a later harvest decision.
- [`app-server-protocol-delta.md`](app-server-protocol-delta.md) records the
  protocol-specific snapshot and regeneration boundary.
- [`../sedna-docs-governance.md`](../sedna-docs-governance.md) defines the
  authority and refresh rules.
