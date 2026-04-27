/**
 * @name Sensitive workflow value reaches log-visible output without masking
 * @description Detects generated or transformed credentials, PEM material, release credentials, and sensitive environment values written to GitHub Actions logs or step summaries before they are masked.
 * @kind problem
 * @problem.severity error
 * @security-severity 8.8
 * @precision high
 * @id actions/sensitive-workflow-value-to-log
 * @tags actions
 *       security
 *       external/cwe/cwe-312
 */

import actions
import LogExposure

from Run run, string name, string sourceKind, string sinkKind, string command
where
  sensitiveVariableInRun(run, name, sourceKind) and
  (
    command = run.getScript().getACommand() and
    logVisibleCommand(command, name, sinkKind)
    or
    tracingExposesVariable(run, name, command) and
    sinkKind = "shell tracing"
  ) and
  not hasPriorMask(run, name)
select run,
  "Sensitive " + sourceKind + " in '" + name + "' reaches " + sinkKind +
    " without a prior ::add-mask:: command."
