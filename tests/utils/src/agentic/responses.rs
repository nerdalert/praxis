// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! OpenAI Responses API mock server for integration tests.
//!
//! Provides a deterministic `/v1/responses` backend that records
//! every inbound request for later assertion. The server runs on
//! a background thread and shuts down when the returned
//! [`ResponsesMockServerGuard`] is dropped.
//!
//! The mock records the **raw request body bytes** so tests can
//! verify byte-for-byte pass-through preservation.

use std::{
    net::TcpStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};

use super::http::{parse_agentic_request, write_response};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Default endpoint path.
const DEFAULT_PATH: &str = "/v1/responses";

/// Deterministic response ID for reproducible tests.
const MOCK_RESPONSE_ID: &str = "resp_mock_test_001";

/// Default model name echoed in responses.
const DEFAULT_MODEL: &str = "gpt-4.1-mini";

// -----------------------------------------------------------------------------
// Configuration
// -----------------------------------------------------------------------------

/// Configuration for a Responses mock server instance.
pub struct ResponsesMockConfig {
    /// Endpoint path the server listens on.
    pub path: String,

    /// Response fixtures keyed by a match predicate.
    /// When empty, the server returns a default final response.
    pub fixtures: Vec<ResponsesFixture>,

    /// Model name echoed in responses when no fixture matches.
    pub model: String,
}

impl Default for ResponsesMockConfig {
    fn default() -> Self {
        Self {
            path: DEFAULT_PATH.to_owned(),
            fixtures: Vec::new(),
            model: DEFAULT_MODEL.to_owned(),
        }
    }
}

/// A response fixture returned when a request matches.
pub struct ResponsesFixture {
    /// Content-Type header for this fixture.
    /// Defaults to `application/json`.
    pub content_type: Option<String>,

    /// Match predicate: returns `true` when this fixture
    /// should be used for the given request body.
    pub matches: Box<dyn Fn(&str) -> bool + Send + Sync>,

    /// The raw response body to return.
    pub response_body: String,
}

// -----------------------------------------------------------------------------
// Recorded Request
// -----------------------------------------------------------------------------

/// A single request captured by the Responses mock server.
#[derive(Clone, Debug)]
pub struct ResponsesRecordedRequest {
    /// Raw request body exactly as received.
    pub body: String,

    /// Request headers as `(lowercase-name, value)` pairs.
    pub headers: Vec<(String, String)>,

    /// HTTP method.
    pub http_method: String,

    /// URL path without query string.
    pub path: String,

    /// Full request URI including query string.
    pub uri: String,
}

// -----------------------------------------------------------------------------
// Server Guard
// -----------------------------------------------------------------------------

/// RAII handle for a running Responses mock server.
///
/// The background listener exits when this guard is
/// dropped. Use the accessor methods to inspect
/// captured request state after sending test traffic.
pub struct ResponsesMockServerGuard {
    /// The configured endpoint path.
    path: String,

    /// Listening port.
    port: u16,

    /// Shared shutdown flag.
    shutdown: Arc<AtomicBool>,

    /// Captured requests.
    state: Arc<Mutex<Vec<ResponsesRecordedRequest>>>,
}

