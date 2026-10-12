//! Core mesh logic: gateway, registry, policy, selection, guardrails, audit.

pub mod cache;
pub mod gateway;
pub mod registry;
pub mod supervisor;

pub use cache::MetadataCache;
pub use gateway::Gateway;
pub use registry::{NamespacedToolId, ToolRegistry, UpstreamId};
pub use supervisor::{CallError, Supervisor, SupervisorConfig, SupervisorEvent, UpstreamSpec};
