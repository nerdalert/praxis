// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Responses API orchestrator filter.
//!
//! Terminal request-phase filter that owns the Responses
//! model/tool/state loop. It buffers the request body,
//! performs inference and tool calls as subrequests, and
//! returns a local Responses-shaped response.
//!
//! This filter does **not** use the normal upstream proxy
//! path. It returns a local response through
//! [`FilterAction::Reject`] with status 200, following
//! the existing `static_response` convention.
//!
//! [`FilterAction::Reject`]: crate::FilterAction::Reject

mod state;

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use bytes::Bytes;
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use super::responses::parser::DetectedFunctionCall;
use crate::{
    actions::{FilterAction, Rejection},
    body::{BodyAccess, BodyMode},
    factory::parse_filter_config,
    filter::{FilterError, HttpFilter, HttpFilterContext},
};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Default maximum request body size.
const DEFAULT_MAX_BODY_BYTES: usize = 1_048_576; // 1 MiB

/// Default maximum orchestration loop iterations.
const DEFAULT_MAX_ITERATIONS: u32 = 10;

/// Default subrequest timeout in milliseconds.
const DEFAULT_TIMEOUT_MS: u64 = 30_000; // 30 s

/// Default backend path for model inference.
const DEFAULT_BACKEND_PATH: &str = "/v1/responses";

/// Default backend path for tool execution.
const DEFAULT_TOOL_PATH: &str = "/tool";

/// Default maximum model response bytes.
const DEFAULT_MAX_MODEL_RESPONSE_BYTES: usize = 10_485_760; // 10 MiB

/// Default maximum tool response bytes.
const DEFAULT_MAX_TOOL_RESPONSE_BYTES: usize = 1_048_576; // 1 MiB

// -----------------------------------------------------------------------------
// Config
// -----------------------------------------------------------------------------

/// Deserialized YAML config for the orchestrator.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponsesOrchestratorConfig {
    /// Maximum request body bytes to buffer.
    #[serde(default = "default_max_body_bytes")]
    max_body_bytes: usize,

    /// Maximum model/tool loop iterations.
    #[serde(default = "default_max_iterations")]
    max_iterations: u32,

    /// Maximum model response body bytes.
    #[serde(default = "default_max_model_response_bytes")]
    max_model_response_bytes: usize,

    /// Maximum tool response body bytes.
    #[serde(default = "default_max_tool_response_bytes")]
    max_tool_response_bytes: usize,

    /// Model backend definitions keyed by model name.
    #[serde(default)]
    models: HashMap<String, ModelBackendConfig>,

    /// Subrequest timeout in milliseconds.
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,

    /// Tool output guardrail configuration.
    #[serde(default)]
    tool_output_guardrails: Option<ToolOutputGuardrailsConfig>,

    /// Local tool definitions keyed by tool name.
    #[serde(default)]
    tools: HashMap<String, ToolBackendConfig>,
}

/// A single model backend endpoint.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelBackendConfig {
    /// Backend endpoint in `host:port` format.
    endpoint: String,

    /// Backend path for inference requests.
    #[serde(default = "default_backend_path")]
    path: String,
}

/// A single tool backend endpoint.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolBackendConfig {
    /// Backend endpoint in `host:port` format.
    endpoint: String,

    /// Backend path for tool requests.
    #[serde(default = "default_tool_path")]
    path: String,
}

/// Guardrail rules applied to tool output before
/// reinference.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolOutputGuardrailsConfig {
    /// Substrings that trigger guardrail rejection.
    #[serde(default)]
    blocked_patterns: Vec<String>,
}

/// Default body buffer limit.
fn default_max_body_bytes() -> usize {
    DEFAULT_MAX_BODY_BYTES
}

/// Default loop iteration limit.
fn default_max_iterations() -> u32 {
    DEFAULT_MAX_ITERATIONS
}

/// Default model response size limit.
fn default_max_model_response_bytes() -> usize {
    DEFAULT_MAX_MODEL_RESPONSE_BYTES
}

/// Default tool response size limit.
fn default_max_tool_response_bytes() -> usize {
    DEFAULT_MAX_TOOL_RESPONSE_BYTES
}

/// Default subrequest timeout.
fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// Default model backend path.
fn default_backend_path() -> String {
    DEFAULT_BACKEND_PATH.to_owned()
}

/// Default tool backend path.
fn default_tool_path() -> String {
    DEFAULT_TOOL_PATH.to_owned()
}

