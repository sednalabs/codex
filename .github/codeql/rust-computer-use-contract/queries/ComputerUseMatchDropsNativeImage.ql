/**
 * @name Computer-use content match drops native image variant
 * @description Computer-use content-item matches that handle text must also preserve native image items.
 * @kind problem
 * @problem.severity error
 * @precision medium
 * @id rust/computer-use-match-drops-native-image
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust

predicate computerUsePipelineFile(SourceFile file) {
  file.getRelativePath().regexpMatch("codex-rs/(protocol|core|app-server|tui)/src/.*\\.rs") and
  not file.getRelativePath().regexpMatch("(?s).*(tests?|_tests)\\.rs$")
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
  exists(MatchArm arm | arm = matchExpr.getAnArm() and contentItemPattern(arm, "InputText")) and
  not exists(MatchArm arm | arm = matchExpr.getAnArm() and contentItemPattern(arm, "InputImage"))
select matchExpr,
  "This computer-use content-item match handles text but not native image content. Preserve InputImage or deliberately delegate through an image-preserving helper."
