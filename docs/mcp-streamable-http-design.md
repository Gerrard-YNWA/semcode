# Native Streamable HTTP transport for `semcode-mcp`

Status: **design proposal** — implementation of M1/M2 in progress in this
worktree.
Branch: `feature/streamable-http-mcp` (worktree `.worktrees/streamable-http-mcp`)

Implementation status (this branch):

- M1 transport skeleton: **done** (`src/bin/semcode_mcp_http.rs`, CLI flags,
  bind/port, fixed `/mcp`, graceful shutdown).
- M2 protocol bridge: **done** (`ServerHandler` over `McpServer`, tool schema
  parsing at startup, lazy/full tool sets, `Send` boundary via
  `spawn_blocking`).
- M3 security: **done for this phase** (Host/Origin, optional Bearer token,
  in-flight gate, request cap; stateless response cap).
- Verification: `cargo check --bin semcode-mcp` passes; all 45 tests in the
  `semcode-mcp` binary pass (including the 7 new module tests); all 7
  wire-level integration tests (`tests/mcp_http.rs`) pass; `cargo fmt --check`
  and `cargo clippy --all-targets -- -D warnings` pass.
- M4 notifications: **not started** — background progress is dropped in HTTP
  mode; `indexing_status` is the durable poll. The `logging` capability is not
  advertised.
- M5 docs/container: **partially done** (README, `Dockerfile.http`,
  `docs/claude-semcode-setup.md`).

Reference implementation: `../docseek` (`rmcp 3.10.0` + `axum 0.8.9`)

This document describes adding the MCP **Streamable HTTP** transport to
`semcode-mcp` without duplicating any tool logic, changing the index format, or
breaking the existing stdio transport. It is written to be implementable
directly: every new type, flag, failure mode, and test is named.

---

## 1. Problem

`semcode-mcp` currently speaks MCP only over stdio:

| Concern | Current code |
|---|---|
| CLI | `Args` at `src/bin/semcode-mcp.rs:2285` |
| stdio loop | `run_stdio_server` at `src/bin/semcode-mcp.rs:6198` |
| JSON-RPC dispatch | `handle_request` at `src/bin/semcode-mcp.rs:3328` |
| `initialize` | `handle_initialize` at `src/bin/semcode-mcp.rs:3357` |
| `tools/list` | `handle_list_tools` at `src/bin/semcode-mcp.rs:3381` |
| `tools/call` | `handle_tool_call` at `src/bin/semcode-mcp.rs:3437` |
| Lazy meta-tools | `handle_call_tool` at `src/bin/semcode-mcp.rs:3533` |
| Progress notifications | `progress_notification` at `src/bin/semcode-mcp.rs:5968`, single-slot sender at `:3158` |

A stdio server cannot be shared by multiple clients, cannot be reached over a
network, and must be respawned by every agent. Users of semcode on a shared
build machine need one long-lived, read-only HTTP endpoint they can point
[CC], Cursor, or a custom agent at.

## 2. Goals and non-goals

### Goals

1. Serve MCP **Streamable HTTP** (single `/mcp` endpoint, POST + GET SSE +
   optional DELETE) using the official Rust SDK (`rmcp`), not a hand-rolled
   JSON-over-POST shim.
2. Expose exactly the same tools, schemas, lazy mode, pagination, and error
   text as the stdio server, from one implementation.
3. Keep stdio working byte-for-byte for existing users.
4. Secure by default: loopback bind, Host/Origin validation, an optional
   Bearer token that is enforced whenever configured, bounded
   request/response sizes, bounded concurrency.
5. Be testable without a real index and without a real MCP client.

### Non-goals (this change)

- TLS termination. Deploy behind a reverse proxy or an SSH tunnel; the server
  binds loopback by default and never implements its own TLS.
- Multi-tenant authorization. A token grants read access to the entire index.
- Write tools, shell access, Git fetch, or indexing administration over HTTP.
- Browser CORS support. Non-browser MCP clients are the target; do not add a
  permissive CORS layer.
- Migrating stdio to `rmcp`. That is a follow-up (Phase 2) so this change
  cannot regress stdio clients.
- Persisting HTTP sessions across process restarts (no external `SessionStore`).

---

## 3. Reference implementation in `docseek`

Docseek already ships this shape, so the risky parts are proven:

- `../docseek/src/http_server.rs:136` — `StreamableHttpService` +
  `LocalSessionManager`, nested at `/mcp`.
- `../docseek/src/http_server.rs:100` — middleware: single-value header
  checks, Host/Origin/Bearer policy, in-flight semaphore, wire-size cap.
- `../docseek/src/transport_policy.rs:15` — `Policy::check`, BLAKE3 constant-time
  token comparison.
- `../docseek/src/mcp_core.rs:273` — tool definitions as JSON values,
  deserialized into `rmcp::model::Tool` in tests.
- `../docseek/src/main.rs:609` — `docseek serve <config> <port>` entry point.
- `../docseek/Cargo.toml` — pinned `rmcp = "=3.10.0"` with
  `default-features = false, features = ["server",
  "transport-streamable-http-server"]`.

What we copy: transport wiring, header policy, gate, test style.
What we change: semcode's core is a 7.5k-line binary with private methods, not a
separate `mcp_core` module, and it has background indexing + progress
notifications that docseek does not.

---

## 4. Architecture

```
                        ┌──────────────────────────────┐
  stdio client ────────►│ run_stdio_server (unchanged) │──┐
                        └──────────────────────────────┘  │
                                                          ▼
                        ┌──────────────────────────────┐
  HTTP client ────────►│ axum Router                  │
  POST/GET/DELETE /mcp │  ├─ security middleware      │
                        │  └─ StreamableHttpService    │
                        │       └─ Handler (rmcp)      │
                        └──────────────┬───────────────┘
                                       │ calls private methods
                                       ▼
                        ┌──────────────────────────────┐
                        │ Arc<McpServer> (unchanged)   │
                        │  db, page_cache,             │
                        │  indexing_state,             │
                        │  notification hub, lazy_mode │
                        └──────────────┬───────────────┘
                                       │
                              background indexing
```

Key properties:

- **One `McpServer` per process**, shared by every HTTP session and (if it were
  enabled simultaneously, which it is not) by stdio. It is already
  `Send + Sync`: `Arc<DatabaseManager>`, `Mutex`-backed `PageCache` and
  `IndexingState`.
