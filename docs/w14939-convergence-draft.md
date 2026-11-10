# w14939 upstream convergence: draft

This is an incomplete source draft, not a release or installation candidate.
The existing candidate was merged with upstream
`c82708f85803d21fe9046af407107a1ee105b6e5` in
`53b8bc981ea124845168e30a6e882be9af76d7f1`. The Sedna fixture source is
`97f6c6b811cbda5aa618356e6761c541da2b5032`. Evidence from older commits and
the preview artifact does not validate this merge or any later overlay draft.

The overlay integration is unfinished. The draft deliberately rejects overlay
configuration until request handling, origin fences and hosted validation are
complete. Its historical adoption binds a local user authorization to one
thread, the SHA-256 of the complete saved baseline and the SHA-256 of the exact
model-baseline edit plan. Only unique, single-match exact removals qualify. It
does not adopt known custom/protected provenance or authorize a different
thread, component or plan. Unknown, protected, absent and ambiguous outcomes
must be visible. The old installed engine and the w18679 source differ; their
full compatibility and role precedence are still being reconciled.

`scripts/w14939-overlay-identities.py` computes the two hashes locally from a
TOML fragment containing the effective `[model_baseline]` plan and the exact
UTF-8 baseline bytes supplied on stdin. It outputs only hashes, never input
text. A local operator must determine those bytes and verify that the fragment
matches the effective plan; a mismatch fails closed. The helper is synthetic-test
validated only, and no real thread, database, transcript or configuration has
been accessed to establish this candidate.

The state startup preflight requires a known successful migration prefix. It
checks all identities before repairing the historical upstream recency-38
record, with the repair and normal migration in one transaction. This rejects
unknown newer state and thread-history migration IDs, including IDs that upstream's
`ignore_missing` behavior would otherwise tolerate. It does not bridge Sedna
history or establish that a schema matches its ledger. The affected source is
`state/src/migrations.rs` and `state/src/sqlite.rs`; the owner is w14939. The
intended disposition is a guarded compatibility bridge once exact historical
identities and schema effects are established, with unknown histories rejected.

The bundled public SQL fixture recreates Sedna histories through versions 39
and 45. They conflict with upstream at 24 first; 39 and 45 have distinct SQL
and effects as well. Sedna 39 is byte-identical to upstream 35, Sedna 43 to
upstream 39, and Sedna 45 has no byte-identical upstream migration. These are
source comparisons, not authorization to rekey an existing ledger. The Python
fixture test checks the reconstructed histories; the Rust tests exercise
startup rejection and semantic preimages. Neither is
proof of a successful historical upgrade or package acceptance.

Two additional public-source snapshots reproduce the migration SQL of OCI
package source `1dea691446732a0ab03836f40f932387d9b1e087` and workstation
source `e03a40398272ee4b0e6cdb694e390498c86ac7d9`. Each has 62 migration
files identical to the corresponding refreshed upstream migrations. Synthetic
schemas and ledgers through 39, 45 and 62 match upstream, and the source
version 45 checksum begins `a7fadc8c`, matching the separately recorded
workstation observation. This does not identify OCI's actual ledger or prove
that either actual schema has no other history. Both recorded source trees
also contain the same eight thread-history migration files as upstream. A
synthetic history fixture applies the later migrations to earlier schema and
checks that fabricated turn, item and projection records survive; it is not
a copy of any real session. A subsequent untested Rust draft additionally
checks thread-history prefixes and refuses unknown ledger identities before
opening that database.

The capacity retry draft retries the selected model on `ServerOverloaded`,
honors the server deadline, and uses capped backoff. It also covers local and
remote v2 compaction, retaining the prior-model fallback. The focused retry
group passed 40 tests locally, including cancellation, bounded ordinary rate
limits, transport fallback, and a tool-output replay witness. Focused local
and remote compaction tests also passed. These results precede the current
mailbox changes, the refresh to this upstream base, and hosted package validation.

The mailbox draft adds optional intent and completion origin fields, keeps
progress messages quiet during native waits, and supports targeted any/all
completion waits. Its protocol compatibility test passed locally; the core
implementation passed focused local queue and wait tests, including targeted
all-completion filtering, before the refresh to this upstream base. A subsequent
draft adds native event waits and lease renewal; those changes remain untested. Full Sedna mailbox compatibility,
including structured receipts and mailbox persistence, is not established.

An MCP device-login draft uses upstream's routed and bounded OAuth HTTP
adapter, issuer-bound advertised discovery, public-client registration,
PKCE fallback, device polling and locked token storage. The rmcp-client library
compiled locally before the refresh; the CLI and device tests have not been compiled or run locally.
Device-only discovery, the `mcp add` hint, preservation of unknown credential
file fields, and Sedna's synthetic CLI login tests are included in a subsequent
untested draft. Bounded same-origin device-request redirects are included in the
draft; cross-origin redirects are rejected. Device-only refresh compatibility
still needs assessment.
Full session-history compatibility, hosted validation, packaging and consumer
acceptance remain unfinished. The owner has been asked only for non-sensitive
migration identity and schema metadata. Do not install or run this draft
against a user profile.
