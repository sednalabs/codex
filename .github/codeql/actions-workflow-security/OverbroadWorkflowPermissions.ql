/**
 * @name Over-broad workflow permissions
 * @description Detects workflow-level write permissions and job write permissions that are broader than the detected workflow sink requires.
 * @kind problem
 * @problem.severity warning
 * @security-severity 6.5
 * @precision medium
 * @id actions/overbroad-workflow-permissions
 * @tags actions
 *       security
 *       external/cwe/cwe-275
 */

import actions
import WorkflowSecurity

from AstNode node, string permissionName, string reason
where
  exists(Workflow workflow |
    node = workflow and
    workflowLevelWritePermission(workflow, permissionName) and
    reason = "workflow-level write permission"
  )
  or
  exists(Job job |
    node = job and
    jobEffectiveWritePermission(job, permissionName) and
    not jobNeedsWritePermission(job, permissionName) and
    reason = "write permission without a matching write sink"
  )
select node,
  "The Actions token grants '" + permissionName + "' as a " + reason +
    ". Prefer job-scoped least-privilege permissions."
