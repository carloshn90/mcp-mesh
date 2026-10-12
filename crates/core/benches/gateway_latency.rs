//! Informational benchmark for the roadmap's M1 exit criterion: "P99
//! proxy-added overhead < 1ms on passthrough calls" -- i.e. how much
//! `Gateway::handle_request` itself adds on top of a trivial upstream's own
//! round-trip, for a `tools/call` passthrough. NOT wired into CI (`cargo
//! bench` is a separate, manual command); use it to spot regressions in the
//! routing path, not as a merge gate.

use std::collections::HashMap;

use criterion::{criterion_group, criterion_main, Criterion};
use mcp_mesh_core::{Gateway, Supervisor, SupervisorConfig, UpstreamSpec};
use mcp_mesh_protocol::jsonrpc::{Id, Request, V2};
use mcp_mesh_protocol::mcp::{ToolsCallParams, METHOD_TOOLS_CALL};

fn fixture_spec(label: &str) -> UpstreamSpec {
    UpstreamSpec {
        id: label.to_string(),
        command: env!("CARGO_BIN_EXE_echo_server").to_string(),
        args: vec![],
        env: vec![("FIXTURE_LABEL".to_string(), label.to_string())],
    }
}

async fn build_gateway() -> Gateway {
    let mut supervisors = HashMap::new();
    for label in ["alpha", "beta", "gamma"] {
        let (supervisor, _events) = Supervisor::spawn(fixture_spec(label), SupervisorConfig::default());
        supervisors.insert(label.to_string(), supervisor);
    }
    // `Gateway::new` blocks on each upstream's handshake internally, so by
    // the time this returns every upstream is already warmed up.
    Gateway::new(supervisors).await
}

fn tools_call_request() -> Request {
    let params = ToolsCallParams {
        name: "alpha__echo".to_string(),
        arguments: None,
    };
    Request {
        jsonrpc: V2,
        id: Id::Number(1),
        method: METHOD_TOOLS_CALL.to_string(),
        params: Some(serde_json::to_value(params).expect("ToolsCallParams always serializes")),
    }
}

fn bench_tools_call_passthrough(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("build a tokio runtime for the benchmark");
    let gateway = runtime.block_on(build_gateway());

    c.bench_function("gateway_tools_call_passthrough", |b| {
        b.to_async(&runtime).iter(|| {
            let request = tools_call_request();
            let gateway = &gateway;
            async move { gateway.handle_request(request).await }
        });
    });
}

criterion_group!(benches, bench_tools_call_passthrough);
criterion_main!(benches);
