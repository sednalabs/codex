## Sedna Release Policy

This fork keeps upstream version provenance visible while making published builds clearly distinct
from upstream OpenAI releases.

### Public topology

- Public repository owner: `sednalabs`
- Public default branch: `main`
- Exact-upstream mirror branch: `upstream-main`

### Release identity

- Release tags use `v<upstream-track>-sedna.<n>`
- Example: `v0.119.0-sedna.2`
- `scripts/resolve_sedna_release_version` is the authoritative version resolver for official
  releases. Humans mark release intent; the resolver chooses and validates the tag.
- Sedna public tags stay human-readable and monotonic. Exact upstream provenance is recorded in
  release metadata instead of being overloaded into the public tag.
- Artifact names include `sedna` so they are not confused with upstream binaries
- Release builds embed `CODEX_RELEASE_VERSION` as the canonical SemVer and add a compact
  provenance label to `codex --version`
- Release artifacts include both `RELEASE-METADATA.txt` and `RELEASE-METADATA.json` with:
  `version_policy`, `release_channel`, `release_marker`, `upstream_track`,
  `upstream_base_commit`, `upstream_base_tag`, `upstream_base_tag_exact`,
  `upstream_distance_from_tag`, `downstream_commit`, `target_commit`, and the compact
  `build_provenance` / `version_display` strings
- Linux `x86_64` is the only supported Sedna release target today. Other upstream platform
  packaging remains parked in the repository and may be revived later, but it is not part of the
  current downstream release contract.

The upstream track is resolved from the target commit's merge-base with `origin/upstream-main`.
That merge-base is the upstream reference point for the release, even if `origin/upstream-main`
has advanced by the time the release runs. The resolver chooses the newest well-formed
`rust-v<semver>` upstream tag whose tag timestamp is at or before that merge-base commit, and
malformed double-prefixed upstream tags are ignored. If the merge-base is ahead of the selected
upstream tag, the release metadata records the commit distance instead of pretending the base was
an exact upstream tag.

### GitHub Actions

Use the `sedna-release` workflow for fork-owned GitHub releases.

- Push to `main` with an exact commit trailer to request an automatic official release:
  - `Sedna-Release: stable`
  - `Sedna-Release: prerelease`
- Ordinary `main` pushes without a `Sedna-Release` trailer are a clean no-op in the release
  workflow.
- `Sedna-Release: stable` refuses upstream prerelease tracks such as `0.126.0-alpha.3`,
  publishes a full GitHub Release, and dispatches public asset verification for that exact tag.
- `Sedna-Release: prerelease` allows upstream prerelease tracks and publishes the GitHub Release as
  a prerelease. The release workflow dispatches asset verification with an explicit
  prerelease allowance for that exact tag.
- Pushing a tag like `v0.119.0-sedna.2` remains supported, but the workflow validates that the tag
  matches the resolver's computed version for the target commit before publishing.
- Manual `workflow_dispatch` accepts an optional `target_sha`, `channel`, and optional
  `release_tag`. If `release_tag` is supplied, it is an assertion checked against the resolver, not
  the source of truth.
- A supplied `release_tag` must match the upstream track computed from the target commit's
  merge-base. Supplying a tag from a newer upstream track fails instead of moving the release onto
  that newer track.
- Existing release tags are immutable in normal release flow. Rerolls use the next trailing
  `sedna.<n>` value rather than clobbering published assets.

Current workflow characteristics:

- GitHub-hosted Linux `x86_64` release build
- Cargo home and `sccache` restore/save around the official release build to reduce duplicate
  compilation when prior release smoke runs warmed matching caches
- Keyless Sigstore signing for Linux binaries
- GitHub Release assets named with the Sedna release identity
- Exact upstream/downstream provenance recorded in release metadata assets
- No dependency on upstream runner groups or upstream release tags

The resolver writes `version_policy=sedna-upstream-track-v1` into release metadata so future policy
changes can be detected explicitly instead of inferred from tag shape alone.

### Branch artifacts and heavy validation

- `validation-lab` is the default remote-first surface for scratch refs, integration refs,
  orphan-branch experiments, and targeted heavy validation that should not pollute ordinary PR
  status surfaces
- `validation-lab` `profile=targeted` with `lane_set=release` is the preferred early Linux
  release-build smoke path when the question is dependency or lockfile drift under
  `cargo build --locked`
- the concrete preflight lane is `sedna.release-linux-smoke`; it also runs as a runtime smoke gate
  for core-heavy PR validation so release-mode compile breaks are caught before an official
  release dispatch is the first full release build
- keep that path separate from official release publication so operators can prove a ref is
  releasable without mutating GitHub Releases
- release smoke runs may warm dependency and compiler caches for the official publisher, but
  `sedna-release` still performs the authoritative build, signing, metadata, checksum, and
  publication steps itself
- `sedna-branch-build` produces disposable preview binaries only when manually dispatched
- `sedna-heavy-tests` runs expensive remote validation without using the local development machine as the
  build factory
- branch artifacts retain for 3 days and are never updater candidates
- only `sedna-release` is allowed to publish official GitHub Releases
- The initial Sedna release lane publishes direct GitHub release binaries. The legacy npm-style
  installer packages and artifact-runtime assets remain upstream-hosted until Sedna reaches asset
  parity for those families.

### Local versus CI builds

- Local builds remain useful for development, targeted tests, and smoke checks
- `validation-lab` is the default offload path for seam-level remote validation and experimental
  sweeps
- When the question is "will the Linux release binary set still build with `--locked`?", prefer
  `validation-lab` `profile=targeted` with `lane_set=release` before escalating to artifact mode
  or `sedna-release`
- When the question is "publish an official release on GitHub Releases," skip `validation-lab`
  publication entirely and use `sedna-release`
- Preview builds are intentionally opt-in rather than every-commit defaults
- GitHub-hosted branch builds remain useful when the actual question is preview artifact
  buildability
- GitHub-hosted release builds are the authoritative public release artifacts
- GitHub prereleases are intentionally opt-in through the `Sedna-Release: prerelease` marker or
  manual prerelease channel and are not the updater's default candidate path
- Local non-release builds may still show the workspace placeholder version when
  `CODEX_RELEASE_VERSION` is not set; published releases should come from CI so the embedded release
  metadata is consistent

### Release install verification workflow

`sedna-release-install` verifies already published Sedna release assets on a GitHub-hosted
runner. It intentionally does not perform production installation, because public self-hosted
Actions logs can expose runner and machine identity before workflow-level masking is available.

- Official release verification is explicitly dispatched by `sedna-release` after publishing a
  non-draft GitHub Release. This avoids relying on implicit follow-on workflow events from the
  release publisher token.
- Manual `workflow_dispatch` runs require `dry_run=true`
- Prerelease installs require `allow_prerelease=true` on `workflow_dispatch`
- The verifier checks the tag shape, release metadata, `SHA256SUMS.txt`, and executable payload
- Production installs should be performed by private deployment infrastructure outside the public
  Actions log surface
- Drafts are not installed, and prereleases are refused unless an explicit dispatch allows them
