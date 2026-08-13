# Codex Sedna

**A maintained downstream of OpenAI Codex for long-running, observable agent work.**

[Releases](https://github.com/sednalabs/codex/releases) ·
[Downstream notes](./docs/downstream.md) ·
[Native computer use](./docs/native-computer-use.md) ·
[Build from source](./docs/install.md)

**Codex Sedna** is the [Sedna Labs](https://github.com/sednalabs) downstream distribution of [OpenAI Codex CLI](https://github.com/openai/codex).

It stays close to upstream Codex while carrying runtime behaviour for work that is long-running, multi-agent, unattended, remote, or otherwise benefits from stronger continuity and runtime evidence.

That includes richer agent lifecycle and orchestration, child model and reasoning selection, bounded recovery, first-party usage and cost evidence, native computer use, headless MCP operation, and additional operator-facing runtime information.

> [!IMPORTANT]
> Codex Sedna is an independent downstream distribution and is not an official OpenAI Codex release.
>
> For stock Codex, official IDE integrations, Codex App, or Codex Web, use the [official Codex documentation](https://developers.openai.com/codex) and [chatgpt.com/codex](https://chatgpt.com/codex).

## Why this fork exists

Codex Sedna grew out of Sedna Labs' internal work on control planes for agent tasks whose logical lifetime extends beyond one interactive session.

That work treats the task as durable while individual agents, processes, and execution environments are replaceable. It exposed runtime requirements around continuity, coordination, recovery, model selection, runtime evidence, and computer use.

Codex Sedna contains the Codex-side changes we maintain in response to those requirements.

Here, long-running describes the logical task. It may span days or weeks across multiple turns, context windows, agents, and processes. It does not mean one `codex` process or model call remains alive throughout.

The higher-level software remains internal to Sedna Labs and is not required to use this fork.

## Should I use Sedna?

Codex Sedna is useful if you want Codex with stronger support for:

- long-running local sessions and agent trees;
- richer sub-agent identity, lifecycle, and coordination;
- child model and reasoning-effort selection;
- blocking and conditional waits;
- persisted agent continuity and resumed work;
- browser, Android, or desktop computer use;
- runtime usage and cost evidence across agent lineage;
- operator-facing usage and quota-pacing information;
- headless or remote MCP workflows;
- bounded recovery from selected transient interruptions;
- explicit tracking and regression protection for differences from upstream.

If you mainly want the standard Codex CLI experience, upstream Codex is usually the simpler choice.

## Install

Official Codex Sedna releases currently support **Linux x86_64**.

Download the latest supported release from [GitHub Releases](https://github.com/sednalabs/codex/releases).

The archive is named:

```text
codex-sedna-<version>-x86_64-unknown-linux-gnu.tar.gz
```

It contains:

```text
codex
codex-responses-api-proxy
```

Extract the archive and run:

```bash
./codex
```

You can then select **Sign in with ChatGPT** to use Codex through your ChatGPT plan. API-key authentication is also supported through the normal Codex authentication flow.

To build from source, see [Installing and building](./docs/install.md).

> [!NOTE]
> OpenAI's npm package, Homebrew package, installers, and official release artifacts install upstream Codex, not Codex Sedna.
>
> Manually dispatched macOS preview artifacts may also be available. These are ad hoc signed, non-notarized previews and are not part of the current supported Sedna release contract.

## What Sedna changes

### Long-running and multi-agent execution

Sedna carries additional runtime behaviour for sessions and agent trees that cannot assume every participating agent will remain continuously resident until the work is finished.

That includes stronger support around:

- agent identity and inventory;
- child model and reasoning configuration;
- agent-tree inspection;
- blocking waits and joins;
- mailbox-based coordination;
- persisted descendant continuity;
- residency, unload, and reload behaviour;
- background terminal completion;
- resumed sessions and agent history.

Within the models supported by the active provider and runtime, orchestrators can select child models and reasoning effort instead of forcing one configuration across the entire tree.

Requested and effective agent configuration is exposed through runtime surfaces where the available evidence supports it.

This allows stronger models and higher reasoning budgets to be used where they materially improve the result, while bounded roles such as waiting, monitoring, or narrow inspection can use lighter execution where appropriate.

The result is a runtime that is easier to inspect and recover when execution changes underneath long-lived work.

### Bounded recovery

Long-running work should not be discarded merely because a recoverable dependency or client boundary briefly fails.

At selected, explicitly classified boundaries, Codex Sedna can reconnect, retry, or continue within fixed limits while preserving the surrounding thread and working state.

This is not a policy of retrying everything.

Recovery remains specific to the operation and failure class. Ambiguous, mutating, or authority-sensitive operations do not acquire generic replay behaviour merely in the name of resilience.

Where the runtime cannot establish that recovery is safe, it stops rather than guessing.

### Usage, lineage, and cost

Sedna treats resource consumption as runtime evidence rather than only as an invoice received after the work is finished.

The fork maintains a first-party local usage ledger in downstream-owned storage.

Depending on the evidence available from the provider and runtime, the ledger can retain information such as:

- thread and agent lineage;
- requested model identity;
- provider-observed model identity;
- provider and service-tier context;
- token usage;
- prompt-cache write usage;
- Fast-mode evidence;
- agent spawn relationships;
- tool and runtime activity;
- versioned Codex credit estimates.

Configured identity and provider-observed identity remain distinct where the available evidence supports that distinction.

This matters because the cost of an orchestration strategy is rarely visible from the root agent alone.

One task might be completed by a single strong model. Another might use a coordinating model, several specialist children, lighter waiting agents, and parallel review. Retries, compaction, rejected attempts, and replacement execution can change the economics again.

Sedna therefore focuses on the **cost of meeting a fixed acceptance bar**.

A cheaper execution strategy that creates more failures, more review work, or an unacceptable implementation is not necessarily cheaper. A stronger model used for work a smaller model can reliably perform is unnecessary expense.

The usage ledger and lineage surfaces provide evidence for comparing those choices. They do not claim to choose the optimal orchestration strategy automatically.

Where evidence or rate information is incomplete, accounting should preserve that uncertainty rather than silently invent precision.

The same information is useful to an individual operator. Downstream TUI surfaces include richer agent identity and model information, agent-tree usage views, and a weekly quota-pacing status-line indicator.

### Native computer use

Codex Sedna gives the open-source Codex runtime first-class model-facing contracts for browser, Android, and desktop interaction.

The native tool surface includes:

```text
browser_observe
browser_step

android_observe
android_step
android_install_build_from_run

desktop_observe
desktop_step
```

Codex owns the model-facing contract:

- tool schemas;
- transcript events;
- app-server routing;
- TUI projection;
- rollout persistence;
- tracing;
- native image results.

Runtime providers own the environment-specific implementation:

- browser or device sessions;
- screenshots and viewport capture;
- UI inspection;
- input execution;
- emulator or desktop setup;
- backend lifecycle and permissions.

A successful visual observation returns **actual image content to the model**.

Screenshot paths, UI hierarchy files, logs, and other artifacts may be useful evidence, but they are not substitutes for model-visible pixels.

The repository includes a built-in Playwright browser provider for local Chrome and Chromium. External providers can implement the same Codex contract for other browser, Android, and desktop environments.

Provider availability depends on the operator's configuration. A model-facing contract in the source tree does not imply that Sedna publishes a runtime provider for every platform.

See [Native computer-use adapter tooling](./docs/native-computer-use.md), the [computer-use cleanroom contracts](./docs/native-computer-use-cleanroom.md), and the [tool surface matrix](./docs/downstream-tool-surface-matrix.md).

### Headless and remote MCP operation

Sedna carries additional MCP behaviour for environments where Codex cannot assume an interactive browser or a perfectly stable local connection.

For compatible HTTP MCP servers, Codex Sedna supports OAuth 2.0 Device Authorization Grant:

```bash
codex mcp login <server-name> --device-auth
```

Codex can perform dynamic client registration when the authorization server supports it.

Otherwise an explicit client ID can be configured:

```bash
codex mcp add <server-name> --oauth-client-id <client-id>
```

The fork also carries downstream behaviour around MCP configuration, bounded recovery, partial catalogue availability, approvals, and remote or headless operation.

## Maintaining the downstream

Sedna is intended to remain a downstream of Codex, not gradually turn into an unrelated codebase.

The public branches have distinct roles:

```text
openai/codex main
        │
        ▼
sednalabs/codex upstream-main
        │
        │ merge
        ▼
sednalabs/codex main
```

- **`upstream-main`** is the fast-forward mirror of upstream `openai/codex`.
- **`main`** is the maintained Sedna downstream and normal pull-request target.

Upstream synchronisation is merge-based.

The maintenance rule is to prefer upstream behaviour wherever it meets the required contract, and to retain downstream code only where it does not.

Where upstream implements an equivalent capability, downstream behaviour should normally converge on it rather than preserve a competing implementation indefinitely.

Where a difference remains necessary, Sedna tries to keep it behind a narrow extension seam, provider boundary, runtime capability, or downstream-owned storage surface rather than spreading it through high-churn upstream code.

### Tracked downstream differences

A maintained fork needs to be able to explain why it differs from upstream.

The machine-readable [divergence registry](./docs/divergences/index.yaml) records live downstream behaviour together with information such as:

- the behaviour being carried;
- affected surfaces and files;
- whether an upstream equivalent exists;
- downstream ownership;
- regression coverage;
- hotspot files;
- the expected extraction or upstreaming direction.

The [Downstream regression matrix](./docs/downstream-regression-matrix.md) maps those differences to focused validation lanes.

The registry also helps identify downstream code that can be removed when upstream gains an equivalent capability.

## Validation

Sedna uses GitHub-hosted validation heavily, particularly for downstream seams vulnerable to upstream drift.

Focused validation exists around areas including:

- multi-agent lifecycle and orchestration;
- child-model and reasoning surfaces;
- blocking and terminal waits;
- agent usage totals and quota pacing;
- usage-ledger behaviour;
- persisted agent state and lineage;
- MCP safety and recovery;
- native computer use and image propagation;
- app-server protocol extensions;
- downstream divergence consistency;
- release buildability.

Heavy or unusual validation can run through targeted hosted lanes rather than making every change execute every expensive repository-wide test.

See the [Validation workflow](./docs/validation_workflow.md), [GitHub CI offload](./docs/github-ci-offload.md), and [Downstream regression matrix](./docs/downstream-regression-matrix.md).

## Releases and provenance

Sedna publishes its own artifacts so downstream binaries remain clearly distinguishable from OpenAI releases.

Release versions retain the upstream track while adding Sedna identity, for example:

```text
v0.119.0-sedna.2
v0.126.0-alpha.5-sedna.1+upstream.1
```

Official release metadata records the upstream reference point and exact downstream commit used for the build.

The current release process includes:

- Linux `x86_64` as the supported public Sedna target;
- Sedna-specific version and artifact naming;
- exact upstream and downstream provenance metadata;
- SHA-256 checksums;
- keyless Sigstore signing;
- separate preview, validation, and official-release paths.

Official releases are produced by Sedna-owned GitHub workflows rather than reusing upstream OpenAI release artifacts.

See [Sedna release policy](./docs/sedna-release.md).

## Scope

Codex Sedna is an agent execution runtime, not a complete managed long-horizon agent platform.

It can be used directly as a CLI or beneath a higher-level orchestration system.

Persistence is a **runtime design objective, not a hosted durability guarantee**. Deployment, host availability, external scheduling, fleet supervision, cross-host failover, and higher-level recovery remain operator concerns unless another system supplies them.

Support in the source tree also does not imply a supported Sedna binary or runtime provider for every platform.

## Documentation

| Topic | Start here |
| --- | --- |
| Fork policy and upstream relationship | [Downstream / fork notes](./docs/downstream.md) |
| Live downstream differences | [Divergence registry](./docs/divergences/index.yaml) |
| Downstream and upstream tool surfaces | [Tool surface matrix](./docs/downstream-tool-surface-matrix.md) |
| Regression ownership | [Regression matrix](./docs/downstream-regression-matrix.md) |
| Browser, Android, and desktop computer use | [Native computer use](./docs/native-computer-use.md) |
| Provider implementation boundaries | [Computer-use cleanroom contracts](./docs/native-computer-use-cleanroom.md) |
| Releases and provenance | [Sedna release policy](./docs/sedna-release.md) |
| Validation | [Validation workflow](./docs/validation_workflow.md) |
| Installing and building | [Install](./docs/install.md) |
| Local memories | [Memories](./docs/memories.md) |
| Contributing | [Contributing](./docs/contributing.md) |

## Contributing

Normal feature, bugfix, documentation, and cleanup work should branch from `main` and return through a pull request targeting `main`.

Changes to intentional downstream behaviour should update the relevant divergence and regression records where applicable.

See [Contributing](./docs/contributing.md).

## License

Codex Sedna is licensed under the [Apache-2.0 License](LICENSE).