- **The `Handler` is a thin adapter** implementing `rmcp::ServerHandler` by
  converting between rmcp model types and the JSON `Value` shapes that
  `handle_list_tools` / `handle_tool_call` already produce.
- **No tool code moves.** `handle_tool_call` and `handle_call_tool` remain the
  only dispatch points, so lazy mode, `refuse_unscopable_tool`, stale-index
  refusal, pagination, and truncation all behave identically.
- **Transport selection is exclusive**: `--transport stdio` (default) or
  `--transport http`. One process runs one transport. Running both would
  double the surface for no user benefit.

### Why `rmcp` rather than a hand-written implementation

Streamable HTTP is not "JSON over POST": it has `Mcp-Session-Id`, protocol
version negotiation, SSE event IDs and resumability, DELETE termination,
content negotiation (`application/json` vs `text/event-stream`), and 202
Accepted semantics. Reimplementing that correctly is the main source of bugs;
`rmcp` 3.10 already implements it and is what docseek ships. We pin it exactly
to avoid API churn.

---

## 5. Dependencies and toolchain impact

Add to `Cargo.toml`:

```toml
rmcp = { version = "=3.10.0", default-features = false, features = [
    "server",
    "transport-streamable-http-server",
] }
```

Already present and compatible:

- `axum = { version = "0.8", features = ["ws"] }` (`Cargo.toml:49`)
- `tower = "0.5"` (`Cargo.toml:50`)
- `tokio = { version = "1.50", features = ["full"] }`
- `blake3 = "1.8.3"` (used by the token policy)
- `tracing`, `anyhow`, `serde_json`

Add only if the graceful-shutdown/cancellation wiring needs them explicitly:

```toml
tokio-util = { version = "0.7", features = ["rt"] }   # CancellationToken
```

`rmcp`'s cancellation token comes from `tokio-util`; it is re-exported through
`rmcp`'s config type, but pinning the crate directly avoids relying on a
transitive re-export.

### MSRV consequence — resolved by container deployment

`rmcp 3.10.0` declares:

- `edition = "2024"`
- `rust-version = "1.88"`

**Decision: the CentOS 7 hosts are not upgraded. `semcode-mcp` is built and
run in a container, which decouples the required toolchain from the host.**
This is viable and is the recommended deployment shape:

- **Builder stage**: a modern image (`rust:1.98-…`, `debian:bookworm` + rustup,
  or the existing `centos7:ke` image with rustup installed). The build itself
  may use any modern toolchain because it never runs on the host.
- **Runtime stage**: a minimal image whose glibc matches the builder
  (e.g. `debian:bookworm-slim`). The host's glibc 2.17 is irrelevant: the
  process uses the container's userland. Do not build on a modern distro and
  then copy the binary onto the CentOS 7 host; that glibc mismatch will fail.
- If policy requires a CentOS 7 *base image*, install Rust 1.88+ inside it with
  rustup and verify that the chosen release still supports glibc 2.17 (the
  official Linux minimum has moved over time). Also validate that its old
  gcc/binutils can still compile the C++ build dependencies; the docseek
  CentOS 7 validation already uses a custom image this way, so it is a known
  path — but treat it as a spike, not an assumption.
- Mount at runtime, do not bake into the image:
  - the index directory (`.semcode.db`) — writeable if background indexing is
    enabled, read-only with `--no-auto-index`;
  - the Git repository at a stable absolute path (git-aware lookups read HEAD,
    the working copy, and commit metadata);
  - the model cache (`~/.cache/semcode/models`) or a pre-populated model
    directory (`--model-path`);
  - the token file, if one is used, through a secret mount.
- Networking: `--bind 127.0.0.1` **inside** a container is unreachable from the
  host. Bind `0.0.0.0` in the container and publish only to host loopback:

  ```bash
  docker run --rm \
    -p 127.0.0.1:8765:8765 \
    -v /srv/semcode/index/.semcode.db:/data/.semcode.db \
    -v /srv/src/linux:/repo:ro \
    -v semcode-models:/root/.cache/semcode/models \
    semcode-mcp:local \
    --transport http --bind 0.0.0.0 --port 8765 \
    --database /data/.semcode.db --git-repo /repo --no-auto-index
  ```

  Clients still send `Host: 127.0.0.1:8765`, so that authority must be in the
  Host allowlist — never put the container's `0.0.0.0` in `--allowed-host`.
  The default allowlist therefore always includes `127.0.0.1:<port>` and
  `localhost:<port>` in addition to the bind address (§8.2).
- The database is large and versioned by the indexer; never copy it into the
  image layer. Mounting it also lets the host update the index without
  rebuilding the image.
- Add a `Dockerfile` and the `docker run` example to the docs in M5.

Pin exact versions in `Cargo.lock` (`rmcp`, `axum`, `tower`, `tokio-util`) and
treat an `rmcp` bump as a reviewed change, not a routine `cargo update`.


Feature-gating `rmcp` behind a Cargo feature would mean a `semcode-mcp` build
without HTTP support, which is a deployment hazard; do not gate it. Accept the
extra link time for the binary.

---

## 6. CLI and configuration

Extend `Args` (`src/bin/semcode-mcp.rs:2285`) without changing existing flags:

| Flag | Default | Meaning |
|---|---|---|
| `--transport <stdio\|http>` | `stdio` | Select transport. |
| `--bind <IP>` | `127.0.0.1` | HTTP listen address (IPv4/IPv6 literal). |
| `--port <u16>` | `8765` | HTTP port; `0` = OS-assigned (printed on startup; for tests). |
| `--stateless-http` | off | Disable sessions (see §9); probes and proxies. Not needed for normal use. |
| `--allowed-host <host[:port]>` | derived | Repeatable. Additional `Host` authorities. |
| `--allowed-origin <origin>` | none | Repeatable. Exact origins (e.g. `http://localhost:8765`). |
| `--max-request-bytes <n>` | `1_048_576` | POST body cap (1 MiB). |
| `--max-response-bytes <n>` | `16_777_216` | Cap for buffered (non-SSE) responses. |
| `--max-in-flight <n>` | `16` | Concurrent POST requests; overflow → 429. |
| `--sse-keep-alive-secs <n>` | `15` | SSE keep-alive interval. |
| `--no-auto-index` | off | Do not start the background indexing task. |
| `--token-file <path>` | — | Read the optional Bearer token from a `0400` file. |
| `SEMCODE_MCP_TOKEN` | unset | Optional Bearer token from the environment (never a CLI flag). When unset, the endpoint is unauthenticated. |
| `SEMCODE_MCP_TOKEN_FILE` | unset | Same as `--token-file`. |

