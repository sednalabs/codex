# Android Provider Execution Contract v1

Status: frozen owner map for the Android connector delivery sequence.

Contract version: `android-provider-execution/v1`.

This is a narrow, cross-repository execution contract for the existing native
Android tools. It complements [Native Computer-Use Adapter
Tooling](native-computer-use.md), which remains the canonical description of
Codex-owned tool, transcript, and image-delivery behavior. This document does
not implement a provider, change a tool schema, or alter an existing work-item
status.

The reviewed baselines are Codex `origin/main` at
`608749a03b9d7636383d1d4e0437d79902f92c8d`, Android provider `origin/main` at
`458dd1fa6106fd9342598136a50c6539dd06ef22`, and Solar Gravity Lab
`origin/main` at `0f42cb08a0226077c09d1b93c36cb2f3864a1b8a`.

## Normative ownership boundary

- **Codex** owns the native tool schemas, `ComputerUse` protocol events,
  app-server routing, transcript and TUI projection, and model-visible
  `inputImage` delivery. Its authorities include
  `codex-rs/tools/src/android_tool.rs`,
  `codex-rs/core/src/tools/handlers/computer_use.rs`,
  `codex-rs/app-server-protocol/src/protocol/v2/item.rs`,
  `codex-rs/app-server/src/bespoke_event_handling.rs`, and
  `codex-rs/tui/src/computer_use_provider.rs`.
- **android-emulator-mcp** owns Android target resolution, provider/session
  lifecycle, device and app readiness, accessibility-tree selection, input
  execution, stable-state observation, UI digest creation, runtime receipts,
  and provider-side build installation. Its authorities include
  `src/tools.rs`, `src/interactive_session.rs`, `src/ui.rs`,
  `src/verification.rs`, `src/tool_surface.rs`, and
  `adapters/codex/provider_manifest.js`.
- **Solar Gravity Lab** is a consumer and hosted acceptance surface. It owns
  the interactive-session provider pin and consumer-flow evidence; it does not
  own the generic Android tool, provider protocol, or Codex transcript schema.

An implementation MUST preserve these boundaries. The provider may enrich a
receipt, but it MUST NOT invent a parallel Codex event shape. Codex may project
the typed receipt, but it MUST NOT infer an Android target from unbound default
state.

## Version and compatibility rules

1. Every v1 request and response carries
   `contract_version: "android-provider-execution/v1"`.
2. A v1 consumer MUST reject a different major version before issuing a
   mutating action. A provider may advertise minor capabilities in
   `capabilities`; unknown optional fields are ignored.
3. Additive optional fields are a minor change. Changing the meaning of an
   identity, observation generation, error kind, or completed action outcome
   requires a new major version.
4. Legacy `android_observe`, `android_step`, and
   `android_install_build_from_run` inputs remain accepted only through an
   explicit compatibility translation. The response MUST set
   `compatibility_mode: "legacy-translated"`, record the resolved target, and
   never silently fill an ambiguous target from a different session.
5. A visual claim requires native `inputImage` content for its observation.
   Artifact paths, XML, logs, and digest text are receipts, not visual
   substitutes.

## Typed contract

The examples below are JSON shapes; an implementation may use generated Rust
or TypeScript types provided the encoded fields and invariants are preserved.

### Target and provenance identity

```json
{
  "target": {
    "environment_id": "env_01",
    "provider_instance_id": "provider_01",
    "session_id": "session_01",
    "device_serial": "emulator-5554",
    "app": {
      "package_name": "com.example.androidapp",
      "activity": "com.example.androidapp.MainActivity"
    },
    "expected_build": {
      "repository": "owner/android-app",
      "commit_sha": "0123456789abcdef0123456789abcdef01234567",
      "workflow_run_id": 123456,
      "artifact_name": "android-apk",
      "artifact_sha256": "sha256:<hex>"
    }
  }
}
```

`environment_id`, `provider_instance_id`, `session_id`, and `device_serial`
identify the execution target as a tuple. They are opaque identifiers, not
user-facing labels. A request that omits one may use a configured default only
when the provider can resolve exactly one active tuple; its response MUST
return every resolved value. A mutating call with more than one viable tuple
MUST fail with `target_ambiguous`, rather than choosing a device or session.

`expected_build` is optional for ordinary observation, required for a
build-install request, and required for a request that claims to act on a
particular installed build. The returned build identity is the installed
manifest actually observed by the provider, not an unverified caller hint.

### Request

