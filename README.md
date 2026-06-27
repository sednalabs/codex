# Codex Sedna

<p align="center">
  <img src="https://github.com/openai/codex/blob/main/.github/codex-cli-splash.png" alt="Codex CLI splash" width="80%" />
</p>

Codex Sedna is the SednaLabs downstream fork of Codex CLI. It stays close to
the upstream OpenAI Codex experience while shipping Sedna-owned release
artifacts, downstream validation policy, and fork-specific runtime behavior.

If you are looking for the upstream OpenAI distribution, IDE integrations, or
Codex Web, use the official [Codex documentation](https://developers.openai.com/codex)
and [chatgpt.com/codex](https://chatgpt.com/codex). The upstream
`npm install -g @openai/codex` and `brew install --cask codex` paths install
the OpenAI distribution, not Sedna-owned release artifacts.

## Install

Use the latest [sednalabs/codex GitHub Release](https://github.com/sednalabs/codex/releases)
for supported Codex Sedna binaries.

Current public Sedna releases publish one supported CLI archive:

- Linux `x86_64`: `codex-sedna-<version>-x86_64-unknown-linux-gnu.tar.gz`

The archive contains `codex` plus `codex-responses-api-proxy`. macOS, Windows,
Linux arm64, and other historical upstream targets remain in the source tree for
future re-enablement, but they are not currently supported Sedna release
targets.

To build from source, follow [Installing and building](./docs/install.md).

Run `codex` and select **Sign in with ChatGPT** to use Codex through your
ChatGPT plan. API-key usage is also supported with the upstream Codex auth
setup described in the official Codex docs.

## What Sedna Adds

Sedna keeps the fork intentionally narrow in hot upstream code and moves
downstream behavior behind explicit seams where possible. The main additions
are:

| Area                      | What changes in this fork                                                                                                                                                                         |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Release ownership         | Sedna publishes its own GitHub releases, version naming, and Linux x86_64 release policy.                                                                                                         |
| Validation                | Heavy validation and buildability checks are GitHub-first, with lane docs for targeted, frontier, and release workflows.                                                                          |
| Native computer use       | The open-source Sedna adapter layer promotes bare Android, browser, and desktop tool names to Codex-owned native computer-use tools with transcript, app-server, TUI, rollout, and trace support. |
| Browser use               | The repo includes a built-in Playwright browser provider for local Chrome/Chromium; command providers can supply signed-in Chrome, in-app-browser, remote, or hosted browser runtimes.            |
| Model-visible screenshots | Successful visual browser, Android, and desktop observations must return native image content to the model. Artifact paths are diagnostics, not the primary visual channel.                       |
| Agent orchestration       | Downstream carries blocking wait patterns, richer sub-agent inventory, usage/accounting projection, and continuity improvements for long-running local workflows.                                 |
| Runtime accounting        | The fork maintains first-party local usage/accounting surfaces so live CLI, TUI, and app-server views can explain active-thread and combined-session usage.                                       |
| MCP and config safety     | Downstream preserves safety controls around MCP configuration, OAuth fallback behavior, headless device login with dynamic client registration, approval memory, and related runtime guardrails.  |

For the full inventory, read [Downstream / fork notes](./docs/downstream.md)
and the [Downstream regression matrix](./docs/downstream-regression-matrix.md).

## Headless MCP Device Login

Codex Sedna supports the
[OAuth 2.0 Device Authorization Grant (RFC 8628)](https://datatracker.ietf.org/doc/html/rfc8628)
for configured HTTP MCP servers that advertise a device authorization endpoint
in their OAuth metadata. This is useful in SSH sessions, remote shells, CI
runners, and other headless environments where a localhost browser callback is
inconvenient.

```bash
codex mcp login <server-name> --device-auth
```

Codex performs dynamic client registration when the server supports it. If the
server does not support dynamic registration, configure the server entry with a
client ID using `codex mcp add --oauth-client-id <client-id>`.

During login, Codex prints the verification URL and user code from the
authorization server, then stores the resulting MCP credentials for the
configured server. MCP servers built with the
[`mcp-toolkit-rs`](https://github.com/sednalabs/mcp-toolkit-rs) hosted HTTP auth
surface can expose the metadata needed for this flow.

## Native Computer Use

Codex Sedna treats native computer use as a Codex-owned contract backed by
replaceable runtime providers.

Codex owns the model-facing tools, transcript events, app-server routing, TUI
projection, rollout persistence, and native image-output requirement. Providers
own browser sessions, Android sessions, desktop permissions, screenshots, UI
digests, input execution, and backend-specific setup.

The open-source implementation in this repository covers the Codex-side
computer-use adapter layer and the built-in Playwright browser provider. Native
macOS desktop providers, Windows in-app-browser shells, signed-in Chrome
extension providers, remote browsers, and Android device providers plug in
through documented provider seams instead of being hard-coded into Codex core.

The stable native tool surface includes:

- `browser_observe` and `browser_step`
- `android_observe`, `android_step`, and `android_install_build_from_run`
- `desktop_observe` and `desktop_step`

The browser bridge supports backend hints such as `auto`, `browser`, `chrome`,
`chromium`, and provider-declared backends such as `iab`. The built-in
Playwright provider can run headless or headed Chrome/Chromium and supports
bounded browser actions, fresh post-action screenshots, compact page metadata,
selector diagnostics, service-account navigation headers, optional audit
artifacts, and per-thread browser profile isolation. External command providers
can implement signed-in Chrome, in-app-browser, remote-browser, or hosted
browser runtimes behind the same request/response contract.

Read [Native computer-use adapter tooling](./docs/native-computer-use.md) for
the runtime contract and [Native computer-use cleanroom contracts](./docs/native-computer-use-cleanroom.md)
for the sanitized provider requirements used for macOS desktop, Windows/browser
shell, Chrome-extension, bundled-plugin, and Android provider work.

## Working With The Fork

This repository has two public branch roles:

- `origin/main`: maintained downstream branch and public PR target.
- `origin/upstream-main`: fast-forward mirror of `upstream/main`.

Completed work on a feature, bugfix, docs, or cleanup branch should be
committed, pushed, and opened as a pull request targeting `origin/main` before
handoff. Do not leave finished branch work only in a local worktree.

Downstream syncs are merge-based from `upstream-main` into `main`. The intended
long-term shape is minimal fork carry in high-churn upstream files, with
downstream behavior expressed through narrow extension seams or provider
boundaries when possible. Some carry is deliberate product behavior rather than
temporary merge residue; the carry ledger and divergence registry mark those
surfaces so sync work preserves them until upstream offers an equivalent
contract.

Release and validation work is intentionally GitHub-first. Public release
artifacts are produced by Sedna release workflows; preview and validation runs
are buildability and regression evidence, not release artifacts.

## Documentation Map

| Need                                                        | Start here                                                                                                                      |
| ----------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| Fork identity, branch policy, and divergence inventory      | [Downstream / fork notes](./docs/downstream.md)                                                                                 |
| Release naming, release artifacts, and supported targets    | [Sedna release policy](./docs/sedna-release.md)                                                                                 |
| Native browser, Android, and desktop computer-use contracts | [Native computer-use adapter tooling](./docs/native-computer-use.md)                                                            |
| Cleanroom provider requirements                             | [Native computer-use cleanroom contracts](./docs/native-computer-use-cleanroom.md)                                              |
| GitHub-first validation lanes and remote offload            | [GitHub CI offload](./docs/github-ci-offload.md)                                                                                |
| Multi-step validation workflow                              | [Validation workflow](./docs/validation_workflow.md)                                                                            |
| Downstream tool and regression coverage                     | [Tool surface matrix](./docs/downstream-tool-surface-matrix.md) and [Regression matrix](./docs/downstream-regression-matrix.md) |
| Installing or building from source                          | [Installing and building](./docs/install.md)                                                                                    |
| Local memories behavior                                     | [Memories](./docs/memories.md)                                                                                                  |
| Contributing guidelines                                     | [Contributing](./docs/contributing.md)                                                                                          |

## License

This repository is licensed under the [Apache-2.0 License](LICENSE).
