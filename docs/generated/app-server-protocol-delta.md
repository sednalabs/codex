# Generated app-server protocol delta

> `status: generated` · `authority: evidence` · `captured: 2026-07-28T13:37:40Z`
>
> This is a ref-to-ref snapshot of generated protocol artifacts. It is not the
> normative app-server contract.

## Snapshot

- Downstream: `origin/main` at `bff348fd68a99e1996d00dce1d46ba8ed9d37be3`
- Upstream: `upstream/main` at `7cde2323f3712999e9ab98b16287e08b7735d52f`
- Comparison: `git diff --name-status upstream/main..origin/main -- codex-rs/app-server-protocol`
- Audit receipt: `2026-07-28T13:37:40Z` (stable)
- Result: the protocol tree has broad generated and handwritten changes; the
  downstream tree adds `ComputerUseCallParams.json` and
  `ComputerUseCallResponse.json` relative to the current upstream ref.

The added computer-use request/response schemas are a concrete downstream
protocol extension. They must remain aligned with the normative behavior and
security boundaries in [`../native-computer-use.md`](../native-computer-use.md).
The rest of the large schema diff is not independently a decision to adopt or
discard upstream work; use the dated harvest record and registry for that.

## Regeneration and validation

When a protocol change is intentionally made, regenerate the checked-in schema
fixtures and run the focused hosted lane. The repository entry points are:

```text
just write-app-server-schema
just test -p codex-app-server-protocol
```

On this workstation, follow the repository policy and prefer the GitHub-hosted
validation lane. The commands above are the reproducible boundary, not a claim
that local builds were run for this docs-only change.

## Authority

For current protocol behavior, consult the app-server source and generated
fixtures at the exact refs under review. For the user-facing computer-use
contract, consult [`../native-computer-use.md`](../native-computer-use.md). For
historical bundle-era commentary, consult
[`../app-server-schema-differences.md`](../app-server-schema-differences.md),
which is intentionally labeled historical.
