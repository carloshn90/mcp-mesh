//! Unix-socket front end: lets additional clients attach to one already
//! running `Gateway`/upstream pool, alongside the CLI's primary stdio
//! client (see `main::serve_stdio`). Framed with the same newline-delimited
//! JSON-RPC codec used everywhere else in the mesh.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::{UnixListener, UnixStream};
use tokio::task::{JoinHandle, JoinSet};

use mcp_mesh_core::Gateway;
use mcp_mesh_protocol::{MessageReader, MessageWriter, Request, Response};

/// A running Unix-socket listener for extra clients.
pub struct UnixSocketServer {
    path: PathBuf,
    accept_task: JoinHandle<()>,
}

impl UnixSocketServer {
    /// Stops accepting new connections and drops every connection still in
    /// flight, then removes the socket file.
    pub fn shutdown(self) {
        self.accept_task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Binds `path` (removing a stale socket file left over from a previous run,
/// if any) and spawns a background task that accepts connections and serves
/// each one against `gateway` concurrently.
pub fn spawn_unix_socket_server(gateway: Arc<Gateway>, path: &Path) -> io::Result<UnixSocketServer> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    let bound_path = path.to_path_buf();
    let log_path = bound_path.clone();

    let accept_task = tokio::spawn(async move {
        let mut connections: JoinSet<()> = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _addr)) => {
                            let gateway = gateway.clone();
                            connections.spawn(serve_connection(gateway, stream));
                        }
                        Err(err) => {
                            eprintln!("unix socket accept error on {}: {err}", log_path.display());
                            break;
                        }
                    }
                }
                // Reaps finished connection tasks so `connections` doesn't
                // grow unbounded over a long-lived server's lifetime.
                Some(_) = connections.join_next() => {}
            }
        }
    });

    Ok(UnixSocketServer {
        path: bound_path,
        accept_task,
    })
}

async fn serve_connection(gateway: Arc<Gateway>, stream: UnixStream) {
    let (read_half, write_half) = stream.into_split();
    let mut reader = MessageReader::<_, Request>::new(read_half);
    let mut writer = MessageWriter::<_, Response>::new(write_half);

    loop {
        let request = match reader.read_message().await {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(_) => return,
        };
        let response = gateway.handle_request(request).await;
        if writer.write_message(&response).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    use mcp_mesh_protocol::jsonrpc::{Id, Outcome, V2};
    use mcp_mesh_protocol::mcp::{ToolsCallParams, METHOD_TOOLS_CALL};

    fn socket_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("mcp-mesh-test-{}-{label}.sock", std::process::id()))
    }

    async fn connect_and_call(path: &Path, id: i64, tool_name: &str) -> Response {
        let stream = UnixStream::connect(path).await.expect("connect to unix socket");
        let (read_half, write_half) = stream.into_split();
        let mut reader = MessageReader::<_, Response>::new(read_half);
        let mut writer = MessageWriter::<_, Request>::new(write_half);

        let params = ToolsCallParams {
            name: tool_name.to_string(),
            arguments: None,
        };
        let request = Request {
            jsonrpc: V2,
            id: Id::Number(id),
            method: METHOD_TOOLS_CALL.to_string(),
            params: Some(serde_json::to_value(params).unwrap()),
        };
        writer.write_message(&request).await.expect("send request");
        reader
            .read_message()
            .await
            .expect("read response")
            .expect("response present")
    }

    #[tokio::test]
    async fn two_concurrent_clients_get_independently_routed_responses() {
        let gateway = Arc::new(Gateway::new(HashMap::new()).await);
        let path = socket_path("two-clients");
        let server = spawn_unix_socket_server(gateway, &path).expect("bind socket");
        tokio::time::sleep(Duration::from_millis(20)).await; // let the accept loop start listening

        let (response_a, response_b) = tokio::join!(
            connect_and_call(&path, 1, "ghost_a__x"),
            connect_and_call(&path, 2, "ghost_b__y"),
        );

        let Outcome::Error { error: error_a } = response_a.outcome else {
            panic!("expected an error response for client A")
        };
        let Outcome::Error { error: error_b } = response_b.outcome else {
            panic!("expected an error response for client B")
        };
        assert!(error_a.message.contains("ghost_a__x"), "{error_a:?}");
        assert!(error_b.message.contains("ghost_b__y"), "{error_b:?}");

        server.shutdown();
    }

    #[tokio::test]
    async fn one_client_disconnecting_does_not_affect_another() {
        let gateway = Arc::new(Gateway::new(HashMap::new()).await);
        let path = socket_path("disconnect");
        let server = spawn_unix_socket_server(gateway, &path).expect("bind socket");
        tokio::time::sleep(Duration::from_millis(20)).await;

        let stream_a = UnixStream::connect(&path).await.expect("connect client A");
        drop(stream_a); // simulate client A disconnecting mid-session
        tokio::time::sleep(Duration::from_millis(20)).await; // let the server notice the close

        let response_b = connect_and_call(&path, 1, "ghost__z").await;
        assert!(matches!(response_b.outcome, Outcome::Error { .. }));

        server.shutdown();
    }
}
