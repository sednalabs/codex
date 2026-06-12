/**
 * @name Sensitive workflow value may reach verbose tool output without masking
 * @description Detects generated or transformed credentials, PEM material, release credentials, and sensitive environment values used with verbose or debug tool output before they are masked.
 * @kind problem
 * @problem.severity warning
 * @security-severity 7.0
 * @precision medium
 * @id actions/sensitive-workflow-value-to-verbose-tool
 * @tags actions
 *       security
 *       external/cwe/cwe-312
 */

import actions
import LogExposure

from Run run, string name, string sourceKind, string sinkKind, string command
where
  sensitiveVariableInRun(run, name, sourceKind) and
  command = run.getScript().getACommand() and
  verboseCredentialCommand(command, name, sinkKind) and
  not hasPriorMask(run, name)
select run,
  "Sensitive " + sourceKind + " in '" + name + "' is used with " + sinkKind +
    " before a prior ::add-mask:: command is proven."
