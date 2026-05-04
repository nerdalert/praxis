// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! MCP gateway filter: tool catalog, prefix management, routing, and sessions.

pub(crate) mod config;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests"
)]
mod tests;

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use tracing::{debug, trace};

use self::config::{McpGatewayConfig, McpServerConfig, build_config};
use crate::{
    FilterAction, FilterError, Rejection,
    body::{BodyAccess, BodyMode},
    factory::parse_filter_config,
    filter::{HttpFilter, HttpFilterContext},
    agentic::state::{GatewaySession, LocalStateStore, ToolEntry},
    builtins::http::payload_processing::json_rpc::{
        config::JsonRpcConfig,
        envelope::{JsonRpcIdKind, parse_json_rpc_envelope},
    },
};

// -----------------------------------------------------------------------------
// McpGatewayFilter
// -----------------------------------------------------------------------------

/// MCP gateway filter that aggregates tool catalogs from multiple backend
/// MCP servers, handles `initialize`/`tools/list`/`tools/call`/`ping`
/// directly, and routes `tools/call` to the correct backend with prefix
/// stripping and backend session tracking.
///
/// # YAML
///
/// ```yaml
/// filter: mcp_gateway
/// path: /mcp
/// max_body_bytes: 65536
/// servers:
///   - name: weather
///     cluster: weather-mcp
///     path: /mcp
///     tool_prefix: weather_
///     tools:
///       - name: get_weather
///         description: Get current weather
///   - name: calendar
///     cluster: calendar-mcp
///     path: /mcp
///     tool_prefix: cal_
///     tools:
///       - name: create_event
///         description: Create a calendar event
/// ```
pub(crate) struct McpGatewayFilter {
    /// Parsed configuration.
    config: McpGatewayConfig,
    /// JSON-RPC config for shared parser.
    json_rpc_config: JsonRpcConfig,
    /// Maximum body bytes for `StreamBuffer`.
    max_body_bytes: usize,
    /// Static tool catalog built from config.
    catalog: Vec<ToolEntry>,
    /// Local state store for sessions.
    state: LocalStateStore,
}

impl McpGatewayFilter {
    /// Create from YAML config.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if the YAML config is invalid.
    pub(crate) fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: McpGatewayConfig = parse_filter_config("mcp_gateway", config)?;
        let (max_body_bytes, validated_config) = build_config(cfg)?;

        let json_rpc_config = build_json_rpc_config(max_body_bytes);
        let catalog = build_static_catalog(&validated_config.servers);

        Ok(Box::new(Self {
            config: validated_config,
            json_rpc_config,
            max_body_bytes,
            catalog,
            state: LocalStateStore::new(),
        }))
    }
}

