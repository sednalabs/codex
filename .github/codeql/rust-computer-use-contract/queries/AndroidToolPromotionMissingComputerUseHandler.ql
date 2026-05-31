/**
 * @name Android native tool promotion missing ComputerUse handler
 * @description Bare Android dynamic tools promoted to canonical Codex schemas must register the native ComputerUse handler.
 * @kind problem
 * @problem.severity error
 * @precision high
 * @id rust/android-tool-promotion-missing-computer-use-handler
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust

predicate toolRegistryPlanFile(File file) {
  file.getRelativePath() = "codex-rs/tools/src/tool_registry_plan.rs"
}

predicate nodeInsideFunction(AstNode node, Function function) {
  node.getFile() = function.getFile() and
  function.getLocation().getStartLine() <= node.getLocation().getStartLine() and
  node.getLocation().getEndLine() <= function.getLocation().getEndLine()
}

predicate referencesCanonicalAndroidTool(Function function) {
  exists(Call call |
    call.getEnclosingCallable() = function and
    call.toString().regexpMatch("(?s).*canonical_android_dynamic_tool.*")
  )
}

predicate referencesComputerUseHandler(Function function) {
  exists(PathAstNode path |
    nodeInsideFunction(path, function) and
    path.getPath().getSegment().getIdentifier().getText() = "ComputerUse"
  )
}

from Function function
where
  toolRegistryPlanFile(function.getFile()) and
  referencesCanonicalAndroidTool(function) and
  not referencesComputerUseHandler(function)
select function,
  "This native Android promotion path references canonical Android tools without registering ToolHandlerKind::ComputerUse."
