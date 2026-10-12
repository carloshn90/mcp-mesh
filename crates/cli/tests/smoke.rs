//! End-to-end smoke test against the real, compiled `mcp-mesh` binary: pipe
//! a `tools/list` request into its stdin and check the response on stdout.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use mcp_mesh_protocol::jsonrpc::{Id, Outcome, Request, Response, V2};
use mcp_mesh_protocol::mcp::{ToolsListResult, METHOD_TOOLS_LIST};

#[test]
fn tools_list_smoke_test_against_the_real_binary() {
    let config_path = std::env::temp_dir().join(format!("mcp-mesh-smoke-{}.toml", std::process::id()));
    std::fs::write(&config_path, "").expect("write an empty (upstream-less) config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_mcp-mesh"))
        .arg(&config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mcp-mesh");

    let request = Request {
        jsonrpc: V2,
        id: Id::Number(1),
        method: METHOD_TOOLS_LIST.to_string(),
        params: None,
    };
    let mut line = serde_json::to_string(&request).expect("encode request");
    line.push('\n');
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(line.as_bytes())
        .expect("write request");

    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut response_line = String::new();
    stdout
        .read_line(&mut response_line)
        .expect("read a response line");

    let response: Response = serde_json::from_str(&response_line).expect("decode response");
    assert_eq!(response.id, Id::Number(1));
    let Outcome::Success { result } = response.outcome else {
        panic!("expected a successful tools/list response, got {:?}", response.outcome)
    };
    let parsed: ToolsListResult = serde_json::from_value(result).expect("decode ToolsListResult");
    assert!(parsed.tools.is_empty(), "no upstreams were configured");

    drop(child.stdin.take()); // close stdin so the binary sees EOF and exits.
    let status = wait_with_timeout(&mut child, Duration::from_secs(5));
    assert!(status.success(), "mcp-mesh exited with {status:?}");

    let _ = std::fs::remove_file(&config_path);
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            panic!("mcp-mesh did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
