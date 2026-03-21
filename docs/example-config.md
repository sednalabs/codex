# Sample configuration

For a sample configuration file, see [this documentation](https://developers.openai.com/codex/config-sample).

## Memories

```toml
[memories]
generate_memories = true
use_memories = true
no_memories_if_mcp_or_web_search = false
max_rollout_age_days = 30
min_rollout_idle_hours = 6
max_rollouts_per_startup = 16
max_raw_memories_for_consolidation = 256
max_unused_days = 30
# extract_model and consolidation_model are unset by default.
```
