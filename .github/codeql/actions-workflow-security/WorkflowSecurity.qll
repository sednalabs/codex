import actions

/**
 * Shared helpers for workflow-security queries.
 */

predicate workflowName(Workflow workflow, string name) {
  name = workflow.getName()
}

bindingset[pattern]
predicate jobConditionMatchesExactly(Job job, string pattern) {
  exists(If condition |
    condition = job.getIf() and
    condition.getCondition().regexpMatch(
      "(?s)^\\s*(?:\\$\\{\\{\\s*" + pattern + "\\s*\\}\\}|" + pattern + ")\\s*$"
    )
  )
}

predicate repoReleaseWorkflow(Workflow workflow) {
  workflowName(workflow, "rust-release") or
  workflowName(workflow, "rusty-v8-release")
}

predicate repoOfficialReleaseJob(Job job) {
  repoReleaseWorkflow(job.getWorkflow()) and
  (
    workflowName(job.getWorkflow(), "rust-release") and
    job.getId() = "release" and
    jobConditionMatchesExactly(
      job,
      "always\\(\\)\\s*&&\\s*needs\\.tag-check\\.result\\s*==\\s*'success'\\s*&&\\s*needs\\.build\\.result\\s*==\\s*'success'\\s*&&\\s*needs\\.finalize-macos\\.result\\s*==\\s*'success'\\s*&&\\s*\\(\\s*needs\\.build-windows\\.result\\s*==\\s*'success'\\s*\\|\\|\\s*\\(\\s*github\\.repository\\s*==\\s*'sednalabs/codex'\\s*&&\\s*needs\\.build-windows\\.result\\s*==\\s*'skipped'\\s*\\)\\s*\\)\\s*&&\\s*needs\\.argument-comment-lint-release-assets\\.result\\s*==\\s*'success'\\s*&&\\s*\\(\\s*needs\\.stage-npm-packages\\.result\\s*==\\s*'success'\\s*\\|\\|\\s*\\(\\s*github\\.repository\\s*==\\s*'sednalabs/codex'\\s*&&\\s*needs\\.stage-npm-packages\\.result\\s*==\\s*'skipped'\\s*\\)\\s*\\)\\s*&&\\s*\\(\\s*needs\\.provisioned-macos-candidate\\.result\\s*==\\s*'success'\\s*\\|\\|\\s*\\(\\s*vars\\.CODEX_PROVISIONED_MACOS_CANDIDATE\\s*!=\\s*'true'\\s*&&\\s*needs\\.provisioned-macos-candidate\\.result\\s*==\\s*'skipped'\\s*\\)\\s*\\)"
    )
    or
    workflowName(job.getWorkflow(), "rusty-v8-release") and
    job.getId() = "publish-release" and
    jobConditionMatchesExactly(
      job,
      "always\\(\\)\\s*&&\\s*needs\\.metadata\\.result\\s*==\\s*'success'\\s*&&\\s*needs\\.build\\.result\\s*==\\s*'success'\\s*&&\\s*\\(\\s*needs\\.build-windows-source\\.result\\s*==\\s*'success'\\s*\\|\\|\\s*\\(\\s*github\\.repository\\s*==\\s*'sednalabs/codex'\\s*&&\\s*needs\\.build-windows-source\\.result\\s*==\\s*'skipped'\\s*\\)\\s*\\)"
    )
  )
}

