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

To use the database's wider rate table instead of the embedded map, choose an
explicit date/time for a **standard-rate scenario**:

```sh
python3 scripts/codex_usage_tree.py \
  --thread-id <root-thread-uuid> \
  --database <effective-sqlite-home>/usage_1.sqlite \
  --standard-ledger-rates-at 2026-10-01T00:00:00Z
```

The argument is an ISO timestamp with an explicit timezone. The selected rates
apply to the whole report as a scenario; they do not prove
the tier or price of each call, and historical calls may have different rates.
Only default/standard Codex token rates are selected. Conflicting overlapping
rates and image/text-specific rate classes are omitted instead of guessed;
unpriced models remain explicitly missing. This option cannot be combined with
`--rates-json`.

Database results are always `partial`: they cover only usage-bearing responses
successfully projected to this database and known lineage rows. They have no
independent expected-response denominator, so they cannot prove a complete
historical census. Large trees and call sets are bounded; truncation is
reported in the result. The command does not treat an absent call row as a
zero-token response. Current limits are 900 known threads and 10,000 response
rows; reaching either limit is reported as truncation. The response-level rows
include request/turn identifiers, token classes, status, requested/effective
model identity, and any provider-reported credits present in the ledger.

Add `--aggregate-recorded` to request a model and role summary over the
selected recorded threads instead of loading detailed rows. This is
an estimate using the selected rate map, not an account debit; requested-model
fallback and missing token dimensions are labelled. The aggregate has a
four-second query limit and a 100-group output limit, and remains partial if
lineage is incomplete or truncated. Normal detailed reports remain separately
bounded.

To compare the two explicitly known ledger files, add one `--also-database`
and keep the report aggregate-only:

```sh
python3 scripts/codex_usage_tree.py \
  --thread-id <root-thread-uuid> \
  --database <active-sqlite-home>/usage_1.sqlite \
  --also-database <historical-sqlite-home>/usage_1.sqlite \
  --aggregate-recorded \
  --checkpoint-file <private-owner-only-checkpoint.sqlite> \
  --continue-until-complete
```

Both paths must name distinct existing databases. The command reads only those
two files, read-only and query-only; it performs no discovery, migration, or
ledger write. Each source gets its own lineage and recorded aggregate. The
optional checkpoint is a separate owner-only SQLite file that retains the
minimum provider-call identity and aggregate state needed to resume; it may
contain sensitive call IDs and should be kept private. It must be a regular,
non-symlink file with owner-only permissions. Reuse the same explicit
checkpoint path, root, source pair, and rate scenario to continue. Without a
checkpoint, bounded work is reported but cannot resume. Each run processes at
most 25,000 provider-call rows per source, with the existing per-query,
per-source, whole-operation, thread, cumulative-row, memory, and group limits
still in force. To make one continuation smaller, use the optional
`--max-rows-per-source-per-pass N` with a checkpoint; it accepts 1 through
25,000 and defaults to the hard 25,000-per-source run limit. For example,
`--max-rows-per-source-per-pass 4096` allows at most one existing indexed page
per source per pass, plus the bounded lookahead row. This soft cap changes only
how far a saved cursor advances and may be changed between resumptions without
changing checkpoint identity; the hard bound and all cumulative limits remain
unchanged. A two-source pass can process up to twice the selected per-source
cap. When a per-run limit is reached, per-source subtotals are returned and
the output marks the result resumable; merged totals remain withheld until
both source scans finish. The cumulative 200,000-row per-source and 300,000-row
total limits are not resumable beyond those caps.

`--continue-until-complete` repeats bounded operations with the same checkpoint,
printing only per-source row counts and continuation status to stderr. It stops
at completion, a detected source/checkpoint/cumulative-limit condition, or
after its pass budget. `--max-resume-passes N` selects a budget from 1 through
16 (default 16); the helper idles for one second between passes. The maximum is
16 × 24 seconds of operation-bound time plus 15 seconds idle, or 6 minutes 39
seconds plus small startup overhead. Combine `--max-resume-passes 1` with a
smaller `--max-rows-per-source-per-pass` for one resumable slice. If a pass
limit is reached, rerun with the same checkpoint and rate scenario; the soft
row cap may be adjusted within its limit. Ctrl-C stops the process with exit
130; committed checkpoint pages remain safe to resume, while the current
uncommitted page is retried. The live ledgers remain read-only throughout.

