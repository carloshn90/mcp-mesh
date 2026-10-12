//! Core mesh logic: gateway, registry, policy, selection, guardrails, audit.

pub mod cache;
pub mod registry;
pub mod supervisor;

pub use cache::MetadataCache;
pub use registry::{NamespacedToolId, ToolRegistry, UpstreamId};
pub use supervisor::{Supervisor, SupervisorConfig, SupervisorEvent, UpstreamSpec};
