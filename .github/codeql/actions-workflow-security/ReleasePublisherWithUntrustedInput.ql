/**
 * @name Release publisher uses untrusted workflow input
 * @description Detects publishing jobs that use workflow-dispatch or event-controlled inputs in checkout or release commands before publishing.
 * @kind problem
 * @problem.severity warning
 * @security-severity 7.5
 * @precision medium
 * @id actions/release-publisher-with-untrusted-input
 * @tags actions
 *       security
 *       external/cwe/cwe-829
 */

import actions
import WorkflowSecurity

from Job job, string sinkKind, string command
where
  jobHasPublishingSink(job, sinkKind) and
  (
    jobChecksOutInputRef(job) and
    command = "actions/checkout ref"
    or
    jobRunCommand(job, command) and
    command.regexpMatch("(?is).*(inputs\\.|github\\.event\\.|INPUT_[A-Z0-9_]+).*") and
    releasePublishingCommand(command, _)
  )
select job,
  sinkKind + " depends on workflow or event-controlled input ('" + command +
    "'). Resolve and validate the target commit before publishing."
