/**
 * @name Android inspect_ui call omits screenshot request
 * @description Android visual observation should ask the provider for screenshot content instead of relying only on UI hierarchy text.
 * @kind problem
 * @problem.severity error
 * @precision high
 * @id rust/android-inspect-ui-screenshot-request-missing
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust
import ComputerUseContract

from Function function
where
  androidComputerUseRuntimeFile(function.getFile()) and
  not insideInlineRustTest(function) and
  functionContainsStringLiteral(function, "\"android.inspect_ui\"") and
  not functionContainsStringLiteral(function, "\"include_screenshot\"")
select function,
  "This Android inspect_ui call path does not request screenshots. Set include_screenshot: true so successful visual observations can include native image output."
