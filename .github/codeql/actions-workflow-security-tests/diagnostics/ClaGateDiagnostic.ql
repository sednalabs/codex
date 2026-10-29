/**
 * Temporary hosted diagnostic for the CLA safe-fixture recognition mismatch.
 */
import actions
import WorkflowSecurity

from Job job, string label, string value
where
  job.getId() = "cla" and
  (
    label = "workflow" and value = job.getWorkflow().getName()
    or
    label = "condition" and value = job.getIf().getCondition()
    or
    label = "gate-match" and value = "yes" and
      jobConditionMatchesExactly(job, "github\\.repository\\s*==\\s*'openai/codex'")
    or
    label = "gate-match" and value = "no" and
      not jobConditionMatchesExactly(job, "github\\.repository\\s*==\\s*'openai/codex'")
    or
    label = "uses-step" and value = "yes" and exists(UsesStep step | step.getEnclosingJob() = job)
    or
    label = "uses-step" and value = "no" and not exists(UsesStep step | step.getEnclosingJob() = job)
    or
    exists(UsesStep step |
      step.getEnclosingJob() = job and
      (
        label = "callee" and value = step.getCallee()
        or
        label = "callee-match" and value = "yes" and
          step.getCallee().regexpMatch("(?i)^contributor-assistant/github-action$")
        or
        label = "callee-match" and value = "no" and
          not step.getCallee().regexpMatch("(?i)^contributor-assistant/github-action$")
        or
        label = "version" and value = step.getVersion()
        or
        label = "version-match" and value = "yes" and
          step.getVersion().regexpMatch("(?i)^ca4a40a7d1004f18d9960b404b97e5f30a505a08$")
        or
        label = "version-match" and value = "no" and
          not step.getVersion().regexpMatch("(?i)^ca4a40a7d1004f18d9960b404b97e5f30a505a08$")
        or
        label = "signatures" and value = step.getArgumentExpr("path-to-signatures").getExpression()
        or
        label = "signatures-match" and value = "yes" and
          step.getArgumentExpr("path-to-signatures").getExpression().regexpMatch("(?i)^['\"]?signatures/cla\\.json['\"]?$")
        or
        label = "signatures-match" and value = "no" and
          not step.getArgumentExpr("path-to-signatures").getExpression().regexpMatch("(?i)^['\"]?signatures/cla\\.json['\"]?$")
        or
        label = "branch" and value = step.getArgumentExpr("branch").getExpression()
        or
        label = "branch-match" and value = "yes" and
          step.getArgumentExpr("branch").getExpression().regexpMatch("(?i)^['\"]?cla-signatures['\"]?$")
        or
        label = "branch-match" and value = "no" and
          not step.getArgumentExpr("branch").getExpression().regexpMatch("(?i)^['\"]?cla-signatures['\"]?$")
        or
        label = "document" and value = step.getArgumentExpr("path-to-document").getExpression()
        or
        label = "document-match" and value = "yes" and
          step.getArgumentExpr("path-to-document").getExpression().regexpMatch("(?i)^['\"]?https://github\\.com/openai/codex/blob/main/docs/CLA\\.md['\"]?$")
        or
        label = "document-match" and value = "no" and
          not step.getArgumentExpr("path-to-document").getExpression().regexpMatch("(?i)^['\"]?https://github\\.com/openai/codex/blob/main/docs/CLA\\.md['\"]?$")
        or
        label = "allowlist" and value = step.getArgumentExpr("allowlist").getExpression()
        or
        label = "allowlist-match" and value = "yes" and
          step.getArgumentExpr("allowlist").getExpression().regexpMatch("(?i)^['\"]?codex,dependabot,dependabot\\[bot\\],github-actions\\[bot\\]['\"]?$")
        or
        label = "allowlist-match" and value = "no" and
          not step.getArgumentExpr("allowlist").getExpression().regexpMatch("(?i)^['\"]?codex,dependabot,dependabot\\[bot\\],github-actions\\[bot\\]['\"]?$")
      )
    )
    or
    label = "checkout" and value = "yes" and jobChecksOutCode(job)
    or
    label = "checkout" and value = "no" and not jobChecksOutCode(job)
    or
    label = "has-run" and value = "yes" and exists(Run run | run.getEnclosingJob() = job)
    or
    label = "has-run" and value = "no" and not exists(Run run | run.getEnclosingJob() = job)
    or
    label = "pinned-predicate" and value = "yes" and jobUsesPinnedClaAssistant(job)
    or
    label = "pinned-predicate" and value = "no" and not jobUsesPinnedClaAssistant(job)
  )
select job, label + "=" + value