The endpoint path is **fixed at `/mcp`**; there is no `--http-path` flag. A
reverse proxy that needs a public prefix can rewrite `/<prefix>/mcp` to
`/mcp`, which keeps the server's URL surface minimal and consistent with
docseek and common MCP clients.

Validation rules (fail fast, exit non-zero, message to stderr):

1. HTTP flags are rejected when `--transport stdio` (avoid silently ignored
   security settings).
2. `--port 0` is allowed only with HTTP; log the actual bound address.
3. A configured token must be 32–4096 visible ASCII characters, matching
   docseek's policy. Never echo the token, never include it in logs or error
   bodies.
4. **No token is not an error** (§8.3). When neither
   `SEMCODE_MCP_TOKEN` nor `--token-file` is set, the endpoint requires no
   `Authorization` header. Log a prominent startup warning naming the bind
   authority, and document that access control then depends on the network
   path (loopback, SSH tunnel, firewall, or a reverse proxy).
5. `--bind` on a non-loopback address without a token is allowed but warned
   about explicitly. It is a read-only index, but it exposes source code, Git
   history, and lore mail to anyone who can reach the port.
6. `--bind 0.0.0.0`/`::` requires the operator to add a `--allowed-host` for
   every DNS name clients will use. The default allowlist covers
   `127.0.0.1:<port>`, `localhost:<port>`, and `[::1]:<port>` only, so a
   container published to host loopback works without extra flags.
7. `--max-in-flight`, `--max-request-bytes`, `--max-response-bytes` must be
   > 0; `--max-in-flight` ≤ 1024.

Do not accept the token through a positional argument or `--token`: process
arguments are visible in `ps` and shell history. Environment or a `0400` file
only.

**Backward compatibility:** with no new flags, behavior is byte-identical to
today (stdio). The only intentional change to stdio is extracting the
instructions string into a shared method (§7.1); its value does not change.

### 6.1 Endpoint shape

Recommended and implemented shape:

| Concern | Decision | Rationale |
|---|---|---|
| Path | `http://<host>:<port>/mcp` | Single well-known path; matches docseek and the examples most MCP clients document. No discovery step. |
| Methods | POST, GET, DELETE on the same path | That is the Streamable HTTP contract: POST carries JSON-RPC, GET opens the SSE stream, DELETE terminates the session. Never split these across paths. |
| Legacy `/sse` | Not implemented | The old HTTP+SSE transport (2024-11-05) is superseded; supporting both doubles the surface and confuses clients. |
| Version in path (`/mcp/v1`) | No | Protocol version is negotiated in `initialize` and the `MCP-Protocol-Version` header; a path version would imply a second, incompatible API. |
| Token in URL (`?token=`) | No | URLs leak into proxy logs, shell history, and referrers. Use `Authorization: Bearer`. |
| Health endpoint | Optional `/healthz` returning `200` with no body details | Useful for container orchestration; it must not disclose index paths or repository names. |
| TLS | Terminated by a reverse proxy or SSH tunnel | The server binds loopback (or a container network) and does not implement TLS. Public URL stays `https://<host>/mcp`. |
| Multi-repo (future) | `/repos/<repo-id>/mcp` | A repository is a real resource; a path segment leaves room for future per-repo HTTP endpoints and keeps one listener, one token, one TLS configuration. Validate IDs as `[A-Za-z0-9._-]+`. |
| Reverse-proxy prefix | Rewrite at the proxy | Keeps the server's configuration surface fixed and avoids path-joining bugs. |

---

## 7. Protocol bridge

### 7.1 Shared pieces extracted from `McpServer`

Add these small methods so both transports read the same source of truth
(no behavior change):

```rust
impl McpServer {
    /// Instructions advertised by initialize (lazy vs full).
    fn instructions(&self) -> String { /* body moved from handle_initialize */ }

    /// Tool definitions actually exposed by tools/list (lazy vs full).
    async fn exposed_tools(&self) -> Vec<Value> { /* body moved from handle_list_tools */ }
}
```

`handle_initialize` and `handle_list_tools` become callers of these. Keeping the
`include_str!("../../docs/semcode-mcp.md")` in one place also avoids a fragile
relative path if the HTTP module lives in a sibling file.

### 7.2 `Handler` (rmcp `ServerHandler`)

```rust
#[derive(Clone)]
struct Handler {
    server: Arc<McpServer>,
    notifications: Arc<NotificationHub>,
}

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder().enable_tools().enable_logging().build(),
        )
        .with_server_info(Implementation::new(
            "semcode-mcp",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(self.server.instructions())
    }

    // MCP 2026-07-28 removes the initialize handshake and requires per-request
    // protocol metadata we do not implement. Advertise through 2025-11-25.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_11_25))
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        if request.is_some_and(|r| r.cursor.is_some()) {
            return Err(ErrorData::invalid_params("cursor not supported", None));
        }
        let value = json!({ "tools": self.server.exposed_tools().await });
        let tools = serde_json::from_value(value["tools"].clone())
            .map_err(|e| ErrorData::internal_error(format!("invalid tool schema: {e}"), None))?;
        Ok(ListToolsResult { tools, next_cursor: None, ..Default::default() })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // Same guard as the stdio path: these request shapes need capabilities
        // semcode does not advertise.
        if request.input_responses.is_some() || request.request_state.is_some() {
            return Err(ErrorData::invalid_params("unsupported tool request", None));
        }
        let params = json!({
            "name": request.name,
            "arguments": Value::Object(request.arguments.unwrap_or_default()),
        });
        let result = self.server.handle_tool_call(&params).await;
        // Existing results are {content, isError}; reject anything else loudly
        // instead of sending a malformed tool result.
        serde_json::from_value::<CallToolResult>(result)
            .map(Into::into)
            .map_err(|e| ErrorData::internal_error(format!("invalid tool result: {e}"), None))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        // Mirror the exposed set. Unknown names are intentionally not rejected
        // here: handle_tool_call returns the established "Unknown tool" tool
        // error text, which clients already show.
        self.server
            .exposed_tools_sync() // cheap cache built at startup, see below
            .into_iter()
            .find(|tool| tool["name"] == name)
            .and_then(|value| serde_json::from_value(value).ok())
    }
}
```

