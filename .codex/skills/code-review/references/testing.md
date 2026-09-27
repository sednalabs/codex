# Test authoring

For agent behavior changes, prefer integration tests over unit tests. Use the
existing `core/suite` and `test_codex` helpers when they cover the behavior.

Behavior changes should identify the major logic changes and user-facing
behaviors that require coverage. If a unit test is the right seam, put it in a
dedicated `*_tests.rs` file and avoid test-only functions in production code.
Check for existing helpers before adding new test scaffolding.