```json
{
  "contract_version": "android-provider-execution/v1",
  "request_id": "android-request-01",
  "target": { "environment_id": "env_01", "session_id": "session_01" },
  "operation": {
    "kind": "step",
    "actions": [
      {
        "action_id": "a1",
        "kind": "tap",
        "selector": { "content_desc": "Continue" },
        "observed_generation": "obs_41"
      }
    ]
  },
  "postcondition": {
    "selector_present": { "text": "Ready" },
    "stable_polls": 2
  },
  "evidence_request": {
    "include_ui_digest": true,
    "include_input_image": true,
    "record_bundle": true
  }
}
```

`operation.kind` is one of `observe`, `step`, or `install_build_from_run`.
Only `step` and `install_build_from_run` are mutating. `actions` is ordered and
each `action_id` is unique within a request. A selector action references the
observation generation that informed it; a coordinate action also carries the
coordinate frame identity from that observation. A multi-touch gesture is one
atomic action: it MUST NOT be emulated as several independent taps or swipes.

### Readiness, response, and postconditions

```json
{
  "contract_version": "android-provider-execution/v1",
  "request_id": "android-request-01",
  "compatibility_mode": "native-v1",
  "resolved_target": {
    "environment_id": "env_01",
    "provider_instance_id": "provider_01",
    "session_id": "session_01",
    "device_serial": "emulator-5554",
    "app": { "package_name": "com.example.androidapp" }
  },
  "readiness": {
    "state": "app_ready",
    "checked_at": "2026-07-21T10:00:00Z",
    "recovery": "not_required"
  },
  "observation": {
    "observation_id": "obs_42",
    "generation": "obs_42",
    "coordinate_frame_id": "android-frame-42",
    "input_image": { "content_type": "image/png", "present": true },
    "ui_digest": { "generation": "ui-42", "stable": true }
  },
  "action_batch": {
    "batch_id": "batch-01",
    "outcomes": [
      {
        "action_id": "a1",
        "status": "applied",
        "effect_receipt_id": "effect-01"
      }
    ]
  },
  "postcondition": { "status": "satisfied", "evidence_generation": "obs_42" },
  "evidence": {
    "bundle_id": "evidence-01",
    "manifest_sha256": "sha256:<hex>"
  }
}
```

Readiness states are exactly `target_unresolved`, `provider_unavailable`,
`device_booting`, `device_ready`, `app_launching`, `app_ready`,
`recovery_required`, or `stale`. A state other than `app_ready` prevents a
mutating action unless the operation itself is an explicitly scoped recovery
or lifecycle action.

A postcondition result is `satisfied`, `not_satisfied`, `not_evaluated`, or
`unavailable`. `not_satisfied` means the action dispatch may already have had
an effect; it MUST include the post-action observation when one is available.
`unavailable` means the current state is unproven and the response MUST direct
the caller to obtain a fresh observation before making a visual claim.

### Error and partial-action receipt

```json
{
  "error": {
    "kind": "partial_action",
    "retryability": "do_not_replay",
    "message": "a2 did not complete after a1 was applied",
    "resolved_target": {
      "session_id": "session_01",
      "device_serial": "emulator-5554"
    },
    "batch": {
      "batch_id": "batch-01",
      "outcomes": [
        {
          "action_id": "a1",
          "status": "applied",
          "effect_receipt_id": "effect-01"
        },
        {
          "action_id": "a2",
          "status": "failed",
          "reason": "selector_no_match"
        },
        { "action_id": "a3", "status": "not_attempted" }
      ]
    },
    "post_failure_observation": {
      "generation": "obs_43",
      "input_image_present": true
    }
  }
}
```

The error kinds are `target_missing`, `target_ambiguous`, `target_mismatch`,
`build_provenance_mismatch`, `not_ready`, `selector_no_match`,
`selector_ambiguous`, `stale_observation`, `postcondition_unmet`,
`partial_action`, `capability_unsupported`, `provider_unavailable`, and
`recovery_required`. An error that follows any mutating attempt MUST carry
completed, failed, and not-attempted outcomes; retrying the whole batch is
never implied by a generic failure string.

### Evidence record

