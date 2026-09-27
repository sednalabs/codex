/**
 * @name Repository workflow security invariant violation
 * @description Detects workflow patterns that violate this repository's public release and runner safety invariants.
 * @kind problem
 * @problem.severity warning
 * @security-severity 7.0
 * @precision medium
 * @id actions/repo-security-invariant-violation
 * @tags actions
 *       security
 *       external/cwe/cwe-693
 */

import actions
import WorkflowSecurity

from Job job, string invariant
where
  jobHasOfficialReleaseSink(job, _) and
  not repoOfficialReleaseJob(job) and
  invariant = "only the sedna-release publish-release job may publish official GitHub Releases"
  or
  job.getARunsOnLabel() = "self-hosted" and
  invariant = "public workflows must not use self-hosted runners"
select job,
  "Repository workflow invariant violated: " + invariant + "."
