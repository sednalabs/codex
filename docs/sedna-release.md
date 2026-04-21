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
- Sedna public tags stay human-readable and monotonic. Exact upstream provenance is recorded in
  release metadata instead of being overloaded into the public tag.
- Artifact names include `sedna` so they are not confused with upstream binaries
- Release builds embed `CODEX_RELEASE_VERSION` as the canonical SemVer and add a compact
  provenance label to `codex --version`
- Release artifacts include both `RELEASE-METADATA.txt` and `RELEASE-METADATA.json` with:
  `upstream_track`, `upstream_base_commit`, `upstream_base_tag` when exact, `downstream_commit`,
  and the compact `build_provenance` / `version_display` strings
- Linux `x86_64` is the only supported Sedna release target today. Other upstream platform
  packaging remains parked in the repository and may be revived later, but it is not part of the
  current downstream release contract.

The upstream track is the release line this fork is closest to. It does not need to be numerically
greater than upstream, and it does not claim that Sedna is synced to the latest upstream prerelease
tag on that line.

### GitHub Actions

Use the `sedna-release` workflow for fork-owned GitHub releases.

- Push a tag like `v0.119.0-sedna.2` to publish immediately
- Or run `sedna-release` manually with a `release_tag` input to build from the selected ref and let
  GitHub create the tag/release for that commit
- If you want a GitHub prerelease, use the workflow-dispatch `prerelease=true` input. The public
  Sedna tag itself stays on the plain downstream release line.

Current workflow characteristics:

- GitHub-hosted Linux `x86_64` release build
- Keyless Sigstore signing for Linux binaries
- GitHub Release assets named with the Sedna release identity
- Exact upstream/downstream provenance recorded in release metadata assets
- No dependency on upstream runner groups or upstream release tags

### Branch artifacts and heavy validation

- `validation-lab` is the default remote-first surface for scratch refs, integration refs,
  orphan-branch experiments, and targeted heavy validation that should not pollute ordinary PR
  status surfaces
- `validation-lab` `profile=targeted` with `lane_set=release` is the preferred early Linux
  release-build smoke path when the question is dependency or lockfile drift under
  `cargo build --locked`
- the concrete preflight lane is `sedna.release-linux-smoke`; keep that path separate from
  official release publication so operators can prove a ref is releasable without mutating
  GitHub Releases
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
- GitHub prereleases are intentionally opt-in and are not the updater's default candidate path
- Local non-release builds may still show the workspace placeholder version when
  `CODEX_RELEASE_VERSION` is not set; published releases should come from CI so the embedded release
  metadata is consistent
