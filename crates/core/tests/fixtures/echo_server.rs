//! Test-only fixture: a minimal stdio MCP upstream used by the Supervisor's
//! integration tests. Speaks the same newline-delimited JSON-RPC as the real
//! codec, synchronously, with no tokio dependency of its own.
//!
//! Behavior is steered entirely by environment variables set by the test
//! that spawns it:
//! - `FIXTURE_CRASH_AFTER_INIT=1`: exit(1) right after answering `initialize`.
//! - `FIXTURE_FAIL_INIT=1`: answer `initialize` with a JSON-RPC error.
//! - `FIXTURE_IGNORE_SIGTERM=1`: install a no-op SIGTERM handler so the
//!   supervisor's graceful shutdown must escalate to SIGKILL.

use std::io::{self, BufRead, Write};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use mcp_mesh_protocol::jsonrpc::{Id, Response};
use mcp_mesh_protocol::mcp::{
    Implementation, InitializeResult, ToolsListResult, METHOD_INITIALIZE, METHOD_TOOLS_LIST,
};

fn env_flag(name: &str) -> bool {
    std::env::var(name).map(|v| v == "1").unwrap_or(false)
}

fn install_sigterm_ignore() {
    let flag = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, flag)
        .expect("register a no-op SIGTERM handler");
}

fn handle(method: &str, id: Id, fail_init: bool) -> Response {
    match method {
        METHOD_INITIALIZE if fail_init => {
            Response::error(id, -32000, "fixture configured to fail initialize")
        }
        METHOD_INITIALIZE => {
            let result = InitializeResult {
                protocol_version: "2025-06-18".to_string(),
                capabilities: serde_json::json!({}),
                server_info: Implementation {
                    name: "echo-fixture".to_string(),
                    version: "0.1.0".to_string(),
                },
            };
            Response::success(
                id,
                serde_json::to_value(result).expect("InitializeResult always serializes"),
            )
        }
        METHOD_TOOLS_LIST => {
            let result = ToolsListResult {
                tools: vec![],
                next_cursor: None,
            };
            Response::success(
                id,
                serde_json::to_value(result).expect("ToolsListResult always serializes"),
            )
        }
        other => Response::error(id, -32601, format!("method not found: {other}")),
    }
}

fn main() {
    if env_flag("FIXTURE_IGNORE_SIGTERM") {
        install_sigterm_ignore();
    }
    let crash_after_init = env_flag("FIXTURE_CRASH_AFTER_INIT");
    let fail_init = env_flag("FIXTURE_FAIL_INIT");

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let id: Id = value
            .get("id")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or(Id::Null);
        let method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");

        let response = handle(method, id, fail_init);
        let encoded = serde_json::to_string(&response).expect("Response always serializes");
        writeln!(stdout, "{encoded}").expect("stdout should accept the response line");
        stdout.flush().expect("stdout should flush");

        if method == METHOD_INITIALIZE && !fail_init && crash_after_init {
            std::process::exit(1);
        }
    }
}
