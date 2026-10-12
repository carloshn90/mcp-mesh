//! Supervisor: spawns a stdio upstream process, performs an `initialize`
//! handshake as a health check, restarts it with backoff on crash, performs
//! SIGTERM-then-SIGKILL graceful shutdown, and forwards arbitrary requests
//! (`tools/list`, `tools/call`, ...) to the currently live child.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use mcp_mesh_protocol::jsonrpc::{Id, Outcome, Request, V2};
use mcp_mesh_protocol::mcp::{Implementation, InitializeParams, METHOD_INITIALIZE};
use mcp_mesh_protocol::{Incoming, MessageReader, MessageWriter};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
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
    /// How long `Supervisor::call` waits for a matching response.
    pub request_timeout: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            health_check_timeout: Duration::from_secs(5),
            restart_backoff: Duration::from_secs(1),
            shutdown_grace_period: Duration::from_secs(5),
            max_restart_attempts: None,
            request_timeout: Duration::from_secs(10),
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

/// Why a forwarded `Supervisor::call` didn't produce an upstream `Outcome`.
#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    /// The supervision task has already stopped (shut down or gave up).
    NotRunning,
    /// No response arrived within `SupervisorConfig::request_timeout`.
    Timeout,
    /// The connection to the upstream was lost before a response arrived.
    UpstreamClosed,
}

/// A request queued for the currently (or soon-to-be) live child, together
/// with where to deliver its eventual outcome.
struct PendingCall {
    method: String,
    params: Option<Value>,
    respond_to: oneshot::Sender<Outcome>,
}

/// Handle to a running supervision task for one upstream.
pub struct Supervisor {
    call_tx: mpsc::Sender<PendingCall>,
    call_timeout: Duration,
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
        let (call_tx, call_rx) = mpsc::channel(32);
        let call_timeout = config.request_timeout;
        let task = tokio::spawn(run(spec, config, events_tx, call_rx, shutdown_rx));
        (
            Self {
                call_tx,
                call_timeout,
                shutdown_tx: Some(shutdown_tx),
                task: Some(task),
            },
            events_rx,
        )
    }

    /// Forwards one JSON-RPC call to the currently live upstream and awaits
    /// its matching response. Queued and sent once the upstream is ready if
    /// a restart is in progress; bounded by `request_timeout` either way.
    pub async fn call(&self, method: &str, params: Option<Value>) -> Result<Outcome, CallError> {
        let (respond_to, rx) = oneshot::channel();
        self.call_tx
            .send(PendingCall {
                method: method.to_string(),
                params,
                respond_to,
            })
            .await
            .map_err(|_| CallError::NotRunning)?;
        match tokio::time::timeout(self.call_timeout, rx).await {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(_)) => Err(CallError::UpstreamClosed),
            Err(_) => Err(CallError::Timeout),
        }
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

fn initialize_request(id: Id) -> Request {
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
        id,
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

type PendingResponses = Arc<AsyncMutex<HashMap<Id, oneshot::Sender<Outcome>>>>;

/// Reads `Response`/`Notification` lines from the child's stdout for as
/// long as the pipe stays open: resolves the matching pending call for a
/// response, or republishes a notification as a `SupervisorEvent` for
/// callers (e.g. a future cache-invalidation listener) to react to.
///
/// Self-terminating: returns as soon as the child closes stdout (crash or
/// normal exit) or sends an undecodable line, with no need for the caller
/// to explicitly cancel it.
async fn run_reader(mut reader: MessageReader<tokio::process::ChildStdout, Incoming>, pending: PendingResponses) {
    loop {
        match reader.read_message().await {
            Ok(Some(Incoming::Response(response))) => {
                if let Some(sender) = pending.lock().await.remove(&response.id) {
                    let _ = sender.send(response.outcome);
                }
            }
            Ok(Some(Incoming::Notification(_notification))) => {
                // Wiring this into MetadataCache::refresh (e.g. on
                // `listChanged`) is the Gateway's job; dropping it here for
                // now just means metadata only refreshes lazily.
            }
            Ok(None) | Err(_) => return,
        }
    }
}

/// Waits out the restart backoff, or stops early if asked to shut down
/// while waiting. Returns `false` when the caller should stop looping
/// (shutdown requested, or `max_restart_attempts` exceeded).
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

enum ConnectedOutcome {
    Exited,
    Shutdown,
}

async fn run(
    spec: UpstreamSpec,
    config: SupervisorConfig,
    events: mpsc::Sender<SupervisorEvent>,
    mut call_rx: mpsc::Receiver<PendingCall>,
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
        // would close its stdin, which the upstream reads as EOF and exits
        // on, masquerading as a crash.
        let mut writer = MessageWriter::<_, Request>::new(stdin);
        let reader = MessageReader::<_, Incoming>::new(stdout);
        let pending: PendingResponses = Arc::new(AsyncMutex::new(HashMap::new()));

        let (ready_tx, ready_rx) = oneshot::channel();
        pending.lock().await.insert(Id::Number(0), ready_tx);
        let handshake_sent = writer
            .write_message(&initialize_request(Id::Number(0)))
            .await
            .is_ok();

        let reader_task = tokio::spawn(run_reader(reader, pending.clone()));

        let healthy = handshake_sent
            && matches!(
                tokio::time::timeout(config.health_check_timeout, ready_rx).await,
                Ok(Ok(Outcome::Success { .. }))
            );

        if !healthy {
            let _ = events.send(SupervisorEvent::HealthCheckFailed).await;
            reader_task.abort();
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

        let mut next_id: i64 = 1;
        let outcome = loop {
            tokio::select! {
                _ = child.wait() => break ConnectedOutcome::Exited,
                _ = &mut shutdown_rx => break ConnectedOutcome::Shutdown,
                call = call_rx.recv() => {
                    let Some(pending_call) = call else {
                        // All Supervisor handles were dropped; shutdown_rx
                        // will also resolve shortly since it lives in the
                        // same struct, so just keep looping until it does.
                        continue;
                    };
                    let id = Id::Number(next_id);
                    next_id += 1;
                    pending.lock().await.insert(id.clone(), pending_call.respond_to);
                    let request = Request {
                        jsonrpc: V2,
                        id,
                        method: pending_call.method,
                        params: pending_call.params,
                    };
                    // A write failure here means the child is on its way
                    // out; child.wait() above will observe that shortly and
                    // the dropped `pending` entry will resolve the caller's
                    // `call()` with `CallError::UpstreamClosed`.
                    let _ = writer.write_message(&request).await;
                }
            }
        };

        reader_task.abort();

        match outcome {
            ConnectedOutcome::Exited => {
                let _ = events.send(SupervisorEvent::Exited).await;
                attempt += 1;
                if !backoff_or_give_up(&events, &mut shutdown_rx, &config, attempt).await {
                    return;
                }
            }
            ConnectedOutcome::Shutdown => {
                graceful_shutdown(&mut child, config.shutdown_grace_period).await;
                let _ = events.send(SupervisorEvent::ShutDown).await;
                return;
            }
        }
    }
}