Notes:

- `exposed_tools()` is `async` only because it is called from async handlers;
  the schema vectors are static. Build a `Vec<Value>` once in `Handler::new`
  (or in `McpServer::new`) and expose it as `exposed_tools_sync()` so
  `get_tool` (a sync trait method) works without blocking.
- `get_info` advertises `logging` because the HTTP transport forwards
  background-indexing messages as `notifications/message` (§7.4). If
  notifications are deferred to Phase 2, omit `enable_logging` rather than
  advertising a capability that is never used.
- `initialize` itself is implemented by `rmcp`: it negotiates the protocol
  version against `supported_protocol_versions`, sets peer info, and returns
  the `ServerConfig` from `get_info`. Do not override it unless we need to
  record the session peer, which `on_initialized` already provides.
- **`Send` boundary (measured, not assumed).** `McpServer::handle_tool_call`
  produces a future that is **not `Send`**: `callchain::write_indirect_reverse_chain`
  and the `lore_writers` functions hold a `&mut dyn Write` across an `await`
  (`src/callchain.rs:1405`, `src/lore_writers.rs:702`), and `grep_function_bodies`
  / `collect_thread_descendants` carry HRTB closures that fail the `Send`
  check. `rmcp`'s `ServerHandler` requires `Send` futures unless its `local`
  feature is enabled (which changes the transport's threading model and does
  not fit an axum multi-threaded server). The adapter therefore runs the call
  on a blocking thread against the existing runtime handle and moves only the
  resulting `Value` across the boundary:

  ```rust
  let result = tokio::task::spawn_blocking(move || {
      tokio::runtime::Handle::current().block_on(server.handle_tool_call(&params))
  }).await?;
  ```

  Consequences to keep in mind:
  - Each in-flight tool call occupies one blocking-pool thread for its
    duration; the `--max-in-flight` gate (§8.2) bounds that.
  - `Handle::block_on` must not be called from an async context; a
    `spawn_blocking` closure is not one, which is why this is the chosen
    boundary.
  - Making the writer functions generic over `W: Write` was tried first and
    rejected: it compiles the library but surfaces the unrelated HRTB/`Send`
    problems above, turning a transport change into a DB-layer refactor.
    Revisit only if the tool layer is made `Send` on its own merits.

### 7.3 Differences from the stdio `initialize`

| Field | stdio today | HTTP via rmcp |
|---|---|---|
| `protocolVersion` | hardcoded `2024-11-05` | negotiated (up to `2025-11-25`) |
| `capabilities.tools` | `{}` | `{}` + optional `logging` |
| `serverInfo.version` | hardcoded `0.1.0` | `CARGO_PKG_VERSION` (0.1.1) |
| `instructions` | lazy/full string | same string |

The stdio version stays untouched in this change to avoid client regressions.
Record the divergence in `docs/semcode-mcp.md`. A follow-up may make stdio
negotiate the version too; that is a one-line change plus tests.

### 7.4 Progress notifications

Today `McpServer.notification_tx` is a **single** `Option<UnboundedSender>`.
That cannot serve multiple HTTP sessions: whichever session registered last
would receive every progress line, and the stdio writer competes for the slot.

Replace it with a small fan-out hub:

```rust
/// Fan-out for background-indexing progress. No subscriber => message dropped.
struct NotificationHub {
    subscribers: Mutex<Vec<mpsc::UnboundedSender<Value>>>,
}

impl NotificationHub {
    fn subscribe(&self) -> mpsc::UnboundedReceiver<Value> { ... }
    fn publish(&self, value: Value) {
        let mut subs = self.subscribers.lock().unwrap();
        // retain() drops closed sessions; send never blocks.
        subs.retain(|tx| tx.send(value.clone()).is_ok());
    }
}
```

- `McpServer.notification_tx: Arc<Mutex<Option<...>>>` becomes
  `notifications: Arc<NotificationHub>`.
- `index_current_commit_background` publishes `progress_notification(&msg)`
  (same JSON payload) instead of sending a string.
- stdio: `run_stdio_server` subscribes once and keeps its writer task, writing
  one JSON line per message. Output is identical.
- HTTP: each session's `Handler::new` subscribes; `on_initialized` stores
  `context.peer.clone()` in the handler. A forwarding task converts the JSON
  into `ServerNotification::LoggingMessageNotification` and calls
  `peer.send_notification(...)`. A failed send ends that task; the hub prunes
  the sender on the next publish.
- If no session is connected, progress is dropped (it is advisory; the
  `indexing_status` tool is the durable way to poll).
- Update the ~20 test constructors that build `McpServer` literally to use
  `Arc::new(NotificationHub::default())`.

Why not per-session `PageCache`? `PageCache` lives inside `McpServer` and is
shared by design. Two sessions paging the same query key can observe each
other's page cursor. Source code is read-only, so this is a UX quirk, not a
security issue. Document it; a follow-up can namespace the cache key with the
rmcp session ID (available from the request context) or give each Handler its
own cache via a per-session wrapper.

### 7.5 Error mapping

| Situation | Response |
|---|---|
| Malformed JSON / bad framing | `rmcp` returns JSON-RPC parse error |
| Unsupported method | `rmcp` `-32601` (stdio keeps its own) |
| Unknown tool | **Tool result** with `isError: true` and `Unknown tool: ...` (existing behavior; do not convert to a protocol error) |
| Tool execution failure | Tool result with `isError: true` (existing handlers) |
| `inputResponses` / `requestState` | JSON-RPC `-32602 Invalid params` |
| Cursor in `tools/list` | JSON-RPC `-32602` |
| Tool schema/result fails conversion | `-32603 Internal error` with a generic message; log the detail to stderr |
| Missing/wrong Bearer (only when a token is configured) | HTTP 401 |
| Bad/double Host, bad Origin | HTTP 403 |
| Body too large | HTTP 413 (rmcp streaming limit) |
| Too many concurrent POSTs | HTTP 429 + `Retry-After: 1` (middleware gate) |
| Server shutting down | 503, then listener closes |

Tool results never include host paths or internal error strings: that property
comes from the existing handlers and must be covered by a test that greps the
response for a temp path and the token.

---

