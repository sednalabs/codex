# Downstream Regression Matrix

This focused matrix maps the native computer-use carry to its narrow source
checks. Substantive Rust validation, generated lock verification, and builds
run on standard GitHub-hosted runners only; the coordination workstation is
not a validation host.

| Outcome                 | Focused hosted proof                                                                                                                                                                                              | Required regression semantics                                                                                                                                                                                                                                                                   |
| ----------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Android adapter         | Standard hosted execution of the `codex-android-computer-use` library target plus the TUI library's Android consumer fixtures. The exact catalog row and invocation belong to the hosted-route owner.             | Config aliases/defaults, MCP request mapping, ordered action batches, first-failure handling, install-build mapping, post-action/post-failure observation, native image requirement, same-turn request correlation, and abandoned-thread rejection before provider start.                       |
| Desktop adapter         | Standard hosted TUI library validation for the exact candidate, including the Desktop provider unit tests and app-server consumer fixture. The exact catalog row and invocation belong to the hosted-route owner. | Config filenames and environment overrides, argv-only command launch, request/stdout/stderr/deadline bounds, owned-child timeout/cancellation cleanup, uncertain-result recovery, native image requirement, same-turn request correlation, and abandoned-thread rejection before provider start. |
| Shared Browser boundary | Existing exact Browser source and hosted consumer lanes.                                                                                                                                                          | Preserve Browser hook signatures, its abandoned-thread guard, existing `codex_tui` and generic routing, and typed image completion.                                                                                                                                                             |

The focused recipe is diagnostic and does not replace exact candidate-bound
hosted inventory/execution, generated Cargo/Bazel lock checks, independent
candidate review, or protected source publication. Never relabel predecessor
CI as proof for a composed candidate.
