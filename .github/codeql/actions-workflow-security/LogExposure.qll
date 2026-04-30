import actions

/**
 * Heuristics for secrets, generated tokens, PEM material, and release
 * credentials that should not reach log-visible workflow output.
 */

bindingset[name]
predicate credentialName(string name) {
  name.regexpMatch(
    "(?i).*(TOKEN|SECRET|PASSWORD|PASSWD|PRIVATE_?KEY|API_?KEY|CLIENT_?SECRET|CREDENTIAL|CERTIFICATE|CODESIGN|SIGNING|NOTARIZATION|NOTARY|P8|P12|PEM).*"
  )
}

predicate directSecretExpression(Expression expr) {
  expr.getNormalizedExpression().regexpMatch("secrets\\.[A-Za-z_][A-Za-z0-9_]*")
}

predicate sensitiveExpression(Expression expr, string sourceKind) {
  expr.getExpression().regexpMatch("(?i).*fromJson\\(\\s*secrets\\..*") and
  sourceKind = "derived secret"
  or
  expr.getExpression().regexpMatch("(?i).*secrets\\..*") and
  not directSecretExpression(expr) and
  sourceKind = "transformed secret"
  or
  expr.getExpression().regexpMatch("(?i).*github\\.token.*") and
  sourceKind = "generated GitHub token"
  or
  expr.getExpression().regexpMatch("(?i).*(steps|needs)\\.[^.]+\\.outputs\\..*(token|secret|password|credential|key).*") and
  sourceKind = "generated credential output"
  or
  expr.getExpression().regexpMatch("(?i).*env\\..*(TOKEN|SECRET|PASSWORD|CREDENTIAL|KEY|CERT|P8|P12|PEM).*") and
  sourceKind = "sensitive environment value"
}

bindingset[data]
predicate sensitiveShellData(string data, string sourceKind) {
  data.regexpMatch("(?is).*(gh\\s+auth\\s+token|openssl\\s+rand|uuidgen).*") and
  sourceKind = "generated token"
  or
  data.regexpMatch("(?is).*(BEGIN (RSA |EC |OPENSSH |)PRIVATE KEY|BEGIN CERTIFICATE|\\.pem|\\.p8|\\.p12|\\.key).*") and
  sourceKind = "PEM or key material"
  or
  data.regexpMatch("(?is).*(base64\\s+(-d|--decode)|jq\\s+(-r|--raw-output)).*(secret|token|password|credential|key|cert).*") and
  sourceKind = "decoded credential material"
  or
  data.regexpMatch("(?is).*(notarytool|codesign|security\\s+find-identity|gh\\s+release|npm\\s+token|twine|sigstore).*") and
  sourceKind = "release credential"
}

predicate sensitiveVariableInRun(Run run, string name, string sourceKind) {
  exists(Expression expr |
    expr = run.getInScopeEnvVarExpr(name) and
    credentialName(name) and
    sensitiveExpression(expr, sourceKind)
  )
  or
  exists(string data |
    run.getScript().getAnAssignment(name, data) and
    credentialName(name) and
    sensitiveShellData(data, sourceKind)
  )
  or
  exists(string data |
    run.getScript().getAWriteToGitHubOutput(name, data) and
    credentialName(name) and
    sensitiveShellData(data, sourceKind)
  )
  or
  exists(Run sourceRun, string data |
    sourceRun.getAFollowingStep() = run and
    sourceRun.getScript().getAWriteToGitHubEnv(name, data) and
    credentialName(name) and
    sensitiveShellData(data, sourceKind)
  )
}

bindingset[command, name]
predicate commandReferencesVariable(string command, string name) {
  command.regexpMatch("(?is).*\\$(\\{" + name + "\\}|" + name + "\\b).*")
  or
  command.regexpMatch("(?is).*\\$env:" + name + "\\b.*")
  or
  command.regexpMatch("(?is).*%" + name + "%.*")
  or
  command.regexpMatch("(?is).*\\$\\{\\{\\s*env\\." + name + "\\s*\\}\\}.*")
}