// -----------------------------------------------------------------------------
// ResponsesOrchestratorFilter
// -----------------------------------------------------------------------------

/// Terminal filter for Responses API orchestration.
///
/// Buffers the request body, resolves the model backend,
/// makes HTTP subrequests for inference and tool calls,
/// and loops until the model produces a final response
/// or `max_iterations` is reached.
///
/// A tool is only executed locally when it is **both**
/// configured in the `tools` map **and** advertised by
/// the client in the request body's `tools` array. This
/// prevents the orchestrator from executing tools that
/// were not intended for local invocation.
///
/// # YAML configuration
///
/// ```yaml
/// filter: responses_orchestrator
/// max_iterations: 10
/// models:
///   gpt-4.1-mini:
///     endpoint: "127.0.0.1:3001"
/// tools:
///   get_weather:
///     endpoint: "127.0.0.1:4001"
/// ```
pub struct ResponsesOrchestratorFilter {
    /// Blocked substrings in tool output.
    blocked_patterns: Vec<String>,

    /// HTTP client for subrequests.
    client: reqwest::Client,

    /// Maximum request body bytes to buffer.
    max_body_bytes: usize,

    /// Maximum model/tool loop iterations.
    max_iterations: u32,

    /// Maximum model response body bytes.
    max_model_response_bytes: usize,

    /// Maximum tool response body bytes.
    max_tool_response_bytes: usize,

    /// Model backends keyed by model name.
    models: HashMap<String, ModelBackend>,

    /// In-memory response/conversation state store.
    state: state::ResponseStateStore,

    /// Local tool backends keyed by tool name.
    tools: HashMap<String, ToolBackend>,
}

/// Resolved model backend.
struct ModelBackend {
    /// Full URL for inference requests.
    url: String,
}

/// Resolved tool backend.
struct ToolBackend {
    /// Full URL for tool requests.
    url: String,
}

/// Response from a model backend call.
struct ModelResponse {
    /// Response body bytes.
    body: Bytes,

    /// Whether the response is SSE (text/event-stream).
    is_sse: bool,

    /// HTTP status code.
    status: u16,
}

/// Result of executing a tool.
struct ToolResult {
    /// The call_id for `function_call_output` correlation.
    call_id: String,

    /// The tool output string.
    output: String,
}

impl ResponsesOrchestratorFilter {
    /// Create from YAML config.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if the YAML config is malformed.
    ///
    /// [`FilterError`]: crate::FilterError
    pub fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: ResponsesOrchestratorConfig = parse_filter_config("responses_orchestrator", config)?;

        let timeout = std::time::Duration::from_millis(cfg.timeout_ms);

        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| FilterError::from(format!("responses_orchestrator: failed to create HTTP client: {e}")))?;

        let models = cfg
            .models
            .into_iter()
            .map(|(name, backend)| {
                let url = format!("http://{}{}", backend.endpoint, backend.path);
                (name, ModelBackend { url })
            })
            .collect();

        let tools = cfg
            .tools
            .into_iter()
            .map(|(name, backend)| {
                let url = format!("http://{}{}", backend.endpoint, backend.path);
                (name, ToolBackend { url })
            })
            .collect();

        let blocked_patterns = cfg
            .tool_output_guardrails
            .map(|g| g.blocked_patterns)
            .unwrap_or_default();

        Ok(Box::new(Self {
            blocked_patterns,
            client,
            max_body_bytes: cfg.max_body_bytes,
            max_iterations: cfg.max_iterations,
            max_model_response_bytes: cfg.max_model_response_bytes,
            max_tool_response_bytes: cfg.max_tool_response_bytes,
            models,
            state: state::ResponseStateStore::new(),
            tools,
        }))
    }
}

