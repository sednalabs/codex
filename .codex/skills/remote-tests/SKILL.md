---
name: remote-tests
description: How to run tests using the specialized remote executor path. Use this only when a test specifically depends on CODEX_TEST_REMOTE_ENV or the remote docker executor; for ordinary heavy validation, prefer the repo's GitHub CI offload workflows instead.
---

Some codex integration tests support running against a remote executor.
This means that when CODEX_TEST_REMOTE_ENV environment variable is set they will attempt to start an executor process in a docker container CODEX_TEST_REMOTE_ENV points to and use it in tests.

This is a specialized path. It is not the default answer for "run the heavy tests remotely" in this
repo anymore. Use the GitHub-hosted Sedna workflows for general heavy validation and preview builds.

Docker container is built and initialized via ./scripts/test-remote-env.sh

Currently running remote tests is only supported on Linux, so you need to use a devbox to run them

You can list devboxes via `applied_devbox ls`, pick the one with `codex` in the name.
Connect to devbox via `ssh <devbox_name>`.
Reuse the same checkout of codex in `~/code/codex`. Reset files if needed. Multiple checkouts take longer to build and take up more space.
Check whether the SHA and modified files are in sync between remote and local.
