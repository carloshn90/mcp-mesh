//! Tool Registry: merges per-upstream tool lists into one namespaced view
//! using stable `<upstream>__<tool>` ids, and resolves those ids back for
//! `tools/call` routing.
//!
//! Assumes upstream ids never contain the `__` separator themselves; that
//! constraint belongs to config validation (not yet implemented), not here.

use std::collections::HashMap;
use std::fmt;

use mcp_mesh_protocol::mcp::Tool;

pub type UpstreamId = String;

const NAMESPACE_SEPARATOR: &str = "__";

/// A tool's stable, namespaced identifier: `<upstream>__<tool-name>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NamespacedToolId(String);

impl NamespacedToolId {
    pub fn new(upstream: &str, tool_name: &str) -> Self {
        Self(format!("{upstream}{NAMESPACE_SEPARATOR}{tool_name}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Splits back into `(upstream, tool_name)` on the first separator.
    fn split(&self) -> Option<(&str, &str)> {
        self.0.split_once(NAMESPACE_SEPARATOR)
    }
}

impl fmt::Display for NamespacedToolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for NamespacedToolId {
    fn from(raw: String) -> Self {
        Self(raw)
    }
}

impl From<&str> for NamespacedToolId {
    fn from(raw: &str) -> Self {
        Self(raw.to_string())
    }
}

/// In-memory, per-client-agnostic view of every tool across every upstream.
#[derive(Debug, Default)]
pub struct ToolRegistry {
    // upstream id -> original tool name -> Tool. Replaced wholesale per
    // upstream on each refresh (initial `tools/list` or `listChanged`).
    upstream_tools: HashMap<UpstreamId, HashMap<String, Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the full tool list for one upstream.
    pub fn set_upstream_tools(&mut self, upstream: UpstreamId, tools: Vec<Tool>) {
        let by_name = tools.into_iter().map(|t| (t.name.clone(), t)).collect();
        self.upstream_tools.insert(upstream, by_name);
    }

    pub fn remove_upstream(&mut self, upstream: &str) {
        self.upstream_tools.remove(upstream);
    }

    /// Resolves a namespaced id back to the upstream that owns it and its
    /// original (un-namespaced) tool definition.
    pub fn resolve(&self, id: &NamespacedToolId) -> Option<(&str, &Tool)> {
        let (upstream, tool_name) = id.split()?;
        let (upstream_key, tools) = self.upstream_tools.get_key_value(upstream)?;
        let tool = tools.get(tool_name)?;
        Some((upstream_key.as_str(), tool))
    }

    /// Lists every tool across every upstream, with `name` rewritten to its
    /// namespaced id, sorted by id for deterministic output.
    pub fn list(&self) -> Vec<Tool> {
        let mut tools: Vec<Tool> = self
            .upstream_tools
            .iter()
            .flat_map(|(upstream, by_name)| {
                by_name.values().map(move |tool| {
                    let mut namespaced = tool.clone();
                    namespaced.name = NamespacedToolId::new(upstream, &tool.name).to_string();
                    namespaced
                })
            })
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.to_string(),
            description: None,
            input_schema: json!({"type": "object"}),
        }
    }

    #[test]
    fn namespaced_id_is_stable_across_repeated_refreshes() {
        let mut registry = ToolRegistry::new();
        registry.set_upstream_tools("pg".to_string(), vec![tool("query")]);
        let first = registry.list();

        // Simulate a listChanged-triggered refresh that re-sends the same tool.
        registry.set_upstream_tools("pg".to_string(), vec![tool("query")]);
        let second = registry.list();

        assert_eq!(first, second);
        assert_eq!(first[0].name, "pg__query");
    }

    #[test]
    fn same_tool_name_on_different_upstreams_does_not_collide() {
        let mut registry = ToolRegistry::new();
        registry.set_upstream_tools("pg".to_string(), vec![tool("query")]);
        registry.set_upstream_tools("sqlite".to_string(), vec![tool("query")]);

        let names: Vec<String> = registry.list().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["pg__query", "sqlite__query"]);

        let pg_query = NamespacedToolId::new("pg", "query");
        let sqlite_query = NamespacedToolId::new("sqlite", "query");
        let (pg_upstream, _) = registry.resolve(&pg_query).unwrap();
        let (sqlite_upstream, _) = registry.resolve(&sqlite_query).unwrap();
        assert_eq!(pg_upstream, "pg");
        assert_eq!(sqlite_upstream, "sqlite");
    }

    #[test]
    fn list_merges_tools_across_every_upstream() {
        let mut registry = ToolRegistry::new();
        registry.set_upstream_tools("pg".to_string(), vec![tool("query"), tool("migrate")]);
        registry.set_upstream_tools("fs".to_string(), vec![tool("read")]);
        registry.set_upstream_tools("git".to_string(), vec![tool("commit")]);

        let names: Vec<String> = registry.list().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec!["fs__read", "git__commit", "pg__migrate", "pg__query"]
        );
    }

    #[test]
    fn resolve_round_trips_to_the_original_tool_definition() {
        let mut registry = ToolRegistry::new();
        registry.set_upstream_tools("fs".to_string(), vec![tool("read")]);

        let id = NamespacedToolId::new("fs", "read");
        let (upstream, resolved) = registry.resolve(&id).expect("tool should resolve");
        assert_eq!(upstream, "fs");
        assert_eq!(resolved.name, "read");
    }

    #[test]
    fn resolve_returns_none_for_unknown_upstream_or_tool() {
        let mut registry = ToolRegistry::new();
        registry.set_upstream_tools("fs".to_string(), vec![tool("read")]);

        assert!(registry.resolve(&NamespacedToolId::new("fs", "write")).is_none());
        assert!(registry.resolve(&NamespacedToolId::new("ghost", "read")).is_none());
        assert!(registry.resolve(&NamespacedToolId::from("no-separator")).is_none());
    }

    #[test]
    fn set_upstream_tools_replaces_the_previous_list_wholesale() {
        let mut registry = ToolRegistry::new();
        registry.set_upstream_tools("fs".to_string(), vec![tool("read"), tool("write")]);
        registry.set_upstream_tools("fs".to_string(), vec![tool("read")]);

        let names: Vec<String> = registry.list().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["fs__read"]);
        assert!(registry.resolve(&NamespacedToolId::new("fs", "write")).is_none());
    }

    #[test]
    fn remove_upstream_drops_its_tools_from_listing_and_resolution() {
        let mut registry = ToolRegistry::new();
        registry.set_upstream_tools("fs".to_string(), vec![tool("read")]);
        registry.remove_upstream("fs");

        assert!(registry.list().is_empty());
        assert!(registry.resolve(&NamespacedToolId::new("fs", "read")).is_none());
    }
}
