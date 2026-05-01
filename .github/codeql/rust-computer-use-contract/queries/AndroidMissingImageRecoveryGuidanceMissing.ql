/**
 * @name Android missing-image response lacks recovery guidance
 * @description Android visual missing-image failures must tell agents not to make visual claims from text-only evidence.
 * @kind problem
 * @problem.severity error
 * @precision high
 * @id rust/android-missing-image-recovery-guidance-missing
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust
import ComputerUseContract

predicate nativeImageGuardCall(Call call, Function handler) {
  visualAndroidRuntimeHandler(handler) and
  call.getEnclosingCallable() = handler and
  exists(Function target |
    target = call.getStaticTarget() and
    target.getName().getText() = "require_native_image_for_visual_response" and
    androidComputerUseRuntimeFile(target.getFile())
  )
}

from Call call, Function handler, StringLiteralExpr message
where
  nativeImageGuardCall(call, handler) and
  message = call.getPositionalArgument(1) and
  not message.toString().regexpMatch("(?s).*visual claims.*")
select message,
  "This Android missing-image failure message does not warn the agent to recover before making visual claims."
