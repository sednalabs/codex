/**
 * @name Android screenshot fallback missing
 * @description Android visual observation must retain a screenshot fallback and artifact-to-native-image bridge when inspect_ui is degraded.
 * @kind problem
 * @problem.severity error
 * @precision high
 * @id rust/android-screenshot-fallback-missing
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust
import ComputerUseContract

from Function function
where
  androidComputerUseRuntimeFile(function.getFile()) and
  function.getName().getText() = "screenshot_fallback_response" and
  (
    not functionContainsStringLiteral(function, "\"android.capture_screenshot\"")
    or
    not exists(Function observation |
      androidComputerUseRuntimeFile(observation.getFile()) and
      observation.getName().getText() = "observation_response" and
      functionContainsStringLiteral(observation, "\"android.read_artifact\"")
    )
  )
select function,
  "Android degraded visual observation must preserve a screenshot fallback and convert provider artifacts into native image output when direct MCP image content is unavailable."