```json
{
  "bundle_id": "evidence-01",
  "contract_version": "android-provider-execution/v1",
  "request_id": "android-request-01",
  "resolved_target": {
    "environment_id": "env_01",
    "provider_instance_id": "provider_01",
    "session_id": "session_01",
    "device_serial": "emulator-5554"
  },
  "provider_revision": "<provider-commit>",
  "codex_revision": "<codex-commit>",
  "build_provenance": {
    "repository": "owner/android-app",
    "commit_sha": "0123456789abcdef0123456789abcdef01234567",
    "workflow_run_id": 123456,
    "artifact_sha256": "sha256:<hex>"
  },
  "before_observation": "obs_41",
  "after_observation": "obs_42",
  "action_batch": "batch-01",
  "postcondition": "satisfied",
  "artifacts": [
    { "kind": "screenshot", "sha256": "sha256:<hex>" },
    { "kind": "ui_hierarchy", "sha256": "sha256:<hex>" }
  ],
  "manifest_sha256": "sha256:<hex>"
}
```

Evidence records are append-only receipts. They may redact user-entered text
and other sensitive payloads, but not the identity tuple, code revisions,
build identity, action outcomes, observation generations, or artifact digests
needed to establish what was proven.

## Requirement-to-owner matrix

`Disposition` classifies the relationship to existing work. `extend-owner`
means the listed owner remains authoritative and the successor owns only the
newly frozen Android-specific invariant. `new-gap` means no listed historical
item owns that exact invariant. No historical item is superseded.

| Requirement | Canonical repository and code authority | Existing Ops owner and disposition | Successor and sole invariant | Natural assertion boundary | Rollout boundary |
| --- | --- | --- | --- | --- | --- |
| Exact session targeting | `android-emulator-mcp`: provider manifest, target resolution, `src/tools.rs`; Codex: `ComputerUseCallParams` and provider routing | `default:w4284` environment binding and lease ownership — **extend-owner** | `default:w10347`: every request/response receipt resolves one identical `(environment, provider, session, serial)` tuple | Whole request/response target equality, including a refusal for ambiguous or stale targets | Provider manifest plus Codex app-server/TUI transport |
| Build provenance | `android-emulator-mcp`: `src/interactive_session.rs`; Solar hosted provider pin | `default:w4387` native install exposure and `default:w4398` fixed provider ref — **extend-owner** | `default:w10347`: installed-build receipt binds run artifact and digest to the same resolved session | Whole install receipt equals requested run/artifact and observed manifest, or returns `build_provenance_mismatch` | Hosted session uses the pinned provider revision and records the build receipt |
| Readiness | `android-emulator-mcp`: boot/app probes and `src/tools.rs` | No existing owner defines the typed provider-device-app state union — **new-gap**; recovery remains with `default:w4294`/`default:w4337` | `default:w10348`: one typed readiness state gates every mutating request | State-machine snapshot for each terminal state and legal transition | Provider release, with recovery compatibility checked against `w4294`/`w4337` |
| Selector behavior | `android-emulator-mcp`: `src/ui.rs`, `src/tools.rs`, schema snapshot | `default:w4242` bridge/action path — **extend-owner** | `default:w10349`: a selector result is unique or returns normalized, bounded candidates | Complete selector result object for unique, no-match, ambiguous, and index-out-of-range cases | Provider schema snapshot and Codex compatibility translation |
| Postconditions and stable frames | `android-emulator-mcp`: `src/verification.rs`, `src/tools.rs` | `default:w4242` post-action capture — **extend-owner** | `default:w10350`: semantic completion uses a typed postcondition and stable observation, never an implicit sleep | Complete postcondition result plus post-action observation generation | Provider contract test; Codex surfaces the resulting receipt unchanged |
| UI digest | `android-emulator-mcp`: UI normalization/output and tool schema | No historical owner defines stable observed-generation semantics — **new-gap** | `default:w10351`: digest facts, source observation, and generation move together | Complete digest object has one stable generation and references its observation | Provider digest/schema rollout; consumers treat it as supplemental to pixels |
| Action batches | `android-emulator-mcp`: bridge/action execution; Codex: native step schema | `default:w4242` batched computer-style bridge — **extend-owner** | `default:w10352`: every action in a batch has an explicit terminal outcome | Whole ordered outcomes array distinguishes applied, failed, and not-attempted | Provider bridge rollout; Codex declares retry guidance without replaying a partial batch |
| Application lifecycle | `android-emulator-mcp`: app launch/stop/relaunch controls in `src/tools.rs` and session helpers | `default:w4294`/`default:w4337` own environment recovery, not per-app lifecycle — **new-gap** | `default:w10353`: lifecycle operations are explicit, target-bound actions with a fresh state receipt | Whole lifecycle receipt names previous and resulting app state; no implicit restart | Provider capability advertisement and recovery interaction review |
| Gestures | `android-emulator-mcp`: input execution and provider capabilities; Codex capability gate | `default:w4242` computer-style action bridge — **extend-owner** | `default:w10354`: multi-touch/scroll vocabulary is advertised and atomically executed or explicitly unsupported | One gesture receipt proves capability use; no decomposition into unrelated single-touch actions | Provider capability manifest, then Codex native tool projection |
| Evidence recording | `android-emulator-mcp`: artifact/receipt writing; Solar: hosted consumer evidence | `default:w4283` hosted acceptance and `default:w4398` provider pin — **extend-owner** | `default:w10355`: evidence manifest joins target, revisions, build, observations, outcomes, and digests | Entire manifest verifies referenced identity and artifact hashes | Hosted stock-app and Solar flows publish the immutable review bundle |
| Codex projection | Codex: Android tool schema, protocol, app-server, TUI, transcript | `default:w4422` provider contract, `default:w4432` capability registry, and `default:w4434` observation/action traceability — **extend-owner** | `default:w10356`: v1 fields project through native Android tools without changing provider ownership | Whole `ComputerUse` request/response/transcript item preserves target, receipt, error, and native image semantics | Codex PR validation and backward-compatible native-tool rollout |

