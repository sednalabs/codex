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
  job.getEnvironment().getName() = "pypi" and
  (
    job.getId() = "publish-python-runtime" and
    jobConditionMatchesExactly(
      job,
      "github\\.repository\\s*==\\s*'openai/codex'\\s*&&\\s*needs\\.resolve-cli-release\\.outputs\\.publish\\s*==\\s*'true'\\s*&&\\s*needs\\.prepare-python-runtime\\.result\\s*==\\s*'success'\\s*&&\\s*needs\\.build-python-sdk\\.result\\s*==\\s*'success'"
    ) and
    jobUsesExpectedPythonReleaseActions(job) and
    jobUsesValidatedPythonReleaseBuilds(job) and
    jobUsesSameRunPythonArtifact(job, "python-runtime-wheels", "dist/python-runtime") and
    jobUsesPinnedPythonPublisher(job, "dist/python-runtime") and
    jobHasOnlyPythonReleaseVerification(
      job,
      "openai-codex-cli-bin",
      "\\$PYTHON_RUNTIME_VERSION"
    ) and
    jobChecksOutOnlyResolvedPythonReleaseSource(job) and
    jobUsesValidatedPythonReleaseResolver(job)
    or
    job.getId() = "publish-python-sdk" and
    jobConditionMatchesExactly(
      job,
      "github\\.repository\\s*==\\s*'openai/codex'\\s*&&\\s*needs\\.resolve-cli-release\\.outputs\\.publish\\s*==\\s*'true'\\s*&&\\s*needs\\.build-python-sdk\\.result\\s*==\\s*'success'\\s*&&\\s*needs\\.publish-python-runtime\\.result\\s*==\\s*'success'"
    ) and
    jobUsesExpectedPythonReleaseActions(job) and
    jobUsesValidatedPythonReleaseBuilds(job) and
    jobUsesSameRunPythonArtifact(job, "python-sdk-package", "dist/python-sdk") and
    jobUsesPinnedPythonPublisher(job, "dist/python-sdk") and
    jobHasOnlyPythonReleaseVerification(
      job,
      "openai-codex",
      "\\$SDK_VERSION"
    ) and
    jobChecksOutOnlyResolvedPythonReleaseSource(job) and
    jobUsesValidatedPythonReleaseResolver(job)
  )
}

predicate jobUsesValidatedPythonReleaseResolver(Job publisher) {
  exists(Job resolver |
    resolver.getWorkflow() = publisher.getWorkflow() and
    resolver.getId() = "resolve-cli-release" and
    publisher.getANeededJob() = resolver and
    jobConditionMatchesExactly(resolver, "github\\.repository\\s*==\\s*'openai/codex'") and
    jobUsesExactlyOneAction(
      resolver,
      "actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd"
    ) and
    jobUsesOnlyAction(
      resolver,
      "actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd"
    ) and
    exists(UsesStep checkout |
      checkout.getEnclosingJob() = resolver and
      checkout.getCallee() = "actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd" and
      actionArgumentMatches(checkout, "persist-credentials", "(?i)^\\s*false\\s*$") and
      actionArgumentAbsent(checkout, "ref") and
      actionArgumentAbsent(checkout, "repository") and
      actionArgumentAbsent(checkout, "path") and
      actionArgumentAbsent(checkout, "token") and
      actionArgumentAbsent(checkout, "submodules") and
      actionArgumentAbsent(checkout, "sparse-checkout")
    ) and
    jobHasOnlyResolverScript(resolver)
  )
}