#[async_trait]
impl HttpFilter for McpGatewayFilter {
    fn name(&self) -> &'static str {
        "mcp_gateway"
    }

    fn request_body_access(&self) -> BodyAccess {
        BodyAccess::ReadWrite
    }

    fn request_body_mode(&self) -> BodyMode {
        BodyMode::StreamBuffer {
            max_bytes: Some(self.max_body_bytes),
        }
    }

    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        if ctx.request.method == http::Method::DELETE {
            return self.handle_delete(ctx);
        }

        Ok(FilterAction::Continue)
    }

    async fn on_request_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        // DELETE is handled in on_request; skip body parsing
        if ctx.request.method == http::Method::DELETE {
            return Ok(FilterAction::Continue);
        }

        // Clone the chunk so we can pass `body` mutably to tools/call
        let Some(chunk) = body.as_ref().cloned() else {
            return Ok(FilterAction::Continue);
        };

        let Ok(Some(envelope)) = parse_json_rpc_envelope(&chunk, &self.json_rpc_config) else {
            return Ok(FilterAction::Reject(Rejection::status(400)));
        };

        let Some(method_str) = &envelope.method else {
            return Ok(FilterAction::Reject(Rejection::status(400)));
        };

        ctx.set_metadata("json_rpc.method", method_str.clone());
        ctx.set_metadata("mcp.method", method_str.clone());

        match method_str.as_str() {
            "initialize" => self.handle_initialize(ctx, &chunk, &envelope),
            "notifications/initialized" => self.handle_notifications_initialized(ctx),
            "tools/list" => self.handle_tools_list(ctx, &envelope),
            "tools/call" => self.handle_tools_call(ctx, body, &chunk, &envelope),
            "ping" => self.handle_ping(&envelope),
            _ => Ok(FilterAction::Release),
        }
    }

    async fn on_response(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        let Some(resp) = &ctx.response_header else {
            return Ok(FilterAction::Continue);
        };

        let gw_sid = ctx.get_metadata("mcp.session_id").map(str::to_owned);
        let server = ctx.get_metadata("mcp.server").map(str::to_owned);

        // Capture backend MCP-Session-Id from successful responses
        if resp.status.is_success() {
            if let (Some(gw_sid), Some(server)) = (&gw_sid, &server) {
                if let Some(backend_sid) = resp.headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) {
                    debug!(
                        server = %server,
                        "captured backend MCP-Session-Id from response"
                    );
                    self.state.put_backend_session(gw_sid, server, backend_sid);
                }
            }
        }

        // Handle backend 404 -> invalidate backend session
        if resp.status == http::StatusCode::NOT_FOUND {
            if let (Some(gw_sid), Some(server)) = (&gw_sid, &server) {
                debug!(
                    server = %server,
                    "backend returned 404, removing backend session mapping"
                );
                self.state.remove_backend_session(gw_sid, server);
            }
        }

        Ok(FilterAction::Continue)
    }
}

// -----------------------------------------------------------------------------
// Request Handlers
// -----------------------------------------------------------------------------

