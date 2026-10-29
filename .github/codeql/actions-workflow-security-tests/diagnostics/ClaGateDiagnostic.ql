/**
 * Temporary hosted diagnostic for the CLA safe-fixture recognition mismatch.
 */
import actions
import WorkflowSecurity

bindingset[value, pattern]
predicate regexStatus(string value, string pattern, string label) {
  value.regexpMatch(pattern) and label = "yes"
  or
  not value.regexpMatch(pattern) and label = "no"
}

predicate gateStatus(Job job, string label) {
  jobConditionMatchesExactly(job, "github\\.repository\\s*==\\s*'openai/codex'") and label = "yes"
  or
  not jobConditionMatchesExactly(job, "github\\.repository\\s*==\\s*'openai/codex'") and label = "no"
}

predicate checkoutStatus(Job job, string label) {
  jobChecksOutCode(job) and label = "yes"
  or
  not jobChecksOutCode(job) and label = "no"
}

predicate runStatus(Job job, string label) {
  exists(Run run | run.getEnclosingJob() = job) and label = "yes"
  or
  not exists(Run run | run.getEnclosingJob() = job) and label = "no"
}

predicate otherUsesStatus(Job job, string label) {
  exists(UsesStep other |
    other.getEnclosingJob() = job and
    not (
      other.getCallee().regexpMatch("(?i)^contributor-assistant/github-action$") and
      other.getVersion().regexpMatch("(?i)^ca4a40a7d1004f18d9960b404b97e5f30a505a08$")
    )
  ) and label = "yes"
  or
  not exists(UsesStep other |
    other.getEnclosingJob() = job and
    not (
      other.getCallee().regexpMatch("(?i)^contributor-assistant/github-action$") and
      other.getVersion().regexpMatch("(?i)^ca4a40a7d1004f18d9960b404b97e5f30a505a08$")
    )
  ) and label = "no"
}

predicate pinnedStatus(Job job, string label) {
  jobUsesPinnedClaAssistant(job) and label = "yes"
  or
  not jobUsesPinnedClaAssistant(job) and label = "no"
}

from Job job, UsesStep step, string gate, string action, string pin,
  string signatures, string branch, string document, string allowlist,
  string checksOut, string hasRun, string otherUses, string pinned
where
  workflowName(job.getWorkflow(), "CLA Assistant") and
  step.getEnclosingJob() = job and
  gateStatus(job, gate) and
  regexStatus(step.getCallee(), "(?i)^contributor-assistant/github-action$", action) and
  regexStatus(step.getVersion(), "(?i)^ca4a40a7d1004f18d9960b404b97e5f30a505a08$", pin) and
  regexStatus(step.getArgumentExpr("path-to-signatures").getExpression(), "(?i)^['\"]?signatures/cla\\.json['\"]?$", signatures) and
  regexStatus(step.getArgumentExpr("branch").getExpression(), "(?i)^['\"]?cla-signatures['\"]?$", branch) and
  regexStatus(step.getArgumentExpr("path-to-document").getExpression(), "(?i)^['\"]?https://github\\.com/openai/codex/blob/main/docs/CLA\\.md['\"]?$", document) and
  regexStatus(step.getArgumentExpr("allowlist").getExpression(), "(?i)^['\"]?codex,dependabot,dependabot\\[bot\\],github-actions\\[bot\\]['\"]?$", allowlist) and
  checkoutStatus(job, checksOut) and
  runStatus(job, hasRun) and
  otherUsesStatus(job, otherUses) and
  pinnedStatus(job, pinned)
select job,
  "id=" + job.getId() +
    "; condition=" + job.getIf().getCondition() +
    "; statuses=" + gate + "/" + action + "/" + pin + "/" + signatures + "/" + branch + "/" + document + "/" + allowlist + "/" + checksOut + "/" + hasRun + "/" + otherUses + "/" + pinned +
    "; callee=" + step.getCallee() +
    "; version=" + step.getVersion() +
    "; signatures=" + step.getArgumentExpr("path-to-signatures").getExpression() +
    "; branch=" + step.getArgumentExpr("branch").getExpression() +
    "; document=" + step.getArgumentExpr("path-to-document").getExpression() +
    "; allowlist=" + step.getArgumentExpr("allowlist").getExpression() +
    "; pinnedPredicate=" + pinned
