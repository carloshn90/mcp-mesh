//! Core mesh logic: gateway, registry, policy, selection, guardrails, audit.

pub mod registry;
pub mod supervisor;

pub use registry::{NamespacedToolId, ToolRegistry, UpstreamId};
pub use supervisor::{Supervisor, SupervisorConfig, SupervisorEvent, UpstreamSpec};
