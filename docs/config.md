# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Client instruction overlays

`instruction_overlays` applies bounded, ordered literal edits to nine selected
client-owned instruction components in outgoing Responses request copies:
`model_baseline`, `review_baseline`, `developer_guidance`, `multi_agent_mode`,
`multi_agent_role`, `multi_agent_usage_hint`, `model_switch_guidance`,
`collaboration_guidance`, and `persistent_guidance`. Review requests can select
only `review_baseline`; other normal eligible requests can select the other
eight. The mode, model, effort, tools, feature flags, channels, and permissions
remain unchanged. Replacing or removing operational wording can therefore make
guidance inconsistent with the still-active runtime state; the operator owns
that text choice, and an overlay grants no runtime capability or authority.

Each component accepts an optional `on_absent` (`error` by default or `skip`),
`selection`, and an ordered `edits` array. An empty `edits` array disables an
inherited component plan. Operations are `prepend`, `append`, `replace`,
`replace_exact`, and `remove_exact`; exact operations require a non-empty
search literal and an `expected_matches` count. Operations are literal,
case-sensitive, and applied in order. `selection = "unique"` is the default:
zero targets follows `on_absent`, one is edited, and multiple targets fail
without sending a request. `selection = "all_classified"` explicitly edits
every well-formed instance of that component in the request copy; counts are
checked independently for each instance and any failure aborts the whole
request. It never infers a latest/current instance. Each request starts from
unchanged history, so append/prepend applies once per selected instance in that
request and does not accumulate in stored history.

`developer_guidance` selects client prose tagged
`generic.developer_instructions`, including configured developer guidance and
client-generated namespace-description updates. Additional tagged fragments
can make `unique` selection ambiguous; explicit `all_classified` edits each
instance, with exact-match counts checked independently per instance.

Limits are at most 64 operations, 32 KiB per literal, 256 KiB of literals
across selected components, 256 KiB per transformed component body, 64 selected
text instances, and 4 MiB combined final selected text per request. Output
growth is checked before expansion; oversized transformations fail closed.

For example:

```toml
[instruction_overlays.developer_guidance]
on_absent = "error"
edits = [{ op = "append", text = "\nUse the local test fixture." }]
```

Put the same table in `~/.codex/config.toml`, at top level in
`~/.codex/work.config.toml` (selected with `codex --profile work`), or in a
role's TOML configuration. A trusted enabled project may also define it in its
local configuration; project plans follow normal configuration-layer
precedence.
For a one-command override, `-c`/`--config` accepts the nested TOML value:

```sh
codex -c 'instruction_overlays.developer_guidance={on_absent="skip",selection="unique",edits=[{op="append",text="\nUse the local test fixture."}]}'
```

Literal search is case-sensitive and uses TOML-decoded text: for example,
`from = "first line\nsecond line"` searches for an actual newline. With
`all_classified`, `expected_matches` is checked independently in each selected
instance; if even one instance has a different count, the whole outgoing
request is rejected.

Configuration follows the existing enabled-layer and trusted-project boundary:
trusted enabled project layers may configure overlays, while disabled or
untrusted project layers remain excluded. Overlays apply only to eligible CLI,
VS Code, exec, MCP, and thread-spawn requests; review requests may select only
`review_baseline`. Internal, compact, memory-consolidation, custom, unknown,
and other non-eligible sources remain unchanged.

`model_baseline` and `model_switch_guidance` are independent. The latter
targets only the separately tagged `<model_switch>` transition fragment; a
baseline edit does not change it. Model-switch, collaboration, and persistent
components require their known wrappers. Saved history and world state are not
rewritten. A separate realtime transport path continues to use the existing
realtime-specific configuration knobs; this Responses setting does not edit
realtime developer instructions. Structured tool/schema/capability declarations
and actual runtime controls also remain unchanged. Managed, administrative,
security, Guardian, provider-enforced, and other unclassified
content is not selected by these components.

Configuration changes take effect when a new process/session reconstructs its
configuration; this is not a live-file watcher or a retroactive conversation
rewrite. To roll back, restore or remove the overlay setting at its configured
layer and start a new process/session. Synthetic request and configuration
tests verify selected client fragments; an instruction-display command is not
evidence that every overlay or final provider request was loaded and transformed.

## Lifecycle hooks

Admins can set top-level `allow_managed_hooks_only = true` in
`requirements.toml` to ignore user, project, and session hook configs while
still allowing managed hooks from requirements and managed config layers. This
setting is only supported in `requirements.toml`; putting it in `config.toml`
does not enable managed-hooks-only mode.
