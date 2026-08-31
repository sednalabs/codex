# Private voice helper foundation

`codex-voice-host` establishes the inherited-pipe lifecycle for the proposed
bundled voice process. It does not open devices, load native plugins, negotiate
WebRTC, or enable voice in the TUI. The existing CLI is unchanged.

Frames are a big-endian u32 length followed by at most 256 bytes of JSON. The
parent sends `hello` with protocol `1` and the helper's exact `buildCommit` before
receiving `ready`. It then sends `close` and receives `closed` before process exit.
Unknown fields, incompatible builds, invalid order and oversized frames fail
closed without echoing input. EOF exits even when the main worker cannot progress.

Bazel stamps the binary with `STABLE_GIT_COMMIT`. Cargo builders must provide the
same variable; an unstamped source build reports `dev` via `--build-commit` and is
not a distributable build identity. The client/control crate has no native audio
dependencies. Installed-package resolution, native loading, media/privacy controls
and actual audio proof belong to the subsequent integration stages.