## 8. HTTP security layer

### 8.1 Router

```rust
fn router(server: Arc<McpServer>, cfg: &HttpConfig) -> Router {
    let handler = Handler::new(server, cfg.notifications.clone());
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(!cfg.stateless)   // sessions by default
        .with_json_response(cfg.stateless)          // JSON body for stateless mode
        .with_max_request_body_bytes(cfg.max_request_bytes)
        .with_sse_keep_alive(Some(cfg.sse_keep_alive))
        .with_allowed_hosts(cfg.allowed_hosts.clone())
        .with_allowed_origins(cfg.allowed_origins.clone())
        .with_cancellation_token(cfg.shutdown.child_token());

    let mcp = StreamableHttpService::new(
        move || Ok(handler.clone()),          // one Handler per session
        Arc::new(LocalSessionManager::default()),
        config,
    );

    Router::new()
        .nest_service(&cfg.path, mcp)
        .layer(middleware::from_fn_with_state(state, protect))
}
```

- `service_factory` is called once per session, so each `Handler` can own its
  subscriber and peer without cross-session state.
- `with_allowed_hosts` / `with_allowed_origins` are rmcp's DNS-rebinding
  defenses. Our middleware repeats the checks deliberately: rmcp does not
  reject duplicate `Authorization`/`Host`/`Origin` headers, and when a token
  is configured we need an auth check that rmcp does not provide. Keep the two
  configurations derived from the **same** `HttpConfig` so they cannot
  disagree.

### 8.2 Middleware `protect`

Port `docseek/src/http_server.rs:100` and `transport_policy.rs:15`, adapted:

1. Read `Host`, `Origin`, `Authorization` with a `single_header` helper.
   Duplicate `Host` or `Origin` values → 403. Duplicate `Authorization` values
   → 401 **only when a token is configured**; with no token configured the
   header is ignored entirely.
2. `Policy::check(host, origin, authorization)`:
   - `Host` must equal one of the configured authorities (`IP:port`,
     `hostname:port`, and the scheme-default form when port is 80).
   - `Origin` is optional; when present it must be an exact member of the
     allowlist. Reject prefixes/suffixes (`http://localhost:8765.evil`).
   - When `Policy` holds a token hash, `Authorization` must be
     `Bearer <token>`; compare `blake3::hash(token)` with the stored hash in
     constant time. Length and character-class checks before hashing, as in
     docseek.
   - When `Policy` holds no token (the default), skip the `Authorization`
     check; there is nothing to authenticate against. Host/Origin validation
     still applies.
3. Apply the in-flight gate **to POST only**:
   `try_acquire_owned()`; failure → 429 with `Retry-After: 1`. Never queue
   unboundedly. GET (SSE) and DELETE do not consume a POST permit so a
   long-lived event stream cannot starve tool calls.
4. Response size: do **not** buffer SSE. In stateful mode the POST response is
   `text/event-stream` and may legitimately stay open while a tool runs.
   - Stateful mode: rely on the tool layer's existing 3,000-line truncation and
     the page cache; no middleware buffering.
   - Stateless mode (`json_response(true)`): the body is a single JSON
     document; buffer it through `to_bytes(body, max_response_bytes)` with a
     timeout and return 413/504 on overflow, exactly like docseek.
   - Determine "stateful" from the response `Content-Type`, not the request:
     if it starts with `text/event-stream`, pass the body through untouched.
5. Do not add permissive CORS. OPTIONS gets rmcp's 405 (and may be rejected
   earlier by the policy if it lacks Host). Browser clients are out of scope.

### 8.3 Token handling (optional)

**Decision: authentication is opt-in. If `SEMCODE_MCP_TOKEN` or
`--token-file` is set, every request must carry the matching Bearer token; if
neither is set, the endpoint is unauthenticated.**

- The raw token is never stored: `Policy` holds `Option<blake3::Hash>`. `None`
  means "no auth configured", not "empty token".
- Startup logs one warning when no token is configured, including the bind
  authority, so an operator cannot miss that the endpoint is open on that
  network path. Do not repeat the warning per request.
- Startup errors never echo the token or the file contents.
- Never log `Authorization`. Session IDs are fine (they are not credentials).
- `--token-file` should warn if the file is group/world-readable (POSIX mode
  check); do not fail on platforms without the check.
- Documentation recommends a token for anything beyond a single-user loopback
  setup. The absence of a token does not remove Host/Origin validation, but
  Host/Origin checks are not authentication.

### 8.4 Network posture

- Default `--bind 127.0.0.1`, matching docseek. Cross-machine use is via
  `ssh -N -L 8765:127.0.0.1:8765 user@server`, a reverse proxy that adds TLS,
  or the container deployment in §5 (bind `0.0.0.0` inside, publish to host
  loopback).
- A token is optional (§8.3). Without one, the network path is the only access
  control; with a non-loopback bind the startup warning must be explicit.
- Tokens grant the same read access as the index itself: every indexed source
  file, commit message, and lore email. Say so in the docs; there is no
  per-repository scoping.
- Do not trust `X-Forwarded-For`/`X-Real-IP`; they are not used for decisions.
- No rate limiting beyond the in-flight gate. If exposed to untrusted users,
  front it with a proxy that does rate limiting.

---

## 9. Session model and lifecycle

### Default: stateful (rmcp `legacy_session_mode = true`)

**Decided: the default is stateful.** Sessions are required for the target
use (one long-lived agent, progress notifications, DELETE termination) and
cost little at this scale.

- `initialize` creates a session; the response carries `Mcp-Session-Id`.
- Clients send it on subsequent POST/GET/DELETE.
- Supports GET SSE (server notifications) and DELETE termination.
- `LocalSessionManager` defaults: 16-slot channel, 5-minute idle keep-alive,
  60-second initialize timeout. Expose the keep-alive/init timeout in a later
  iteration if deployments need it; the defaults are sane.
- Memory: one session worker + handler per connected agent (tens of KiB).
  A client that connects and disappears is cleaned up by keep-alive; do not
  set it to `None`.

### Optional: `--stateless-http`

- `legacy_session_mode = false`, `json_response = true`, and
  `NeverSessionManager` (sessions are meaningless without state).
- Each request is independent; `initialize` is still answered for clients that
  send it, but no session ID is issued.
- No GET SSE and no server-initiated progress; background progress is dropped
  (clients poll `indexing_status`).
