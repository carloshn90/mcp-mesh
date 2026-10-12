use std::time::{Duration, Instant};

use mcp_mesh_core::{Supervisor, SupervisorConfig, SupervisorEvent, UpstreamSpec};

fn fixture_spec(env: Vec<(String, String)>) -> UpstreamSpec {
    UpstreamSpec {
        id: "echo".to_string(),
        command: env!("CARGO_BIN_EXE_echo_server").to_string(),
        args: vec![],
        env,
    }
}

fn fast_config() -> SupervisorConfig {
    SupervisorConfig {
        health_check_timeout: Duration::from_millis(500),
        restart_backoff: Duration::from_millis(100),
        shutdown_grace_period: Duration::from_millis(500),
        max_restart_attempts: None,
    }
}

#[tokio::test]
async fn spawns_and_becomes_ready() {
    let (supervisor, mut events) = Supervisor::spawn(fixture_spec(vec![]), fast_config());

    assert_eq!(events.recv().await, Some(SupervisorEvent::Spawned));
    assert_eq!(events.recv().await, Some(SupervisorEvent::Ready));

    supervisor.shutdown().await;
}

#[tokio::test]
async fn crash_after_init_triggers_restart_and_recovers() {
    let env = vec![("FIXTURE_CRASH_AFTER_INIT".to_string(), "1".to_string())];
    let (supervisor, mut events) = Supervisor::spawn(fixture_spec(env), fast_config());

    assert_eq!(events.recv().await, Some(SupervisorEvent::Spawned));
    assert_eq!(events.recv().await, Some(SupervisorEvent::Ready));
    assert_eq!(events.recv().await, Some(SupervisorEvent::Exited));
    assert_eq!(
        events.recv().await,
        Some(SupervisorEvent::Restarting {
            attempt: 1,
            backoff: Duration::from_millis(100)
        })
    );
    // Recovers: the supervisor spawns the upstream again and it becomes
    // ready a second time, proving the crash didn't wedge the loop.
    assert_eq!(events.recv().await, Some(SupervisorEvent::Spawned));
    assert_eq!(events.recv().await, Some(SupervisorEvent::Ready));

    supervisor.shutdown().await;
}

#[tokio::test]
async fn backoff_elapses_before_the_restart() {
    let env = vec![("FIXTURE_CRASH_AFTER_INIT".to_string(), "1".to_string())];
    let config = SupervisorConfig {
        restart_backoff: Duration::from_millis(300),
        ..fast_config()
    };
    let (supervisor, mut events) = Supervisor::spawn(fixture_spec(env), config);

    assert_eq!(events.recv().await, Some(SupervisorEvent::Spawned));
    assert_eq!(events.recv().await, Some(SupervisorEvent::Ready));
    assert_eq!(events.recv().await, Some(SupervisorEvent::Exited));

    let before = Instant::now();
    assert!(matches!(
        events.recv().await,
        Some(SupervisorEvent::Restarting { .. })
    ));
    assert_eq!(events.recv().await, Some(SupervisorEvent::Spawned));
    let elapsed = before.elapsed();
    assert!(
        elapsed >= Duration::from_millis(280),
        "expected the backoff to elapse before respawning, got {elapsed:?}"
    );

    supervisor.shutdown().await;
}

#[tokio::test]
async fn health_check_failure_retries_then_gives_up_after_max_attempts() {
    let env = vec![("FIXTURE_FAIL_INIT".to_string(), "1".to_string())];
    let config = SupervisorConfig {
        max_restart_attempts: Some(2),
        restart_backoff: Duration::from_millis(50),
        ..fast_config()
    };
    let (supervisor, mut events) = Supervisor::spawn(fixture_spec(env), config);

    for attempt in 1..=2 {
        assert_eq!(events.recv().await, Some(SupervisorEvent::Spawned));
        assert_eq!(events.recv().await, Some(SupervisorEvent::HealthCheckFailed));
        assert!(matches!(
            events.recv().await,
            Some(SupervisorEvent::Restarting { attempt: a, .. }) if a == attempt
        ));
    }
    assert_eq!(events.recv().await, Some(SupervisorEvent::Spawned));
    assert_eq!(events.recv().await, Some(SupervisorEvent::HealthCheckFailed));
    assert_eq!(
        events.recv().await,
        Some(SupervisorEvent::GivenUp { attempts: 3 })
    );

    supervisor.shutdown().await;
}

#[tokio::test]
async fn graceful_shutdown_terminates_the_child_promptly() {
    let (supervisor, mut events) = Supervisor::spawn(fixture_spec(vec![]), fast_config());
    assert_eq!(events.recv().await, Some(SupervisorEvent::Spawned));
    assert_eq!(events.recv().await, Some(SupervisorEvent::Ready));

    let start = Instant::now();
    supervisor.shutdown().await;
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "a cooperative child should exit on SIGTERM well within the grace period"
    );
}

#[tokio::test]
async fn shutdown_escalates_to_sigkill_when_sigterm_is_ignored() {
    let env = vec![("FIXTURE_IGNORE_SIGTERM".to_string(), "1".to_string())];
    let config = SupervisorConfig {
        shutdown_grace_period: Duration::from_millis(200),
        ..fast_config()
    };
    let (supervisor, mut events) = Supervisor::spawn(fixture_spec(env), config);
    assert_eq!(events.recv().await, Some(SupervisorEvent::Spawned));
    assert_eq!(events.recv().await, Some(SupervisorEvent::Ready));

    let start = Instant::now();
    supervisor.shutdown().await;
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(190),
        "expected to wait out the grace period before escalating: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "expected the SIGKILL fallback to land quickly, not hang: {elapsed:?}"
    );
}