predicate jobUsesValidatedPythonReleaseBuilds(Job publisher) {
  exists(ExternalJob runtimeBuilder, ExternalJob sdkBuilder |
    publisher.getANeededJob() = runtimeBuilder and
    runtimeBuilder.getId() = "prepare-python-runtime" and
    runtimeBuilder.getCallee() = "./.github/workflows/python-runtime-build.yml" and
    jobConditionMatchesExactly(
      runtimeBuilder,
      "needs\\.resolve-cli-release\\.outputs\\.publish\\s*==\\s*'true'"
    ) and
    actionArgumentMatches(
      runtimeBuilder,
      "runtime_version",
      "(?s)^\\s*['\"]?\\$\\{\\{\\s*needs\\.resolve-cli-release\\.outputs\\.version\\s*\\}\\}['\"]?\\s*$"
    ) and
    actionArgumentMatches(
      runtimeBuilder,
      "source_ref",
      "(?s)^\\s*['\"]?\\$\\{\\{\\s*needs\\.resolve-cli-release\\.outputs\\.source_sha\\s*\\}\\}['\"]?\\s*$"
    ) and
    publisher.getANeededJob() = sdkBuilder and
    sdkBuilder.getId() = "build-python-sdk" and
    sdkBuilder.getCallee() = "./.github/workflows/python-sdk-build.yml" and
    jobConditionMatchesExactly(
      sdkBuilder,
      "needs\\.resolve-cli-release\\.outputs\\.publish\\s*==\\s*'true'"
    ) and
    actionArgumentMatches(
      sdkBuilder,
      "sdk_version",
      "(?s)^\\s*['\"]?\\$\\{\\{\\s*needs\\.resolve-cli-release\\.outputs\\.version\\s*\\}\\}['\"]?\\s*$"
    ) and
    actionArgumentMatches(
      sdkBuilder,
      "runtime_version",
      "(?s)^\\s*['\"]?\\$\\{\\{\\s*needs\\.resolve-cli-release\\.outputs\\.version\\s*\\}\\}['\"]?\\s*$"
    ) and
    actionArgumentMatches(
      sdkBuilder,
      "source_ref",
      "(?s)^\\s*['\"]?\\$\\{\\{\\s*needs\\.resolve-cli-release\\.outputs\\.source_sha\\s*\\}\\}['\"]?\\s*$"
    ) and
    exists(Job resolver |
      resolver.getWorkflow() = publisher.getWorkflow() and
      resolver.getId() = "resolve-cli-release" and
      runtimeBuilder.getANeededJob() = resolver and
      sdkBuilder.getANeededJob() = resolver
    )
  )
}

predicate jobHasOnlyResolverScript(Job job) {
  exists(Run resolverScript |
    resolverScript.getEnclosingJob() = job and
    resolverScript.getScript().getRawScript().regexpMatch(
      "(?s)^\\s*python3\\s+\\.github/scripts/resolve_python_cli_release\\.py\\s+\"\\$CLI_RUN_ID\"\\s*\\\\?\\s*--repository\\s+\"\\$GITHUB_REPOSITORY\"\\s*\\\\?\\s*--event-path\\s+\"\\$GITHUB_EVENT_PATH\"\\s*\\\\?\\s*--github-output\\s+\"\\$GITHUB_OUTPUT\"\\s*$"
    ) and
    not exists(Run otherRun |
      otherRun.getEnclosingJob() = job and
      otherRun != resolverScript
    )
  )
}

predicate jobUsesExpectedPythonReleaseActions(Job job) {
  jobUsesExactlyOneAction(
    job,
    "actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd"
  ) and
  jobUsesExactlyOneAction(
    job,
    "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c"
  ) and
  jobUsesExactlyOneAction(
    job,
    "pypa/gh-action-pypi-publish@dc37677b2e1c63e2034f94d8a5b11f265b73ba33"
  ) and
  jobUsesExactlyOneAction(
    job,
    "astral-sh/setup-uv@07a047e608db84b3d8c06cd9a7c7dcc9c355c18e"
  ) and
  jobUsesOnlyActions(
    job,
    "actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd",
    "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c",
    "pypa/gh-action-pypi-publish@dc37677b2e1c63e2034f94d8a5b11f265b73ba33",
    "astral-sh/setup-uv@07a047e608db84b3d8c06cd9a7c7dcc9c355c18e"
  ) and
  exists(UsesStep checkout, UsesStep uv |
    checkout.getEnclosingJob() = job and
    checkout.getCallee() = "actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd" and
    actionArgumentMatches(
      checkout,
      "ref",
      "(?s)^\\s*['\"]?\\$\\{\\{\\s*needs\\.resolve-cli-release\\.outputs\\.source_sha\\s*\\}\\}['\"]?\\s*$"
    ) and
    actionArgumentMatches(checkout, "persist-credentials", "(?i)^\\s*false\\s*$") and
    actionArgumentAbsent(checkout, "repository") and
    actionArgumentAbsent(checkout, "path") and
    actionArgumentAbsent(checkout, "token") and
    actionArgumentAbsent(checkout, "submodules") and
    actionArgumentAbsent(checkout, "sparse-checkout") and
    uv.getEnclosingJob() = job and
    uv.getCallee() = "astral-sh/setup-uv@07a047e608db84b3d8c06cd9a7c7dcc9c355c18e" and
    actionArgumentMatches(uv, "version", "(?i)^\\s*['\"]?0\\.13\\.5['\"]?\\s*$")
  )
}