The first page for each source freezes its known thread lineage and records a
`usage_provider_calls.rowid` high-water mark. Later pages use indexed keyset
queries up to that mark, so appends after it are excluded instead of forcing a
restart. The two source snapshots are captured independently and are not
atomic with one another or across separate command invocations. In-repository
runtime call rows are inserted once; repeated thread registration may upsert
`usage_threads` parent/root/fork metadata after the lineage snapshot, and this
mode does not reconcile those changes. The later `final_model` and
`model_snapshot` update is excluded from the report's effective-model and
deduplication fields; effective model uses `actual_model_used` only, so it does
not fall back to a later finalized model. The report assumes call rows at or below the saved
high-water mark are otherwise immutable. It detects source path/device/inode,
SQLite format-header, or usage-schema replacement and invalidates those saved
pages. It cannot detect out-of-band in-place call-row edits or deletes,
`VACUUM`/rebuild that preserves the observed source identity, rowid reuse after
deletion, replacement preserving every observed identity marker, or manual or
runtime lineage changes after a source's frozen first page. Output exposes
these limits under `snapshot`; a completed scan means all rows selected under
that bounded snapshot contract were read, not that a single atomic historical
database snapshot was proven.

The cross-ledger merge counts a non-empty `provider_call_id` once only when its
thread, turn, request, provider, time, model provenance, status, token and
reported-credit content match and both schemas contain every compared column.
Matching IDs with different content are conflicts; matching IDs from a schema
missing any compared column are unverifiable. Unkeyed, conflicting, and
unverifiable rows remain visible in per-source results and are withheld from
the merged totals. If a source cannot be scanned completely, merged totals are
withheld altogether. The output reports per-source column coverage, scanned
timestamps, dedupe/conflict/unverifiable counts, and coverage limits without
printing database paths or provider-call IDs.

Cross-ledger work is bounded to 900 known threads per source, 25,000 rows per
source per invocation, 200,000 scanned call rows per source, 300,000 total
rows, a 192 MiB estimated retained-merge budget, at most 100 merged groups,
4 seconds per query, 12 seconds per source, and 24 seconds for the complete
operation. The provider-call query plan must use an exact-thread index;
otherwise the merge fails closed. Cumulative row, memory, and group limits are
terminal for that request. Per-run time/row limits are checkpoint-resumable;
the output keeps per-source aggregates visible and withholds merged totals
until both sources finish. Results remain partial for historical lineage
regardless of cross-ledger deduplication.

If a source has no indexed parent/fork lookup, the helper uses its existing
`root_thread_id` index with a 901-row sentinel query and verifies selected
parent/fork chains within the 900-thread memory bound. It does not scan the
whole thread table. The output labels this as
`root_thread_id_index_fallback`; merged totals are scoped and labelled
`known_root_indexed_rows_only`, not as a complete descendant census. The
fallback cannot discover descendants whose stored root marker is absent or
incorrect. Broken or cyclic parent chains, inconsistent root metadata, missing
root indexes, or the thread cap make lineage incomplete and withhold merged
totals; a missing persistent root index fails closed before its candidate
query.
Calculated credit estimates retain known subtotals and missing-rate/token
labels; provider-reported credits are not verified account debits, and actual
debit remains unobserved. If a standard ledger-rate scenario is selected, the
rates loaded from the primary database apply to both sources as a scenario,
not as historical billing evidence.

This command currently does not join the effective-dated per-call credit view
or billing-policy table. It uses the selected model-rate map to provide useful
scenario estimates, keeps provider-effective model separate from requested-model
fallback, and reports per-source counts of records missing provider, effective
model, timestamp, service-tier/source, speed, billing-surface, or account-plan
context. Consequently, an active ledger rate row is not silently treated as a
historical match for the other ledger: exact per-call policy/rate matching
remains unavailable here when those fields or an applicable interval are
missing. Use the estimates only as labelled scenarios until that context is
present and matched; do not treat them as account charge or debit.
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
