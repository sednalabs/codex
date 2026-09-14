# Guarded history publication

The default publisher performs one atomic transaction over the approved full
reference map. A manifest without `publication_plan` and
`publication_plan_sha256` retains that behavior. Staged publication is an
explicit alternative, not a fallback after an error.

## Explicit staged contract

Build the plan with `staged_publication.build_plan` and include both the
complete returned `publication_plan` and its canonical SHA-256 in the approved
manifest. The plan fixes:

- One changed, noncritical branch as a useful canary.
- Deterministically ordered batches of at most 100 changed refs. This is a
  conservative operating choice, not a claimed GitHub limit.
- One final coupled batch containing the protected refs, main, the publication
  source branch, and the operator-identified current release refs. The source
  branch remains unchanged until this final batch so interrupted work can
  resume using the same reviewed workflow source.
- Every complete intermediate namespace digest, with unchanged refs preserved.
  Intermediate states are observable: atomicity applies within a batch, not
  across the whole staged operation.

The manifest and administrator witness bind the exact mode, plan, source,
prepared artifact, current run and current attempt. The existing approval
freshness interval is unchanged. There is no automatic permission expansion,
freshness extension, source substitution or scope inference.

## Before mutation

The protected workflow exports the entire plan, exact per-batch leases and
run/attempt binding in `publication-staged-intent.json`. It uploads that with
the existing immutable recovery-intent artifact **before publisher credentials
or mutations**, then verifies the actual artifact metadata, bytes and contents.
Publication verifies that custody again. A missing, expired, mismatched or
incomplete intent artifact prevents staged mutation.

The global uploaded intent covers every permitted batch before any of them
runs. Local `staged-checkpoint.json` records progress with atomic replacement
and fsync, but is diagnostic evidence, not the sole cross-run recovery source.

## Execution and continuation

Before each batch, the publisher checks current approval freshness,
administrative controls, writer state and the complete expected namespace.
Immediately before the Git mutation it checks freshness again. Each ref has an
explicit expected-old-object lease, and each batch uses `git push --atomic`.

Every attempted batch receives an authoritative full-namespace readback:

- The exact next prefix means the batch applied, including after response loss.
- The exact unchanged prefix stops execution without retry.
- Any other state, extra/missing refs or unavailable readback stops execution
  as ambiguous; the operator must reconcile it without overwriting foreign work.

Expiry or another non-clearing mutation gate produces a checkpointed stop
(CLI exit 2), not an automatic retry or rollback. The workflow still attempts
control restoration and token revocation. Resume only after new exact
reconciliation and approval, reusing the same-head prepared artifact when its
original identity and retention remain valid.

After process/runner loss, determine progress from the entire remote namespace
and the immutable plan: it must equal exactly one approved prefix. Never infer
progress from an arbitrary mixture of old and new refs or a partial listing.
Loss of a local checkpoint does not erase a completed batch. Missing prior-run
receive-capture custody remains a separate reconciliation obligation; a resumed
run must not claim that its own capture covers the preceding run.

## Completion and recovery

Completion still requires the exact full final namespace, a clean isolated
fetch, matching object types, fsck proof, custody reconciliation and control
restoration. A successful push or checkpoint is not completion.

**Control restoration does not roll back already updated refs.** The helper
`reverse_lease_plan` only describes recovery from a proven prefix, in reverse
batch order, using each approved new OID as the expected-old lease and the
original OID as its destination. It neither grants authority nor executes a
rollback. Any ref rollback needs separate exact operator authorization,
available verified original objects, applicable controls and approval,
authoritative per-step readback, and a stop on foreign state. Never force a
rollback merely to make the namespace look original.

Do not publish raw diagnostics, credentials or private archive contents.
