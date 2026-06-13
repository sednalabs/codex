/**
 * @name Computer-use response reports success with error
 * @description A successful computer-use response should not also carry an error payload.
 * @kind problem
 * @problem.severity error
 * @precision high
 * @id rust/computer-use-response-success-with-error
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust

predicate computerUsePipelineFile(File file) {
  file.getRelativePath().regexpMatch("codex-rs/(protocol|core|app-server|tui|computer-use-runtime)/src/.*\\.rs") and
  not file.getRelativePath().regexpMatch("(?s).*(/tests?/.*|.*_tests\\.rs|test\\.rs|tests\\.rs)$")
}

predicate cfgTestMeta(Meta meta) {
  meta.getPath().getText() = "cfg" and
  meta.toString().regexpMatch("(?s).*\\btest\\b.*")
}

predicate inlineRustTestModule(Module modItem) {
  cfgTestMeta(modItem.getAnAttr().getMeta()) or
  modItem.getName().getText() = "tests"
}

predicate insideInlineRustTest(AstNode node) {
  exists(Module modItem |
    inlineRustTestModule(modItem) and
    modItem.getFile() = node.getFile() and
    modItem.getLocation().getStartLine() <= node.getLocation().getStartLine() and
    node.getLocation().getEndLine() <= modItem.getLocation().getEndLine()
  )
}

predicate computerUseCallResponseExpr(StructExpr responseExpr) {
  computerUsePipelineFile(responseExpr.getFile()) and
  not insideInlineRustTest(responseExpr) and
  responseExpr.getPath().getSegment().getIdentifier().getText() = "ComputerUseCallResponse"
}

predicate fieldIsTrue(StructExpr responseExpr, string fieldName) {
  exists(StructExprField field, BooleanLiteralExpr value |
    field = responseExpr.getFieldExpr(fieldName) and
    value = field.getExpr() and
    value.getTextValue() = "true"
  )
}

predicate fieldIsSome(StructExpr responseExpr, string fieldName) {
  exists(StructExprField field, TupleVariantExpr value |
    field = responseExpr.getFieldExpr(fieldName) and
    value = field.getExpr() and
    value.getVariant().getName().getText() = "Some"
  )
}

from StructExpr responseExpr
where
  computerUseCallResponseExpr(responseExpr) and
  fieldIsTrue(responseExpr, "success") and
  fieldIsSome(responseExpr, "error")
select responseExpr,
  "This computer-use response sets success to true while also setting error. Use error: None for successful responses, or success: false for failures."
