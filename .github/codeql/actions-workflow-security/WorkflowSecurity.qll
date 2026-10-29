import actions

/**
 * Shared helpers for workflow-security queries.
 */

predicate workflowName(Workflow workflow, string name) {
  name = workflow.getName()
}

bindingset[pattern]
predicate jobConditionMatches(Job job, string pattern) {
  exists(If condition |
    condition = job.getIf() and
    condition.getCondition().regexpMatch(pattern)
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
    jobConditionMatches(job, "(?is).*needs\\.tag-check\\.result\\s*==\\s*'success'.*") and
    jobConditionMatches(job, "(?is).*needs\\.build\\.result\\s*==\\s*'success'.*")
    or
    workflowName(job.getWorkflow(), "rusty-v8-release") and
    job.getId() = "publish-release" and
    jobConditionMatches(job, "(?is).*needs\\.metadata\\.result\\s*==\\s*'success'.*") and
    jobConditionMatches(job, "(?is).*needs\\.build\\.result\\s*==\\s*'success'.*")
  )
}

predicate jobUsesPinnedClaAssistant(Job job) {
  workflowName(job.getWorkflow(), "CLA Assistant") and
  job.getId() = "cla" and
  jobConditionMatches(job, "(?is).*github\\.repository\\s*==\\s*'openai/codex'.*") and
  exists(UsesStep claStep, Expression signaturesPath, Expression signaturesBranch, Expression claDocument, Expression allowlist |
    claStep.getEnclosingJob() = job and
    claStep.getCallee().regexpMatch(
      "(?i)^contributor-assistant/github-action@ca4a40a7d1004f18d9960b404b97e5f30a505a08$"
    ) and
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
    not step.getCallee().regexpMatch(
      "(?i)^contributor-assistant/github-action@ca4a40a7d1004f18d9960b404b97e5f30a505a08$"
    )
  )
}

predicate jobUsesValidatedPythonReleasePublisher(Job job) {
  workflowName(job.getWorkflow(), "python-sdk-cli-release") and
  jobUsesActionMatching(
    job,
    "(?i)^pypa/gh-action-pypi-publish@dc37677b2e1c63e2034f94d8a5b11f265b73ba33$",
    _
  ) and
  jobChecksOutOnlyResolvedPythonReleaseSource(job) and
  (
    job.getId() = "publish-python-runtime" and
    jobConditionMatches(
      job,
      "(?is).*github\\.repository\\s*==\\s*'openai/codex'.*needs\\.resolve-cli-release\\.outputs\\.publish\\s*==\\s*'true'.*needs\\.prepare-python-runtime\\.result\\s*==\\s*'success'.*needs\\.build-python-sdk\\.result\\s*==\\s*'success'.*"
    )
    or
    job.getId() = "publish-python-sdk" and
    jobConditionMatches(
      job,
      "(?is).*github\\.repository\\s*==\\s*'openai/codex'.*needs\\.resolve-cli-release\\.outputs\\.publish\\s*==\\s*'true'.*needs\\.build-python-sdk\\.result\\s*==\\s*'success'.*needs\\.publish-python-runtime\\.result\\s*==\\s*'success'.*"
    )
  ) and
  exists(Job resolver |
    resolver.getWorkflow() = job.getWorkflow() and
    resolver.getId() = "resolve-cli-release" and
    job.getANeededJob() = resolver and
    jobConditionMatches(resolver, "(?is).*github\\.repository\\s*==\\s*'openai/codex'.*") and
    jobUsesActionMatching(
      resolver,
      "(?i)^actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd$",
      _
    ) and
    exists(string command |
      jobRunCommand(resolver, command) and
      command.regexpMatch("(?is).*resolve_python_cli_release\\.py.*")
    )
  )
}

predicate jobChecksOutResolvedPythonReleaseSource(Job job) {
  exists(UsesStep checkout, Expression ref |
    checkout.getEnclosingJob() = job and
    checkout.getCallee().regexpMatch("(?i)^actions/checkout@") and
    ref = checkout.getArgumentExpr("ref") and
    ref.getExpression().regexpMatch("(?is).*needs\\.resolve-cli-release\\.outputs\\.source_sha.*")
  )
}

predicate jobChecksOutOnlyResolvedPythonReleaseSource(Job job) {
  jobChecksOutResolvedPythonReleaseSource(job) and
  not exists(UsesStep checkout |
    checkout.getEnclosingJob() = job and
    checkout.getCallee().regexpMatch("(?i)^actions/checkout@") and
    not exists(Expression ref |
      ref = checkout.getArgumentExpr("ref") and
      ref.getExpression().regexpMatch("(?is).*needs\\.resolve-cli-release\\.outputs\\.source_sha.*")
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
    step.getCallee().regexpMatch("(?i)^actions/github-script@.*") and
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

predicate jobUsesAction(Job job, string callee) {
  exists(UsesStep step |
    step.getEnclosingJob() = job and
    callee = step.getCallee()
  )
}

bindingset[pattern]
predicate jobUsesActionMatching(Job job, string pattern, string callee) {
  jobUsesAction(job, callee) and
  callee.regexpMatch(pattern)
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
    step.getCallee().regexpMatch("(?i)^actions/checkout@") and
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

bindingset[callee]
predicate releasePublishingAction(string callee, string sinkKind) {
  (
    callee.regexpMatch("(?i)^(softprops/action-gh-release|ncipollo/release-action)@.*") and
    sinkKind = "GitHub Release publication"
  )
  or
  (
    callee.regexpMatch("(?i)^docker/build-push-action@.*") and
    sinkKind = "container image publication"
  )
}

predicate jobUsesReleasePublishingAction(Job job, string sinkKind) {
  exists(string callee |
    jobUsesAction(job, callee) and
    releasePublishingAction(callee, sinkKind)
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
  exists(string callee |
    jobUsesAction(job, callee) and
    callee.regexpMatch("(?i)^(softprops/action-gh-release|ncipollo/release-action)@.*") and
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
  jobUsesActionMatching(job, "(?i)^github/codeql-action/", _)
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