## Unassigned and overlap checks

The matrix has eleven required behaviors and eleven assigned rows. Every row
has one successor, repository authority, existing-owner disposition, natural
assertion boundary, and rollout boundary; therefore the unassigned-requirement
count is **zero**.

The invariants are intentionally non-overlapping:

- `w10347` establishes which target and build are being discussed; `w10348`
  only decides whether that resolved target is ready.
- `w10349` resolves the intended UI node; `w10350` proves the outcome after an
  action; `w10351` versions the digest of that resulting observation.
- `w10352` reports ordered action outcomes; `w10353` owns application process
  state; `w10354` owns a gesture's atomic input semantics.
- `w10355` records evidence without changing tool behavior; `w10356` projects
  the settled contract without defining provider runtime behavior; `w10357`
  adjudicates the exact integrated tree and owns no implementation field.

This yields an overlap count of **zero** at the invariant level. Shared code
paths are allowed; shared ownership of a contract field is not.

## Confirmed dependency order

The existing dependency edges already express the required serial order; no
graph repair is needed:

1. `default:w10347` exact session and provenance receipt
2. `default:w10348` readiness state machine
3. `default:w10349` selector normalization and diagnostics
4. `default:w10350` postconditions and stable-frame waits
5. `default:w10351` observed-generation UI digest
6. `default:w10352` partial-failure batch receipts
7. `default:w10353` explicit application lifecycle
8. `default:w10354` multi-touch and scroll gestures
9. `default:w10355` provenance-bound evidence bundle
10. `default:w10356` Codex native-tool projection
11. `default:w10357` exact-tree and hosted acceptance adjudication

Each successor may start only after its predecessor has accepted evidence. The
final adjudication consumes the exact promoted provider, Codex, and consumer
revisions; a green provider-only test, Codex-only test, or build artifact is
scoped evidence and not a substitute.

## Rejected ownership interpretations

- **Solar as generic connector owner — rejected.** Solar supplies consumer
  acceptance and the session pin, not the Android provider or Codex protocol.
- **Provider-defined Codex transcript schema — rejected.** The provider returns
  typed runtime receipts; Codex owns `ComputerUse` event and transcript shape.
- **Recovery equals application lifecycle — rejected.** `w4294` and `w4337`
  cover hosted capability rehydration. `w10353` covers deliberate lifecycle
  actions inside a resolved Android target.
- **Build installation alone proves provenance — rejected.** `w4387` exposes
  installation and `w4398` pins a provider revision; `w10347` still must bind
  the installed manifest and session receipt to the requested artifact.
- **The generic provider-contract tree is superseded — rejected.** `w4422`,
  `w4432`, and `w4434` retain adapter-neutral ownership. `w10356` is the
  Android v1 projection and compatibility consumer of that work.
- **A textual digest or artifact path is visual evidence — rejected.** A visual
  claim needs native `inputImage`; text and artifacts remain review receipts.

## Acceptance of this contract freeze

This freeze is complete when the matrix remains the only v1 owner map, all
eleven successor boundaries retain their unique invariant, and changes follow
the sequence above. It deliberately leaves implementation and acceptance proof
to the assigned successors.