#[async_trait]
impl HttpFilter for ResponsesOrchestratorFilter {
    fn name(&self) -> &'static str {
        "responses_orchestrator"
    }

    async fn on_request(&self, _ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        Ok(FilterAction::Continue)
    }

    fn request_body_access(&self) -> BodyAccess {
        BodyAccess::ReadOnly
    }

    fn request_body_mode(&self) -> BodyMode {
        BodyMode::StreamBuffer {
            max_bytes: Some(self.max_body_bytes),
        }
    }

    async fn on_request_body(
        &self,
        _ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        if !end_of_stream {
            return Ok(FilterAction::Continue);
        }

        let body_bytes = body.as_deref().unwrap_or_default();
        let request_json: Value = serde_json::from_slice(body_bytes).unwrap_or_else(|_| serde_json::json!({}));
        let model = request_json
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        info!(
            model = %model,
            max_iterations = self.max_iterations,
            body_len = body_bytes.len(),
            "responses_orchestrator handling request"
        );

        if self.models.is_empty() {
            return Ok(build_placeholder_local_response(&model));
        }

        if model.is_empty() {
            warn!("responses_orchestrator: missing required field: model");
            return Ok(build_error_response(400, "missing required field: model"));
        }

        let Some(backend) = self.models.get(&model) else {
            warn!(model = %model, "responses_orchestrator: unknown model");
            return Ok(build_error_response(404, &format!("unknown model: {model}")));
        };

        let advertised_tools = extract_advertised_tools(body_bytes);
        let store_enabled = request_json.get("store").and_then(Value::as_bool).unwrap_or(true);
        let conversation_id = extract_conversation_id(&request_json);

        let effective_previous_id = resolve_previous_response_id(&request_json, &self.state, &conversation_id);

        if let Some(ref prev_id) = effective_previous_id {
            let Some(prior) = self.state.load_response(prev_id) else {
                warn!(
                    previous_response_id = %prev_id,
                    "responses_orchestrator: previous response not found"
                );
                return Ok(build_error_response(
                    404,
                    &format!("previous response not found: {prev_id}"),
                ));
            };

            info!(
                previous_response_id = %prev_id,
                prior_items = prior.items.len(),
                "responses_orchestrator loaded prior state"
            );

            let enriched = build_state_enriched_body(&request_json, &prior.items);
            return self
                .run_loop(
                    backend,
                    &enriched,
                    &advertised_tools,
                    store_enabled,
                    conversation_id.as_deref(),
                )
                .await;
        }

        self.run_loop(
            backend,
            body_bytes,
            &advertised_tools,
            store_enabled,
            conversation_id.as_deref(),
        )
        .await
    }
}

impl ResponsesOrchestratorFilter {
    /// Run the model/tool loop up to `max_iterations`.
    async fn run_loop(
        &self,
        backend: &ModelBackend,
        initial_body: &[u8],
        advertised_tools: &HashSet<String>,
        store_enabled: bool,
        conversation_id: Option<&str>,
    ) -> Result<FilterAction, FilterError> {
        let mut current_body = initial_body.to_vec();

        for iteration in 0..self.max_iterations {
            info!(iteration, "responses_orchestrator loop iteration");

            let model_resp = match self.call_model_backend(backend, &current_body).await {
                Ok(result) => result,
                Err(e) => {
                    warn!(error = %e, "responses_orchestrator model call failed");
                    return Ok(build_error_response(502, &format!("model backend error: {e}")));
                },
            };

            info!(
                status = model_resp.status,
                response_len = model_resp.body.len(),
                is_sse = model_resp.is_sse,
                "responses_orchestrator model call completed"
            );

            let (detected_calls, final_body) = if model_resp.is_sse {
                let body_str = String::from_utf8_lossy(&model_resp.body);
                let mut sse_parser = super::responses::sse::SseParser::new();
                sse_parser.feed(&body_str);
                let sse_result = sse_parser.finish();
                info!(
                    function_calls = sse_result.function_calls.len(),
                    response_completed = sse_result.response_completed,
                    "responses_orchestrator SSE parse completed"
                );

                if !sse_result.response_completed {
                    warn!("responses_orchestrator: SSE stream truncated (no terminal event)");
                    return Ok(build_error_response(502, "SSE stream truncated: no terminal event"));
                }

                if sse_result.has_incomplete_function_calls {
                    warn!("responses_orchestrator: SSE stream has incomplete function calls");
                    return Ok(build_error_response(
                        502,
                        "SSE stream has incomplete function calls (missing arguments.done)",
                    ));
                }

                let synthesized = Bytes::from(super::responses::sse::synthesize_final_json(&sse_result));
                (sse_result.function_calls, synthesized)
            } else {
                let calls = super::responses::parser::parse_model_response(&model_resp.body)
                    .map(|r| r.function_calls)
                    .unwrap_or_default();
                (calls, model_resp.body)
            };

            if detected_calls.is_empty() {
                if store_enabled {
                    self.persist_response(&current_body, &final_body, conversation_id);
                }

                return Ok(FilterAction::Reject(
                    Rejection::status(model_resp.status)
                        .with_header("Content-Type", "application/json")
                        .with_body(final_body),
                ));
            }

            let calls = &detected_calls;

            for fc in calls {
                info!(
                    tool_name = %fc.name,
                    call_id = %fc.call_id,
                    "responses_orchestrator detected function_call"
                );
            }

            if let Err(action) = self.prevalidate_tools(calls, advertised_tools) {
                return Ok(action);
            }

            let tool_results = match self.execute_tools(calls).await {
                Ok(results) => results,
                Err(action) => return Ok(action),
            };

            if let Some(blocked) = self.check_tool_output_guardrails(&tool_results) {
                warn!(
                    blocked_pattern = %blocked,
                    "responses_orchestrator tool output blocked by guardrail"
                );
                return Ok(build_guardrail_blocked_response(&blocked));
            }

            current_body = build_reinference_body(&current_body, calls, &tool_results);
        }

        warn!(
            max_iterations = self.max_iterations,
            "responses_orchestrator max iterations reached"
        );
        Ok(build_incomplete_response(self.max_iterations))
    }

