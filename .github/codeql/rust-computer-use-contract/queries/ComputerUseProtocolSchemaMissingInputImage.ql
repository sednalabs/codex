/**
 * @name Computer-use protocol schema missing InputImage
 * @description The app-server computer-use response schema must keep the inputImage variant for native visual evidence.
 * @kind problem
 * @problem.severity error
 * @precision high
 * @id rust/computer-use-protocol-schema-missing-input-image
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust
import ComputerUseContract

from Function anchor
where
  appServerProtocolV2File(anchor.getFile()) and
  anchor.getName().getText() = "from" and
  functionContainsPathSegment(anchor, "ComputerUseCallOutputContentItem") and
  not functionContainsPathSegment(anchor, "InputImage")
select anchor,
  "The computer-use app-server protocol conversion no longer includes InputImage. Keep inputImage in the schema and bridge so clients can return native visual evidence."
