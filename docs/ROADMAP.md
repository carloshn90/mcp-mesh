# mcp-mesh — Local-First MCP Mesh & Governance Proxy

**One-liner:** A single-binary, local-first MCP mesh proxy that fans N servers into M clients, enforces policy-as-code, and uses a typed decision model (**Jev** — the local model that scores tool-selection candidates) for tool selection and guardrails — with a full deterministic fallback when the model isn't available. Guardrail scanning is handled by a second model, **Noul**, which screens tool results for injection attempts.

**Positioning:** System 1 in the MCP path. Typed, auditable decisions in <100ms P99 end-to-end; the LLM only ever sees the schemas it needs.

**Status:** Draft v0.2 (Rust) · Working title (name check pending) · License: Apache-2.0

## 1. Goals / Non-Goals

### Goals (v1)

- **Fan-in / Fan-out:** Any number of upstream MCP servers (stdio + streamable HTTP) mapped to any number of local clients (Claude Code, Cursor, custom agents) via stdio + HTTP.
- **Per-Client Least Privilege:** Scoped tool views with stable `server__tool` namespacing.
- **Deterministic Policy Engine:** Allow, deny, and approve-on-destructive rules with explicit per-rule failure semantics.
- **Intelligent Tool Selection:** Multi-stage pipeline (recall → Jev → policy gate → schema injection) featuring two-threshold hysteresis and a rules-only fallback.
- **Guardrails:** Injection scans (Noul) on every tool result, gray-zone quarantine, and interactive destructive-call approvals.
- **Supervision:** Stdio upstream lifecycle management (spawn, crash-restart, health checks, and structured logs).
- **Metadata Caching:** Caching for `tools/list`, prompts, and resources, invalidated dynamically on `listChanged`.
- **Audit Trail:** JSONL logging for every select, gate, scan, and decision, coupled with an `mcp-mesh explain <trace-id>` utility.

### Non-Goals (v1)

