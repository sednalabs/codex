## Sedna Release Policy

This fork keeps upstream version provenance visible while making published builds clearly distinct
from upstream OpenAI releases.

### Public topology

- Recommended public repository owner: `SednaLabs`
- Recommended public default branch: `main`
- Optional exact-upstream mirror branch: `upstream-main`

The internal maintenance model can keep using `carry/main` while the public branch model is being
transitioned. The release contract below does not depend on that migration being complete.

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

### Local versus CI builds

- Local builds remain useful for development, targeted tests, and smoke checks
- GitHub-hosted release builds are the authoritative public release artifacts
- Local non-release builds may still show the workspace placeholder version when
  `CODEX_RELEASE_VERSION` is not set; published releases should come from CI so the embedded release
  metadata is consistent
