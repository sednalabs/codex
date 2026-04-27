import actions

/**
 * Shared helpers for workflow-security queries.
 */

predicate workflowName(Workflow workflow, string name) {
  name = workflow.getName()
}

predicate repoReleaseWorkflow(Workflow workflow) {
  workflowName(workflow, "sedna-release")
}

predicate repoOfficialReleaseJob(Job job) {
  repoReleaseWorkflow(job.getWorkflow()) and
  job.getId() = "release-linux"
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

predicate releasePublishingAction(string callee, string sinkKind) {
  callee.regexpMatch("(?i)^(softprops/action-gh-release|ncipollo/release-action)@.*") and
  sinkKind = "GitHub Release publication"
  or
  callee.regexpMatch("(?i)^docker/build-push-action@.*") and
  sinkKind = "container image publication"
}

predicate jobHasPublishingSink(Job job, string sinkKind) {
  exists(string command |
    jobRunCommand(job, command) and
    releasePublishingCommand(command, sinkKind)
  )
  or
  exists(string callee |
    jobUsesAction(job, callee) and
    releasePublishingAction(callee, sinkKind)
  )
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
}
