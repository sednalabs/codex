/**
 * @name Computer-use content match may drop native image variant
 * @description Computer-use content-item matches that handle text may also need to preserve native image items.
 * @kind problem
 * @problem.severity warning
 * @precision medium
 * @id rust/computer-use-match-drops-native-image
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust

predicate computerUsePipelineFile(SourceFile file) {
  file.getRelativePath().regexpMatch("codex-rs/(protocol|core|app-server|tui)/src/.*\\.rs") and
  not file.getRelativePath().regexpMatch("(?s).*(/tests?/.*|.*_tests\\.rs|test\\.rs|tests\\.rs)$")
}

predicate cfgTestMeta(Meta meta) {
  meta.getPath().getText() = "cfg" and
  meta.toString().regexpMatch("(?s).*\\btest\\b.*")
}

predicate insideInlineRustTest(AstNode node) {
  exists(Module module |
    cfgTestMeta(module.getAnAttr().getMeta()) and
    module.getFile() = node.getFile() and
    module.getLocation().getStartLine() <= node.getLocation().getStartLine() and
    node.getLocation().getEndLine() <= module.getLocation().getEndLine()
  )
}

predicate contentItemPattern(MatchArm arm, string variant) {
  arm.hasPat() and
  arm.getPat().toString().regexpMatch(
    "(?s).*(ComputerUse(Output|CallOutput)ContentItem|FunctionCallOutputContentItem|DynamicToolCallOutputContentItem)::" +
      variant + "\\b.*"
  )
}

from MatchExpr matchExpr
where
  computerUsePipelineFile(matchExpr.getFile()) and
  not insideInlineRustTest(matchExpr) and
  exists(MatchArm arm | arm = matchExpr.getAnArm() and contentItemPattern(arm, "InputText")) and
  not exists(MatchArm arm | arm = matchExpr.getAnArm() and contentItemPattern(arm, "InputImage"))
select matchExpr,
  "This match handles text but may drop or fail to preserve native image content. Verify that InputImage is deliberately handled or delegated."
