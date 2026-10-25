# Bounded Codex tree usage query

Run from the repository root with an installed Codex CLI and the intended
account already available to it. Starting a local app-server is not strictly
read-only, so the command requires an explicit acknowledgement:

```sh
python3 scripts/codex_usage_tree.py \
  --thread-id <root-thread-uuid> \
  --allow-local-app-server-startup
```

The command starts `codex app-server --listen stdio://` and opts into the
experimental API. The normal `codex app-server` CLI path does not accept
`--session-source`; it uses the CLI's built-in VS Code source. This query uses
only these app-server RPC methods:

1. `account/usage/read` for the root thread and each unique descendant.
2. `thread/list` for spawned descendants of the root, with
   `useStateDbOnly: true`, both archived states, and bounded cursor pagination.

The default ceiling is 100 listed descendants per archived state (at most 201
usage reads including the root). `--max-threads-per-state` can raise that to
500. If pagination is still available at the cap, output marks lineage
incomplete. Each archived-state query also stops after the same number of pages
as its thread cap, which bounds empty/repeating cursor responses while allowing
short pages. `--timeout-seconds` bounds each app-server request;
`--overall-timeout-seconds` bounds the complete query (default 300 seconds,
maximum 3600). After a timeout the child is terminated, with a bounded
terminate/kill wait. A complete result means all rows returned by the
state-DB-only query were paged within the caps, every listed thread had a usage
row, and token/rate classes were known. It is not an independent census of every
historical event or provider record.

The RPC calls themselves are reads, but creating this app-server runs normal
startup. Exact-U startup initializes the local SQLite state runtime and waits
for rollout metadata backfill; if its persisted backfill is not complete, that
startup invokes the broad rollout backfill before serving requests. Startup can
also recover damaged state databases. `useStateDbOnly: true` prevents rollout
scanning only within this `thread/list` request; it does not disable startup
initialization or backfill. This script does not attach to an existing server.
Do not use it where normal app-server startup effects are unacceptable; the
explicit flag is an acknowledgement of that boundary, not a no-write guarantee.

Output is JSON on stdout. It includes the root, unique children with parent
IDs, root-only / child-only / whole-tree sums grouped by the exact usage API
model, reasoning effort, and speed. Null token fields and missing thread usage
remain incomplete rather than becoming zero. Native estimated credits remain
separate from locally calculated credits; native estimates are not debit or
invoice records. Local credits are the sum of `tokens × rate / 1,000,000`,
using `netNewInputTokens` as uncached input, `cachedInputTokens` as cached input,
and `outputTokens` as output. The dated operator rate table embedded in the
script is included in each result; unknown models or rate classes are named in
`missingRates`, and incomplete costs have `credits: null` plus any known
subtotal. `--rates-json` can supply a replacement map with this format:

```json
{
  "source": "operator rate source",
  "date": "YYYY-MM-DD",
  "credits_per_million_tokens": {
    "exact-model-name": {"input": 50, "cached_input": 2.5, "output": 250}
  }
}
```

Exit status is 0 only for a complete result; partial or unavailable results
exit 2 and state the boundary in the JSON. In particular, accounts for which
`account/usage/read` is denied or absent must not treat the command as
available: the native route may be plan/account gated. This adapter does not
replace or retire existing usage capability until a real consumer confirms
route availability and accepts the resulting scope. It does not read raw
response history, scan rollouts, write a runtime ledger, or persist results.
