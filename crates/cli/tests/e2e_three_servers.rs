//! End-to-end exit-criteria demo for milestone M1 ("3 servers behind proxy,
//! 2 clients attached"): spawns the real `mcp-mesh` binary in front of 3
//! real stdio upstream processes, attaches 2 concurrent clients over the
//! Unix socket, and exercises `tools/list`/`tools/call` through the whole
//! stack (CLI -> Gateway -> Supervisor -> upstream process).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use mcp_mesh_protocol::jsonrpc::{Id, Outcome, Request, Response, V2};
use mcp_mesh_protocol::mcp::{ToolsCallParams, ToolsListResult, METHOD_TOOLS_CALL, METHOD_TOOLS_LIST};
use mcp_mesh_protocol::{MessageReader, MessageWriter};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

const UPSTREAM_LABELS: [&str; 3] = ["alpha", "beta", "gamma"];

fn unique_path(label: &str, extension: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "mcp-mesh-e2e-{}-{label}.{extension}",
        std::process::id()
    ))
}

fn write_config(path: &Path) {
    let echo_server = env!("CARGO_BIN_EXE_echo_server");
    let mut toml = String::new();
    for label in UPSTREAM_LABELS {
        toml += &format!(
            "[[upstream]]\nid = \"{label}\"\ncommand = \"{echo_server}\"\n\n\
             [upstream.env]\nFIXTURE_LABEL = \"{label}\"\n\n"
        );
    }
    std::fs::write(path, toml).expect("write e2e config");
}

type Client = (
    MessageReader<OwnedReadHalf, Response>,
    MessageWriter<OwnedWriteHalf, Request>,
);

async fn connect(path: &Path) -> Client {
    let stream = UnixStream::connect(path)
        .await
        .expect("connect to mesh socket");
    let (read_half, write_half) = stream.into_split();
    (MessageReader::new(read_half), MessageWriter::new(write_half))
}

fn request(id: i64, method: &str, params: Option<serde_json::Value>) -> Request {
    Request {
        jsonrpc: V2,
        id: Id::Number(id),
        method: method.to_string(),
        params,
    }
}

async fn wait_for_socket(path: &Path, timeout: Duration) {
    let start = Instant::now();
    loop {
        if UnixStream::connect(path).await.is_ok() {
            return;
        }
        if start.elapsed() > timeout {
            panic!("mesh socket at {} never became connectable", path.display());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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

#[tokio::test]
async fn three_upstreams_two_clients_tools_list_and_tools_call() {
    let config_path = unique_path("config", "toml");
    let socket_path = unique_path("socket", "sock");
    write_config(&config_path);

    let mut mesh = Command::new(env!("CARGO_BIN_EXE_mcp-mesh"))
        .arg(&config_path)
        .arg(&socket_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mcp-mesh");

    wait_for_socket(&socket_path, Duration::from_secs(5)).await;

    let (mut reader_a, mut writer_a) = connect(&socket_path).await;
    let (mut reader_b, mut writer_b) = connect(&socket_path).await;

    // Both independently-attached clients see all 3 upstreams' namespaced tools.
    for (writer, reader) in [(&mut writer_a, &mut reader_a), (&mut writer_b, &mut reader_b)] {
        writer
            .write_message(&request(1, METHOD_TOOLS_LIST, None))
            .await
            .expect("send tools/list");
        let response = reader
            .read_message()
            .await
            .expect("read tools/list response")
            .expect("a response, not EOF");
        let Outcome::Success { result } = response.outcome else {
            panic!("expected a successful tools/list response, got {:?}", response.outcome)
        };
        let parsed: ToolsListResult = serde_json::from_value(result).unwrap();
        let mut names: Vec<&str> = parsed.tools.iter().map(|t| t.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["alpha__echo", "beta__echo", "gamma__echo"]);
    }

    // Each client calls a *different* upstream's tool concurrently; routing
    // must land each call on the correct live upstream process.
    let call_a = async {
        let params = ToolsCallParams {
            name: "alpha__echo".to_string(),
            arguments: Some(serde_json::json!({"message": "from client A"})),
        };
        writer_a
            .write_message(&request(
                2,
                METHOD_TOOLS_CALL,
                Some(serde_json::to_value(params).unwrap()),
            ))
            .await
            .expect("send tools/call for client A");
        reader_a
            .read_message()
            .await
            .expect("read tools/call response for A")
            .expect("a response, not EOF")
    };
    let call_b = async {
        let params = ToolsCallParams {
            name: "gamma__echo".to_string(),
            arguments: Some(serde_json::json!({"message": "from client B"})),
        };
        writer_b
            .write_message(&request(
                3,
                METHOD_TOOLS_CALL,
                Some(serde_json::to_value(params).unwrap()),
            ))
            .await
            .expect("send tools/call for client B");
        reader_b
            .read_message()
            .await
            .expect("read tools/call response for B")
            .expect("a response, not EOF")
    };
    let (response_a, response_b) = tokio::join!(call_a, call_b);

    let Outcome::Success { result: result_a } = response_a.outcome else {
        panic!("expected success for client A, got {:?}", response_a.outcome)
    };
    let Outcome::Success { result: result_b } = response_b.outcome else {
        panic!("expected success for client B, got {:?}", response_b.outcome)
    };
    assert_eq!(result_a["content"][0]["text"], "alpha says: from client A");
    assert_eq!(result_b["content"][0]["text"], "gamma says: from client B");

    // Tear down: closing the mesh's stdin is its whole-process shutdown
    // trigger (see main::serve_stdio) -- it should take down the socket
    // server and all 3 upstreams and exit cleanly.
    drop(mesh.stdin.take());
    let status = wait_with_timeout(&mut mesh, Duration::from_secs(5));
    assert!(status.success(), "mcp-mesh exited with {status:?}");

    let _ = std::fs::remove_file(&config_path);
    let _ = std::fs::remove_file(&socket_path);
}