impl ResponsesMockServerGuard {
    /// The `host:port` address string.
    pub fn endpoint(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// The last recorded request body, if any.
    ///
    /// # Panics
    ///
    /// Panics if the state mutex is poisoned.
    pub fn last_request_body(&self) -> Option<String> {
        self.state.lock().unwrap().last().map(|r| r.body.clone())
    }

    /// The configured endpoint path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The port the server is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Clone of all captured requests.
    ///
    /// # Panics
    ///
    /// Panics if the state mutex is poisoned.
    pub fn received_requests(&self) -> Vec<ResponsesRecordedRequest> {
        self.state.lock().unwrap().clone()
    }

    /// Total count of recorded requests.
    ///
    /// # Panics
    ///
    /// Panics if the state mutex is poisoned.
    pub fn request_count(&self) -> usize {
        self.state.lock().unwrap().len()
    }

    /// Count of POST requests to the configured path.
    ///
    /// Excludes health-check probes and other non-API
    /// traffic that reaches the mock.
    ///
    /// # Panics
    ///
    /// Panics if the state mutex is poisoned.
    pub fn responses_request_count(&self) -> usize {
        let reqs = self.state.lock().unwrap();
        reqs.iter()
            .filter(|r| r.http_method == "POST" && r.path == self.path)
            .count()
    }

    /// The most recent POST request to the configured
    /// path, excluding health-check probes.
    ///
    /// # Panics
    ///
    /// Panics if the state mutex is poisoned.
    pub fn last_responses_request(&self) -> Option<ResponsesRecordedRequest> {
        let reqs = self.state.lock().unwrap();
        reqs.iter()
            .rev()
            .find(|r| r.http_method == "POST" && r.path == self.path)
            .cloned()
    }
}

impl Drop for ResponsesMockServerGuard {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect(format!("127.0.0.1:{}", self.port));
    }
}

// -----------------------------------------------------------------------------
// Server Lifecycle
// -----------------------------------------------------------------------------

/// Start a Responses mock server with default configuration.
///
/// # Panics
///
/// Panics if the server fails to bind or the config
/// path is invalid.
pub fn start_responses_mock_server() -> ResponsesMockServerGuard {
    start_responses_mock_server_with_config(ResponsesMockConfig::default())
}

/// Start a Responses mock server with custom configuration.
///
/// # Panics
///
/// Panics if the server fails to bind or the config
/// path is invalid.
pub fn start_responses_mock_server_with_config(config: ResponsesMockConfig) -> ResponsesMockServerGuard {
    super::validate_config_path(&config.path);

    let (listener, port) = crate::net::port::bind_unique_port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let state: Arc<Mutex<Vec<ResponsesRecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));

    let flag = Arc::clone(&shutdown);
    let shared_state = Arc::clone(&state);
    let path = config.path.clone();
    let config = Arc::new(config);

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if flag.load(Ordering::Acquire) {
                break;
            }
            let cfg = Arc::clone(&config);
            let st = Arc::clone(&shared_state);
            std::thread::spawn(move || handle_connection(stream, &cfg, &st));
        }
    });

    ResponsesMockServerGuard {
        path,
        port,
        shutdown,
        state,
    }
}

// -----------------------------------------------------------------------------
// Connection Handler
// -----------------------------------------------------------------------------

/// Per-connection entry point.
fn handle_connection(
    mut stream: TcpStream,
    config: &ResponsesMockConfig,
    state: &Mutex<Vec<ResponsesRecordedRequest>>,
) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    let Some(req) = parse_agentic_request(&mut stream) else {
        return;
    };

    let record = ResponsesRecordedRequest {
        body: req.body.clone(),
        headers: req.headers.clone(),
        http_method: req.method.clone(),
        path: req.path.clone(),
        uri: req.uri.clone(),
    };

    state.lock().unwrap().push(record);

    if let Some(status) = reject_early(&req, config) {
        write_response(&mut stream, status, reason_for(status), &[], "");
        return;
    }

    dispatch_response(&mut stream, config, &req.body);
}

/// `Some(status)` when the request is invalid.
fn reject_early(req: &super::http::AgenticHttpRequest, config: &ResponsesMockConfig) -> Option<u16> {
    if req.method != "POST" {
        return Some(405);
    }
    if req.path != config.path {
        return Some(404);
    }
    if req.body.is_empty() {
        return Some(400);
    }
    None
}

/// Route to a fixture response or the default final response.
fn dispatch_response(stream: &mut TcpStream, config: &ResponsesMockConfig, body: &str) {
    for fixture in &config.fixtures {
        if (fixture.matches)(body) {
            let ct = fixture.content_type.as_deref().unwrap_or("application/json").to_owned();
            write_response(stream, 200, "OK", &[("Content-Type", ct)], &fixture.response_body);
            return;
        }
    }

    write_default_response(stream, config, body);
}

