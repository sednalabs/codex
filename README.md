<p align="center"><strong>Codex Sedna</strong> is the SednaLabs downstream fork of Codex CLI.
<p align="center">
  <img src="https://github.com/openai/codex/blob/main/.github/codex-cli-splash.png" alt="Codex CLI splash" width="80%" />
</p>
</br>
Codex Sedna keeps close to the upstream Codex experience while shipping Sedna-owned releases, downstream CI policy, and fork-specific operational behavior.
</br>Use <a href="https://github.com/sednalabs/codex/releases">sednalabs/codex releases</a> for supported binaries, and see <a href="./docs/downstream.md">downstream notes</a> plus <a href="./docs/sedna-release.md">Sedna release policy</a> for fork-specific workflow details.
</br>If you are looking for the upstream OpenAI distribution, IDE integrations, or Codex Web, use the official <a href="https://developers.openai.com/codex">Codex docs</a> and <a href="https://chatgpt.com/codex">chatgpt.com/codex</a>.</p>

---

## Codex Sedna identity

This repository publishes the Codex Sedna fork maintained by SednaLabs, not a lightly edited upstream mirror. We keep the openai/codex sources in sync, but Sedna controls the public release cadence, version naming (for example, `v0.119.0-sedna.2`), and downstream CI policy described in `docs/sedna-release.md` and `docs/github-ci-offload.md`. The sections under `docs/downstream.md` explain how we track `upstream/main`, protect the downstream branch, and manage fork-only additions.

---

## Quickstart

### Installing and running Codex Sedna

The supported Sedna distribution path is the latest GitHub Release from `sednalabs/codex`.

<details open>
<summary>Download a binary from the <a href="https://github.com/sednalabs/codex/releases">GitHub Releases page</a>.</summary>

Each GitHub Release currently contains one supported downstream CLI archive:

- Linux
  - x86_64: `codex-sedna-<version>-x86_64-unknown-linux-gnu.tar.gz`

The archive contains `codex` plus `codex-responses-api-proxy`.

macOS, Windows, Linux arm64, and other historical upstream targets remain in the source tree for future re-enablement, but they are not currently supported Sedna release targets.

</details>

If you prefer to build from source, follow [`docs/install.md`](./docs/install.md).

The upstream `npm install -g @openai/codex` and `brew install --cask codex` paths continue to point at the OpenAI distribution rather than Sedna-owned release artifacts.

### Using Codex with your ChatGPT plan

Run `codex` and select **Sign in with ChatGPT**. We recommend signing into your ChatGPT account to use Codex as part of your Plus, Pro, Business, Edu, or Enterprise plan. [Learn more about what's included in your ChatGPT plan](https://help.openai.com/en/articles/11369540-codex-in-chatgpt).

You can also use Codex with an API key, but this requires [additional setup](https://developers.openai.com/codex/auth#sign-in-with-an-api-key).

## Docs

- [**Sedna release policy**](./docs/sedna-release.md)
- [**Downstream / fork notes**](./docs/downstream.md)
- [**Native computer-use and Android tooling**](./docs/native-computer-use.md)
- [**Codex Documentation**](https://developers.openai.com/codex)
- [**Contributing**](./docs/contributing.md)
- [**Installing & building**](./docs/install.md)
- [**Open source fund**](./docs/open-source-fund.md)

This repository is licensed under the [Apache-2.0 License](LICENSE).