    /// Call a model backend and return a [`ModelResponse`].
    async fn call_model_backend(&self, backend: &ModelBackend, request_body: &[u8]) -> Result<ModelResponse, String> {
        let response = self
            .client
            .post(&backend.url)
            .header("Content-Type", "application/json")
            .body(request_body.to_vec())
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = response.status().as_u16();
        let is_sse = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("text/event-stream"));

        let body = read_response_bytes(response, self.max_model_response_bytes).await?;

        Ok(ModelResponse { body, is_sse, status })
    }

    /// Prevalidate all tool names before executing any.
    ///
    /// Each tool must be both advertised in the request
    /// and configured in the local tool registry.
    fn prevalidate_tools(
        &self,
        calls: &[DetectedFunctionCall],
        advertised_tools: &HashSet<String>,
    ) -> Result<(), FilterAction> {
        for fc in calls {
            if !advertised_tools.contains(&fc.name) {
                warn!(
                    tool_name = %fc.name,
                    call_id = %fc.call_id,
                    "responses_orchestrator: tool not advertised in request"
                );
                return Err(build_error_response(
                    400,
                    &format!("tool '{}' not advertised in request", fc.name),
                ));
            }

            if !self.tools.contains_key(&fc.name) {
                warn!(
                    tool_name = %fc.name,
                    call_id = %fc.call_id,
                    "responses_orchestrator: unknown tool, failing closed"
                );
                return Err(build_error_response(400, &format!("unknown tool: {}", fc.name)));
            }
        }
        Ok(())
    }

    /// Execute validated tool calls against the local
    /// tool registry.
    async fn execute_tools(&self, calls: &[DetectedFunctionCall]) -> Result<Vec<ToolResult>, FilterAction> {
        let mut results = Vec::new();

        for fc in calls {
            let tool_backend = self.tools.get(&fc.name).expect("prevalidated");

            info!(
                tool_name = %fc.name,
                call_id = %fc.call_id,
                "responses_orchestrator executing tool"
            );

            match self.call_tool_backend(tool_backend, &fc.arguments).await {
                Ok(output) => {
                    info!(
                        tool_name = %fc.name,
                        call_id = %fc.call_id,
                        output_len = output.len(),
                        "responses_orchestrator tool call completed"
                    );
                    results.push(ToolResult {
                        call_id: fc.call_id.clone(),
                        output,
                    });
                },
                Err(e) => {
                    warn!(
                        tool_name = %fc.name,
                        call_id = %fc.call_id,
                        error = %e,
                        "responses_orchestrator tool call failed"
                    );
                    return Err(build_error_response(
                        502,
                        &format!("tool backend error for {}: {e}", fc.name),
                    ));
                },
            }
        }

        Ok(results)
    }

    /// Call a tool backend and return the output string.
    async fn call_tool_backend(&self, backend: &ToolBackend, arguments: &str) -> Result<String, String> {
        let response = self
            .client
            .post(&backend.url)
            .header("Content-Type", "application/json")
            .body(arguments.to_owned())
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            return Err(format!("tool returned status {}", response.status().as_u16()));
        }

        let body_bytes = read_response_bytes(response, self.max_tool_response_bytes).await?;

        String::from_utf8(body_bytes.to_vec()).map_err(|e| e.to_string())
    }

    /// Persist a completed response to the state store.
    ///
    /// Stores the full conversation transcript: input
    /// items from the request body plus output items from
    /// the model response. This enables `previous_response_id`
    /// continuations to replay the full history.
    fn persist_response(&self, request_body: &[u8], response_body: &[u8], conversation_id: Option<&str>) {
        let Ok(response_json) = serde_json::from_slice::<Value>(response_body) else {
            return;
        };

        let Some(response_id) = response_json.get("id").and_then(Value::as_str) else {
            return;
        };

        let status = response_json.get("status").and_then(Value::as_str).unwrap_or("");
        if status != "completed" {
            return;
        }

        let request_json: Value = serde_json::from_slice(request_body).unwrap_or_else(|_| serde_json::json!({}));

        let input_items = normalize_input_to_items(request_json.get("input"));

        let output_items = response_json
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut items = input_items;
        items.extend(output_items);

        let model = response_json
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        info!(
            response_id = %response_id,
            transcript_items = items.len(),
            "responses_orchestrator persisting response"
        );

        self.state
            .store_response(response_id, state::StoredResponse { items, model });

        if let Some(conv_id) = conversation_id {
            self.state.set_conversation_latest(conv_id, response_id);
            info!(
                conversation_id = %conv_id,
                response_id = %response_id,
                "responses_orchestrator updated conversation pointer"
            );
        }
    }

    /// Check tool outputs against blocked patterns.
    ///
    /// Returns the first matching pattern, or `None` if
    /// all outputs pass.
    fn check_tool_output_guardrails(&self, results: &[ToolResult]) -> Option<String> {
        for pattern in &self.blocked_patterns {
            for result in results {
                if result.output.contains(pattern.as_str()) {
                    return Some(pattern.clone());
                }
            }
        }
        None
    }
}