/// HTTP reason phrase for early-rejection status codes.
fn reason_for(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Unknown",
    }
}

// -----------------------------------------------------------------------------
// Response Builders
// -----------------------------------------------------------------------------

/// Default Responses-shaped final response with no tool calls.
fn write_default_response(stream: &mut TcpStream, config: &ResponsesMockConfig, request_body: &str) {
    let model = serde_json::from_str::<Value>(request_body)
        .ok()
        .and_then(|v| v.get("model").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| config.model.clone());

    let response = json!({
        "id": MOCK_RESPONSE_ID,
        "object": "response",
        "created_at": 1700000000_i64,
        "status": "completed",
        "model": model,
        "output": [
            {
                "type": "message",
                "id": "msg_mock_001",
                "role": "assistant",
                "status": "completed",
                "content": [
                    {
                        "type": "output_text",
                        "text": "This is a mock response.",
                        "annotations": []
                    }
                ]
            }
        ],
        "usage": {
            "input_tokens": 10,
            "output_tokens": 5,
            "total_tokens": 15
        }
    });

    let body = response.to_string();
    write_response(
        stream,
        200,
        "OK",
        &[("Content-Type", "application/json".to_owned())],
        &body,
    );
}

/// Build a Responses-shaped function_call response.
///
/// Useful for creating fixtures that trigger the
/// orchestrator's tool execution path.
pub fn function_call_response(call_id: &str, name: &str, arguments: &str) -> String {
    let response = json!({
        "id": MOCK_RESPONSE_ID,
        "object": "response",
        "created_at": 1700000000_i64,
        "status": "completed",
        "model": DEFAULT_MODEL,
        "output": [
            {
                "type": "function_call",
                "id": format!("fc_{call_id}"),
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
                "status": "completed"
            }
        ],
        "usage": {
            "input_tokens": 10,
            "output_tokens": 8,
            "total_tokens": 18
        }
    });
    response.to_string()
}

/// Build a Responses-shaped final text response.
///
/// Useful for creating fixtures that represent a final
/// model answer after tool execution.
pub fn final_text_response(text: &str) -> String {
    let response = json!({
        "id": MOCK_RESPONSE_ID,
        "object": "response",
        "created_at": 1700000000_i64,
        "status": "completed",
        "model": DEFAULT_MODEL,
        "output": [
            {
                "type": "message",
                "id": "msg_mock_final",
                "role": "assistant",
                "status": "completed",
                "content": [
                    {
                        "type": "output_text",
                        "text": text,
                        "annotations": []
                    }
                ]
            }
        ],
        "usage": {
            "input_tokens": 20,
            "output_tokens": 10,
            "total_tokens": 30
        }
    });
    response.to_string()
}

/// Build SSE response body with a function_call whose
/// arguments are split across two delta events.
///
/// Returns the raw SSE text (not JSON).
pub fn streaming_function_call_response(call_id: &str, name: &str, arguments: &str) -> String {
    let mid = arguments.len() / 2;
    let (part1, part2) = arguments.split_at(mid);
    let escaped1 = part1.replace('\\', "\\\\").replace('\"', "\\\"");
    let escaped2 = part2.replace('\\', "\\\\").replace('\"', "\\\"");

    format!(
        "data: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"type\":\"function_call\",\"call_id\":\"{call_id}\",\"name\":\"{name}\"}}}}\n\
         data: {{\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{escaped1}\"}}\n\
         data: {{\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{escaped2}\"}}\n\
         data: {{\"type\":\"response.function_call_arguments.done\",\"output_index\":0,\"arguments\":\"{escaped1}{escaped2}\"}}\n\
         data: {{\"type\":\"response.output_item.done\"}}\n\
         data: {{\"type\":\"response.completed\"}}\n\
         data: [DONE]\n"
    )
}
