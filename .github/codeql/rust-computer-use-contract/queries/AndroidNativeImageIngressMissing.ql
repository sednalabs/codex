/**
 * @name Android native image ingress missing
 * @description The Android computer-use runtime must construct model-facing native image output items.
 * @kind problem
 * @problem.severity error
 * @precision high
 * @id rust/android-native-image-ingress-missing
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust
import ComputerUseContract

from Function anchor
where
  androidComputerUseRuntimeFile(anchor.getFile()) and
  anchor.getName().getText() = "handle_android_computer_use" and
  not exists(StructExpr imageExpr |
    androidComputerUseRuntimeFile(imageExpr.getFile()) and
    not insideInlineRustTest(imageExpr) and
    inputImageConstructor(imageExpr)
  )
select anchor,
  "The Android computer-use runtime no longer constructs native InputImage output. Provider screenshots must be converted into model-facing image content."
