/**
 * @name Core computer-use model output is text-only
 * @description Core computer-use responses must reach the model as content items so native images remain visible.
 * @kind problem
 * @problem.severity error
 * @precision high
 * @id rust/core-computer-use-model-output-text-only
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust
import ComputerUseContract

from Function function
where
  computerUseHandlerFile(function.getFile()) and
  function.getName().getText() = "computer_use_response_content_for_model" and
  (
    functionContainsPathSegment(function, "function_call_output_content_items_to_text") or
    not functionContainsPathSegment(function, "FunctionCallOutputContentItem")
  )
select function,
  "This core computer-use response path can become text-only before reaching the model. Convert native content items into FunctionCallOutputContentItem values and pass them through FunctionToolOutput::from_content."
