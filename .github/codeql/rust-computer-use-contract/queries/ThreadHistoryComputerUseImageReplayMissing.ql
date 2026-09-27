/**
 * @name Thread history computer-use image replay missing
 * @description Thread-history replay must keep computer-use image items so resumed sessions preserve visual evidence.
 * @kind problem
 * @problem.severity error
 * @precision high
 * @id rust/thread-history-computer-use-image-replay-missing
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust
import ComputerUseContract

from Function function
where
  threadHistoryFile(function.getFile()) and
  function.getName().getText() = "convert_computer_use_content_items" and
  not (
    functionContainsPathSegment(function, "InputImage") or
    functionContainsPathSegment(function, "ComputerUseCallOutputContentItem")
  )
select function,
  "Thread-history computer-use replay no longer preserves native image content items. Replay must convert or explicitly handle InputImage variants."
