/**
 * @name Write-capable token on untrusted trigger
 * @description Detects jobs on untrusted triggers that receive write-capable GITHUB_TOKEN permissions.
 * @kind problem
 * @problem.severity error
 * @security-severity 8.6
 * @precision high
 * @id actions/write-token-on-untrusted-trigger
 * @tags actions
 *       security
 *       external/cwe/cwe-266
 */

import actions
import WorkflowSecurity

from Job job, string eventName, string permissionName
where
  jobHasUntrustedTrigger(job, eventName) and
  jobEffectiveWritePermission(job, permissionName)
select job,
  "The job runs on '" + eventName + "' with write-capable '" + permissionName +
    "' token permissions. Split untrusted input handling from write operations."
