// SPDX-License-Identifier: MIT OR Apache-2.0
//! MCP Streamable HTTP transport for `semcode-mcp`.
//!
//! The tool layer is not duplicated here: every request is answered by
//! `McpServer::handle_tool_call` and `McpServer::exposed_tool_schemas`, the
//! same functions the stdio transport uses. This module only adapts them to
//! `rmcp::ServerHandler` and wraps the endpoint in the security policy
//! described in `docs/mcp-streamable-http-design.md`.
//!
//! The endpoint path is fixed at `/mcp`. A reverse proxy that needs a public
//! prefix should rewrite it rather than asking the server to grow a path knob.

use crate::McpServer;
use anyhow::{Context as _, Result};
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use futures::StreamExt as _;
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerConfig, Tool,
    },
    service::{RequestContext, RoleServer},
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData, ServerHandler,
};
use serde_json::{json, Value};
use std::{
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::sync::Semaphore;

/// Path the MCP endpoint is mounted at.
const MCP_PATH: &str = "/mcp";

/// Upper bound on waiting for a buffered (non-SSE) response body.
///
/// Semantic searches can be slow on a cold model cache, so this is far more
/// generous than a health check's timeout. SSE responses are never buffered.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);

/// Transport configuration, produced from the command line by `main`.
pub(crate) struct HttpConfig {
    pub(crate) bind: IpAddr,
    pub(crate) port: u16,
    pub(crate) stateless: bool,
    pub(crate) allowed_hosts: Vec<String>,
    pub(crate) allowed_origins: Vec<String>,
    pub(crate) max_request_bytes: usize,
    pub(crate) max_response_bytes: usize,
    pub(crate) max_in_flight: usize,
    /// `None` disables SSE keep-alive pings.
    pub(crate) sse_keep_alive: Option<Duration>,
    /// `None` means the endpoint is unauthenticated. That is a deliberate
    /// mode: the operator configures a token or relies on the network path.
    pub(crate) token: Option<String>,
}

/// Resolve the optional Bearer token.
///
/// Precedence: `--token-file`, then `SEMCODE_MCP_TOKEN`, then
/// `SEMCODE_MCP_TOKEN_FILE`. The token is never accepted as a command-line
/// argument, because process arguments are visible in `ps` and shell history.
pub(crate) fn resolve_token(token_file: Option<&Path>) -> Result<Option<String>> {
    if let Some(path) = token_file {
        return read_token_file(path);
    }
    if let Ok(token) = std::env::var("SEMCODE_MCP_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            validate_token(token)?;
            return Ok(Some(token.to_string()));
        }
    }
    if let Ok(path) = std::env::var("SEMCODE_MCP_TOKEN_FILE") {
        let path = path.trim();
        if !path.is_empty() {
            return read_token_file(Path::new(path));
        }
    }
    Ok(None)
}

fn read_token_file(path: &Path) -> Result<Option<String>> {
    warn_if_group_or_world_readable(path);
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read token file '{}'", path.display()))?;
    let token = contents.trim();
    validate_token(token)?;
    Ok(Some(token.to_string()))
}

