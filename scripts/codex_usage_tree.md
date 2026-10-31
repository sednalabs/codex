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

## Explicit local rollout mode

When the account-scoped usage API is unavailable, the command also accepts
explicit rollout paths and can aggregate native per-response usage already
persisted in those files without starting app-server or searching the sessions
directory:

```sh
python3 scripts/codex_usage_tree.py \
  --thread-id <root-thread-uuid> \
  --rollout-file <exact-root-rollout.jsonl> \
  --rollout-file <exact-child-rollout.jsonl> \
  --expected-child-id <child-thread-uuid>
```

Supply one `--rollout-file` per known tree thread. Supply every descendant ID
from a previously complete, bounded `thread/list` result with repeated
`--expected-child-id`; the command cross-checks those IDs and the persisted
parent chain against the named files. It cannot independently prove that the
caller supplied a complete descendant list. The command reads no SQLite
database, starts no app-server, discovers no rollout paths, and emits no raw
rollout lines. Each explicitly named JSONL file is limited to 10 MiB; only
session identity, parent identity, turn model/effort, and token-usage fields
are retained for the report.

Local usage comes from the per-response `TokenUsageRecord.usage` fields joined
to persisted `turn_context` model/effort. The record's cumulative turn/thread
snapshots are ignored so they are not double-counted. This is best-effort:
those rollout events do not
provide an independent expected-response denominator, so this mode always
reports `status: partial`, `usageComplete: false`, and exit status 2 even when
all supplied files and rate classes are present. Missing files and null token
fields remain null/incomplete. Uncached input is calculated as total input
minus cached input only when cache-write input is zero. A missing cache-write
field follows the native `TokenUsage` default of zero; a null or nonzero
cache-write class has no supplied operator rate and makes affected uncached
input/cost incomplete. Local calculated credits use the dated operator rate
map; they are estimates, not provider-reported credits, quota usage, or debit.
The API route remains the only source this tool can classify as complete.
Local rollout and database modes are intentionally best-effort and do not
replace the API or rewrite the existing ledger.

## Explicit usage database mode

To query rows recorded in the local usage ledger, pass the exact database file
path. The helper does not infer the effective Codex home from environment
variables or start an app-server:

```sh
python3 scripts/codex_usage_tree.py \
  --thread-id <root-thread-uuid> \
  --database <effective-sqlite-home>/usage_1.sqlite
```

Use the `sqlite_home` selected by the running Codex configuration when it is
known; `--database` is authoritative for this read. This mode opens that file
read-only and does not run migrations. It follows persisted `usage_threads`
parent, fork-parent, and root lineage, then reports root, descendant, and
whole-tree response rows grouped by requested model and provider-effective
model. If no provider-effective model was recorded, it remains null; any
rate-card estimate falls back to the requested model and is labelled as such.
`providerReportedCredits` is shown separately and is not independently
verified as an account debit; `actualDebit` remains `not_observed`.
For new projected responses, stored timestamps are observation time; provider
start time and latency are not independently measured.

Database results are always `partial`: they cover only usage-bearing responses
successfully projected to this database and known lineage rows. They have no
independent expected-response denominator, so they cannot prove a complete
historical census. Large trees and call sets are bounded; truncation is
reported in the result. The command does not treat an absent call row as a
zero-token response. Current limits are 900 known threads and 10,000 response
rows; reaching either limit is reported as truncation. The response-level rows
include request/turn identifiers, token classes, status, requested/effective
model identity, and any provider-reported credits present in the ledger.
`selectedLedgerRowsComplete` and the separate lineage/response truncation flags
show whether those bounded queried rows were fully read; they do not change the
always-false `historicalCoverageComplete` claim.

Output is JSON on stdout. It includes the root, unique children with parent
IDs, and root-only / child-only / whole-tree sums grouped by the exact model
reported by the usage API or joined from persisted `turn_context` in local
mode, plus reasoning effort and speed where available. Null token fields and
missing thread usage remain incomplete rather than becoming zero. Native
estimated credits remain separate from locally calculated credits; native
estimates are not debit or invoice records. Local credits are the sum of
`tokens × rate / 1,000,000`, using `netNewInputTokens` as uncached input,
`cachedInputTokens` as cached input, and `outputTokens` as output. The dated operator rate table embedded in the
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
`account/usage/read` is denied or absent must not treat API mode as available:
the native route may be plan/account gated. API mode does not read rollout
history; local mode reads only explicitly named rollout files. Neither mode
writes a runtime ledger or persists results. This adapter does not replace or
retire existing usage capability until a real consumer confirms route
availability and accepts the resulting scope.
