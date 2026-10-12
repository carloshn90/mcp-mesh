//! Core mesh logic: gateway, registry, policy, selection, guardrails, audit.

pub mod registry;

pub use registry::{NamespacedToolId, ToolRegistry, UpstreamId};
