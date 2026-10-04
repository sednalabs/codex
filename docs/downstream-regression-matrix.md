# Downstream Regression Matrix

This focused matrix maps the native computer-use carry to its narrow source
checks. Substantive Rust validation, generated lock verification, and builds
run on standard GitHub-hosted runners only; the coordination workstation is
not a validation host.

| Outcome | Focused hosted proof | Required regression semantics |
| --- | --- | --- |
| Android adapter | Named-test catalog target `codex-android-computer-use/lib`, profiles `rust_minimal` and `rust_integration`; `just tui-native-computer-use-targeted` for TUI routing. | Config aliases/defaults, MCP request mapping, ordered action batches, first-failure handling, install-build mapping, post-action/post-failure observation, native image requirement, same-turn request correlation, and abandoned-thread rejection before provider start. |
| Desktop adapter | `just tui-native-computer-use-targeted` on `codex-tui/lib`. | Explicit opt-in argv configuration, request rejection before spawn, full-duplex bounded I/O, complete JSON parsing, stderr drain/truncation marker, stdout overflow failure, 120-second default/300-second maximum, timeout/cancellation child cleanup, no automatic replay, native image requirement, and same-turn request correlation. |
| Shared Browser boundary | Existing exact Browser source and hosted consumer lanes. | Preserve Browser hook signatures, its abandoned-thread guard, existing `codex_tui` and generic routing, and typed image completion. |

The focused recipe is diagnostic and does not replace exact candidate-bound
hosted inventory/execution, generated Cargo/Bazel lock checks, independent
candidate review, or protected source publication. Never relabel predecessor
CI as proof for a composed candidate.

## Local control-plane diagnostics

These are required qualification controls for the source-prepared diagnostic
slice, not claims that they have run. Retained computer-use rows above preserve
their incoming contract and do not supply analytics qualification evidence.

| Source consumer | Focused hosted proof | Required regression semantics |
| --- | --- | --- |
| Recorder and common reducer | `codex-diagnostics` library target on an exact frozen source. | Default-off noninterference, bounded retained allocation, full identity, duplicate/conflict conservation, missing-start and loss incompleteness, exact requested/resolved targets, complete lifecycle output and both scheduler publication/registration orders. |
| Finite CLI query | `codex-cli` binary test target plus complete emitted-output consumer checks. | Actual parser/dispatch/handler, real envelope call-ID pairing, bounded exact status projections, separate helper return mode and primitive target mode, request/return versus blocked duration, unsupported/malformed/overflow inputs, unknown host lineage/queue/acknowledgement, and no body/private-path disclosure. Binary-target tests alone are not installed-process proof. |
| Existing usage snapshot reader and join | `codex-state` library target and CLI-to-reader-to-join whole-output controls using a frozen synthetic fixture. | No database creation, migration, profile discovery or new companion files; explicit custodian claims versus read observations; exact half-open window/source/call identities; actual versus requested models; missing/unpriced/covered/scenario separation; partition and token/credit conservation; unsafe/WAL-ambiguous/incompatible/conflicting inputs fail safely. |
| Actual runtime producer integration | Exact owned hook emission and consumer controls when a separately frozen hooked candidate exists. | Genuine event wake versus timeout, quiet progress versus action-needed wake, queue-only pending sleep versus durable idle scheduling, compaction/recovery/replay/deduplication, exact lineage and unchanged provider-call counts. Typed fixtures do not attest a missing runtime producer. |

Hosted observer controls must distinguish known pass and fail before reporting
measurements. Proof binds exact source/tree, harness, run/attempt and complete
bounded public-safe artifact bytes; raw logs, private rollouts and live databases
are not publication inputs. Generated Cargo/Bazel locks are derived by canonical
hosted recipes and consumed as a separately committed exact successor.
