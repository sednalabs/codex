# Native voice source inputs

This stage pins and prepares sources for a privately bundled, GStreamer-based
audio runtime, including its native dependencies and build tools. It does not
compile native libraries, link them into Codex or enable voice.

`sources.json` records the versions, URLs and SHA-256 digests of 11 archives:

| Purpose | Sources |
| --- | --- |
| GStreamer framework and plugin sources | `gstreamer`, `gst-plugins-base`, `gst-plugins-good` |
| Supporting native libraries | `glib`, `libffi`, `pcre2`, `zlib`, `proxy-libintl` |
| Audio codec | `opus` |
| Build tools, not runtime libraries | `meson`, `ninja` |

GLib also includes `gvdb` in its archive; it is recorded without a separate fetch.
These inputs do not include the complete platform toolchain for native builds.

Bazel uses standard `http_archive` rules to fetch, verify, unpack and cache the
archives from that manifest. Run from the repository root:

```sh
bazel build //third_party/voice:sources
```

For offline preparation without Bazel, use Python 3.12 or newer and an existing
archive directory whose filenames match the manifest:

```sh
python3 third_party/voice/prepare_sources.py --archives /path/to/archives --output /path/to/new-sources
python3 -m unittest discover -s third_party/voice -p 'test_*.py'
```

The adapter verifies archive digests and bounds, then extracts with Python's
`tarfile` data filter. It preserves links where supported and copies their archive
targets when link creation is unavailable, including on Windows. It refuses an
existing output directory and cleans up incomplete output. `prepared.json` records
successful preparation, not the integrity of later edits to the extracted tree.

Ordinary CLI builds do not run either path. The Bazel `:sources` target is
`manual`, so wildcard builds do not fetch these archives. `:source_inputs`
exports the manifest and adapter for standalone consumers; `:sources` exposes
extracted archives with Bazel build metadata. Neither target compiles libraries.

Checksums establish input identity, not security or license approval. Native
compilation, final Cargo/Bazel linking, installed packages, minimum OS support
and duplex audio validation remain separate stages. These inputs do not establish
a shared Opus build with Rust consumers or a reduced dependency count.
