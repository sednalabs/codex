---
name: code-review
description: Run a final, repository-scoped pull-request review in one bounded pass, using the linked guidance for breaking changes, context, change size, and testing.
---

# Code review

Review the target pull request as one bounded reviewer pass. Read the linked
reference guidance before assessing the diff, then apply all four areas
together rather than splitting them into separate reviewer tasks:

- [Breaking changes](references/breaking-changes.md)
- [Change size](references/change-size.md)
- [Model-visible context](references/context.md)
- [Test authoring](references/testing.md)

Inspect the complete diff and the surrounding code needed to establish
behavior. Do not stop after finding the first issue. Report every material
issue you find, using raw Markdown, numbered findings, and a specific file path
and line number for each finding. If there are no findings, say so explicitly.

Use the testing reference to identify missing or inadequate coverage when the
change affects behavior; do not require tests for inert documentation-only
changes. Apply the change-size and context references to the actual diff and
review path, not as generic project policy.

If the authenticated GitHub user owns the pull request, add the
`code-reviewed` label. Do not leave GitHub comments unless the operator
explicitly asks for them.
