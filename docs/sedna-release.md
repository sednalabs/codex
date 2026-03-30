## Sedna Release Policy

This fork keeps upstream version provenance visible while making published builds clearly distinct
from upstream OpenAI releases.

### Public topology

- Public repository owner: `SednaLabs`
- Public default branch: `main`
- Exact-upstream mirror branch: `upstream-main`

### Release identity

- Release tags use `v<upstream-base>-sedna.<n>`
- Example: `v0.117.0-sedna.1`
- Artifact names include `sedna` so they are not confused with upstream binaries
- Release builds embed `CODEX_RELEASE_VERSION` so UI and `codex --version` reflect the tagged fork
  release instead of the workspace placeholder version

The upstream base is the release line this fork is closest to. It does not need to be numerically
greater than upstream.

### GitHub Actions

Use the `sedna-release` workflow for fork-owned GitHub releases.

- Push a tag like `v0.117.0-sedna.1` to publish immediately
- Or run `sedna-release` manually with a `release_tag` input to build from the selected ref and let
  GitHub create the tag/release for that commit

Current workflow characteristics:

- GitHub-hosted Linux release build
- Keyless Sigstore signing for Linux binaries
- GitHub Release assets named with the Sedna release identity
- No dependency on upstream runner groups or upstream release tags

### Branch artifacts and heavy validation

- `validation-lab` is the default remote-first surface for scratch refs, integration refs,
  orphan-branch experiments, and targeted heavy validation that should not pollute ordinary PR
  status surfaces
- `sedna-branch-build` produces disposable preview binaries only when manually dispatched
- `sedna-heavy-tests` runs expensive remote validation without using the shared local machine as the
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
- Preview builds are intentionally opt-in rather than every-commit defaults
- GitHub-hosted branch builds remain useful when the actual question is preview artifact
  buildability
- GitHub-hosted release builds are the authoritative public release artifacts
- Local non-release builds may still show the workspace placeholder version when
  `CODEX_RELEASE_VERSION` is not set; published releases should come from CI so the embedded release
  metadata is consistent
