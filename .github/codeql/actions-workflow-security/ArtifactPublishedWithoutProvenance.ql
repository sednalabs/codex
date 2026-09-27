/**
 * @name Published artifact lacks provenance markers
 * @description Detects release or package publication without checksum, signing, metadata, or native artifact-attestation markers.
 * @kind problem
 * @problem.severity warning
 * @security-severity 6.8
 * @precision medium
 * @id actions/artifact-published-without-provenance
 * @tags actions
 *       security
 *       external/cwe/cwe-353
 */

import actions
import WorkflowSecurity

from Job job, string sinkKind
where
  jobHasPublishingSink(job, sinkKind) and
  not jobHasRepoApprovedProvenance(job)
select job,
  sinkKind +
    " publishes artifacts without repository-approved provenance markers. Add checksums, signing metadata, and preferably GitHub artifact attestations."