// -----------------------------------------------------------------------------
// Utilities
// -----------------------------------------------------------------------------

/// Normalize the request `input` field into an item array.
fn normalize_input_to_items(input: Option<&Value>) -> Vec<Value> {
    match input {
        Some(Value::Array(arr)) => arr.clone(),
        Some(Value::String(s)) => vec![serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": s}]
        })],
        _ => Vec::new(),
    }
}

/// Extract advertised tool names from the request body's
/// `tools` array.
fn extract_advertised_tools(body: &[u8]) -> HashSet<String> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("tools").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// Extract the `conversation` field as a string ID.
///
/// Supports both `"conversation": "id"` and
/// `"conversation": {"id": "..."}` forms.
fn extract_conversation_id(request: &Value) -> Option<String> {
    match request.get("conversation")? {
        Value::String(s) => Some(s.clone()),
        Value::Object(obj) => obj.get("id").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

/// Resolve the effective previous response ID.
///
/// Explicit `previous_response_id` takes priority.
/// Otherwise, if a conversation ID is present, look up
/// its latest response.
fn resolve_previous_response_id(
    request: &Value,
    store: &state::ResponseStateStore,
    conversation_id: &Option<String>,
) -> Option<String> {
    if let Some(prev) = request.get("previous_response_id").and_then(Value::as_str) {
        return Some(prev.to_owned());
    }

    if let Some(conv_id) = conversation_id {
        return store.get_conversation_latest(conv_id);
    }

    None
}

/// Build the first model request body with prior state
/// injected before the new input.
fn build_state_enriched_body(request: &Value, prior_items: &[Value]) -> Vec<u8> {
    let mut enriched = request.clone();

    let new_input = request.get("input").cloned().unwrap_or(Value::Null);

    let new_items: Vec<Value> = match new_input {
        Value::Array(arr) => arr,
        Value::String(s) => vec![serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": s}]
        })],
        _ => Vec::new(),
    };

    let mut combined = prior_items.to_vec();
    combined.extend(new_items);

    enriched["input"] = Value::Array(combined);

    if enriched.get("previous_response_id").is_some() {
        enriched.as_object_mut().map(|o| o.remove("previous_response_id"));
    }

    serde_json::to_vec(&enriched).unwrap_or_default()
}

/// Read response bytes up to a size limit.
async fn read_response_bytes(response: reqwest::Response, max_bytes: usize) -> Result<Bytes, String> {
    let content_length = response.content_length().unwrap_or(0);

    if content_length > max_bytes as u64 {
        return Err(format!(
            "response too large: {content_length} bytes exceeds limit of {max_bytes}"
        ));
    }

    let bytes = response.bytes().await.map_err(|e| e.to_string())?;

    if bytes.len() > max_bytes {
        return Err(format!(
            "response too large: {} bytes exceeds limit of {max_bytes}",
            bytes.len()
        ));
    }

    Ok(bytes)
}

