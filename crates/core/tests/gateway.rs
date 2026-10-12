use std::collections::HashMap;
use std::time::Duration;

use mcp_mesh_core::{Gateway, Supervisor, SupervisorConfig, SupervisorEvent, UpstreamSpec};
use mcp_mesh_protocol::jsonrpc::{Id, Outcome, Request, V2};
use mcp_mesh_protocol::mcp::{ToolsCallParams, ToolsListResult, METHOD_TOOLS_CALL, METHOD_TOOLS_LIST};

fn fixture_spec(label: &str) -> UpstreamSpec {
    UpstreamSpec {
        id: label.to_string(),
        command: env!("CARGO_BIN_EXE_echo_server").to_string(),
        args: vec![],
        env: vec![("FIXTURE_LABEL".to_string(), label.to_string())],
    }
}

fn fast_config() -> SupervisorConfig {
    SupervisorConfig {
        health_check_timeout: Duration::from_millis(500),
        restart_backoff: Duration::from_millis(100),
        shutdown_grace_period: Duration::from_millis(500),
        max_restart_attempts: None,
        request_timeout: Duration::from_secs(2),
    }
}

fn request(id: i64, method: &str, params: Option<serde_json::Value>) -> Request {
    Request {
        jsonrpc: V2,
        id: Id::Number(id),
        method: method.to_string(),
        params,
    }
}

async fn spawn_ready(label: &str) -> Supervisor {
    let (supervisor, mut events) = Supervisor::spawn(fixture_spec(label), fast_config());
    assert_eq!(events.recv().await, Some(SupervisorEvent::Spawned));
    assert_eq!(events.recv().await, Some(SupervisorEvent::Ready));
    supervisor
}

/// Spawns 3 distinct fixture upstreams ("pg", "fs", "git"), each exposing
/// its own "echo" tool, and builds a Gateway over all of them -- exercising
/// the roadmap's M1 exit criterion shape (multiple upstreams behind one
/// proxy) at the request-routing layer.
async fn three_upstream_gateway() -> Gateway {
    let mut supervisors = HashMap::new();
    for label in ["pg", "fs", "git"] {
        supervisors.insert(label.to_string(), spawn_ready(label).await);
    }
    Gateway::new(supervisors).await
}

#[tokio::test]
async fn tools_list_aggregates_namespaced_tools_from_every_upstream() {
    let gateway = three_upstream_gateway().await;

    let response = gateway
        .handle_request(request(1, METHOD_TOOLS_LIST, None))
        .await;
    let Outcome::Success { result } = response.outcome else {
        panic!("expected a successful tools/list response, got {:?}", response.outcome)
    };
    let parsed: ToolsListResult = serde_json::from_value(result).unwrap();

    let mut names: Vec<&str> = parsed.tools.iter().map(|t| t.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["fs__echo", "git__echo", "pg__echo"]);
}

#[tokio::test]
async fn tools_call_routes_to_the_correct_upstream() {
    let gateway = three_upstream_gateway().await;

    let params = ToolsCallParams {
        name: "fs__echo".to_string(),
        arguments: Some(serde_json::json!({"message": "hello"})),
    };
    let response = gateway
        .handle_request(request(
            2,
            METHOD_TOOLS_CALL,
            Some(serde_json::to_value(params).unwrap()),
        ))
        .await;

    let Outcome::Success { result } = response.outcome else {
        panic!("expected a successful tools/call response, got {:?}", response.outcome)
    };
    let text = result["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, "fs says: hello");
}

#[tokio::test]
async fn tools_call_with_an_unknown_tool_id_returns_an_error() {
    let gateway = three_upstream_gateway().await;

    let params = ToolsCallParams {
        name: "ghost__echo".to_string(),
        arguments: None,
    };
    let response = gateway
        .handle_request(request(
            3,
            METHOD_TOOLS_CALL,
            Some(serde_json::to_value(params).unwrap()),
        ))
        .await;

    assert!(matches!(response.outcome, Outcome::Error { .. }));
}

#[tokio::test]
async fn tools_call_with_an_unknown_method_returns_method_not_found() {
    let gateway = three_upstream_gateway().await;

    let response = gateway
        .handle_request(request(4, "not/a/real/method", None))
        .await;

    assert!(matches!(response.outcome, Outcome::Error { .. }));
}

#[tokio::test]
async fn shutdown_tears_down_every_upstream_concurrently() {
    let grace_period = Duration::from_millis(200);
    let config = SupervisorConfig {
        shutdown_grace_period: grace_period,
        ..fast_config()
    };

    let mut supervisors = HashMap::new();
    for label in ["pg", "fs", "git"] {
        let mut spec = fixture_spec(label);
        spec.env
            .push(("FIXTURE_IGNORE_SIGTERM".to_string(), "1".to_string()));
        let (supervisor, mut events) = Supervisor::spawn(spec, config);
        assert_eq!(events.recv().await, Some(SupervisorEvent::Spawned));
        assert_eq!(events.recv().await, Some(SupervisorEvent::Ready));
        supervisors.insert(label.to_string(), supervisor);
    }
    let gateway = Gateway::new(supervisors).await;

    let start = std::time::Instant::now();
    gateway.shutdown().await;
    let elapsed = start.elapsed();

    // Every upstream ignores SIGTERM, so each needs its own grace period
    // before the SIGKILL fallback lands. If they were shut down one at a
    // time this would take ~3x the grace period; concurrently it stays
    // close to 1x.
    assert!(
        elapsed < grace_period * 2,
        "expected concurrent shutdown, took {elapsed:?} for 3 upstreams with a {grace_period:?} grace period"
    );
}
