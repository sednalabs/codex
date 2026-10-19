# Codex keyring source carry

This package is the published `keyring` 3.6.3 crate from crates.io, retained
with its upstream source and license files unchanged except for three
whitespace-only import cleanups listed below. The original crate
archive SHA-256 is
`eebcc3aff044e5944a8fbaf69eb277d11986064cba30c468730e8b9909fb551c`.
The archive's `.cargo_vcs_info.json` identifies upstream source commit
`315cbdf6c6a9153d8c9f88b56568f29862d3e39d`.

The functional source change is in `Cargo.toml`: the maintained Linux and
BSD async Secret Service dependency moves to the official Secret Service v6
implementation at the immutable revision recorded there, and the keyring
Tokio/async-io features forward to that dependency. Keeping the published
keyring source preserves its public `Entry` and default credential-builder
behavior while allowing the downstream dependency graph to leave the
unmaintained zbus 4 family. Do not patch credential behavior here without a
separate reviewed need.

The only source-file deviations are trimming trailing spaces from two
documentation lines in `src/secret_service.rs` and removing the final blank
line from the upstream `.gitignore`, so the imported files pass `git diff
--check`. No executable source logic was changed.