/// Build a Responses-shaped response when tool output
/// is blocked by a guardrail.
fn build_guardrail_blocked_response(pattern: &str) -> FilterAction {
    let body = serde_json::json!({
        "id": "resp_guardrail_blocked",
        "object": "response",
        "status": "failed",
        "error": {
            "message": format!("tool output blocked by guardrail: matched pattern '{pattern}'"),
            "type": "guardrail_violation",
            "code": "tool_output_blocked"
        }
    });

    FilterAction::Reject(
        Rejection::status(400)
            .with_header("Content-Type", "application/json")
            .with_body(Bytes::from(body.to_string())),
    )
}

/// Build a Responses-shaped error response.
fn build_error_response(status: u16, message: &str) -> FilterAction {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": "invalid_request_error"
        }
    });

    FilterAction::Reject(
        Rejection::status(status)
            .with_header("Content-Type", "application/json")
            .with_body(Bytes::from(body.to_string())),
    )
}

/// Build a Responses-shaped incomplete response when
/// max iterations is reached.
fn build_incomplete_response(max_iterations: u32) -> FilterAction {
    let body = serde_json::json!({
        "id": "resp_incomplete",
        "object": "response",
        "status": "incomplete",
        "incomplete_details": {
            "reason": "max_output_tokens",
            "message": format!("orchestrator reached max iterations ({max_iterations})")
        },
        "output": []
    });

    FilterAction::Reject(
        Rejection::status(200)
            .with_header("Content-Type", "application/json")
            .with_body(Bytes::from(body.to_string())),
    )
}

/// Build a placeholder local response for configs with
/// no model backends (skeleton/CP3 mode).
fn build_placeholder_local_response(model: &str) -> FilterAction {
    let display_model = if model.is_empty() { "unknown" } else { model };

    let response = serde_json::json!({
        "id": "resp_orchestrator_placeholder",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": display_model,
        "output": [
            {
                "type": "message",
                "id": "msg_placeholder",
                "role": "assistant",
                "status": "completed",
                "content": [
                    {
                        "type": "output_text",
                        "text": "Orchestrator placeholder response.",
                        "annotations": []
                    }
                ]
            }
        ],
        "usage": {
            "input_tokens": 0,
            "output_tokens": 0,
            "total_tokens": 0
        }
    });

    FilterAction::Reject(
        Rejection::status(200)
            .with_header("Content-Type", "application/json")
            .with_body(Bytes::from(response.to_string())),
    )
}

/// Build the second model request body with
/// `function_call_output` items injected by `call_id`.
fn build_reinference_body(original_body: &[u8], calls: &[DetectedFunctionCall], results: &[ToolResult]) -> Vec<u8> {
    let mut request: Value = serde_json::from_slice(original_body).unwrap_or_else(|_| serde_json::json!({}));

    let input = request.get("input").cloned().unwrap_or(Value::Null);

    let mut items: Vec<Value> = match input {
        Value::Array(arr) => arr,
        Value::String(s) => vec![serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": s}]
        })],
        _ => Vec::new(),
    };

    for (fc, result) in calls.iter().zip(results.iter()) {
        items.push(serde_json::json!({
            "type": "function_call",
            "id": format!("fc_{}", fc.call_id),
            "call_id": fc.call_id,
            "name": fc.name,
            "arguments": fc.arguments,
            "status": "completed"
        }));

        items.push(serde_json::json!({
            "type": "function_call_output",
            "call_id": result.call_id,
            "output": result.output
        }));
    }

    request["input"] = Value::Array(items);

    serde_json::to_vec(&request).unwrap_or_default()
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests"
)]
mod tests {
    use super::*;

    #[test]
    fn from_config_defaults() {
        let filter = ResponsesOrchestratorFilter::from_config(&serde_yaml::Value::Null).unwrap();
        assert_eq!(
            filter.name(),
            "responses_orchestrator",
            "default config should produce responses_orchestrator"
        );
    }

