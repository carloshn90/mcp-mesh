# M1 "Core Mesh" — Step-by-Step Implementation Plan

## Context

The repo is freshly scaffolded (3-crate workspace: `protocol`, `core`, `cli`, all stubs) with CI already running `cargo test` + `cargo clippy`. The next job is to actually build milestone **M1 — Core Mesh** from [docs/ROADMAP.md](../../Documents/projects/rust/mcp-mesh/docs/ROADMAP.md): routing stdio upstreams, stable `server__tool` namespacing, metadata caching with `listChanged` invalidation, and supervision (spawn/health/restart/shutdown), ending in a runnable binary that meets the roadmap's M1 exit criteria (3 servers behind the proxy, 2 clients attached, <1ms P99 proxy overhead on passthrough).

Scope for this plan: **Linux first**, **stdio upstreams only** (streamable HTTP to upstreams is a later milestone, per §4.3 of the roadmap — out of scope here). M0 (research spikes) is out of scope; this starts straight into implementation.

Each step below is a complete, compiling, tested, clippy-clean vertical slice — intended to be reviewed and committed on its own before moving to the next step, per the user's standing clean-code/testing preference.

## Decisions made

- **Client transport (step 8):** Unix domain sockets, framed with the same newline-delimited JSON codec as upstreams — not HTTP. Keeps M1 Linux-first and dependency-light; streamable HTTP for clients is deferred to a later milestone.
- **Fake upstream test doubles:** an in-repo Rust fixture binary (built via `CARGO_BIN_EXE_*`) speaking real JSON-RPC over stdio — dependency-free and type-checked, reused across steps 4, 6, 9.
- **Graceful shutdown:** add the `nix` crate (small, Linux-first, unsafe-code-free in the parts we use) to send a real `SIGTERM` before falling back to `tokio::process::Child::kill()` (`SIGKILL`). `tokio::process` alone can't send SIGTERM.
- **Performance exit criterion:** the <1ms P99 number is checked by a separate, non-gating `criterion` benchmark (step 9), not a hard CI assertion — CI runners are too noisy for a reliable hard gate.

## Steps

1. **JSON-RPC 2.0 + MCP envelope types** — `crates/protocol/src/{lib.rs,jsonrpc.rs,mcp.rs}` + `Cargo.toml` (`serde`, `serde_json`). `Request`/`Response`/`Notification`/`Id` per JSON-RPC 2.0, plus typed `initialize`, `tools/list`, `tools/call`, `notifications/tools/list_changed`; everything else as `serde_json::Value`. Pure data modeling, no I/O.
   - Tests: round-trip (de)serialization per type, malformed-payload errors, `Id` null/number/string variants.
   - Commit: `Add JSON-RPC 2.0 and MCP envelope types to protocol crate`

2. **Stdio framing codec** — `crates/protocol/src/codec.rs` (+ `tokio` with `io-util`/`process`). Newline-delimited JSON reader/writer generic over `AsyncRead`/`AsyncWrite`; reused later by both the supervisor and the client-facing socket server.
   - Tests: round-trip over `tokio::io::duplex`, partial-read / multi-message-per-read framing, oversized/garbage line handling.
   - Commit: `Add newline-delimited JSON-RPC codec over async I/O`

3. **Tool Registry & namespacing** — `crates/core/src/registry.rs` (new; `mcp-mesh-core` depends on `mcp-mesh-protocol`). In-memory `ToolRegistry`: per-upstream tool lists, stable `server__tool` ID generation, reverse lookup to `(upstream, original_tool_name)`.
   - Tests: ID stability, duplicate-name/collision handling, merged cross-server listing, reverse lookup.
   - Commit: `Add Tool Registry with stable server__tool namespacing`

4. **Supervisor: spawn, health, crash-restart, shutdown** — `crates/core/src/supervisor.rs`, plus a test-fixture binary `crates/core/tests/fixtures/echo_server.rs`. Uses `tokio::process::Command` to spawn a stdio upstream, wires stdin/stdout through step 2's codec, does an `initialize` handshake as health check, detects exit and restarts with backoff, sends `SIGTERM` (via `nix`) then `SIGKILL` on shutdown.
   - Tests: fixture spawns and responds; kill-and-restart recovers within N attempts; graceful shutdown terminates the child; backoff timing between restarts.
   - Commit: `Add Supervisor for stdio upstream spawn/health/restart/shutdown`

5. **Metadata cache with listChanged invalidation** — `crates/core/src/cache.rs`. Caches each upstream's `tools/list` keyed by upstream id; invalidated/refreshed on `notifications/tools/list_changed` fed from the supervisor's reader loop, without blocking other upstreams.
   - Tests: cache hit avoids re-query; `list_changed` triggers exactly one refresh; concurrent invalidations on different upstreams don't block each other.
   - Commit: `Add tools/list metadata cache with listChanged invalidation`

6. **Gateway: compose registry + cache + supervisor, route requests** — `crates/core/src/gateway.rs`. `Gateway` owns N supervised upstreams; `handle_request(JsonRpcRequest) -> JsonRpcResponse` resolves namespaced tool IDs, strips the prefix for `tools/call`, serves `tools/list` from the cache (merged + re-namespaced).
   - Tests: integration test spawning 2-3 fixture upstreams; routes `tools/call` to the correct one; aggregated `tools/list` returns namespaced IDs; unknown-tool-id error path.
   - Commit: `Add Gateway composing registry, cache, and supervisor for request routing`

7. **CLI config + wiring** — `crates/cli/src/{main.rs,config.rs}` (+ `serde`, `toml`). Minimal TOML config listing stdio upstream commands/args; CLI loads config, builds a `Gateway`, serves it over stdio to a single client via step 2's codec. First real runnable `mcp-mesh` binary.
   - Tests: config parsing (valid/invalid TOML); smoke test piping a `tools/list` request into the binary and checking the response.
   - Commit: `Wire CLI entrypoint to load config and serve Gateway over stdio`

8. **Multi-client support via Unix domain sockets** — `crates/cli/src/{main.rs,server.rs}`. Adds a Unix-socket front end (same codec as step 2) so multiple clients share one running `Gateway`/upstream pool, satisfying "2 clients attached."
   - Tests: two concurrent fake client connections both get correctly, independently-routed responses; one disconnecting doesn't affect the other.
   - Commit: `Add multi-client Unix socket front-end sharing one Gateway instance`

9. **End-to-end exit-criteria demo + latency smoke test** — `crates/cli/tests/e2e_three_servers.rs` (reusing/extending the step-4 fixture binary) + a `criterion` micro-benchmark (non-CI-gating). Stands up 3 fixture stdio upstreams behind one `Gateway`/CLI instance, attaches 2 concurrent client connections over the step-8 socket, exercises `tools/list`/`tools/call` end-to-end, and separately benches proxy-added latency toward the P99 < 1ms target.
   - Tests: the e2e test itself; the bench as a non-gating artifact.
   - Commit: `Add end-to-end 3-upstream/2-client demo and latency benchmark`

## Verification

After each step: `cargo test --workspace --all-targets` and `cargo clippy --workspace --all-targets -- -D warnings` must both pass clean (same checks as CI) before handing you the commit message. Step 9 additionally gives you a runnable demo: build the CLI, point its config at 3 fixture/echo upstreams, connect 2 client sockets, and confirm correct routed responses plus a benchmark report for the latency target.
