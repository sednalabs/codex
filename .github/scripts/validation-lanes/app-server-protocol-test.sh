#!/usr/bin/env bash
set -euo pipefail

cd codex-rs
cargo test --locked -p codex-app-server-protocol \
  protocol::event_mapping::tests::collab_spawn_identity_is_phase_compatible_across_current_and_historic_protocol_conversions \
  --lib -- --exact
exec cargo test --locked -p codex-app-server-protocol
