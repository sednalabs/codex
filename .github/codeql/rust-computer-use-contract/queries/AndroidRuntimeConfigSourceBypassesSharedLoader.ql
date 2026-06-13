/**
 * @name Android runtime config source bypasses shared loader
 * @description Android runtime config source names should live in the shared codex-tools loader so core acquisition and the TUI provider cannot drift.
 * @kind problem
 * @problem.severity error
 * @precision high
 * @id rust/android-runtime-config-source-bypasses-shared-loader
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust

predicate guardedSourceFile(File file) {
  file.getRelativePath().regexpMatch("codex-rs/(core|tui|computer-use-runtime)/src/.*\\.rs")
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

predicate androidRuntimeConfigSourceLiteral(StringLiteralExpr literal) {
  literal.toString() = "\"CODEX_ANDROID_MCP_URL\"" or
  literal.toString() = "\"SOLARLAB_ANDROID_MCP_URL\"" or
  literal.toString() = "\"CODEX_ANDROID_MCP_HOSTNAME\"" or
  literal.toString() = "\"SOLARLAB_ANDROID_MCP_HOSTNAME\"" or
  literal.toString() = "\"CODEX_ANDROID_MCP_CF_ACCESS_CLIENT_ID\"" or
  literal.toString() = "\"SOLARLAB_ANDROID_MCP_CF_ACCESS_CLIENT_ID\"" or
  literal.toString() = "\"CODEX_ANDROID_MCP_CF_ACCESS_CLIENT_SECRET\"" or
  literal.toString() = "\"SOLARLAB_ANDROID_MCP_CF_ACCESS_CLIENT_SECRET\"" or
  literal.toString() = "\"android-computer-use.json\"" or
  literal.toString() = "\"android-dynamic-tools.json\"" or
  literal.toString() = "\"solarlab-android-dynamic-tools.json\""
}

from StringLiteralExpr literal
where
  guardedSourceFile(literal.getFile()) and
  not insideInlineRustTest(literal) and
  androidRuntimeConfigSourceLiteral(literal)
select literal,
  "Android runtime config source names should be read through codex_tools::load_android_runtime_config instead of being duplicated in core or TUI code."
