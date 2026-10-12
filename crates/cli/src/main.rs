mod config;

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;

use mcp_mesh_core::{Gateway, Supervisor, SupervisorConfig};
use mcp_mesh_protocol::{MessageReader, MessageWriter, Request, Response};

use config::Config;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let Some(config_path) = std::env::args().nth(1) else {
        eprintln!("usage: mcp-mesh <config.toml>");
        return ExitCode::FAILURE;
    };

    let config = match Config::load(Path::new(&config_path)) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

    let supervisors = spawn_upstreams(config);
    let gateway = Gateway::new(supervisors).await;

    serve_stdio(&gateway).await;

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

/// Serves the gateway to a single client over stdin/stdout until the client
/// closes its end (clean EOF) or a framing error makes the stream unusable.
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
