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

from Function function
where
  androidComputerUseProviderFile(function.getFile()) and
  // Regression sentinel for the pre-native-image bridge helper. This helper
  // returned structuredContent as a raw JSON value and lost sibling MCP
  // content[] image entries before the Android response could become native
  // computer-use image output.
  function.getName().getText() = "tool_structured_or_text"
select function,
  "This Android MCP tool-result parser can drop native image content. Preserve structuredContent and content[] together before building the computer-use response."
