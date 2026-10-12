//! Gateway: composes the Tool Registry, Metadata Cache, and one Supervisor
//! per upstream into a single `tools/list` / `tools/call` entry point.

use std::collections::HashMap;
use std::sync::RwLock;

use mcp_mesh_protocol::jsonrpc::{Id, Outcome};
use mcp_mesh_protocol::mcp::{Tool, ToolsCallParams, ToolsListResult, METHOD_TOOLS_CALL, METHOD_TOOLS_LIST};
use mcp_mesh_protocol::{Request, Response};

use crate::cache::MetadataCache;
use crate::registry::{NamespacedToolId, ToolRegistry};
use crate::supervisor::Supervisor;
use crate::UpstreamId;

/// Owns every upstream's `Supervisor`, and routes `tools/list`/`tools/call`
/// requests through the registry's namespacing and the metadata cache.
pub struct Gateway {
    supervisors: HashMap<UpstreamId, Supervisor>,
    registry: RwLock<ToolRegistry>,
    cache: MetadataCache,
}

impl Gateway {
    /// Takes ownership of already-spawned supervisors (one per upstream)
    /// and fetches each one's `tools/list` to populate the registry. An
    /// upstream whose initial fetch fails simply contributes no tools until
    /// a later refresh succeeds.
    pub async fn new(supervisors: HashMap<UpstreamId, Supervisor>) -> Self {
        let gateway = Self {
            supervisors,
            registry: RwLock::new(ToolRegistry::new()),
            cache: MetadataCache::new(),
        };
        let upstreams: Vec<UpstreamId> = gateway.supervisors.keys().cloned().collect();
        for upstream in upstreams {
            let _ = gateway.refresh_upstream_tools(&upstream).await;
        }
        gateway
    }

    /// Gracefully shuts down every upstream concurrently (each via its own
    /// SIGTERM-then-SIGKILL sequence) and waits for all of them to finish.
    pub async fn shutdown(self) {
        let tasks: Vec<_> = self
            .supervisors
            .into_values()
            .map(|supervisor| tokio::spawn(supervisor.shutdown()))
            .collect();
        for task in tasks {
            let _ = task.await;
        }
    }

    /// Re-fetches `upstream`'s tool list through the cache and updates the
    /// registry's namespaced view. Returns the fetch error, if any, so
    /// callers can log it; a failure leaves the previous tools in place.
    pub async fn refresh_upstream_tools(&self, upstream: &str) -> Result<(), String> {
        let supervisor = self
            .supervisors
            .get(upstream)
            .ok_or_else(|| format!("unknown upstream: {upstream}"))?;
        let tools = self
            .cache
            .get_or_fetch(upstream, || fetch_tools(supervisor))
            .await?;
        self.registry
            .write()
            .expect("registry lock is never poisoned")
            .set_upstream_tools(upstream.to_string(), tools);
        Ok(())
    }

    pub async fn handle_request(&self, request: Request) -> Response {
        match request.method.as_str() {
            METHOD_TOOLS_LIST => self.handle_tools_list(request.id),
            METHOD_TOOLS_CALL => self.handle_tools_call(request).await,
            other => Response::error(request.id, -32601, format!("method not found: {other}")),
        }
    }

    fn handle_tools_list(&self, id: Id) -> Response {
        let tools = self
            .registry
            .read()
            .expect("registry lock is never poisoned")
            .list();
        let result = ToolsListResult {
            tools,
            next_cursor: None,
        };
        Response::success(
            id,
            serde_json::to_value(result).expect("ToolsListResult always serializes"),
        )
    }

    async fn handle_tools_call(&self, request: Request) -> Response {
        let id = request.id;
        let params: ToolsCallParams = match request.params.and_then(|p| serde_json::from_value(p).ok()) {
            Some(params) => params,
            None => return Response::error(id, -32602, "invalid params for tools/call"),
        };

        let resolution = {
            let registry = self.registry.read().expect("registry lock is never poisoned");
            registry
                .resolve(&NamespacedToolId::from(params.name.clone()))
                .map(|(upstream, tool)| (upstream.to_string(), tool.name.clone()))
        };
        let Some((upstream, original_name)) = resolution else {
            return Response::error(id, -32602, format!("unknown tool: {}", params.name));
        };

        let Some(supervisor) = self.supervisors.get(&upstream) else {
            return Response::error(id, -32001, format!("upstream not running: {upstream}"));
        };

        let forwarded = ToolsCallParams {
            name: original_name,
            arguments: params.arguments,
        };
        let forwarded_value =
            serde_json::to_value(forwarded).expect("ToolsCallParams always serializes");

        match supervisor.call(METHOD_TOOLS_CALL, Some(forwarded_value)).await {
            Ok(outcome) => Response::from_outcome(id, outcome),
            Err(err) => Response::error(id, -32002, format!("upstream call failed: {err:?}")),
        }
    }
}

async fn fetch_tools(supervisor: &Supervisor) -> Result<Vec<Tool>, String> {
    let outcome = supervisor
        .call(METHOD_TOOLS_LIST, None)
        .await
        .map_err(|err| format!("{err:?}"))?;
    match outcome {
        Outcome::Success { result } => serde_json::from_value::<ToolsListResult>(result)
            .map(|parsed| parsed.tools)
            .map_err(|err| format!("malformed tools/list result: {err}")),
        Outcome::Error { error } => Err(format!("upstream tools/list error: {}", error.message)),
    }
}