predicate jobUsesPinnedClaAssistant(Job job) {
  workflowName(job.getWorkflow(), "CLA Assistant") and
  job.getId() = "cla" and
  jobConditionMatchesExactly(job, "github\\.repository\\s*==\\s*'openai/codex'") and
  exists(UsesStep claStep, Expression signaturesPath, Expression signaturesBranch, Expression claDocument, Expression allowlist |
    claStep.getEnclosingJob() = job and
    claStep.getCallee().regexpMatch("(?i)^contributor-assistant/github-action$") and
    claStep.getVersion().regexpMatch("(?i)^ca4a40a7d1004f18d9960b404b97e5f30a505a08$") and
    signaturesPath = claStep.getArgumentExpr("path-to-signatures") and
    signaturesPath.getExpression().regexpMatch("(?i)^['\"]?signatures/cla\\.json['\"]?$") and
    signaturesBranch = claStep.getArgumentExpr("branch") and
    signaturesBranch.getExpression().regexpMatch("(?i)^['\"]?cla-signatures['\"]?$") and
    claDocument = claStep.getArgumentExpr("path-to-document") and
    claDocument.getExpression().regexpMatch(
      "(?i)^['\"]?https://github\\.com/openai/codex/blob/main/docs/CLA\\.md['\"]?$"
    ) and
    allowlist = claStep.getArgumentExpr("allowlist") and
    allowlist.getExpression().regexpMatch(
      "(?i)^['\"]?codex,dependabot,dependabot\\[bot\\],github-actions\\[bot\\]['\"]?$"
    )
  ) and
  not jobChecksOutCode(job) and
  not exists(Run run | run.getEnclosingJob() = job) and
  not exists(UsesStep step |
    step.getEnclosingJob() = job and
    not (
      step.getCallee().regexpMatch("(?i)^contributor-assistant/github-action$") and
      step.getVersion().regexpMatch("(?i)^ca4a40a7d1004f18d9960b404b97e5f30a505a08$")
    )
  )
}

/**
 * This fork carries upstream-only Python publication workflows. The exact
 * canonical repository condition makes these jobs unreachable in
 * sednalabs/codex. This is a fork-local unreachable-job exception only; it
 * does not assess or attest the canonical OpenAI publisher or its reusable
 * build workflows.
 */
predicate jobIsForkUnreachableCanonicalPythonPublisher(Job job) {
  workflowName(job.getWorkflow(), "python-sdk-cli-release") and
  job.getEnvironment().getName() = "pypi" and
  (
    job.getId() = "publish-python-runtime" and
    jobConditionMatchesExactly(
      job,
      "github\\.repository\\s*==\\s*'openai/codex'\\s*&&\\s*needs\\.resolve-cli-release\\.outputs\\.publish\\s*==\\s*'true'\\s*&&\\s*needs\\.prepare-python-runtime\\.result\\s*==\\s*'success'\\s*&&\\s*needs\\.build-python-sdk\\.result\\s*==\\s*'success'"
    )
    or
    job.getId() = "publish-python-sdk" and
    jobConditionMatchesExactly(
      job,
      "github\\.repository\\s*==\\s*'openai/codex'\\s*&&\\s*needs\\.resolve-cli-release\\.outputs\\.publish\\s*==\\s*'true'\\s*&&\\s*needs\\.build-python-sdk\\.result\\s*==\\s*'success'\\s*&&\\s*needs\\.publish-python-runtime\\.result\\s*==\\s*'success'"
    )
  )
}

predicate jobUsesTrustedPypiPublisher(Job job) {
  jobUsesActionMatching(job, "(?i)^pypa/gh-action-pypi-publish@.*", _)
}

bindingset[apiCall]
predicate jobGithubScriptCalls(Job job, string apiCall) {
  exists(UsesStep step, Expression script |
    step.getEnclosingJob() = job and
    step.getCallee().regexpMatch("(?i)^actions/github-script$") and
    script = step.getArgumentExpr("script") and
    script.getExpression().regexpMatch("(?is).*github\\.rest\\." + apiCall + "\\s*\\(.*")
  )
}

predicate untrustedTrigger(Event event) {
  event.getName() = "pull_request_target"
  or
  event.getName() = "workflow_run"
  or
  event.getName() = "issue_comment"
  or
  event.getName() = "discussion_comment"
}

predicate jobHasUntrustedTrigger(Job job, string eventName) {
  exists(Event event |
    event = job.getATriggerEvent() and
    untrustedTrigger(event) and
    eventName = event.getName()
  )
}

