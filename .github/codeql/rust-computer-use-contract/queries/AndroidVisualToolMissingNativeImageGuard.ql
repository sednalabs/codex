/**
 * @name Android visual computer-use tool missing native-image guard
 * @description Android observe/step handlers must require native image output before a visual response can be treated as successful.
 * @kind problem
 * @problem.severity error
 * @precision high
 * @id rust/android-visual-tool-missing-native-image-guard
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust

predicate androidComputerUseProviderFile(SourceFile file) {
  file.getRelativePath() = "codex-rs/tui/src/android_computer_use_provider.rs"
}

predicate androidVisualToolHandler(Function function) {
  androidComputerUseProviderFile(function.getFile()) and
  (function.getName() = "observe" or function.getName() = "step")
}

predicate callsNativeImageGuard(Function function) {
  exists(InvocationExpr call |
    call.getEnclosingCallable() = function and
    call.toString().regexpMatch("(?s).*require_native_image_for_visual_response\\s*\\(.*")
  )
}

from Function function
where androidVisualToolHandler(function) and not callsNativeImageGuard(function)
select function,
  "This Android visual computer-use handler can return without enforcing native image output. Call require_native_image_for_visual_response before returning to the model."