predicate jobUsesSameRunPythonArtifact(Job job, string artifactName, string artifactPath) {
  exists(UsesStep download |
    download.getEnclosingJob() = job and
    download.getCallee() = "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c" and
    actionArgumentMatches(
      download,
      "name",
      "(?i)^\\s*['\"]?" + artifactName + "['\"]?\\s*$"
    ) and
    actionArgumentMatches(
      download,
      "path",
      "(?i)^\\s*['\"]?" + artifactPath + "['\"]?\\s*$"
    ) and
    actionArgumentAbsent(download, "run-id") and
    actionArgumentAbsent(download, "repository") and
    actionArgumentAbsent(download, "github-token") and
    actionArgumentAbsent(download, "pattern") and
    actionArgumentAbsent(download, "artifact-ids") and
    actionArgumentAbsent(download, "merge-multiple")
  )
}

predicate jobUsesPinnedPythonPublisher(Job job, string packagesDirectory) {
  exists(UsesStep publisher |
    publisher.getEnclosingJob() = job and
    publisher.getCallee() = "pypa/gh-action-pypi-publish@dc37677b2e1c63e2034f94d8a5b11f265b73ba33" and
    actionArgumentMatches(
      publisher,
      "packages-dir",
      "(?i)^\\s*['\"]?" + packagesDirectory + "['\"]?\\s*$"
    ) and
    actionArgumentMatches(publisher, "skip-existing", "(?i)^\\s*true\\s*$") and
    actionArgumentAbsent(publisher, "password") and
    actionArgumentAbsent(publisher, "repository-url")
  )
}

predicate jobHasOnlyPythonReleaseVerification(Job job, string packageName, string versionVariablePattern) {
  exists(Run verification |
    verification.getEnclosingJob() = job and
    verification.getScript().getRawScript().regexpMatch(
      "(?s)^\\s*uv\\s+run\\s+--no-project\\s+--with\\s+packaging==26\\.2\\s+python\\s+\\.github/scripts/verify_pypi_release\\.py\\s*\\\\?\\s*" +
        packageName + "\\s+\"" + versionVariablePattern + "\"\\s*$"
    ) and
    not exists(Run otherRun |
      otherRun.getEnclosingJob() = job and
      otherRun != verification
    )
  )
}

predicate actionArgumentMatches(Uses step, string argumentName, string pattern) {
  exists(Expression argument |
    argument = step.getArgumentExpr(argumentName) and
    argument.getExpression().regexpMatch(pattern)
  )
}

predicate actionArgumentAbsent(Uses step, string argumentName) {
  not exists(Expression argument | argument = step.getArgumentExpr(argumentName))
}

predicate jobUsesExactlyOneAction(Job job, string callee) {
  exists(UsesStep matching |
    matching.getEnclosingJob() = job and
    matching.getCallee() = callee and
    not exists(UsesStep duplicate |
      duplicate.getEnclosingJob() = job and
      duplicate.getCallee() = callee and
      duplicate != matching
    )
  )
}

predicate jobUsesOnlyActions(Job job, string actionOne, string actionTwo, string actionThree, string actionFour) {
  not exists(UsesStep step |
    step.getEnclosingJob() = job and
    step.getCallee() != actionOne and
    step.getCallee() != actionTwo and
    step.getCallee() != actionThree and
    step.getCallee() != actionFour
  )
}

predicate jobUsesOnlyAction(Job job, string callee) {
  not exists(UsesStep step |
    step.getEnclosingJob() = job and
    step.getCallee() != callee
  )
}

predicate jobChecksOutResolvedPythonReleaseSource(Job job) {
  jobUsesExactlyOneAction(
    job,
    "actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd"
  ) and
  exists(UsesStep checkout |
    checkout.getEnclosingJob() = job and
    checkout.getCallee() = "actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd" and
    actionArgumentMatches(
      checkout,
      "ref",
      "(?s)^\\s*['\"]?\\$\\{\\{\\s*needs\\.resolve-cli-release\\.outputs\\.source_sha\\s*\\}\\}['\"]?\\s*$"
    ) and
    actionArgumentMatches(checkout, "persist-credentials", "(?i)^\\s*false\\s*$")
  )
}

predicate jobChecksOutOnlyResolvedPythonReleaseSource(Job job) {
  jobChecksOutResolvedPythonReleaseSource(job) and
  not exists(UsesStep checkout |
    checkout.getEnclosingJob() = job and
    checkout.getCallee().regexpMatch("(?i)^actions/checkout@") and
    checkout.getCallee() != "actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd"
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
