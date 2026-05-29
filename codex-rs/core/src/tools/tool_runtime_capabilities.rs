//! Downstream-owned tool runtime capability declarations.
//!
//! The core tool handlers stay wired to narrow typed capabilities so recurring
//! downstream behavior can be audited or extracted without scattering policy
//! checks through every dispatch path.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnifiedExecBlockingWaitCapability {
    pub(crate) max_terminal_wait_ms: u64,
    pub(crate) heartbeat_interval: bool,
}

impl UnifiedExecBlockingWaitCapability {
    pub(crate) const fn downstream_default() -> Self {
        Self {
            max_terminal_wait_ms: 2 * 60 * 60 * 1_000,
            heartbeat_interval: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalOutcomeCapability {
    pub(crate) preserve_completed_lifecycle_after_cancellation: bool,
}

impl TerminalOutcomeCapability {
    pub(crate) const fn downstream_default() -> Self {
        Self {
            preserve_completed_lifecycle_after_cancellation: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubagentInventoryCapability {
    pub(crate) include_active_descendants: bool,
    pub(crate) inspect_tree: bool,
}

impl SubagentInventoryCapability {
    pub(crate) const fn downstream_default() -> Self {
        Self {
            include_active_descendants: true,
            inspect_tree: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WaitAgentCapability {
    pub(crate) return_when: bool,
    pub(crate) pending_ids: bool,
    pub(crate) completion_reason: bool,
    pub(crate) mailbox_wake: bool,
}

impl WaitAgentCapability {
    pub(crate) const fn downstream_default() -> Self {
        Self {
            return_when: true,
            pending_ids: true,
            completion_reason: true,
            mailbox_wake: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ToolRuntimeCapabilities {
    pub(crate) unified_exec_blocking_waits: Option<UnifiedExecBlockingWaitCapability>,
    pub(crate) terminal_outcome: Option<TerminalOutcomeCapability>,
    pub(crate) subagent_inventory: Option<SubagentInventoryCapability>,
    pub(crate) wait_agent: Option<WaitAgentCapability>,
}

impl ToolRuntimeCapabilities {
    pub(crate) const fn upstream_default() -> Self {
        Self {
            unified_exec_blocking_waits: None,
            terminal_outcome: None,
            subagent_inventory: None,
            wait_agent: None,
        }
    }

    pub(crate) const fn downstream_default() -> Self {
        Self {
            unified_exec_blocking_waits: Some(
                UnifiedExecBlockingWaitCapability::downstream_default(),
            ),
            terminal_outcome: Some(TerminalOutcomeCapability::downstream_default()),
            subagent_inventory: Some(SubagentInventoryCapability::downstream_default()),
            wait_agent: Some(WaitAgentCapability::downstream_default()),
        }
    }

    fn merge(mut self, other: Self) -> Self {
        self.unified_exec_blocking_waits = match (
            self.unified_exec_blocking_waits,
            other.unified_exec_blocking_waits,
        ) {
            (Some(left), Some(right)) => Some(UnifiedExecBlockingWaitCapability {
                max_terminal_wait_ms: left.max_terminal_wait_ms.max(right.max_terminal_wait_ms),
                heartbeat_interval: left.heartbeat_interval || right.heartbeat_interval,
            }),
            (Some(capability), None) | (None, Some(capability)) => Some(capability),
            (None, None) => None,
        };
        self.terminal_outcome = match (self.terminal_outcome, other.terminal_outcome) {
            (Some(left), Some(right)) => Some(TerminalOutcomeCapability {
                preserve_completed_lifecycle_after_cancellation: left
                    .preserve_completed_lifecycle_after_cancellation
                    || right.preserve_completed_lifecycle_after_cancellation,
            }),
            (Some(capability), None) | (None, Some(capability)) => Some(capability),
            (None, None) => None,
        };
        self.subagent_inventory = match (self.subagent_inventory, other.subagent_inventory) {
            (Some(left), Some(right)) => Some(SubagentInventoryCapability {
                include_active_descendants: left.include_active_descendants
                    || right.include_active_descendants,
                inspect_tree: left.inspect_tree || right.inspect_tree,
            }),
            (Some(capability), None) | (None, Some(capability)) => Some(capability),
            (None, None) => None,
        };
        self.wait_agent = match (self.wait_agent, other.wait_agent) {
            (Some(left), Some(right)) => Some(WaitAgentCapability {
                return_when: left.return_when || right.return_when,
                pending_ids: left.pending_ids || right.pending_ids,
                completion_reason: left.completion_reason || right.completion_reason,
                mailbox_wake: left.mailbox_wake || right.mailbox_wake,
            }),
            (Some(capability), None) | (None, Some(capability)) => Some(capability),
            (None, None) => None,
        };
        self
    }
}

pub(crate) trait ToolRuntimeCapabilityProvider: Sync {
    fn capabilities(&self) -> ToolRuntimeCapabilities;
}

struct DownstreamToolRuntimeCapabilityProvider;

impl ToolRuntimeCapabilityProvider for DownstreamToolRuntimeCapabilityProvider {
    fn capabilities(&self) -> ToolRuntimeCapabilities {
        ToolRuntimeCapabilities::downstream_default()
    }
}

static DOWNSTREAM_TOOL_RUNTIME_CAPABILITY_PROVIDER: DownstreamToolRuntimeCapabilityProvider =
    DownstreamToolRuntimeCapabilityProvider;
static TOOL_RUNTIME_CAPABILITY_PROVIDERS: &[&dyn ToolRuntimeCapabilityProvider] =
    &[&DOWNSTREAM_TOOL_RUNTIME_CAPABILITY_PROVIDER];

pub(crate) fn registered_tool_runtime_capabilities() -> ToolRuntimeCapabilities {
    merge_tool_runtime_capabilities(TOOL_RUNTIME_CAPABILITY_PROVIDERS)
}

fn merge_tool_runtime_capabilities(
    providers: &[&dyn ToolRuntimeCapabilityProvider],
) -> ToolRuntimeCapabilities {
    providers.iter().fold(
        ToolRuntimeCapabilities::upstream_default(),
        |capabilities, provider| capabilities.merge(provider.capabilities()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestProvider(ToolRuntimeCapabilities);

    impl ToolRuntimeCapabilityProvider for TestProvider {
        fn capabilities(&self) -> ToolRuntimeCapabilities {
            self.0
        }
    }

    #[test]
    fn upstream_default_disables_downstream_runtime_surfaces() {
        assert_eq!(
            ToolRuntimeCapabilities::upstream_default(),
            ToolRuntimeCapabilities {
                unified_exec_blocking_waits: None,
                terminal_outcome: None,
                subagent_inventory: None,
                wait_agent: None,
            }
        );
    }

    #[test]
    fn registered_downstream_provider_declares_runtime_capabilities() {
        let capabilities = registered_tool_runtime_capabilities();

        assert_eq!(
            capabilities.unified_exec_blocking_waits,
            Some(UnifiedExecBlockingWaitCapability::downstream_default())
        );
        assert_eq!(
            capabilities.terminal_outcome,
            Some(TerminalOutcomeCapability::downstream_default())
        );
        assert_eq!(
            capabilities.subagent_inventory,
            Some(SubagentInventoryCapability::downstream_default())
        );
        assert_eq!(
            capabilities.wait_agent,
            Some(WaitAgentCapability::downstream_default())
        );
    }

    #[test]
    fn merging_providers_combines_capability_flags() {
        let first = TestProvider(ToolRuntimeCapabilities {
            unified_exec_blocking_waits: Some(UnifiedExecBlockingWaitCapability {
                max_terminal_wait_ms: 1_000,
                heartbeat_interval: false,
            }),
            ..ToolRuntimeCapabilities::upstream_default()
        });
        let second = TestProvider(ToolRuntimeCapabilities {
            unified_exec_blocking_waits: Some(UnifiedExecBlockingWaitCapability {
                max_terminal_wait_ms: 2_000,
                heartbeat_interval: true,
            }),
            wait_agent: Some(WaitAgentCapability {
                return_when: true,
                pending_ids: false,
                completion_reason: false,
                mailbox_wake: false,
            }),
            ..ToolRuntimeCapabilities::upstream_default()
        });

        let merged = merge_tool_runtime_capabilities(&[&first, &second]);

        assert_eq!(
            merged.unified_exec_blocking_waits,
            Some(UnifiedExecBlockingWaitCapability {
                max_terminal_wait_ms: 2_000,
                heartbeat_interval: true,
            })
        );
        assert_eq!(
            merged.wait_agent,
            Some(WaitAgentCapability {
                return_when: true,
                pending_ids: false,
                completion_reason: false,
                mailbox_wake: false,
            })
        );
    }
}