    #[test]
    fn from_config_with_models_and_tools() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
models:
  gpt-4.1-mini:
    endpoint: "127.0.0.1:3001"
tools:
  get_weather:
    endpoint: "127.0.0.1:4001"
"#,
        )
        .unwrap();
        let filter = ResponsesOrchestratorFilter::from_config(&yaml).unwrap();
        assert_eq!(
            filter.name(),
            "responses_orchestrator",
            "config with models and tools should parse"
        );
    }

    #[test]
    fn from_config_rejects_unknown_fields() {
        let yaml: serde_yaml::Value = serde_yaml::from_str("unknown_field: true").unwrap();
        let result = ResponsesOrchestratorFilter::from_config(&yaml);
        assert!(result.is_err(), "unknown fields should be rejected");
    }

    #[test]
    fn extract_advertised_tools_present() {
        let body = br#"{"model":"test","tools":[{"type":"function","name":"get_weather"},{"type":"function","name":"search"}]}"#;
        let tools = extract_advertised_tools(body);
        assert_eq!(tools.len(), 2, "should extract two tools");
        assert!(tools.contains("get_weather"), "should contain get_weather");
        assert!(tools.contains("search"), "should contain search");
    }

    #[test]
    fn extract_advertised_tools_absent() {
        let body = br#"{"model":"test","input":"hello"}"#;
        let tools = extract_advertised_tools(body);
        assert!(tools.is_empty(), "missing tools should return empty set");
    }

    #[test]
    fn reinference_body_injects_function_call_output() {
        let original = br#"{"model":"test","input":"Hello"}"#;
        let calls = vec![DetectedFunctionCall {
            arguments: r#"{"city":"Boston"}"#.to_owned(),
            call_id: "call_001".to_owned(),
            name: "get_weather".to_owned(),
        }];
        let results = vec![ToolResult {
            call_id: "call_001".to_owned(),
            output: r#"{"temp":"72F"}"#.to_owned(),
        }];

        let new_body = build_reinference_body(original, &calls, &results);
        let parsed: Value = serde_json::from_slice(&new_body).unwrap();

        let input = parsed["input"].as_array().unwrap();
        assert!(input.len() >= 3, "should have user message + function_call + output");

        let fc_output = input.iter().find(|i| i["type"] == "function_call_output").unwrap();
        assert_eq!(fc_output["call_id"], "call_001", "call_id should match");
        assert_eq!(fc_output["output"], r#"{"temp":"72F"}"#, "output should match");

        assert_eq!(parsed["model"], "test", "model should be preserved");
    }

    #[tokio::test]
    async fn returns_placeholder_when_no_models_configured() {
        let filter = ResponsesOrchestratorFilter::from_config(&serde_yaml::Value::Null).unwrap();
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/responses");
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let json = br#"{"model":"gpt-4.1-mini","input":"Hello"}"#;
        let mut body = Some(Bytes::from_static(json));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        match action {
            FilterAction::Reject(r) => {
                assert_eq!(r.status, 200, "should return 200");
                let body_str = String::from_utf8_lossy(r.body.as_ref().unwrap());
                assert!(
                    body_str.contains("resp_orchestrator_placeholder"),
                    "body should contain placeholder ID: {body_str}"
                );
            },
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn returns_error_for_unknown_model() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
models:
  known-model:
    endpoint: "127.0.0.1:9999"
"#,
        )
        .unwrap();
        let filter = ResponsesOrchestratorFilter::from_config(&yaml).unwrap();
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/responses");
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let json = br#"{"model":"unknown-model","input":"test"}"#;
        let mut body = Some(Bytes::from_static(json));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        match action {
            FilterAction::Reject(r) => {
                assert_eq!(r.status, 404, "unknown model should return 404");
            },
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn returns_error_for_missing_model() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
models:
  known-model:
    endpoint: "127.0.0.1:9999"
"#,
        )
        .unwrap();
        let filter = ResponsesOrchestratorFilter::from_config(&yaml).unwrap();
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/responses");
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let json = br#"{"input":"no model"}"#;
        let mut body = Some(Bytes::from_static(json));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        match action {
            FilterAction::Reject(r) => {
                assert_eq!(r.status, 400, "missing model should return 400");
            },
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn continues_before_end_of_stream() {
        let filter = ResponsesOrchestratorFilter::from_config(&serde_yaml::Value::Null).unwrap();
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/responses");
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let mut body = Some(Bytes::from_static(b"partial"));

        let action = filter.on_request_body(&mut ctx, &mut body, false).await.unwrap();

        assert!(
            matches!(action, FilterAction::Continue),
            "should continue before end_of_stream"
        );
    }
}