#[cfg(unix)]
fn warn_if_group_or_world_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path) {
        if metadata.permissions().mode() & 0o077 != 0 {
            eprintln!(
                "WARNING: token file '{}' is accessible by group or others; restrict it to 0400",
                path.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_group_or_world_readable(_path: &Path) {}

/// A token must be 32-4096 visible ASCII bytes. Never include the token itself
/// in the error.
fn validate_token(token: &str) -> Result<()> {
    if !(32..=4096).contains(&token.len()) || !token.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(anyhow::anyhow!(
            "HTTP bearer token must be 32-4096 visible ASCII characters"
        ));
    }
    Ok(())
}

/// Header policy: Host and Origin always, Bearer only when a token is set.
struct Policy {
    token_hash: Option<blake3::Hash>,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Rejection {
    Unauthorized,
    InvalidHost,
    InvalidOrigin,
}

impl Policy {
    fn new(
        token: Option<&str>,
        allowed_hosts: Vec<String>,
        allowed_origins: Vec<String>,
    ) -> Result<Self> {
        let token_hash = match token {
            Some(token) => {
                validate_token(token)?;
                Some(blake3::hash(token.as_bytes()))
            }
            None => None,
        };
        Ok(Self {
            token_hash,
            allowed_hosts,
            allowed_origins,
        })
    }

    fn requires_auth(&self) -> bool {
        self.token_hash.is_some()
    }

    fn check(
        &self,
        host: Option<&str>,
        origin: Option<&str>,
        authorization: Option<&str>,
    ) -> Result<(), Rejection> {
        let Some(host) = host else {
            return Err(Rejection::InvalidHost);
        };
        if !self.allowed_hosts.iter().any(|allowed| allowed == host) {
            return Err(Rejection::InvalidHost);
        }
        if let Some(origin) = origin {
            if !self.allowed_origins.iter().any(|allowed| allowed == origin) {
                return Err(Rejection::InvalidOrigin);
            }
        }
        if let Some(expected) = &self.token_hash {
            let Some((scheme, token)) = authorization.and_then(|value| value.split_once(' '))
            else {
                return Err(Rejection::Unauthorized);
            };
            if !scheme.eq_ignore_ascii_case("bearer")
                || validate_token(token).is_err()
                || blake3::hash(token.as_bytes()) != *expected
            {
                return Err(Rejection::Unauthorized);
            }
        }
        Ok(())
    }
}

/// Authorities a client may put in `Host`. Loopback names are always accepted
/// so a container published to host loopback works without extra flags.
fn allowed_hosts_for(bind: IpAddr, port: u16, extra: &[String]) -> Vec<String> {
    let mut hosts = vec![
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ];
    if !bind.is_unspecified() {
        hosts.push(SocketAddr::new(bind, port).to_string());
    }
    if port == 80 {
        hosts.extend(["127.0.0.1", "localhost", "[::1]"].map(str::to_string));
    }
    hosts.extend(extra.iter().cloned());
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Read one header, rejecting duplicates. Duplicates are a smuggling vector:
/// two components may disagree about which value is authoritative.
fn single_header(headers: &HeaderMap, name: header::HeaderName) -> Result<Option<&str>, ()> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    value.to_str().map(Some).map_err(|_| ())
}

#[derive(Clone)]
struct HttpState {
    policy: Arc<Policy>,
    gate: Arc<Semaphore>,
    max_response_bytes: usize,
}

async fn protect(State(state): State<HttpState>, request: Request, next: Next) -> Response {
    let host = match single_header(request.headers(), header::HOST) {
        Ok(value) => value.map(str::to_owned),
        Err(()) => return StatusCode::FORBIDDEN.into_response(),
    };
    let origin = match single_header(request.headers(), header::ORIGIN) {
        Ok(value) => value.map(str::to_owned),
        Err(()) => return StatusCode::FORBIDDEN.into_response(),
    };
    let authorization = if state.policy.requires_auth() {
        match single_header(request.headers(), header::AUTHORIZATION) {
            Ok(value) => value.map(str::to_owned),
            Err(()) => return StatusCode::UNAUTHORIZED.into_response(),
        }
    } else {
        // No token configured: there is nothing to authenticate, so the header
        // is ignored entirely (including duplicates).
        None
    };

    match state
        .policy
        .check(host.as_deref(), origin.as_deref(), authorization.as_deref())
    {
        Ok(()) => {}
        Err(Rejection::Unauthorized) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(Rejection::InvalidHost | Rejection::InvalidOrigin) => {
            return StatusCode::FORBIDDEN.into_response()
        }
    }

    // Only POST carries work. A long-lived GET (SSE) or a DELETE must not
    // consume a slot, or an idle event stream could starve tool calls.
    let permit = if request.method() == Method::POST {
        match state.gate.clone().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                let mut response = StatusCode::TOO_MANY_REQUESTS.into_response();
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
                return response;
            }
        }
    } else {
        None
    };

    let response = next.run(request).await;
    let (mut parts, body) = response.into_parts();
    let is_sse = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"));

    if is_sse {
        // Streaming responses cannot be buffered or size-checked without
        // breaking the transport. Hold the POST permit until the stream ends
        // so the in-flight gate still bounds real work.
        let body = match permit {
            Some(permit) => Body::from_stream(body.into_data_stream().map(move |chunk| {
                let _hold = &permit;
                chunk
            })),
            None => body,
        };
        return Response::from_parts(parts, body);
    }

    drop(permit);
    match tokio::time::timeout(RESPONSE_TIMEOUT, to_bytes(body, state.max_response_bytes)).await {
        Ok(Ok(bytes)) => {
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, Body::from(bytes))
        }
        Ok(Err(_)) => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        Err(_) => StatusCode::GATEWAY_TIMEOUT.into_response(),
    }
}

