/**
 * Temporary hosted diagnostic for the CLA safe-fixture recognition mismatch.
 */
import actions
import WorkflowSecurity

from Job job, UsesStep step
where
  workflowName(job.getWorkflow(), "CLA Assistant") and
  step.getEnclosingJob() = job
select job,
  "id=" + job.getId() +
    "; condition=" + job.getIf().getCondition() +
    "; gate=" + (if jobConditionMatchesExactly(job, "github\\.repository\\s*==\\s*'openai/codex'") then "yes" else "no") +
    "; callee=" + step.getCallee() +
    "; version=" + step.getVersion() +
    "; action=" + (if step.getCallee().regexpMatch("(?i)^contributor-assistant/github-action$") then "yes" else "no") +
    "; pin=" + (if step.getVersion().regexpMatch("(?i)^ca4a40a7d1004f18d9960b404b97e5f30a505a08$") then "yes" else "no") +
    "; signatures=" + step.getArgumentExpr("path-to-signatures").getExpression() +
    "; branch=" + step.getArgumentExpr("branch").getExpression() +
    "; document=" + step.getArgumentExpr("path-to-document").getExpression() +
    "; allowlist=" + step.getArgumentExpr("allowlist").getExpression() +
    "; checksOut=" + (if jobChecksOutCode(job) then "yes" else "no") +
    "; hasRun=" + (if exists(Run run | run.getEnclosingJob() = job) then "yes" else "no") +
    "; hasOtherUses=" +
      (if exists(UsesStep other |
        other.getEnclosingJob() = job and
        not (
          other.getCallee().regexpMatch("(?i)^contributor-assistant/github-action$") and
          other.getVersion().regexpMatch("(?i)^ca4a40a7d1004f18d9960b404b97e5f30a505a08$")
        )
      ) then "yes" else "no") +
    "; pinnedPredicate=" + (if jobUsesPinnedClaAssistant(job) then "yes" else "no")
