//! Supervisor: spawns a stdio upstream process, performs an `initialize`
//! handshake as a health check, restarts it with backoff on crash, and
//! performs SIGTERM-then-SIGKILL graceful shutdown.
//!
//! Scope: this module only supervises the process lifecycle. Forwarding
//! arbitrary `tools/call` requests to a live upstream is the Gateway's job
//! (a later step) and will extend this module when that's actually needed.

use std::process::Stdio;
use std::time::Duration;

use mcp_mesh_protocol::jsonrpc::{Id, Outcome, Request, V2};
use mcp_mesh_protocol::mcp::{Implementation, InitializeParams, METHOD_INITIALIZE};
use mcp_mesh_protocol::{MessageReader, MessageWriter, Response};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::UpstreamId;

/// Static description of one stdio upstream to spawn and supervise.
#[derive(Debug, Clone)]
pub struct UpstreamSpec {
    pub id: UpstreamId,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy)]
pub struct SupervisorConfig {
    pub health_check_timeout: Duration,
    pub restart_backoff: Duration,
    pub shutdown_grace_period: Duration,
    pub max_restart_attempts: Option<u32>,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            health_check_timeout: Duration::from_secs(5),
            restart_backoff: Duration::from_secs(1),
            shutdown_grace_period: Duration::from_secs(5),
            max_restart_attempts: None,
        }
    }
}

/// Observable lifecycle events, emitted so callers (tests, future audit
/// logging) can see what the supervisor is doing without polling state.
#[derive(Debug, Clone, PartialEq)]
pub enum SupervisorEvent {
    Spawned,
    Ready,
    HealthCheckFailed,
    Exited,
    Restarting { attempt: u32, backoff: Duration },
    GivenUp { attempts: u32 },
    ShutDown,
}

/// Handle to a running supervision task for one upstream.
pub struct Supervisor {
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Supervisor {
    /// Spawns the background supervision task and returns a handle plus a
    /// channel of lifecycle events.
    pub fn spawn(
        spec: UpstreamSpec,
        config: SupervisorConfig,
    ) -> (Self, mpsc::Receiver<SupervisorEvent>) {
        let (events_tx, events_rx) = mpsc::channel(32);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(run(spec, config, events_tx, shutdown_rx));
        (
            Self {
                shutdown_tx: Some(shutdown_tx),
                task: Some(task),
            },
            events_rx,
        )
    }

    /// Requests graceful shutdown (SIGTERM, then SIGKILL after the grace
    /// period) and waits for the supervision task to finish.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

fn initialize_request() -> Request {
    let params = InitializeParams {
        protocol_version: "2025-06-18".to_string(),
        capabilities: serde_json::json!({}),
        client_info: Implementation {
            name: "mcp-mesh".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    };
    Request {
        jsonrpc: V2,
        id: Id::Number(0),
        method: METHOD_INITIALIZE.to_string(),
        params: Some(serde_json::to_value(params).expect("InitializeParams always serializes")),
    }
}

fn spawn_child(spec: &UpstreamSpec) -> std::io::Result<Child> {
    Command::new(&spec.command)
        .args(&spec.args)
        .envs(spec.env.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
}

/// Sends SIGTERM, waits up to `grace_period` for exit, then escalates to
/// SIGKILL if the child is still alive.
async fn graceful_shutdown(child: &mut Child, grace_period: Duration) {
    if let Some(raw_pid) = child.id() {
        let _ = signal::kill(Pid::from_raw(raw_pid as i32), Signal::SIGTERM);
    }
    if tokio::time::timeout(grace_period, child.wait()).await.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

/// Waits out the restart backoff, or stops early if asked to shut down while
/// waiting. Returns `false` when the caller should stop looping (shutdown
/// requested, or `max_restart_attempts` exceeded).
async fn backoff_or_give_up(
    events: &mpsc::Sender<SupervisorEvent>,
    shutdown_rx: &mut oneshot::Receiver<()>,
    config: &SupervisorConfig,
    attempt: u32,
) -> bool {
    if let Some(max) = config.max_restart_attempts {
        if attempt > max {
            let _ = events
                .send(SupervisorEvent::GivenUp { attempts: attempt })
                .await;
            return false;
        }
    }
    let _ = events
        .send(SupervisorEvent::Restarting {
            attempt,
            backoff: config.restart_backoff,
        })
        .await;
    tokio::select! {
        _ = tokio::time::sleep(config.restart_backoff) => true,
        _ = shutdown_rx => {
            let _ = events.send(SupervisorEvent::ShutDown).await;
            false
        }
    }
}

async fn run(
    spec: UpstreamSpec,
    config: SupervisorConfig,
    events: mpsc::Sender<SupervisorEvent>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let mut attempt: u32 = 0;

    loop {
        let mut child = match spawn_child(&spec) {
            Ok(child) => child,
            Err(_) => {
                attempt += 1;
                if !backoff_or_give_up(&events, &mut shutdown_rx, &config, attempt).await {
                    return;
                }
                continue;
            }
        };
        let _ = events.send(SupervisorEvent::Spawned).await;

        let stdin = child.stdin.take().expect("spawned with piped stdin");
        let stdout = child.stdout.take().expect("spawned with piped stdout");
        // Held for the lifetime of this child: dropping the writer early
        // would close its stdin, which the fixture/upstream reads as EOF
        // and exits on, masquerading as a crash.
        let mut writer = MessageWriter::<_, Request>::new(stdin);
        let mut reader = MessageReader::<_, Response>::new(stdout);

        let handshake = async {
            writer.write_message(&initialize_request()).await?;
            reader.read_message().await
        };

        let healthy = matches!(
            tokio::time::timeout(config.health_check_timeout, handshake).await,
            Ok(Ok(Some(Response {
                outcome: Outcome::Success { .. },
                ..
            })))
        );

        if !healthy {
            let _ = events.send(SupervisorEvent::HealthCheckFailed).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            attempt += 1;
            if !backoff_or_give_up(&events, &mut shutdown_rx, &config, attempt).await {
                return;
            }
            continue;
        }

        attempt = 0;
        let _ = events.send(SupervisorEvent::Ready).await;

        tokio::select! {
            _ = child.wait() => {
                let _ = events.send(SupervisorEvent::Exited).await;
                attempt += 1;
                if !backoff_or_give_up(&events, &mut shutdown_rx, &config, attempt).await {
                    return;
                }
            }
            _ = &mut shutdown_rx => {
                graceful_shutdown(&mut child, config.shutdown_grace_period).await;
                let _ = events.send(SupervisorEvent::ShutDown).await;
                return;
            }
        }
    }
}