- Useful behind round-robin proxies and for simple CLI probes such as `curl`.
- The transport tests should cover both modes, but the documented default is
  stateful.

### Startup / shutdown

1. Resolve CLI + token before touching the database, so a misconfiguration
   fails in milliseconds.
2. Create `McpServer`, spawn background indexing unless `--no-auto-index`,
   then bind. If bind fails, abort the indexing task and exit non-zero.
3. Print (stderr, `tracing::info!`): transport, bind address, actual port,
   endpoint path, session mode, and whether authentication is configured —
   **never** the token.
4. Graceful shutdown on SIGINT/SIGTERM:
   - cancel the rmcp `CancellationToken` (terminates sessions and SSE streams),
   - `axum::serve(...).with_graceful_shutdown(...)`,
   - abort the background indexing task and await it,
   - let `Drop` release the database.
5. `--port 0`: print the actual `listener.local_addr()` so tests can parse it.

---

## 10. Module layout

Minimal-diff layout that keeps the private `McpServer` accessible:

```
src/bin/semcode-mcp.rs          + mod declaration, Args fields, main branch
src/bin/semcode_mcp_http.rs     new: Handler, Policy, router, serve, tests
```

In `semcode-mcp.rs`:

```rust
#[path = "semcode_mcp_http.rs"]
mod mcp_http;
```

Rust resolves a `#[path]` module relative to the containing file (`src/bin/`),
and a child module may use private items of the crate root, so `McpServer`,
`handle_tool_call`, and `handle_list_tools` need no visibility changes. This
avoids moving the 7.5k-line binary into a directory (`src/bin/semcode-mcp/`)
and the churn that entails (`include_str!` paths, `Cargo.toml` bin path, git
rename noise).

If Phase 2 migrates stdio to `rmcp`, promote the shared adapter pieces into a
library module (`src/mcp_server.rs`) and have the binary call it for both
transports. Do not do that now.

Estimated size: 450–650 lines including tests; the policy and gate are direct
ports from docseek.

---

## 11. Testing

### 11.1 Unit tests (in `semcode_mcp_http.rs`)

- `Policy::check`: valid/invalid Host, valid/absent/invalid Origin; with a
  token: exact/wrong/malformed/duplicate `Authorization`; without a token:
  missing `Authorization` accepted, duplicate `Authorization` ignored.
- Token length/character validation when a token is supplied; no-token
  configuration is accepted (and expected to log a warning, not fail).
- Tool schema conversion: every value from `exposed_tools()` (lazy and full)
  deserializes into `rmcp::model::Tool`.
- Tool result conversion: `{content:[{type:"text",text}]}` and
  `{content:[...], isError:true}` deserialize into `CallToolResult`;
  a malformed shape returns an internal error, not a panic.
- Notification hub fan-out: two subscribers both receive, a dropped subscriber
  is pruned, no subscribers does not block.

### 11.2 Integration tests (`tests/mcp_http.rs`)

Spawn `env!("CARGO_BIN_EXE_semcode-mcp")` twice, once without
`SEMCODE_MCP_TOKEN` and once with it set:

```
--transport http --bind 127.0.0.1 --port 0 \
--database <tempdir> --git-repo <tempdir> --no-auto-index \
--max-in-flight 2
# optional: SEMCODE_MCP_TOKEN=<32+ chars>
```

Parse the actual port from stderr. Use raw `TcpStream` HTTP like
`docseek/src/http_server.rs:211` (no test-only HTTP client dependency), or add
`rmcp` as a dev-dependency with client features under a separate feature;
prefer the raw version so the test also pins the wire format.

Cases:

1. **Auth configured**: no header → 401; wrong token → 401; duplicate
   `Authorization` → 401; correct token → 200.
2. **Auth not configured**: no header → 200; a stray `Authorization` header is
   ignored; the startup warning is present on stderr.
3. **Host/Origin**: `Host: evil:PORT` → 403; mismatched Origin → 403;
   `Origin: http://127.0.0.1:PORT` → accepted.
4. **Handshake**: `initialize` returns a supported `protocolVersion`
   (`2025-11-25` when requested), `Mcp-Session-Id` in stateful mode and none
   in stateless mode; `notifications/initialized` is accepted.
5. **tools/list**: contains `find_function`, `grep_functions`, `indexing_status`
   (non-lazy); with `--lazy`, contains exactly `list_categories`, `get_tools`,
   `call_tool`; no write-like tool names appear.
6. **tools/call**: `indexing_status` succeeds; unknown tool returns a tool
   result with `isError: true`; `inputResponses` → `-32602`.
7. **SSE**: stateful mode POST response is `text/event-stream` with a
   well-formed `data:` JSON-RPC message; parse it and assert the id matches.
8. **Session lifecycle**: second request without `Mcp-Session-Id` → 400/404
   (whatever rmcp returns; assert it is not 200); DELETE terminates; a request
   after DELETE is rejected.
9. **Gate**: hold two in-flight requests and assert the third POST gets 429.
10. **Body cap**: a POST larger than `--max-request-bytes` → 413.
11. **No secrets/paths**: every response body lacks the token and the temp
    directory path; with a token configured, also assert it is absent from
    stderr.
12. **Concurrency**: two sessions issue `tools/list` and `indexing_status`
    simultaneously; both succeed.
13. **Graceful shutdown**: SIGTERM exits 0 within a bounded time; port closes.

### 11.3 Regression tests

