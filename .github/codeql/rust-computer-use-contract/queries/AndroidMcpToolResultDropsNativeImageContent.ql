/**
 * @name Android MCP tool-result parser drops native image content
 * @description Android MCP tool-result parsing must not return structuredContent while discarding MCP content[] image items.
 * @kind problem
 * @problem.severity error
 * @precision high
 * @id rust/android-mcp-tool-result-drops-native-image-content
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust

predicate androidComputerUseProviderFile(File file) {
  file.getRelativePath() = "codex-rs/tui/src/android_computer_use_provider.rs"
}

predicate referencesStructuredContent(Function function) {
  exists(StringLiteralExpr literal |
    literal.getEnclosingCallable() = function and
    literal.toString() = "\"structuredContent\""
  )
}

predicate returnsClonedStructuredJson(Function function, ReturnExpr returnExpr) {
  returnExpr.getEnclosingCallable() = function and
  (
    returnExpr.getExpr().toString() = "structured.clone()" or
    returnExpr.getExpr().toString() = "structured.to_owned()"
  )
}

from Function function, ReturnExpr returnExpr
where
  androidComputerUseProviderFile(function.getFile()) and
  // Regression sentinel for the pre-native-image bridge helper shape. Returning
  // the structured JSON value directly from an MCP tool-result parser loses
  // sibling content[] image entries before the Android response can become
  // native computer-use image output. Keep this intentionally narrow to the
  // Android TUI bridge; it is a high-signal contract check, not a general
  // proof that all MCP result parsers preserve image content.
  referencesStructuredContent(function) and
  returnsClonedStructuredJson(function, returnExpr)
select returnExpr,
  "This Android MCP tool-result parser can drop native image content. Preserve structuredContent and content[] together before building the computer-use response."
