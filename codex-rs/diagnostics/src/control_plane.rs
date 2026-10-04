//! Bounded content-free control-plane observation and shared reduction.
mod recorder;
mod summary;
mod types;

pub use recorder::ControlPlaneRecorder;
pub use summary::{
    ExternalDuration, QueueTimeline, SchedulerTimeline, SleepTimeline, Summary, WaitTimeline,
};
pub use types::*;