predicate jobUsesAction(Job job, string actionReference) {
  exists(UsesStep step |
    step.getEnclosingJob() = job and
    actionReference = step.getCallee() + "@" + step.getVersion()
  )
}

bindingset[pattern]
predicate jobUsesActionMatching(Job job, string pattern, string actionReference) {
  jobUsesAction(job, actionReference) and
  actionReference.regexpMatch(pattern)
}

predicate jobRunCommand(Job job, string command) {
  exists(Run run |
    run.getEnclosingJob() = job and
    command = run.getScript().getACommand()
  )
}

predicate jobRawScript(Job job, string script) {
  exists(Run run |
    run.getEnclosingJob() = job and
    script = run.getScript().getRawScript()
  )
}

predicate jobChecksOutCode(Job job) {
  jobUsesActionMatching(job, "(?i)^actions/checkout@", _)
}

predicate jobChecksOutInputRef(Job job) {
  exists(UsesStep step, Expression expr |
    step.getEnclosingJob() = job and
    step.getCallee().regexpMatch("(?i)^actions/checkout$") and
    expr = step.getArgumentExpr("ref") and
    expr.getExpression().regexpMatch("(?i).*(inputs\\.|github\\.event\\.).*")
  )
}

bindingset[command]
predicate releasePublishingCommand(string command, string sinkKind) {
  command.regexpMatch("(?is).*\\bgh\\s+release\\s+(create|upload|edit)\\b.*") and
  sinkKind = "GitHub Release publication"
  or
  command.regexpMatch("(?is).*\\bgh\\s+api\\b.*/repos/.*/releases\\b.*") and
  sinkKind = "GitHub Release API publication"
  or
  command.regexpMatch("(?is).*\\b(npm|pnpm|yarn)\\s+publish\\b.*") and
  sinkKind = "npm package publication"
  or
  command.regexpMatch("(?is).*\\bcargo\\s+publish\\b.*") and
  sinkKind = "Cargo package publication"
  or
  command.regexpMatch("(?is).*\\btwine\\s+upload\\b.*") and
  sinkKind = "Python package publication"
  or
  command.regexpMatch("(?is).*\\bdocker\\s+(push|buildx\\s+build\\b.*--push)\\b.*") and
  sinkKind = "container image publication"
}

bindingset[actionReference]
predicate releasePublishingAction(string actionReference, string sinkKind) {
  (
    actionReference.regexpMatch("(?i)^(softprops/action-gh-release|ncipollo/release-action)@.*") and
    sinkKind = "GitHub Release publication"
  )
  or
  (
    actionReference.regexpMatch("(?i)^docker/build-push-action@.*") and
    sinkKind = "container image publication"
  )
}

predicate jobUsesReleasePublishingAction(Job job, string sinkKind) {
  exists(string actionReference |
    jobUsesAction(job, actionReference) and
    releasePublishingAction(actionReference, sinkKind)
  )
}

predicate jobHasPublishingSink(Job job, string sinkKind) {
  exists(string command |
    jobRunCommand(job, command) and
    releasePublishingCommand(command, sinkKind)
  )
  or
  jobUsesReleasePublishingAction(job, sinkKind)
}

predicate jobHasOfficialReleaseSink(Job job, string sinkKind) {
  exists(string command |
    jobRunCommand(job, command) and
    command.regexpMatch("(?is).*\\bgh\\s+release\\s+create\\b.*") and
    sinkKind = "GitHub Release publication"
  )
  or
  exists(string actionReference |
    jobUsesAction(job, actionReference) and
    actionReference.regexpMatch("(?i)^(softprops/action-gh-release|ncipollo/release-action)@.*") and
    sinkKind = "GitHub Release publication"
  )
}

predicate jobHasChecksumMarker(Job job) {
  exists(string command |
    jobRunCommand(job, command) and
    command.regexpMatch("(?is).*(sha256sum|shasum|SHA256SUMS).*")
  )
}

