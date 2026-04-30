/**
 * @name Unsafe release publishing path
 * @description Detects release or package publishing from workflows outside the repository's approved release publisher or from jobs that checkout input-controlled refs before publishing.
 * @kind problem
 * @problem.severity warning
 * @security-severity 7.8
 * @precision medium
 * @id actions/unsafe-release-publishing-path
 * @tags actions
 *       security
 *       external/cwe/cwe-494
 */

import actions
import WorkflowSecurity

from Job job, string sinkKind, string reason
where
  jobHasPublishingSink(job, sinkKind) and
  (
    not repoOfficialReleaseJob(job) and
    reason = "outside the repository-approved release publisher"
    or
    jobChecksOutInputRef(job) and
    reason = "after checking out an input-controlled ref"
  )
select job,
  sinkKind + " occurs " + reason +
    ". Keep official publication in the release workflow and validate any manually supplied target before publication."