bindingset[command, name]
predicate commandMasksVariable(string command, string name) {
  command.regexpMatch("(?is).*::add-mask::.*\\$(\\{" + name + "\\}|" + name + "\\b).*")
  or
  command.regexpMatch("(?is).*::add-mask::.*\\$env:" + name + "\\b.*")
  or
  command.regexpMatch("(?is).*::add-mask::.*%" + name + "%.*")
  or
  command.regexpMatch("(?is).*::add-mask::.*\\$\\{\\{\\s*env\\." + name + "\\s*\\}\\}.*")
}

bindingset[run, name]
predicate runMasksVariable(Run run, string name) {
  exists(string command |
    command = run.getScript().getACommand() and
    commandMasksVariable(command, name)
  )
}

bindingset[run, name]
predicate sinkAppearsBeforeMask(Run run, string name) {
  run.getScript().getRawScript().regexpMatch(
    "(?is).*(echo|printf|cat|tee|env|printenv|set\\s+-x|GITHUB_STEP_SUMMARY).*\\$(\\{" + name +
      "\\}|" + name + "\\b).*::add-mask::.*\\$(\\{" + name + "\\}|" + name + "\\b).*"
  )
}

bindingset[run, name]
predicate hasPriorMask(Run run, string name) {
  exists(Run maskRun |
    maskRun.getAFollowingStep() = run and
    runMasksVariable(maskRun, name)
  )
  or
  runMasksVariable(run, name) and
  not sinkAppearsBeforeMask(run, name)
}

bindingset[command]
predicate environmentDumpCommand(string command, string sinkKind) {
  (
    command.regexpMatch("(?is)^\\s*(env|printenv)\\b.*") or
    command.regexpMatch("(?is)^\\s*set\\s*$")
  ) and
  not command.regexpMatch("(?is).*GITHUB_(ENV|OUTPUT|PATH).*") and
  sinkKind = "an environment dump"
}

bindingset[command, name]
predicate logVisibleCommand(string command, string name, string sinkKind) {
  command.regexpMatch("(?is)^\\s*(echo|printf)\\b.*") and
  commandReferencesVariable(command, name) and
  sinkKind = "shell output"
  or
  command.regexpMatch("(?is)^\\s*cat\\b.*") and
  commandReferencesVariable(command, name) and
  sinkKind = "cat output"
  or
  command.regexpMatch("(?is).*\\btee\\b.*") and
  commandReferencesVariable(command, name) and
  sinkKind = "tee output"
  or
  command.regexpMatch("(?is).*GITHUB_STEP_SUMMARY.*") and
  commandReferencesVariable(command, name) and
  sinkKind = "the GitHub step summary"
  or
  environmentDumpCommand(command, sinkKind)
}

bindingset[command]
predicate setXCommand(string command) {
  command.regexpMatch("(?is)^\\s*(set\\s+-x|set\\s+-[^\\n]*x|bash\\s+-x|sh\\s+-x)\\b.*")
}

bindingset[run, name]
predicate tracingExposesVariable(Run run, string name, string command) {
  command = run.getScript().getACommand() and
  setXCommand(command) and
  commandReferencesVariable(run.getScript().getRawScript(), name)
}

bindingset[command, name]
predicate verboseCredentialCommand(string command, string name, string sinkKind) {
  command.regexpMatch("(?is).*(--verbose|\\s-v(\\s|$)|--debug|ACTIONS_STEP_DEBUG|RUNNER_DEBUG).*") and
  commandReferencesVariable(command, name) and
  sinkKind = "verbose tool output"
  or
  command.regexpMatch("(?is).*(notarytool|codesign|security|gh\\s+release|gh\\s+auth|npm|twine|sigstore).*(--verbose|\\s-v(\\s|$)|--debug).*") and
  sinkKind = "credential-adjacent verbose tool output"
}