predicate jobHasSigningMarker(Job job) {
  exists(string command |
    jobRunCommand(job, command) and
    command.regexpMatch("(?is).*(cosign|sigstore|gitsign|codesign|notarytool|\\.sigstore).*")
  )
  or
  jobUsesActionMatching(job, "(?i).*(sigstore|cosign|attest|provenance).*", _)
}

predicate jobHasMetadataMarker(Job job) {
  exists(string command |
    jobRunCommand(job, command) and
    command.regexpMatch("(?is).*(RELEASE-METADATA|build_provenance|target_commit|upstream_base_commit).*")
  )
}

predicate jobHasNativeAttestationMarker(Job job) {
  jobUsesActionMatching(job, "(?i)^actions/attest(-build-provenance)?@.*", _)
  or
  jobUsesActionMatching(job, "(?i)^github/actions/attest-build-provenance@.*", _)
}

predicate jobHasRepoApprovedProvenance(Job job) {
  jobHasNativeAttestationMarker(job)
  or
  jobHasChecksumMarker(job) and
  jobHasSigningMarker(job) and
  jobHasMetadataMarker(job)
}

predicate writePermissionName(string permissionName) {
  permissionName = "actions"
  or
  permissionName = "attestations"
  or
  permissionName = "checks"
  or
  permissionName = "contents"
  or
  permissionName = "deployments"
  or
  permissionName = "discussions"
  or
  permissionName = "id-token"
  or
  permissionName = "issues"
  or
  permissionName = "packages"
  or
  permissionName = "pages"
  or
  permissionName = "pull-requests"
  or
  permissionName = "repository-projects"
  or
  permissionName = "security-events"
  or
  permissionName = "statuses"
}

predicate permissionBlockGrantsWrite(Permissions permissions, string permissionName) {
  permissionName = "write-all" and
  permissions.getAPermission() = "write-all"
  or
  writePermissionName(permissionName) and
  permissions.getPermission(permissionName) = "write"
}

predicate jobEffectiveWritePermission(Job job, string permissionName) {
  exists(Permissions permissions |
    permissions = job.getPermissions() and
    permissionBlockGrantsWrite(permissions, permissionName)
  )
  or
  not exists(job.getPermissions()) and
  exists(Permissions permissions |
    permissions = job.getWorkflow().getPermissions() and
    permissionBlockGrantsWrite(permissions, permissionName)
  )
}

predicate workflowLevelWritePermission(Workflow workflow, string permissionName) {
  exists(Permissions permissions |
    permissions = workflow.getPermissions() and
    permissionBlockGrantsWrite(permissions, permissionName)
  )
}

predicate jobNeedsWritePermission(Job job, string permissionName) {
  permissionName = "contents" and
  jobHasPublishingSink(job, _)
  or
  permissionName = "packages" and
  jobHasPublishingSink(job, _)
  or
  permissionName = "id-token" and
  jobHasSigningMarker(job)
  or
  permissionName = "id-token" and
  jobUsesTrustedPypiPublisher(job)
  or
  permissionName = "attestations" and
  jobHasNativeAttestationMarker(job)
  or
  permissionName = "security-events" and
  jobUsesActionMatching(job, "(?i)^github/codeql-action@.*", _)
  or
    permissionName = "actions" and
    exists(string command |
      jobRunCommand(job, command) and
      command.regexpMatch("(?is).*\\bgh\\s+workflow\\s+run\\b.*")
    )
  or
  permissionName = "contents" and
  jobUsesPinnedClaAssistant(job)
  or
  permissionName = "issues" and
  jobUsesPinnedClaAssistant(job)
  or
  permissionName = "actions" and
  jobUsesPinnedClaAssistant(job)
  or
  permissionName = "pull-requests" and
  jobGithubScriptCalls(job, "pulls\\.update")
  or
  permissionName = "issues" and
  (
    jobGithubScriptCalls(job, "issues\\.createComment") or
    jobGithubScriptCalls(job, "issues\\.update") or
    jobGithubScriptCalls(job, "issues\\.lock") or
    jobGithubScriptCalls(job, "issues\\.unlock")
  )
}
