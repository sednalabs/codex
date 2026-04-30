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

predicate computerUsePipelineFile(File file) {
  file.getRelativePath().regexpMatch("codex-rs/(protocol|core|app-server|tui)/src/.*\\.rs") and
  not file.getRelativePath().regexpMatch("(?s).*(/tests?/.*|.*_tests\\.rs|test\\.rs|tests\\.rs)$")
}

predicate cfgTestMeta(Meta meta) {
  meta.getPath().getText() = "cfg" and
  meta.toString().regexpMatch("(?s).*\\btest\\b.*")
}

predicate insideInlineRustTest(AstNode node) {
  exists(Module modItem |
    cfgTestMeta(modItem.getAnAttr().getMeta()) and
    modItem.getFile() = node.getFile() and
    modItem.getLocation().getStartLine() <= node.getLocation().getStartLine() and
    node.getLocation().getEndLine() <= modItem.getLocation().getEndLine()
  )
}

predicate inputTextContentItemPattern(MatchArm arm) {
  arm.hasPat() and
  arm.getPat().toString().regexpMatch(
    "(?s).*(ComputerUse(Output|CallOutput)ContentItem|FunctionCallOutputContentItem|DynamicToolCallOutputContentItem)::InputText\\b.*"
  )
}

predicate inputImageContentItemPattern(MatchArm arm) {
  arm.hasPat() and
  arm.getPat().toString().regexpMatch(
    "(?s).*(ComputerUse(Output|CallOutput)ContentItem|FunctionCallOutputContentItem|DynamicToolCallOutputContentItem)::InputImage\\b.*"
  )
}

from MatchExpr matchExpr
where
  computerUsePipelineFile(matchExpr.getFile()) and
  not insideInlineRustTest(matchExpr) and
  exists(MatchArm arm | arm = matchExpr.getAnArm() and inputTextContentItemPattern(arm)) and
  not exists(MatchArm arm | arm = matchExpr.getAnArm() and inputImageContentItemPattern(arm))
select matchExpr,
  "This match handles text but may drop or fail to preserve native image content. Verify that InputImage is deliberately handled or delegated."