/// MCP handler backed by the existing `McpServer`.
#[derive(Clone)]
struct Handler {
    server: Arc<McpServer>,
    tools: Arc<Vec<Tool>>,
}

impl Handler {
    /// Parse the tool definitions once. A schema that cannot be represented by
    /// the SDK is a startup error, not a per-request surprise.
    fn new(server: Arc<McpServer>) -> Result<Self> {
        let tools = server
            .exposed_tool_schemas()
            .into_iter()
            .map(|value| {
                serde_json::from_value::<Tool>(value).context("invalid tool schema definition")
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            server,
            tools: Arc::new(tools),
        })
    }
}

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "semcode-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(self.server.instructions())
    }

    /// MCP `2026-07-28` removes the initialize handshake and requires
    /// per-request protocol metadata this server does not implement, so the
    /// HTTP transport negotiates no further than `2025-11-25`.
    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        std::borrow::Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_11_25))
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        if request.is_some_and(|request| request.cursor.is_some()) {
            return Err(ErrorData::invalid_params("cursor is not supported", None));
        }
        Ok(ListToolsResult::with_all_items((*self.tools).clone()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|tool| tool.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // These request shapes require capabilities (elicitation, tasks) that
        // semcode does not advertise; reject them instead of ignoring them.
        if request.input_responses.is_some() || request.request_state.is_some() {
            return Err(ErrorData::invalid_params("unsupported tool request", None));
        }
        let params = json!({
            "name": request.name,
            "arguments": Value::Object(request.arguments.unwrap_or_default()),
        });
        // `McpServer::handle_tool_call` is a legacy stdio-shaped future: some
        // of its paths hold a `&mut dyn Write` across an await, so the whole
        // future is not `Send`. rmcp requires `Send` handler futures. Run the
        // call on a blocking thread against the existing runtime handle and
        // send only the resulting JSON value back across the boundary. The
        // tool layer itself stays untouched; the in-flight gate bounds how many
        // blocking threads this can occupy.
        let server = self.server.clone();
        let result = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(server.handle_tool_call(&params))
        })
        .await
        .map_err(|_| ErrorData::internal_error("tool execution task failed", None))?;
        serde_json::from_value::<CallToolResult>(result)
            .map(Into::into)
            .map_err(|error| {
                eprintln!("HTTP transport produced an invalid tool result: {error}");
                ErrorData::internal_error("invalid tool result", None)
            })
    }
}

fn router(handler: Handler, state: HttpState, config: StreamableHttpServerConfig) -> Router {
    let mcp = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    Router::new()
        .nest_service(MCP_PATH, mcp)
        .layer(middleware::from_fn_with_state(state, protect))
}

