# mcp-mesh

A local-first MCP mesh proxy: it fans any number of upstream [MCP](https://modelcontextprotocol.io) servers
(spawned over stdio) into any number of local clients, giving every tool a
stable `<upstream>__<tool>` name and routing calls to the right upstream.

Full design background lives in [docs/ROADMAP.md](docs/ROADMAP.md). This
README covers the part that's built so far (milestone M1, "Core Mesh") and
how to run it.

## Build

```bash
cargo build --release
```

The binary is `target/release/mcp-mesh`.

## Configure

Point it at a TOML file listing the upstream MCP servers to spawn. Each
`[[upstream]]` entry needs an `id` (used to namespace that upstream's tools)
and a `command`; `args` and an `env` table are optional.

```toml
# mesh.toml
[[upstream]]
id = "fs"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/home/me/project"]

[[upstream]]
id = "git"
command = "uvx"
args = ["mcp-server-git", "--repository", "/home/me/project"]

[upstream.env]
RUST_LOG = "info"
```

With this config, the filesystem server's tools show up as `fs__read_file`,
`fs__write_file`, etc., and the git server's as `git__log`, `git__diff`, and
so on — each client sees one flat, disambiguated tool list no matter how
many upstreams are behind it.

## Run

```bash
mcp-mesh mesh.toml
```

mcp-mesh speaks newline-delimited JSON-RPC 2.0 (one message per line) on its
own stdin/stdout, so the usual way to use it is to have your MCP client
*spawn* `mcp-mesh mesh.toml` as its server process, the same way it would
spawn any single stdio MCP server directly. Closing stdin (the client
exiting) shuts the whole mesh down — every upstream gets a graceful SIGTERM,
then SIGKILL if it doesn't exit in time.

You can also drive it by hand to see what's going on:

```bash
mcp-mesh mesh.toml <<'EOF'
{"jsonrpc":"2.0","id":1,"method":"tools/list"}
EOF
```

which prints back one JSON-RPC response line per request, e.g.

```json
{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"fs__read_file", ...}, {"name":"git__log", ...}]}}
```

### Multiple clients

Pass a second argument — a socket path — to additionally serve the *same*
running mesh and upstream pool over a Unix domain socket, so more than one
client can attach at once:

```bash
mcp-mesh mesh.toml /tmp/mcp-mesh.sock
```

Any client that speaks the same newline-delimited JSON-RPC framing can now
connect to `/tmp/mcp-mesh.sock` independently of the primary stdio client.
All connected clients share one registry, one metadata cache, and one set
of supervised upstream processes.

## Project layout

```
crates/
├── protocol/   JSON-RPC 2.0 + MCP wire types, newline-delimited codec
├── core/       Tool Registry, Supervisor, metadata cache, Gateway
└── cli/        mcp-mesh binary: config loading, stdio + Unix-socket serving
docs/           Design roadmap and implementation plans
```

## Development

```bash
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Both are run in CI on every push and pull request (see
[.github/workflows/ci.yml](.github/workflows/ci.yml)).
