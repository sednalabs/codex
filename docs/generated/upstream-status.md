# Generated upstream status

> `status: generated` · `authority: evidence` · `captured: 2026-09-24`

This page is the exact P7 composition receipt for the frozen candidate. Hosted
proof and semantic review remain pending on this exact product head.

| Field | Value |
| --- | --- |
| Product repository | `sednalabs/codex` |
| P6 branch/head | `repair/w14078-p6-tui-realtime` at `0d0f73e7fd44ec0d5bc2dc0ba5625a8559ed8a9c` |
| P6 tree | `6be24ec81e116584c248c937302e0dacb0949c94` |
| Frozen upstream | `openai/codex` at `392f56a611c412b9b2eb1d9d4e59a3b42bee483a` |
| Frozen upstream tree | `ac81df5b0332908409b60a0addc7e967eed868ea` |
| Historical source composition SHA/tree | `77968332f63d31490d71ce462a868a7e224b75f1` / `867e02cec96c67e5afaf363f965204ab28c9cfb2` |
| Final candidate SHA/tree | `external exact-delivery receipt; rehydrate before proof or landing` |
| Candidate ancestry | `must remain rooted at frozen upstream; old origin/main is unrelated` |
| Live origin/main | `c338b65e0a037eaa31e370d287805d757469a876` |
| Live upstream/main | `b19cebecc0169097bda7539af03c886e03bdeafe` (next train only) |
| Hosted proof | `required on the final exact product head from the external delivery receipt; prior runs 35998821683 and 36002658222 are historical evidence only` |
| Semantic review | `Luna-high required; no review result yet` |
| Root cutover | `not performed; root-owned` |

## Divergence evidence

The frozen upstream is an ancestor of the accepted P6 head with divergence
`0` upstream-ahead and `58` downstream-ahead commits. The live origin/main and
upstream histories have no merge base after sanitation; their old count is not
a product-candidate acceptance metric. Do not import that old lineage.

## Path and preservation boundary

P7 product paths are limited to `.github/**`, `justfile`, validation
configuration (`validation-lanes.json` and `test_ci_planners.py`), permitted Cargo/Bazel locks, and the three divergence/status
evidence families named in w14079. PR #854, foreign dirty paths, proof
overlays, and old origin/main are preserved with explicit track/owner
dispositions. This status page does not claim provisional P5 Windows
acceptance.