async fn wait_for_shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// Serve the MCP Streamable HTTP endpoint until SIGINT/SIGTERM.
pub(crate) async fn serve(server: Arc<McpServer>, cfg: HttpConfig) -> Result<()> {
    let handler = Handler::new(server)?;

    let listener = tokio::net::TcpListener::bind(SocketAddr::new(cfg.bind, cfg.port))
        .await
        .with_context(|| format!("cannot bind {}:{}", cfg.bind, cfg.port))?;
    let address = listener
        .local_addr()
        .context("cannot read listener address")?;

    let allowed_hosts = allowed_hosts_for(cfg.bind, address.port(), &cfg.allowed_hosts);
    let policy = Arc::new(Policy::new(
        cfg.token.as_deref(),
        allowed_hosts,
        cfg.allowed_origins.clone(),
    )?);
    if !policy.requires_auth() {
        eprintln!(
            "WARNING: HTTP MCP endpoint has no authentication configured for {address}; \
             anyone who can reach it can read the indexed source, history, and lore mail"
        );
    }

    let state = HttpState {
        policy,
        gate: Arc::new(Semaphore::new(cfg.max_in_flight)),
        max_response_bytes: cfg.max_response_bytes,
    };
    let server_config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(!cfg.stateless)
        .with_json_response(cfg.stateless)
        .with_max_request_body_bytes(cfg.max_request_bytes)
        .with_sse_keep_alive(cfg.sse_keep_alive)
        .with_allowed_hosts(state.policy.allowed_hosts.clone())
        .with_allowed_origins(state.policy.allowed_origins.clone());
    let shutdown = server_config.cancellation_token.clone();
    let app = router(handler, state, server_config);

    eprintln!("MCP HTTP endpoint: http://{address}{MCP_PATH}");
    eprintln!(
        "Session mode: {}",
        if cfg.stateless {
            "stateless"
        } else {
            "stateful"
        }
    );
    eprintln!(
        "Authentication: {}",
        if cfg.token.is_some() {
            "bearer token required"
        } else {
            "none"
        }
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            wait_for_shutdown().await;
            shutdown.cancel();
        })
        .await
        .context("HTTP server failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn hosts() -> Vec<String> {
        vec!["127.0.0.1:8765".into(), "localhost:8765".into()]
    }

    #[test]
    fn requires_bearer_only_when_a_token_is_configured() {
        let policy = Policy::new(Some(TOKEN), hosts(), vec![]).unwrap();
        assert!(policy.requires_auth());
        assert_eq!(
            policy.check(Some("127.0.0.1:8765"), None, None),
            Err(Rejection::Unauthorized)
        );
        assert_eq!(
            policy.check(Some("127.0.0.1:8765"), None, Some("Bearer wrong")),
            Err(Rejection::Unauthorized)
        );
        assert!(policy
            .check(
                Some("127.0.0.1:8765"),
                None,
                Some(&format!("bearer {TOKEN}"))
            )
            .is_ok());
    }

    #[test]
    fn accepts_requests_without_authorization_when_no_token_is_configured() {
        let policy = Policy::new(None, hosts(), vec![]).unwrap();
        assert!(!policy.requires_auth());
        assert!(policy.check(Some("localhost:8765"), None, None).is_ok());
    }

    #[test]
    fn rejects_unknown_hosts_and_origin_suffixes() {
        let policy = Policy::new(None, hosts(), vec!["http://localhost:8765".into()]).unwrap();
        assert_eq!(
            policy.check(Some("evil.example:8765"), None, None),
            Err(Rejection::InvalidHost)
        );
        assert_eq!(policy.check(None, None, None), Err(Rejection::InvalidHost));
        assert_eq!(
            policy.check(
                Some("localhost:8765"),
                Some("http://localhost:8765.evil"),
                None
            ),
            Err(Rejection::InvalidOrigin)
        );
        assert!(policy
            .check(Some("localhost:8765"), Some("http://localhost:8765"), None)
            .is_ok());
    }

    #[test]
    fn duplicate_headers_are_detected() {
        let mut headers = HeaderMap::new();
        headers.append(header::AUTHORIZATION, HeaderValue::from_static("Bearer a"));
        headers.append(header::AUTHORIZATION, HeaderValue::from_static("Bearer b"));
        assert!(single_header(&headers, header::AUTHORIZATION).is_err());
        assert!(single_header(&headers, header::HOST).unwrap().is_none());
    }

    #[test]
    fn token_length_and_characters_are_validated() {
        assert!(validate_token("short").is_err());
        assert!(validate_token(&"a".repeat(31)).is_err());
        assert!(validate_token(&"a".repeat(32)).is_ok());
        assert!(validate_token(&"a".repeat(4097)).is_err());
        assert!(validate_token(&format!("{} token", "a".repeat(32))).is_err());
    }

    #[test]
    fn loopback_names_are_always_allowed() {
        let hosts = allowed_hosts_for("0.0.0.0".parse().unwrap(), 9000, &[]);
        assert!(hosts.contains(&"127.0.0.1:9000".to_string()));
        assert!(hosts.contains(&"localhost:9000".to_string()));
        assert!(hosts.contains(&"[::1]:9000".to_string()));
        assert!(!hosts.iter().any(|host| host.starts_with("0.0.0.0")));
    }

    #[test]
    fn every_advertised_tool_schema_parses_with_the_official_sdk() {
        for schemas in [crate::lazy_tool_schemas(), crate::get_all_tool_schemas()] {
            assert!(!schemas.is_empty());
            for schema in schemas {
                let tool: Tool = serde_json::from_value(schema.clone())
                    .unwrap_or_else(|error| panic!("{error}: {schema}"));
                assert!(!tool.name.is_empty());
            }
        }
    }
}
