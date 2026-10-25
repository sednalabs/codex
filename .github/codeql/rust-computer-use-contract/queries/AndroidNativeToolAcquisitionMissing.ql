/**
 * @name Android native computer-use acquisition missing from session startup
 * @description Session startup must acquire configured native Android computer-use tools before finalizing the session dynamic tool list.
 * @kind problem
 * @problem.severity error
 * @precision high
 * @id rust/android-native-tool-acquisition-missing
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust

predicate sessionModuleFile(File file) {
  file.getRelativePath() = "codex-rs/core/src/session/mod.rs"
}

predicate sessionConfigurationExpr(StructExpr expr) {
  sessionModuleFile(expr.getFile()) and
  expr.getPath().getSegment().getIdentifier().getText() = "SessionConfiguration"
}

predicate dynamicToolsField(StructExpr expr, StructExprField field) {
  field = expr.getFieldExpr("dynamic_tools")
}

predicate acquisitionCallBefore(StructExpr expr) {
  exists(Call call |
    call.getFile() = expr.getFile() and
    call.toString().regexpMatch("(?s).*augment_with_acquired_native_android_tools.*") and
    call.getLocation().getStartLine() < expr.getLocation().getStartLine()
  )
}

from StructExpr expr, StructExprField field
where
  sessionConfigurationExpr(expr) and
  dynamicToolsField(expr, field) and
  not acquisitionCallBefore(expr)
select field,
  "SessionConfiguration.dynamic_tools can be finalized without native Android computer-use acquisition. Call augment_with_acquired_native_android_tools before constructing the session configuration."
