mod config;
mod server;

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use mcp_mesh_core::{Gateway, Supervisor, SupervisorConfig};
use mcp_mesh_protocol::{MessageReader, MessageWriter, Request, Response};

use config::Config;
use server::{spawn_unix_socket_server, UnixSocketServer};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(config_path) = args.next() else {
        eprintln!("usage: mcp-mesh <config.toml> [socket-path]");
        return ExitCode::FAILURE;
    };
    let socket_path = args.next();

    let config = match Config::load(Path::new(&config_path)) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

    let supervisors = spawn_upstreams(config);
    let gateway = Arc::new(Gateway::new(supervisors).await);

    // An optional second client front-end: additional clients can attach
    // over this socket, sharing the same Gateway/upstream pool as the
    // primary client talking over our own stdin/stdout below.
    let unix_server: Option<UnixSocketServer> = match socket_path {
        Some(path) => match spawn_unix_socket_server(gateway.clone(), Path::new(&path)) {
            Ok(server) => Some(server),
            Err(err) => {
                eprintln!("failed to listen on unix socket {path}: {err}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    serve_stdio(&gateway).await;

    if let Some(server) = unix_server {
        server.shutdown();
    }
    gateway.shutdown().await;
    ExitCode::SUCCESS
}

/// Spawns one supervised upstream per config entry, logging its lifecycle
/// events to stderr as they happen.
fn spawn_upstreams(config: Config) -> HashMap<String, Supervisor> {
    let mut supervisors = HashMap::new();
    for upstream in config.upstreams {
        let id = upstream.id.clone();
        let (supervisor, mut events) = Supervisor::spawn(upstream.into(), SupervisorConfig::default());
        let log_id = id.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                eprintln!("[{log_id}] {event:?}");
            }
        });
        supervisors.insert(id, supervisor);
    }
    supervisors
}

/// Serves the gateway to the primary client over stdin/stdout until it
/// closes its end (clean EOF) or a framing error makes the stream unusable.
/// This is the trigger for the whole process's shutdown: once this returns,
/// `main` tears down the Unix-socket server (if any) and every upstream.
async fn serve_stdio(gateway: &Gateway) {
    let mut reader = MessageReader::<_, Request>::new(tokio::io::stdin());
    let mut writer = MessageWriter::<_, Response>::new(tokio::io::stdout());

    loop {
        let request = match reader.read_message().await {
            Ok(Some(request)) => request,
            Ok(None) => break,
            Err(err) => {
                eprintln!("error reading request: {err}");
                break;
            }
        };
        let response = gateway.handle_request(request).await;
        if writer.write_message(&response).await.is_err() {
            break;
        }
    }
}
