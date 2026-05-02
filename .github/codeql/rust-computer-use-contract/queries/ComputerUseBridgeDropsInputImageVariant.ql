/**
 * @name Computer-use bridge drops InputImage variant
 * @description Protocol bridge matches over computer-use content items must preserve InputImage, not only text.
 * @kind problem
 * @problem.severity error
 * @precision high
 * @id rust/computer-use-bridge-drops-input-image-variant
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust
import ComputerUseContract

predicate guardedBridgeFile(File file) {
  appServerProtocolV2File(file) or threadHistoryFile(file)
}

from MatchExpr matchExpr
where
  guardedBridgeFile(matchExpr.getFile()) and
  not insideInlineRustTest(matchExpr) and
  (
    fileContainsPathSegment(matchExpr.getFile(), "ComputerUseCallOutputContentItem") or
    fileContainsPathSegment(matchExpr.getFile(), "ComputerUseOutputContentItem")
  ) and
  matchHandlesVariant(matchExpr, "InputText") and
  not matchHandlesVariant(matchExpr, "InputImage")
select matchExpr,
  "This computer-use protocol bridge handles InputText but not InputImage. Preserve native image items across app-server/protocol conversions."
