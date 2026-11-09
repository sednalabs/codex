# w14939 upstream convergence: draft

This is an incomplete source draft, not a release or installation candidate.
Its upstream base is `9f04d886f3663b584fba87d6fd71001fb6f9fcf3`; the Sedna
fixture source is `97f6c6b811cbda5aa618356e6761c541da2b5032`.

The state startup preflight requires a known successful migration prefix. It
checks all identities before repairing the historical upstream recency-38
record, with the repair and normal migration in one transaction. This rejects
unknown newer state migration IDs, including IDs that upstream's
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

The sampling retry draft retries the selected model on `ServerOverloaded`,
honors a server deadline and otherwise uses capped backoff. It does not yet
cover all compaction paths, and it conflicts with upstream tests that expect
terminal overloads. Cancellation, ordinary bounded rate-limit handling and
no-tool-replay behavior need exact-candidate integration coverage.

Mailbox intent and targeted wake behavior, MCP device-code login with its
issuer and endpoint security checks, full session-history compatibility,
hosted validation, packaging and consumer acceptance remain unfinished. Do
not install or run this draft against a user profile.
