// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Integration tests for Responses API model-based routing.
//!
//! Validates that Praxis extracts the `model` field from
//! Responses request bodies and routes to the correct
//! model-specific backend. Unknown or missing models must
//! receive a clear error and not reach any backend.

use std::collections::HashMap;

use praxis_test_utils::{
    ProxyGuard,
    agentic::{
        ResponsesFixture, ResponsesMockConfig, ResponsesMockServerGuard, start_responses_mock_server_with_config,
    },
    free_port, http_send, load_example_config,
    net::http_client::json_post,
    parse_body, parse_status, start_proxy,
};

// -----------------------------------------------------------------------------
// Model Routing
// -----------------------------------------------------------------------------

#[test]
fn responses_model_a_routes_to_backend_a() {
    let (mock_a, mock_b, proxy) = setup_model_routing();

    let body = r#"{"model":"model-a","input":"Hello from model A"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "model-a should return 200");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("backend-a"),
        "response should come from backend-a: {response_body}"
    );

    assert_eq!(
        mock_a.responses_request_count(),
        1,
        "backend-a should receive exactly one request"
    );
    assert_eq!(
        mock_b.responses_request_count(),
        0,
        "backend-b should receive zero requests"
    );
}

#[test]
fn responses_model_b_routes_to_backend_b() {
    let (mock_a, mock_b, proxy) = setup_model_routing();

    let body = r#"{"model":"model-b","input":"Hello from model B"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "model-b should return 200");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("backend-b"),
        "response should come from backend-b: {response_body}"
    );

    assert_eq!(
        mock_a.responses_request_count(),
        0,
        "backend-a should receive zero requests"
    );
    assert_eq!(
        mock_b.responses_request_count(),
        1,
        "backend-b should receive exactly one request"
    );
}

#[test]
fn responses_unknown_model_returns_error() {
    let (mock_a, mock_b, proxy) = setup_model_routing();

    let body = r#"{"model":"unknown-model","input":"test"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 404, "unknown model should return 404 from router");

    assert_eq!(
        mock_a.responses_request_count(),
        0,
        "backend-a should not receive unknown model request"
    );
    assert_eq!(
        mock_b.responses_request_count(),
        0,
        "backend-b should not receive unknown model request"
    );
}

#[test]
fn responses_missing_model_returns_error() {
    let (mock_a, mock_b, proxy) = setup_model_routing();

    let body = r#"{"input":"no model field at all"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 404, "missing model should return 404 from router");

    assert_eq!(
        mock_a.responses_request_count(),
        0,
        "backend-a should not receive request without model"
    );
    assert_eq!(
        mock_b.responses_request_count(),
        0,
        "backend-b should not receive request without model"
    );
}

#[test]
fn responses_model_routing_preserves_body() {
    let (mock_a, _mock_b, proxy) = setup_model_routing();

    let body = r#"{"model":"model-a","input":"test","store":false,"tools":[{"type":"function","name":"get_weather"}],"previous_response_id":"resp_xyz","custom_field":42}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "routed request should return 200");

    let recorded = mock_a
        .last_responses_request()
        .expect("backend-a should record a request");

    assert_eq!(
        recorded.body, body,
        "body must be preserved byte-for-byte after model extraction"
    );
}

#[test]
fn responses_model_routing_non_responses_path_not_routed() {
    let (mock_a, mock_b, proxy) = setup_model_routing();

    let body = r#"{"model":"model-a","messages":[{"role":"user","content":"hi"}]}"#;
    let req = json_post("/v1/chat/completions", body);
    let raw = http_send(proxy.addr(), &req);

    assert_eq!(parse_status(&raw), 404, "non-Responses path should not match any route");

    let a_posts = mock_a
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(a_posts, 0, "backend-a should not receive any POST request");

    let b_posts = mock_b
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(b_posts, 0, "backend-b should not receive any POST request");
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

fn named_mock(backend_name: &str) -> ResponsesMockServerGuard {
    let name = backend_name.to_owned();
    start_responses_mock_server_with_config(ResponsesMockConfig {
        fixtures: vec![ResponsesFixture {
            content_type: None,
            matches: Box::new(|_| true),
            response_body: serde_json::json!({
                "id": "resp_mock_001",
                "object": "response",
                "created_at": 1_700_000_000_i64,
                "status": "completed",
                "model": "routed-model",
                "backend": name,
                "output": [{
                    "type": "message",
                    "id": "msg_001",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{
                        "type": "output_text",
                        "text": format!("Response from {name}"),
                        "annotations": []
                    }]
                }]
            })
            .to_string(),
        }],
        ..ResponsesMockConfig::default()
    })
}

struct ModelRoutingSetup {
    _proxy: ProxyGuard,
    addr: String,
}

impl ModelRoutingSetup {
    fn addr(&self) -> &str {
        &self.addr
    }
}

fn setup_model_routing() -> (ResponsesMockServerGuard, ResponsesMockServerGuard, ModelRoutingSetup) {
    let mock_a = named_mock("backend-a");
    let mock_b = named_mock("backend-b");
    let proxy_port = free_port();

    let config = load_example_config(
        "ai/responses-model-routing.yaml",
        proxy_port,
        HashMap::from([("127.0.0.1:3001", mock_a.port()), ("127.0.0.1:3002", mock_b.port())]),
    );
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    (mock_a, mock_b, ModelRoutingSetup { _proxy: proxy, addr })
}

fn send_responses_request(addr: &str, body: &str) -> String {
    let req = json_post("/v1/responses", body);
    http_send(addr, &req)
}