- Tool-result caching (due to semantic hazards; reserved for opt-in experiments).
- "Solving" prompt injection globally (ships as defense-in-depth).
- Cloud, SaaS, or multi-tenant hosting (focused strictly on Linux and macOS first).
- Fine-tuning Jev per deployment (unless M0 proves it's necessary and feasible).

## 2. Architecture

```
Clients (Claude Code, Cursor, CI agent, …)
    │  stdio / streamable HTTP
    ▼
┌────────────────────────────────────────────────────────┐
│                      mcp-mesh (1 static binary)          │
│                                                          │
│  Protocol Gateway ──► Selection Pipeline ──► Policy      │
│  (JSON-RPC 2.0,     (0 rules → 1 recall →   Engine       │
│   MCP spec)          2 Jev → 3 gate → 4 assemble)        │
│        │                      │            │            │
│  Metadata Cache     Guardrail Scanner    Approval        │
│  (tools/list,       (Noul on results +    Flow           │
│   listChanged)       regex prefilter)    (elicitation    │
│        │                      │          / TUI)          │
│  Supervisor ─────────────────┴──────► Audit (JSONL)      │
│  (spawn/restart/health)              & Metrics           │
└────────────────────────────────────────────────────────┘
    │  stdio / HTTP (one connection per upstream)
    ▼
 Upstreams: postgres, filesystem, git, remote (OAuth) …
```

### Components & Responsibilities

| Component | Responsibility | Notes |
|---|---|---|
| Protocol Gateway | JSON-RPC 2.0, MCP spec (streamable HTTP + stdio), method routing | Thin design: relies purely on the stable JSON-RPC envelope and method names. |
| Supervisor | Spawn/kill stdio upstreams, backoff restarts, health checks, log capture | Built on `tokio::process`; issues SIGTERM → SIGKILL on shutdown. |
| Tool Registry | Namespaced IDs (`pg__query`), stable slugs, annotations, per-client views | IDs remain stable across restarts for selection and audit correlation. |
| Selection Pipeline | Executes stages 0–4 of the selection workflow | Details outlined in §2.1. |
| Policy Engine | Deterministic rule evaluation and approval orchestration | The core durable asset of the proxy. |
| Guardrail Scanner | Noul injection scan on tool results with regex prefilter | Failures are never silent; logged explicitly as `scan_failed`. |
| Metadata Cache | Handles `tools/list` caching with a TTL backstop and `listChanged` hooks | Ensures low-overhead schema lookups. |
| Audit & Metrics | JSONL event logs and optional OTel integration | Feeds the downstream recalibration loop. |
| Decision Adapter | Trait wrapper abstracting `rules \| embed \| jev` | Keeps Jev decoupled from core architectural logic. |

### 2.1 Selection Lifecycle

1. **Deterministic Policy:** Pinned per-client tools, hard deny rules, and annotation flags bypass models entirely.
2. **Recall:** BM25 (plus optional local embeddings) searches the full index to isolate top candidates (k ≤ 255).
3. **Jev Evaluation:** Executes one Noul query per candidate ("does this turn need tool X?") alongside an optional Score re-rank in a single call.
4. **Policy Gate:** Enforces two-threshold hysteresis (keep ≥ 0.6, drop < 0.3, hold for 3 turns); destructive actions require approval regardless of probability, while gray zones trigger quarantine.
5. **Assembly:** Injects only selected schemas into the payload, logging the state-hash and full probability vector.

### 2.2 Request Lifecycle (`tools/call`)

1. Gateway resolves the client profile and namespaced tool against the registry.
2. The policy engine evaluates deterministic deny and approve rules.
3. If an action is flagged as destructive and requires approval, an elicitation or local prompt triggers (fail-closed on timeout).
4. The request forwards to the upstream; the result passes through the Guardrail Scanner before returning.
5. Every execution step appends an audit record tied to a shared trace-id.

### 2.3 Crate Layout

Structured as a modular 3-crate workspace:

```
mcp-mesh/
├── crates/
│   ├── protocol/    # JSON-RPC 2.0 + MCP method types (fuzzed)
│   ├── core/        # Gateway, registry, policy, selection, guardrails, audit
│   └── cli/         # Binary entrypoint: config, wiring, subcommands
└── evals/           # Golden sets and poisoned corpuses (data assets)
```

## 3. Key Design Decisions

- **D1 — Language (Rust):** Chosen for memory safety guarantees via `#![forbid(unsafe_code)]`, single static musl binary distribution, sub-millisecond overhead profiles, and robust state-machine modeling.
- **D1a — SDK Spike (M0):** Evaluates whether an official Rust SDK exists in the MCP org; otherwise adopts `rmcp` or a thin 1–2k LOC custom codec.
- **D2 — Decision Adapter Trait:** Decouples the decision engine behind an async trait (`Selector`), keeping Jev fully swappable. Hysteresis logic is implemented as a pure function to enable exhaustive property testing (`proptest`).
- **D3 — Failure Semantics:**
  - Selection timeout → fallback to rules selector (fail-closed: no model, no schema injection beyond pinned tools).
  - Injection scan error → pass and emit `scan_failed` (**fail-open, by deliberate choice** — the one exception to the fail-closed posture everywhere else in this list. Rationale: a scanner outage must not stall every tool call mesh-wide. The trade-off is that unscanned results reach the client during an outage; `scan_failed` is logged at error severity and surfaced in `mcp-mesh explain` so the gap is never silent. Revisit if M4's poisoned-corpus evals show this window is exploitable in practice.)
  - Destructive approval timeout → Deny (fail-closed).
  - Upstream failure → remove from pool, mark degraded, backoff restart.
- **D4 — Configuration:** Centralized TOML configuration file handling proxy bindings, upstream declarations, client profiles, selection thresholds, and audit paths.
- **D5 — Supply Chain Security:** Enforces `#![forbid(unsafe_code)]`, minimizes hot-path dependencies (`tokio`, `serde`, `reqwest` with `rustls` only), and utilizes `cargo-deny` in CI.
- **D6 — Build & Packaging:** Targets static compilation on Linux (`*-unknown-linux-musl`) and the native toolchain on macOS (`*-apple-darwin`; musl is a Linux-only libc and has no macOS target, so macOS binaries are dynamically linked against system frameworks as usual) across x86_64 and aarch64, distributed via GitHub Releases, Homebrew, and AUR.

## 4. Key Technical Considerations

1. **Asynchronous Notification Routing:** MCP servers frequently emit unsolicited JSON-RPC notifications (e.g., streaming logs, progress indicators, and `notifications/tools/list_changed`) independently of request-response cycles. The protocol gateway must multiplex these notifications upstream-to-client asynchronously without blocking primary message pipelines or stalling client threads.
2. **Latency Budget Realities:** The selection hop must fit inside the <100ms P99 headline budget (§ Positioning), so the Jev call itself is capped well under that: a hard 80ms timeout, enforced via aggressive HTTP/2 connection pooling (`reqwest` with keep-alive enabled), leaves headroom for recall, policy-gate evaluation, and schema assembly on either side of it. If the external decision model stalls past 80ms, the proxy must instantly drop back to deterministic rules without hanging the active agent session — the timeout exists specifically so the 100ms end-to-end number stays true even in the stall case.
3. **Streamable HTTP Transport Resilience:** While local environments heavily rely on stdio subprocess communication, remote or containerized upstreams using streamable HTTP/SSE demand resilient reconnection loops, exponential backoff, and robust heartbeat monitoring within the supervisor thread.

## 5. Security Model & Trust Statement

**Trust Statement:** The LLM is the trust boundary. mcp-mesh is defense in depth: it shrinks the tool surface, gates destructive actions, scans results, and records everything. It does not make an LLM safe.

- **Tool Poisoning Mitigation:** Guardrail scans (Noul) execute on every tool result; gray-zone results are quarantined.
- **State Hijacking Protection:** Injection scans run prior to selection, down-weighting selection probability if elevated anomaly flags trip.
- **Exfiltration Control:** Least-privilege client profiles limit potential exposure, though network-egress tools require proper upstream network isolation.

## 6. Milestones & Schedule

| Phase | Weeks | Scope & Deliverables | Exit Criteria |
|---|---|---|---|
| M0 — Spike & Validate | 1 | Developer interviews, token bloat analysis, Jev integration spike, protocol SDK spike. **Kill-switch question:** does Jev beat the rules-only fallback by enough margin to justify the added latency and failure-mode surface? The SDK and protocol spikes are groundwork, not gating — if Jev doesn't clear that bar, the project still ships as a deterministic-only proxy. | One-pager with measured metrics and go/no-go decision |
| M1 — Core Mesh | 2–5 | Routing stdio/HTTP upstreams, namespacing, metadata cache, supervision | 3 servers behind proxy, 2 clients attached; P99 proxy-added overhead < 1ms on passthrough calls (i.e. deterministic routing path, excluding Jev/Noul model hops and upstream round-trip time) |
| M2 — Policy Engine | 5–7 | Rule evaluation, approval workflows (TUI/elicitation), failure semantics | Destructive calls gated per profile; 100% audit explainability |
| M3 — Intelligent Selection | 7–10 | Recall pipeline, Jev adapter with rules fallback, hysteresis property tests | P@k ≥ 0.90 on golden set (k = recall-stage candidate count, k ≤ 255 per §2.1 — i.e. the correct tool is retained among the top-k candidates passed to Jev in ≥ 90% of golden-set turns); ≥ 70% schema token savings |
| M4 — Guardrails | 10–12 | Result scanning (Noul), quarantine/redaction actions, poisoned-corpus evals | Published TPR/FPR benchmark metrics |
| M5 — Ship v1.0 | 12–14 | README benchmarks, use-case guides, cargo-dist packaging, external beta launch | v1.0 tag released with active telemetry feedback loop |
