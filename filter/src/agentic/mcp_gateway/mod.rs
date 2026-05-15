// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! MCP gateway filter: static tool catalog, prefix management, and broker
//! behavior for `initialize`, `tools/list`, `ping`, and `notifications`.

pub(crate) mod config;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::err_expect,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::needless_raw_strings,
    clippy::needless_raw_string_hashes,
    reason = "tests"
)]
mod tests;

use async_trait::async_trait;
use bytes::Bytes;
use tracing::{debug, trace};

use self::config::{CatalogTool, McpGatewayConfig, build_config};
use crate::{
    FilterAction, FilterError, Rejection,
    body::{BodyAccess, BodyMode},
    builtins::http::payload_processing::json_rpc::{
        config::JsonRpcConfig,
        contains_control_chars,
        envelope::{JsonRpcEnvelope, JsonRpcIdKind, JsonRpcKind, parse_json_rpc_value},
    },
    factory::parse_filter_config,
    filter::{HttpFilter, HttpFilterContext},
};

// -----------------------------------------------------------------------------
// McpGatewayFilter
// -----------------------------------------------------------------------------

/// MCP gateway filter that aggregates tool catalogs from multiple backend
/// MCP servers and handles `initialize`, `tools/list`, `tools/call`, `ping`,
/// and `notifications/initialized` directly as a static broker.
///
/// This first gateway change short-circuits all methods: no request is
/// forwarded to backends. `tools/call` returns a controlled `-32601` error
/// until backend routing is added.
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
    /// Shared JSON-RPC parser configuration.
    json_rpc_config: JsonRpcConfig,
    /// Maximum body bytes for `StreamBuffer`.
    max_body_bytes: usize,
    /// Public path this gateway handles (e.g. `/mcp`).
    public_path: String,
    /// Static tool catalog built from config.
    catalog: Vec<CatalogTool>,
}

impl McpGatewayFilter {
    /// Create a filter from parsed YAML config.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if the YAML config is invalid or if
    /// the static tool catalog cannot be serialized.
    pub(crate) fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: McpGatewayConfig = parse_filter_config("mcp_gateway", config)?;
        let (validated, catalog) = build_config(cfg)?;

        let json_rpc_config = build_json_rpc_config(validated.max_body_bytes);