- `cargo test` (existing stdio tests untouched).
- A manual smoke script: start the server, then
  `claude mcp add --transport http semcode http://127.0.0.1:8765/mcp --header "Authorization: Bearer $SEMCODE_MCP_TOKEN"`,
  list tools, call `indexing_status`.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` (mandatory
  in this repo).

### 11.4 Acceptance criteria

- Existing stdio users see no change.
- One HTTP server serves at least two concurrent clients with distinct
  sessions.
- All security negatives above pass.
- The server can be pointed at by an unmodified MCP client over loopback and
  over an SSH tunnel.
- No tool implementation is duplicated: `git diff` shows zero changes inside
  tool handler bodies (only signature/visibility/refactor of the two shared
  helpers).

---

## 12. Documentation updates

- `README.md` — add an "HTTP transport" subsection with the startup command,
  token handling, SSH tunnel example, and client configuration.
- `docs/semcode-mcp.md` — transport section, tool discovery unchanged; document
  the protocol-version difference between stdio and HTTP and the session-mode
  options.
- `docs/semcode-zh.md` — mirror the HTTP section (it currently states the
  server is stdio-only at line 167).
- `docs/claude-semcode-setup.md` — add the HTTP client setup variant.
- Container deployment section: `Dockerfile`, `docker run` with volume mounts
  (index, Git repo, model cache), publishing to host loopback, and why the
  CentOS 7 host does not need a newer toolchain (§5).
- Example client config (header omitted when no token is configured):

```json
{
  "mcpServers": {
    "semcode": {
      "type": "http",
      "url": "http://127.0.0.1:8765/mcp",
      "headers": { "Authorization": "Bearer ${SEMCODE_MCP_TOKEN}" }
    }
  }
}
```

(Exact key support varies by client; the docs should show both the
`claude mcp add --transport http ... --header ...` command and the JSON form,
plus the no-auth variant.)

---

## 13. Implementation plan

### M0 — Decisions and dependency spike (0.5 day)

1. Confirm the container build path for Rust 1.88+ (§5): builder image,
   runtime image, and the `docker run` mounts. Record it in README.
2. Add pinned `rmcp`; run `cargo check --all-targets` on the worktree.
3. Confirm `rmcp` 3.10 API against this repo: `ServerHandler` signatures,
   `CallToolResponse`, `StreamableHttpServerConfig`, `LocalSessionManager`,
   `NeverSessionManager`, `ProtocolVersion::known_up_to`.
4. Do not modify `Cargo.lock` by hand; commit the resolved lock.

### M1 — Transport skeleton (1 day)

1. Create `src/bin/semcode_mcp_http.rs` with `HttpConfig`, `Policy`,
   `single_header`, `protect`, `router`, `serve`.
2. Add the `#[path]` module declaration, CLI fields, validation, and the
   `main` dispatch branch.
3. Bind, print the address, handle `--port 0`, graceful shutdown.
4. Unit tests for `Policy`; one end-to-end `initialize` + `tools/list` test.

### M2 — Protocol bridge (1 day)

1. Extract `instructions()` and `exposed_tools()` from the existing methods.
2. Implement `ServerHandler` (`get_info`, `supported_protocol_versions`,
   `list_tools`, `call_tool`, `get_tool`) and schema/result conversion tests.
3. Verify lazy and non-lazy modes over HTTP produce identical tool sets to the
   stdio methods.

### M3 — Security hardening (0.5–1 day)

1. Gate, body cap, stateless response buffering, timeouts.
2. Negative tests: 401/403/413/429, duplicate headers, Origin suffixes,
   token/path leakage.
3. Test both auth modes: token configured (all requests must authenticate) and
   no token (requests succeed, startup warning is emitted, including for a
   non-loopback bind).

### M4 — Notifications and lifecycle (0.5–1 day)

1. Replace the single-slot sender with `NotificationHub`; update stdio and the
   ~20 test constructors.
2. Forward progress per HTTP session via `on_initialized` + peer; verify two
   sessions both receive or are cleanly dropped.
3. SIGINT/SIGTERM test; session keep-alive behavior.

### M5 — Docs and polish (0.5 day)

1. README, `docs/semcode-mcp.md`, `docs/semcode-zh.md`,
   `docs/claude-semcode-setup.md`.
2. Container docs: `Dockerfile`, `docker run` example, volume and port-publish
   notes, and the CentOS 7 host story (§5).
3. Startup logging and error messages.
4. Manual client smoke test; `cargo fmt`, clippy, full `cargo test`.

Total estimate: **4–5 engineering days**, including the container build and
smoke test, excluding real-client acceptance on the target hosts.

---

## 14. Risks and open questions

| Risk | Impact | Mitigation |
|---|---|---|
| `rmcp` 3.10 requires Rust 1.88 / edition 2024 | Blocks old host toolchains (CentOS 7) | Build and run in a container (§5); pin exact version |
| `rmcp` API churn (it tracks a fast-moving spec) | Bump breaks the adapter | Pin `=3.10.0`; adapter surface is small; tests cover it |
| Tool result shape drifts from `CallToolResult` | Tool calls fail with `-32603` | Conversion tests over representative results; normalize centrally |
| Large tool output over SSE | Memory/latency; proxy buffering | No middleware buffering in stateful mode; existing truncation and page cache; document proxy requirements |
| Cross-session `PageCache` pagination interference | Wrong page for a concurrent client | Document; later namespace by session ID or per-session cache |
| Background indexing on a shared server | Startup CPU/DB writes; progress to all sessions | `--no-auto-index` for read-only deployments; hub fan-out |
| Token leakage in logs/errors | Credential exposure | Hash-only storage; no echo; tests grep bodies for the token |
| Stateless mode loses notifications | Client never sees progress | Document; `indexing_status` is the durable poll; stateful is the default |
| Exposing non-loopback by mistake | Source/history disclosure | Loopback default; prominent startup warning without a token; explicit `--allowed-host`; document SSH tunnel/container publish-to-loopback |
| Duplicate Host/Origin logic differs from rmcp's | Inconsistent rejection | Derive both from one `HttpConfig`; unit-test the policy |
| `--transport http` + `--no-auto-index` with an empty DB | Tools return "database empty"-style errors | Existing behavior; docs should say to index first |

### Resolved decisions (2026-11-19)

1. **Toolchain / CentOS 7**: hosts are not upgraded. Build and run
   `semcode-mcp` in a container with a Rust ≥ 1.88 builder and a matching
   runtime image (§5). Container deployment is the supported path.
2. **Session mode**: **stateful** by default (`Mcp-Session-Id`, GET SSE,
   DELETE). `--stateless-http` remains an optional mode for probes and
   sessionless proxies (§9).
3. **Token policy**: **optional**. If `SEMCODE_MCP_TOKEN` or `--token-file`
   is set, every request must authenticate; if not, the server runs without
   `Authorization` and logs a startup warning. Host/Origin validation always
   applies (§8.3).
4. **Endpoint**: fixed `http://<host>:<port>/mcp`; no `--http-path` flag. No
   legacy `/sse`, no version in the path, no token in the URL (§6.1).

5. **Multi-repository scope**: **single repository per server** for this
   change. Implement Option A semantics now, but shape the HTTP construction
   entry point as a repository list (§15) so path-per-repo routing can be added
   without restructuring `Handler` or `serve`.

