pub(crate) mod agent_resolver;
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
#[allow(dead_code, unused_imports)]
=======
pub(crate) mod child_config;
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
pub(crate) mod control;
pub(crate) mod goal_notifications;
mod lifecycle;
mod registry;
#[allow(dead_code, unused_imports)]
pub(crate) mod role;
pub(crate) mod status;

pub(crate) use codex_protocol::protocol::AgentStatus;
pub(crate) use control::AgentControl;
pub(crate) use registry::SpawnPublicationDecision;
pub(crate) use registry::exceeds_thread_spawn_depth_limit;
pub(crate) use registry::next_thread_spawn_depth;
pub(crate) use status::agent_status_from_event;