        Ok(Box::new(Self {
            json_rpc_config,
            max_body_bytes: validated.max_body_bytes,
            public_path: validated.path.clone(),
            catalog,
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
        if !request_path_matches(&ctx.request.uri, &self.public_path) {
            return Ok(FilterAction::Reject(Rejection::status(404)));
        }

        match ctx.request.method {
            http::Method::POST => Ok(FilterAction::Continue),
            http::Method::DELETE => Ok(handle_delete(ctx)),
            _ => Ok(FilterAction::Reject(Rejection::status(405))),
        }
    }

    async fn on_request_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        if ctx.request.method != http::Method::POST {
            return Ok(FilterAction::Continue);
        }

        if !request_path_matches(&ctx.request.uri, &self.public_path) {
            return Ok(FilterAction::Reject(Rejection::status(404)));
        }

        if !end_of_stream {
            return Ok(FilterAction::Continue);
        }

        let Some(chunk) = body.as_ref() else {
            return Ok(FilterAction::Continue);
        };

        let Ok(value) = serde_json::from_slice::<serde_json::Value>(chunk) else {
            return Ok(FilterAction::Reject(Rejection::status(400)));
        };

        let Ok(Some(envelope)) = parse_json_rpc_value(&value, &self.json_rpc_config) else {
            return Ok(FilterAction::Reject(Rejection::status(400)));
        };

        let Some(ref method_str) = envelope.method else {
            return Ok(FilterAction::Reject(Rejection::status(400)));
        };

        if !contains_control_chars(method_str) {
            ctx.set_metadata("json_rpc.method", method_str.clone());
            ctx.set_metadata("mcp.method", method_str.clone());
        }

        dispatch_method(ctx, &self.catalog, &value, &envelope, method_str)
    }
}

// -----------------------------------------------------------------------------
// Method Dispatch
// -----------------------------------------------------------------------------

/// Maps a JSON-RPC method to the gateway handler that owns it.
/// Never returns [`FilterAction::Release`] — all paths produce
/// a terminal synthetic response.
fn dispatch_method(
    ctx: &mut HttpFilterContext<'_>,
    catalog: &[CatalogTool],
    value: &serde_json::Value,
    envelope: &JsonRpcEnvelope,
    method_str: &str,
) -> Result<FilterAction, FilterError> {
    if method_str.starts_with("notifications/") {
        return Ok(handle_notification(envelope));
    }

    if !has_valid_request_id(envelope) {
        return Ok(invalid_request_action(envelope));
    }

    let action = match method_str {
        "initialize" => handle_initialize(ctx, value, envelope),
        "tools/list" => handle_tools_list(catalog, envelope)?,
        "tools/call" => json_rpc_error_action(envelope, -32601, "method not yet supported"),
        "ping" => handle_ping(envelope),
        _ => {
            debug!(method_len = method_str.len(), "unsupported MCP method");
            json_rpc_error_action(envelope, -32601, "method not found")
        },
    };

    Ok(action)
}

/// MCP notifications are one-way messages, so successful handling must not
/// produce a JSON-RPC response body.
fn handle_notification(envelope: &JsonRpcEnvelope) -> FilterAction {
    if matches!(envelope.kind, JsonRpcKind::Notification) && matches!(envelope.id_kind, JsonRpcIdKind::Missing) {
        FilterAction::Reject(Rejection::status(202))
    } else {
        invalid_request_action(envelope)
    }
}

/// MCP request ids are narrower than JSON-RPC's parser accepts.
fn has_valid_request_id(envelope: &JsonRpcEnvelope) -> bool {
    matches!(envelope.id_kind, JsonRpcIdKind::String | JsonRpcIdKind::Integer)
}

/// Invalid request responses use id `null` when the client omitted or nulled
/// the request id, matching JSON-RPC error-envelope conventions.
fn invalid_request_action(envelope: &JsonRpcEnvelope) -> FilterAction {
    let id_json = match envelope.id_kind {
        JsonRpcIdKind::String | JsonRpcIdKind::Integer => format_id_json(envelope),
        JsonRpcIdKind::Number | JsonRpcIdKind::Null | JsonRpcIdKind::Missing => "null".to_owned(),
    };
    json_rpc_error_action_with_id(&id_json, -32600, "invalid request")
}

// -----------------------------------------------------------------------------
// Request Handlers
// -----------------------------------------------------------------------------

/// Returns 204 when a valid `Mcp-Session-Id` header is present, 400 otherwise.
fn handle_delete(ctx: &HttpFilterContext<'_>) -> FilterAction {
    if ctx
        .request
        .headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .is_some()
    {
        FilterAction::Reject(Rejection::status(204))
    } else {
        FilterAction::Reject(Rejection::status(400))
    }
}

/// Generates a new gateway session and returns MCP capabilities.
/// Does not initialize backends — that belongs to follow-up backend session work.
fn handle_initialize(
    ctx: &mut HttpFilterContext<'_>,
    value: &serde_json::Value,
    envelope: &JsonRpcEnvelope,
) -> FilterAction {
    if let Some(version) = value
        .get("params")
        .and_then(|p| p.get("protocolVersion"))
        .and_then(|v| v.as_str())
        && !contains_control_chars(version)
    {
        ctx.set_metadata("mcp.protocol_version", version.to_owned());
    }

    let id_json = format_id_json(envelope);
    let session_id = generate_session_id();

    debug!(session_id_len = session_id.len(), "gateway initialize");
    ctx.set_metadata("mcp.session_id", session_id.clone());

    let response_body = format!(
        r#"{{"jsonrpc":"2.0","id":{id_json},"result":{{"protocolVersion":"2025-03-26","capabilities":{{"tools":{{"listChanged":false}}}},"serverInfo":{{"name":"praxis-mcp-gateway","version":"0.1.0"}}}}}}"#,
    );

    FilterAction::Reject(
        Rejection::status(200)
            .with_header("content-type", "application/json")
            .with_header("mcp-session-id", &session_id)
            .with_body(Bytes::from(response_body)),
    )
}

/// Returns the aggregated static catalog. Dynamic backend discovery
/// and per-identity filtering belong to later PRs.
fn handle_tools_list(catalog: &[CatalogTool], envelope: &JsonRpcEnvelope) -> Result<FilterAction, FilterError> {
    let tools_json = serialize_catalog(catalog)?;
    let id_json = format_id_json(envelope);
    let response_body = format!(r#"{{"jsonrpc":"2.0","id":{id_json},"result":{{"tools":{tools_json}}}}}"#,);

    trace!(tool_count = catalog.len(), "serving aggregated tools/list");

    Ok(FilterAction::Reject(
        Rejection::status(200)
            .with_header("content-type", "application/json")
            .with_body(Bytes::from(response_body)),
    ))
}

/// Fails at request time if serialization fails, so callers must
/// return a controlled error rather than silently degrading.
fn serialize_catalog(catalog: &[CatalogTool]) -> Result<String, FilterError> {
    let tools: Vec<serde_json::Value> = catalog.iter().map(catalog_tool_to_json).collect();
    serde_json::to_string(&tools)
        .map_err(|e| FilterError::from(format!("mcp_gateway: failed to serialize tool catalog: {e}")))
}

/// Produces the MCP tool object shape (`name`, optional `description`
/// and `inputSchema`) expected by `tools/list` responses.
fn catalog_tool_to_json(tool: &CatalogTool) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("name".to_owned(), serde_json::Value::String(tool.exposed_name.clone()));
    if let Some(ref desc) = tool.description {
        obj.insert("description".to_owned(), serde_json::Value::String(desc.clone()));
    }
    obj.insert("inputSchema".to_owned(), tool.input_schema.clone());
    if let Some(ref annotations) = tool.annotations {
        obj.insert("annotations".to_owned(), annotations.clone());
    }
    serde_json::Value::Object(obj)
}

/// Returns `{"result":{}}` with the caller's JSON-RPC id preserved.
fn handle_ping(envelope: &JsonRpcEnvelope) -> FilterAction {
    let id_json = format_id_json(envelope);
    let response_body = format!(r#"{{"jsonrpc":"2.0","id":{id_json},"result":{{}}}}"#);

    FilterAction::Reject(
        Rejection::status(200)
            .with_header("content-type", "application/json")
            .with_body(Bytes::from(response_body)),
    )
}

// -----------------------------------------------------------------------------
// JSON-RPC Helpers
// -----------------------------------------------------------------------------

/// Format the JSON-RPC `id` field for response serialization.
///
/// String ids are escaped with [`serde_json::to_string`] so special
/// characters (quotes, backslashes, control chars) produce valid JSON.
fn format_id_json(envelope: &JsonRpcEnvelope) -> String {
    let id = envelope.id.as_deref().unwrap_or("null");
    match envelope.id_kind {
        JsonRpcIdKind::String => serde_json::to_string(id).unwrap_or_else(|_| "null".to_owned()),
        JsonRpcIdKind::Integer | JsonRpcIdKind::Number => id.to_owned(),
        JsonRpcIdKind::Null | JsonRpcIdKind::Missing => "null".to_owned(),
    }
}

/// Build a JSON-RPC error [`FilterAction::Reject`] response.
///
/// The message is JSON-escaped so future caller-supplied values
/// (e.g. tool names from backend routing) cannot break the response envelope.
fn json_rpc_error_action(envelope: &JsonRpcEnvelope, code: i32, message: &str) -> FilterAction {
    let id_json = format_id_json(envelope);
    json_rpc_error_action_with_id(&id_json, code, message)
}

/// Some protocol errors cannot safely reuse the parsed request id.
fn json_rpc_error_action_with_id(id_json: &str, code: i32, message: &str) -> FilterAction {
    let message_json = serde_json::to_string(message).unwrap_or_else(|_| "\"internal error\"".to_owned());
    let body = Bytes::from(format!(
        r#"{{"jsonrpc":"2.0","error":{{"code":{code},"message":{message_json}}},"id":{id_json}}}"#,
    ));
    FilterAction::Reject(
        Rejection::status(400)
            .with_header("content-type", "application/json")
            .with_body(body),
    )
}

// -----------------------------------------------------------------------------
// Path Matching
// -----------------------------------------------------------------------------

/// Returns `true` when the request URI path matches the configured gateway
/// path. Uses exact match on the path component only.
fn request_path_matches(uri: &http::Uri, public_path: &str) -> bool {
    uri.path() == public_path
}

// -----------------------------------------------------------------------------
// Session ID
// -----------------------------------------------------------------------------

/// Generate a cryptographically random gateway session ID.
fn generate_session_id() -> String {
    let bytes: [u8; 16] = rand::random();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("gw-{hex}")
}

// -----------------------------------------------------------------------------
// Shared Parser Config
// -----------------------------------------------------------------------------

/// Build a [`JsonRpcConfig`] for the shared parser with gateway-appropriate
/// defaults.
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