---

## 15. Multi-repository deployments

**Current reality: one `semcode-mcp` serves exactly one repository.** This is
not a transport limitation; it is how the database and tool layer are built:

- `Args` has single-valued `--database` and `--git-repo`
  (`src/bin/semcode-mcp.rs:2285`), and `McpServer` owns exactly one
  `Arc<DatabaseManager>` (`:3150`).
- The LanceDB schema has **no repository identifier**. Records are keyed by
  `name`, `file_path`, and `git_sha`; `file_path` is relative to the repository
  root (`src/git_range.rs:1136`), and every git-aware lookup resolves HEAD,
  branches, working-copy checks, and `find_commit` against the single
  `git_repo_path` (`src/database/connection.rs:2065`, `:2225`).
- The indexer requires `--source` to be inside a Git repository
  (`gix::discover`, `src/bin/index.rs:1680–1720`). Pointing it at a parent
  directory containing several repositories does not merge them: Git history
  and SHAs are per-repository, and submodule gitlinks are not regular files.
- No tool takes a repository argument. `semcode-lsp` likewise uses one
  workspace/database (`src/bin/semcode-lsp.rs:403`).
- Docseek's `repo_id` + snapshot model is its own catalog/manifest layer; it
  cannot be reused directly because semcode's schema has no equivalent column.

### Options

| Option | What it is | Cost | Recommendation |
|---|---|---|---|
| **A. One process per repository** | N `semcode-mcp` instances, N databases, N client registrations | Zero code | Works today; fine for 2–3 repos |
| **B. One HTTP process, path per repository** | One axum listener mounts `/repos/<id>/mcp`; each repo gets its own `Arc<McpServer>` and `StreamableHttpService`; tools unchanged | Small (registry + routing + lifecycle) | **Recommended for an HTTP multi-repo deployment** |
| **C. Single endpoint, repo parameter** | Add `repo`/`repo_id` to every tool, or a session-level `use_repo`; dispatch resolves the repo | Medium–large: 19 schemas, lazy mode, page-cache keys, result attribution; cross-repo federated search is a separate design | Only if clients must query several repos through one MCP registration |
| **D. Monorepo** | All code is in one Git repository (subdirectories covered by `--include`) | Zero code | Use this whenever the code actually lives in one repo |

### If Option B is adopted, how it fits this design

1. Replace the single `McpServer` construction with a registry:

   ```rust
   struct RepoConfig { id: String, database: String, git_repo: String, model_path: Option<String> }
   struct RepoHandle { server: Arc<McpServer>, handler: Handler, indexing: JoinHandle<()> }
   struct Registry { repos: BTreeMap<String, RepoHandle> }
   ```

   Configuration: a small `--repo <id>=<path>` repeatable flag (database
   defaults to `<path>/.semcode.db`) or a JSON/TOML file. Validate unique IDs
   and an absolute, existing path per repo.
2. `router` mounts one `StreamableHttpService` per repo:

   ```rust
   for (id, handle) in &registry.repos {
       router = router.nest_service(
           format!("/repos/{id}/mcp"),
           StreamableHttpService::new(
               { let h = handle.handler.clone(); move || Ok(h.clone()) },
               Arc::new(LocalSessionManager::default()),
               config.clone(),
           ),
       );
   }
   ```

   Keep one `McpServer` per repo (one DB, one page cache, one indexing state).
   Do **not** try to share a session manager across services: session IDs must
   map to a single repo for the lifetime of the session.
3. Keep the security middleware global (one token, one Host/Origin policy,
   one in-flight gate) and add a readiness check: a repo whose database cannot
   be opened is logged and omitted from routing, so one broken path does not
   kill the whole endpoint. Return 404 for unknown repo IDs.
4. Resource management matters more than the routing:
   - Each repo opens a LanceDB connection and (unless `--no-auto-index`) runs
     its own background indexing task. Aggregate and log per-repo status.
   - `Vectorizer::new_with_config` currently loads the model from disk on
     **every** semantic query (`src/vectorizer.rs:32–72`) and there is no
     process-wide cache. One process with N repos still loads the model per
     query, not per repo, but concurrent queries multiply both CPU and memory.
     Use the existing global in-flight gate and consider a shared model
     instance or a per-process semantic-query semaphore before serving many
     repos.
   - A repo's index is tied to its `--git-repo` HEAD and history. There is no
     cross-repo commit equivalence: do not merge results from different repos
     into an answer that implies they describe the same revision.
5. Client configuration is one MCP registration per repo URL:
   `http://127.0.0.1:8765/repos/linux/mcp`, `.../repos/bpf/mcp`.
6. Scope decision for this change: **implement Option A semantics first**
   (single repo per server, as designed above) and leave Option B as a
   follow-up. The HTTP module should therefore take a `Vec<RepoConfig>`-shaped
   construction path from day one, even if the first version only accepts one
   repo, so adding routing later does not restructure `Handler` or `serve`.

Option C should only be designed together with a database-schema change
(`repo_id` on every table) or a strict registry-level dispatch layer. It also
raises questions this design cannot answer locally: federated ranking, result
attribution, per-repo model compatibility, and pagination keys that stay valid
across repos. Treat it as a separate project.

---

## 16. Appendix: docseek vs. semcode

| Aspect | docseek | semcode |
|---|---|---|
| Core | transport-independent `mcp_core::Service` | private `McpServer` inside a binary |
| Tools | 5 read-only doc tools | 19 code/git/lore tools + lazy meta-tools |
| Tool result size | capped at 45 KiB in core | 3,000-line truncation + pagination |
| State | model + read sessions, no background task | LanceDB, page cache, background indexing, notifications |
| Session mode | stateless (`legacy_session_mode=false`, JSON responses) | stateful default; stateless opt-in |
| Auth | mandatory `DOCSEEK_BEARER_TOKEN` | optional; enforced whenever `SEMCODE_MCP_TOKEN`/`--token-file` is set |
| Bind | `127.0.0.1` only | loopback default, opt-in wider bind |
| Protocol | negotiates up to 2025-11-25 in tests | stdio hardcodes 2024-11-05; HTTP negotiates ≤ 2025-11-25 |
| MSRV | `rmcp` 3.10 / Rust 1.88 | same requirement; satisfied by building in a container on CentOS 7 hosts |
