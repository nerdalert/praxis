// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! HTTP tool mock server for integration tests.
//!
//! Provides a deterministic tool backend that records
//! every inbound request and returns a configurable
//! JSON response. The server runs on a background
//! thread and shuts down when the returned
//! [`ToolHttpMockServerGuard`] is dropped.

use std::{
    net::TcpStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use super::http::{parse_agentic_request, write_response};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Default endpoint path.
const DEFAULT_PATH: &str = "/tool";

// -----------------------------------------------------------------------------
// Configuration
// -----------------------------------------------------------------------------

/// Configuration for a tool HTTP mock server.
pub struct ToolHttpMockConfig {
    /// Endpoint path the server listens on.
    pub path: String,

    /// Response body returned for successful calls.
    pub response_body: String,
}

impl Default for ToolHttpMockConfig {
    fn default() -> Self {
        Self {
            path: DEFAULT_PATH.to_owned(),
            response_body: r#"{"result":"mock tool result"}"#.to_owned(),
        }
    }
}

// -----------------------------------------------------------------------------
// Recorded Request
// -----------------------------------------------------------------------------

/// A single request captured by the tool HTTP mock.
#[derive(Clone, Debug)]
pub struct ToolHttpRecordedRequest {
    /// Raw request body exactly as received.
    pub body: String,

    /// Request headers as `(lowercase-name, value)` pairs.
    pub headers: Vec<(String, String)>,

    /// HTTP method.
    pub http_method: String,

    /// URL path without query string.
    pub path: String,
}

// -----------------------------------------------------------------------------
// Server Guard
// -----------------------------------------------------------------------------

/// RAII handle for a running tool HTTP mock server.
pub struct ToolHttpMockServerGuard {
    /// The configured endpoint path.
    path: String,

    /// Listening port.
    port: u16,

    /// Shared shutdown flag.
    shutdown: Arc<AtomicBool>,

    /// Captured requests.
    state: Arc<Mutex<Vec<ToolHttpRecordedRequest>>>,
}

impl ToolHttpMockServerGuard {
    /// The `host:port` address string.
    pub fn endpoint(&self) -> String {
        format!("127.0.0.1:{}", self.port)
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
    pub fn received_requests(&self) -> Vec<ToolHttpRecordedRequest> {
        self.state.lock().unwrap().clone()
    }

    /// Count of POST requests to the configured path.
    ///
    /// # Panics
    ///
    /// Panics if the state mutex is poisoned.
    pub fn tool_request_count(&self) -> usize {
        let reqs = self.state.lock().unwrap();
        reqs.iter()
            .filter(|r| r.http_method == "POST" && r.path == self.path)
            .count()
    }

    /// The most recent POST request to the configured
    /// path.
    ///
    /// # Panics
    ///
    /// Panics if the state mutex is poisoned.
    pub fn last_tool_request(&self) -> Option<ToolHttpRecordedRequest> {
        let reqs = self.state.lock().unwrap();
        reqs.iter()
            .rev()
            .find(|r| r.http_method == "POST" && r.path == self.path)
            .cloned()
    }
}

impl Drop for ToolHttpMockServerGuard {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect(format!("127.0.0.1:{}", self.port));
    }
}

// -----------------------------------------------------------------------------
// Server Lifecycle
// -----------------------------------------------------------------------------

/// Start a tool HTTP mock server with default config.
///
/// # Panics
///
/// Panics if the server fails to bind.
pub fn start_tool_http_mock_server() -> ToolHttpMockServerGuard {
    start_tool_http_mock_server_with_config(ToolHttpMockConfig::default())
}

/// Start a tool HTTP mock server with custom config.
///
/// # Panics
///
/// Panics if the server fails to bind or the config
/// path is invalid.
pub fn start_tool_http_mock_server_with_config(config: ToolHttpMockConfig) -> ToolHttpMockServerGuard {
    super::validate_config_path(&config.path);

    let (listener, port) = crate::net::port::bind_unique_port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let state: Arc<Mutex<Vec<ToolHttpRecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));

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

    ToolHttpMockServerGuard {
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
fn handle_connection(mut stream: TcpStream, config: &ToolHttpMockConfig, state: &Mutex<Vec<ToolHttpRecordedRequest>>) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    let Some(req) = parse_agentic_request(&mut stream) else {
        return;
    };

    let record = ToolHttpRecordedRequest {
        body: req.body.clone(),
        headers: req.headers.clone(),
        http_method: req.method.clone(),
        path: req.path.clone(),
    };

    state.lock().unwrap().push(record);

    if req.method != "POST" || req.path != config.path {
        write_response(&mut stream, 404, "Not Found", &[], "");
        return;
    }

    write_response(
        &mut stream,
        200,
        "OK",
        &[("Content-Type", "application/json".to_owned())],
        &config.response_body,
    );
}