impl McpGatewayFilter {
    /// Handle `DELETE` for session cleanup.
    #[allow(clippy::unnecessary_wraps, reason = "returns Result to match trait-required caller signature")]
    fn handle_delete(&self, ctx: &HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        if let Some(session_id) = ctx
            .request
            .headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            debug!(session_id_len = session_id.len(), "MCP DELETE session");
            self.state.delete_gateway_session(session_id);
            return Ok(FilterAction::Reject(Rejection::status(204)));
        }
        Ok(FilterAction::Reject(Rejection::status(400)))
    }

    /// Handle `initialize` request by returning gateway capabilities
    /// and creating a gateway session.
    #[allow(clippy::unnecessary_wraps, reason = "returns Result to match trait-required caller signature")]
    fn handle_initialize(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &[u8],
        envelope: &crate::builtins::http::payload_processing::json_rpc::envelope::JsonRpcEnvelope,
    ) -> Result<FilterAction, FilterError> {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body)
            && let Some(version) = value
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|v| v.as_str())
        {
            ctx.set_metadata("mcp.protocol_version", version.to_owned());
        }

        let id_json = format_id_json(envelope);
        let gateway_session_id = generate_session_id();
        ctx.set_metadata("mcp.session_id", gateway_session_id.clone());

        self.state.put_gateway_session(GatewaySession {
            session_id: gateway_session_id.clone(),
            created_at: Instant::now(),
            last_used: Instant::now(),
            protocol_version: ctx.get_metadata("mcp.protocol_version").map(str::to_owned),
        });

        let response_body = format!(
            r#"{{"jsonrpc":"2.0","id":{id_json},"result":{{"protocolVersion":"2025-03-26","capabilities":{{"tools":{{"listChanged":false}}}},"serverInfo":{{"name":"praxis-mcp-gateway","version":"0.1.0"}}}}}}"#,
        );

        Ok(FilterAction::Reject(
            Rejection::status(200)
                .with_header("content-type", "application/json")
                .with_header("mcp-session-id", &gateway_session_id)
                .with_body(Bytes::from(response_body)),
        ))
    }

    /// Handle `notifications/initialized` by touching the session.
    #[allow(clippy::unnecessary_wraps, reason = "returns Result to match trait-required caller signature")]
    fn handle_notifications_initialized(
        &self,
        ctx: &HttpFilterContext<'_>,
    ) -> Result<FilterAction, FilterError> {
        if let Some(session_id) = ctx
            .request
            .headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            && self.state.get_gateway_session(session_id).is_some()
        {
            self.state.touch_gateway_session(session_id);
        }
        Ok(FilterAction::Reject(Rejection::status(204)))
    }

    /// Handle `tools/list` by returning the aggregated catalog.
    #[allow(
        clippy::unnecessary_wraps,
        clippy::too_many_lines,
        reason = "returns Result to match trait-required caller signature; logic is sequential and clear"
    )]
    fn handle_tools_list(
        &self,
        ctx: &HttpFilterContext<'_>,
        envelope: &crate::builtins::http::payload_processing::json_rpc::envelope::JsonRpcEnvelope,
    ) -> Result<FilterAction, FilterError> {
        if let Some(session_id) = ctx
            .request
            .headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            self.state.touch_gateway_session(session_id);
        }

        let id_json = format_id_json(envelope);

        let tools: Vec<serde_json::Value> = self
            .catalog
            .iter()
            .map(|t| {
                let mut tool = serde_json::Map::new();
                tool.insert("name".to_owned(), serde_json::Value::String(t.exposed_name.clone()));
                if let Some(desc) = &t.description {
                    tool.insert("description".to_owned(), serde_json::Value::String(desc.clone()));
                }
                if !t.schema.is_null() {
                    tool.insert("inputSchema".to_owned(), t.schema.clone());
                }
                serde_json::Value::Object(tool)
            })
            .collect();

        let tools_json = serde_json::to_string(&tools).unwrap_or_else(|_| "[]".to_owned());
        let response_body = format!(
            r#"{{"jsonrpc":"2.0","id":{id_json},"result":{{"tools":{tools_json}}}}}"#,
        );

        trace!(tool_count = self.catalog.len(), "serving aggregated tools/list");

        Ok(FilterAction::Reject(
            Rejection::status(200)
                .with_header("content-type", "application/json")
                .with_body(Bytes::from(response_body)),
        ))
    }

    /// Handle `tools/call` by routing to the correct backend with prefix
    /// stripping.
    #[allow(
        clippy::unnecessary_wraps,
        clippy::too_many_lines,
        reason = "returns Result to match trait-required caller signature; sequential routing logic"
    )]
    fn handle_tools_call(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        raw_body: &[u8],
        envelope: &crate::builtins::http::payload_processing::json_rpc::envelope::JsonRpcEnvelope,
    ) -> Result<FilterAction, FilterError> {
        if let Some(session_id) = ctx
            .request
            .headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            self.state.touch_gateway_session(session_id);
            ctx.set_metadata("mcp.session_id", session_id.to_owned());
        }

        let tool_name = extract_tool_name(raw_body);
        let Some(tool_name) = tool_name else {
            let id_json = format_id_json(envelope);
            let error_body = format!(
                r#"{{"jsonrpc":"2.0","error":{{"code":-32602,"message":"missing params.name"}},"id":{id_json}}}"#,
            );
            return Ok(FilterAction::Reject(
                Rejection::status(400)
                    .with_header("content-type", "application/json")
                    .with_body(Bytes::from(error_body)),
            ));
        };

        ctx.set_metadata("mcp.name", tool_name.clone());

        let tool_entry = self.catalog.iter().find(|t| t.exposed_name == tool_name);
        let Some(tool_entry) = tool_entry else {
            let id_json = format_id_json(envelope);
            let error_body = format!(
                r#"{{"jsonrpc":"2.0","error":{{"code":-32602,"message":"unknown tool: {tool_name}"}},"id":{id_json}}}"#,
            );
            return Ok(FilterAction::Reject(
                Rejection::status(400)
                    .with_header("content-type", "application/json")
                    .with_body(Bytes::from(error_body)),
            ));
        };

        ctx.set_metadata("mcp.server", tool_entry.server_name.clone());
        ctx.cluster = Some(Arc::from(find_cluster_for_server(&self.config.servers, &tool_entry.server_name)));

        if let Some(server_config) = self.config.servers.iter().find(|s| s.name == tool_entry.server_name) {
            ctx.rewritten_path = Some(server_config.path.clone());
        }

        // Strip prefix from tool name in body when exposed != original
        if tool_entry.exposed_name != tool_entry.original_name
            && let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(raw_body)
        {
            if let Some(params) = value.get_mut("params")
                && let Some(name) = params.get_mut("name")
            {
                *name = serde_json::Value::String(tool_entry.original_name.clone());
            }
            if let Ok(new_body) = serde_json::to_vec(&value) {
                *body = Some(Bytes::from(new_body));
            }
        }

        // Add backend session header if we have one
        if let Some(gw_sid) = ctx.get_metadata("mcp.session_id").map(str::to_owned)
            && let Some(backend_sid) = self.state.get_backend_session(&gw_sid, &tool_entry.server_name)
        {
            ctx.extra_request_headers.push((
                Cow::Borrowed("mcp-session-id"),
                backend_sid,
            ));
        }

        debug!(
            exposed_tool = %tool_entry.exposed_name,
            original_tool = %tool_entry.original_name,
            server = %tool_entry.server_name,
            "routing MCP tools/call"
        );

        Ok(FilterAction::Release)
    }

    /// Handle `ping` by returning an empty result.
    #[allow(clippy::unnecessary_wraps, reason = "returns Result to match trait-required caller signature")]
    #[allow(clippy::unused_self, reason = "method on McpGatewayFilter for consistency with other handlers")]
    fn handle_ping(
        &self,
        envelope: &crate::builtins::http::payload_processing::json_rpc::envelope::JsonRpcEnvelope,
    ) -> Result<FilterAction, FilterError> {
        let id_json = format_id_json(envelope);
        let response_body = format!(r#"{{"jsonrpc":"2.0","id":{id_json},"result":{{}}}}"#);

        Ok(FilterAction::Reject(
            Rejection::status(200)
                .with_header("content-type", "application/json")
                .with_body(Bytes::from(response_body)),
        ))
    }
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// Build a `JsonRpcConfig` for the shared parser with gateway-appropriate defaults.
fn build_json_rpc_config(max_body_bytes: usize) -> JsonRpcConfig {
    use crate::builtins::http::payload_processing::json_rpc::config::{
        BatchPolicy, InvalidJsonRpcBehavior, JsonRpcHeaders,
    };

    JsonRpcConfig {
        max_body_bytes,
        batch_policy: BatchPolicy::Reject,
        on_invalid: InvalidJsonRpcBehavior::Continue,
        headers: JsonRpcHeaders {
            method: None,
            id: None,
            kind: None,
        },
    }
}

/// Build a static tool catalog from configured servers.
fn build_static_catalog(servers: &[McpServerConfig]) -> Vec<ToolEntry> {
    let mut catalog = Vec::new();
    for server in servers {
        for tool in &server.tools {
            let exposed_name = if let Some(prefix) = &server.tool_prefix {
                format!("{prefix}{}", tool.name)
            } else {
                tool.name.clone()
            };
            catalog.push(ToolEntry {
                exposed_name,
                original_name: tool.name.clone(),
                server_name: server.name.clone(),
                schema: tool.schema.clone().unwrap_or(serde_json::Value::Null),
                description: tool.description.clone(),
            });
        }
    }
    catalog
}

/// Find the cluster name for a server.
fn find_cluster_for_server(servers: &[McpServerConfig], server_name: &str) -> String {
    servers
        .iter()
        .find(|s| s.name == server_name)
        .map_or_else(|| server_name.to_owned(), |s| s.cluster.clone())
}

/// Extract tool name from request body.
fn extract_tool_name(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value
        .get("params")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_owned)
}

/// Format the JSON-RPC `id` field for response serialization.
///
/// Uses `serde_json` for string IDs to ensure special characters
/// (quotes, backslashes, control chars) are properly escaped.
fn format_id_json(
    envelope: &crate::builtins::http::payload_processing::json_rpc::envelope::JsonRpcEnvelope,
) -> String {
    let id = envelope.id.as_deref().unwrap_or("null");
    if envelope.id_kind == JsonRpcIdKind::String {
        serde_json::to_string(id).unwrap_or_else(|_| "null".to_owned())
    } else {
        id.to_owned()
    }
}

/// Generate a gateway session ID using the current timestamp.
fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("gw-{timestamp:x}")
}
