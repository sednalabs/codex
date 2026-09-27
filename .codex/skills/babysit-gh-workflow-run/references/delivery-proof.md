### Exact merge-queue delivery receipt

Use `scripts/gh_pr_delivery_watch` when one pull request must be proved through
all three delivery identities: its expected full head SHA, the synthetic
`merge_group` candidate, and the resulting merge commit on `main`. It performs
one bounded, receipt-owned PR-delivery observation after the merge-group run;
both long workflow waits remain delegated to the existing
`gh_workflow_run_watch --watch-until-terminal` helper.

```bash
.codex/skills/babysit-gh-workflow-run/scripts/gh_pr_delivery_watch \
  --repo sednalabs/codex \
  --pr 123 \
  --expected-head-sha 0123456789012345678901234567890123456789 \
  --merge-group-run-id 123456789
```

- `--expected-head-sha` must be a full 40-character SHA; prefixes are rejected.
- Supplying `--merge-group-run-id` is preferred. Without it, a single discovery
  read is accepted only when exactly one `blocking-ci` queue candidate names the
  PR; absent or multiple candidate SHAs stop the receipt.
- A queue ref that names the PR is only a discovery selector. Before waiting,
  the candidate must be proven by GitHub commit ancestry to contain both the
  exact expected PR head and the PR's current base SHA; an older or otherwise
  uncorrelatable candidate stops the receipt.
- Workflow inputs accept a display name, file basename, or a
  `.github/workflows/*.yml` / `.yaml` path. They are compared by their
  normalized workflow basename, so `Blocking CI` matches `blocking-ci.yml`.
- GitHub's downstream watcher receipt can omit an authoritatively selected
  exact run's `event`. The delivery receipt accepts that absence only after
  stage-specific exact identity checks: a `merge_group` receipt needs the
  selected candidate run id, queue ref, full candidate SHA, and normalized
  workflow; a `post_merge` receipt needs a direct Actions read of the nested
  exact-target selection's run id to prove `push`, the full merge SHA, main
  ref, and normalized workflow. The compact nested receipt alone is not event
  evidence for a post-merge run. A non-empty event other than `merge_group` or
  `push`, respectively, remains an identity mismatch and stops the proof.
- The defaults require a successful `blocking-ci` `merge_group` run and one
  selected `postmerge-ci` `push` run on the exact merge commit. The singular
  `--post-merge-workflow` selector is deliberately not an inventory of every
  workflow that a merge may independently trigger.
- A successful candidate proves delivery only when GitHub ancestry establishes
  that the selected candidate SHA reaches the eventual merge commit. A later
  superseding queue candidate fails closed instead of borrowing the earlier
  success. `GH_PR_DELIVERY_WATCH_PYTHON` is forwarded to the nested watcher as
  `GH_WORKFLOW_RUN_WATCH_PYTHON` when an explicit interpreter is required.
- After the merge-group run succeeds, the helper waits for the normal GitHub
  queue-to-merged transition for at most
  `--merge-observation-timeout-seconds` (default: 300). It uses the existing
  `--poll-seconds` interval (default: 60), caps the final sleep at the deadline,
  and applies the shrinking remaining deadline to every GitHub read in this
  observation. A hung read therefore stops with
  `stop_merge_observation_timeout` rather than exceeding the requested bound.
  This is receipt-owned bounded observation, not a model or operator polling
  loop. Every poll, including one whose first PR read is already merged,
  reasserts the selected run identity, candidate association, and absence of a
  newer candidate SHA; still-queued polls also reassert the current exact PR
  head/base. Changed invariants fail closed. A normal process interrupt returns
  the scoped `stop_merge_observation_interrupted` receipt; it does not change
  the selected-workflow proof scope.
- Standard output is one compact JSON receipt. A non-zero status still emits a
  receipt with a stable `stop_*` action and the first failed job when available.

`stop_pr_delivery_proven` means the selected-workflow delivery proof succeeded:
the exact merge-group candidate and the receipt's
`proof_scope.selected_post_merge_workflows` each matched the expected identity.
It does **not** mean every independently triggered post-merge workflow passed,
and it does **not** establish whole-repository health. The receipt makes that
boundary machine-readable with `proof_scope.whole_repository_health_proven:
false` and records the selector also at `post_merge.selected_workflow`.

For a relevant merge where `Native Windows Bazel health` is required evidence,
that workflow must first be introduced and landed on `main` by its separate CI
PR. It is not resolvable from a watcher-only branch or from a base that does not
yet contain the workflow. Once the CI PR is landed and the relevant merge has
completed, this separate exact-main-SHA watch is mandatory; it remains outside
the default `postmerge-ci` proof:

```bash
.codex/skills/babysit-gh-workflow-run/scripts/gh_workflow_run_watch \
  --repo sednalabs/codex \
  --target "workflow=Native Windows Bazel health,ref=main,head-sha=<merge-commit-sha>" \
  --watch-until-terminal
```

The helper fails closed if the PR head changes, a queue entry disappears without
a merge, the merge commit cannot be correlated to `main`, or either watched run
reports an identity different from its exact target SHA.
