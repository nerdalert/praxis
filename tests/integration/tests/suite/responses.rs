// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Integration tests for Responses API stateless pass-through.
//!
//! Validates that Praxis forwards Responses API requests to the
//! configured backend without modifying the request body.

use std::collections::HashMap;

use praxis_test_utils::{
    agentic::{ResponsesMockServerGuard, start_responses_mock_server},
    free_port, http_send, load_example_config,
    net::http_client::json_post,
    parse_body, parse_status, start_proxy,
};

// -----------------------------------------------------------------------------
// Stateless Pass-Through
// -----------------------------------------------------------------------------

#[test]
fn responses_stateless_simple_request_forwarded() {
    let (mock, proxy) = setup_passthrough();

    let body = r#"{"model":"gpt-4.1-mini","input":"Hello, world!"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "proxy should return 200");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("resp_mock_test_001"),
        "client should receive mock response ID: {response_body}"
    );

    assert_passthrough(&mock, body);
}

#[test]
fn responses_stateless_preserves_unknown_fields() {
    let (mock, proxy) = setup_passthrough();

    let body = r#"{"model":"gpt-4.1-mini","input":"test","custom_field":"preserve_me","nested":{"deep":true}}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "proxy should return 200");
    assert_passthrough(&mock, body);
}

#[test]
fn responses_stateless_preserves_previous_response_id() {
    let (mock, proxy) = setup_passthrough();

    let body = r#"{"model":"gpt-4.1-mini","input":"follow up","previous_response_id":"resp_abc123"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "proxy should return 200");
    assert_passthrough(&mock, body);
}

#[test]
fn responses_stateless_preserves_store_and_tools() {
    let (mock, proxy) = setup_passthrough();

    let body = r#"{"model":"gpt-4.1-mini","input":"test","store":false,"tools":[{"type":"function","name":"get_weather","parameters":{"type":"object"}}],"reasoning":{"effort":"high"},"include":["usage"]}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "proxy should return 200");
    assert_passthrough(&mock, body);
}

#[test]
fn responses_stateless_preserves_item_array_input() {
    let (mock, proxy) = setup_passthrough();

    let body = r#"{"model":"gpt-4.1-mini","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Hello"}]}]}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "proxy should return 200");
    assert_passthrough(&mock, body);
}

#[test]
fn responses_stateless_mock_returns_valid_response_structure() {
    let (_mock, proxy) = setup_passthrough();

    let body = r#"{"model":"gpt-4.1-mini","input":"test"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "proxy should return 200");

    let response_body = parse_body(&raw);
    let response: serde_json::Value = serde_json::from_str(&response_body).expect("response should be valid JSON");

    assert_eq!(
        response["id"].as_str(),
        Some("resp_mock_test_001"),
        "response ID should match mock"
    );
    assert_eq!(
        response["object"].as_str(),
        Some("response"),
        "object type should be 'response'"
    );
    assert_eq!(
        response["status"].as_str(),
        Some("completed"),
        "status should be 'completed'"
    );
    assert_eq!(
        response["model"].as_str(),
        Some("gpt-4.1-mini"),
        "model should echo request model"
    );
}

#[test]
fn responses_stateless_non_responses_path_not_routed() {
    let (mock, proxy) = setup_passthrough();

    let body = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
    let req = json_post("/v1/chat/completions", body);
    let raw = http_send(proxy.addr(), &req);

    assert_eq!(
        parse_status(&raw),
        404,
        "non-Responses path should not match the exact /v1/responses route"
    );

    assert_eq!(
        mock.responses_request_count(),
        0,
        "/v1/chat/completions POST should not reach the backend"
    );
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

struct PassthroughSetup {
    _proxy: praxis_test_utils::ProxyGuard,
    addr: String,
}

impl PassthroughSetup {
    fn addr(&self) -> &str {
        &self.addr
    }
}

fn setup_passthrough() -> (ResponsesMockServerGuard, PassthroughSetup) {
    let mock = start_responses_mock_server();
    let proxy_port = free_port();

    let config = load_example_config(
        "ai/responses-stateless-pass-through.yaml",
        proxy_port,
        HashMap::from([("127.0.0.1:3000", mock.port())]),
    );
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    (mock, PassthroughSetup { _proxy: proxy, addr })
}

fn send_responses_request(addr: &str, body: &str) -> String {
    let req = json_post("/v1/responses", body);
    http_send(addr, &req)
}

fn assert_passthrough(mock: &ResponsesMockServerGuard, expected_body: &str) {
    assert_eq!(
        mock.responses_request_count(),
        1,
        "backend should receive exactly one /v1/responses POST"
    );

    let recorded = mock
        .last_responses_request()
        .expect("should have a recorded /v1/responses POST");

    assert_eq!(recorded.http_method, "POST", "recorded request method should be POST");
    assert_eq!(
        recorded.path, "/v1/responses",
        "recorded request path should be /v1/responses"
    );
    assert_eq!(
        recorded.body, expected_body,
        "backend should receive the exact request body byte-for-byte"
    );
}
