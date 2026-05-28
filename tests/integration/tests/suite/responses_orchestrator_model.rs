// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Integration tests for responses_orchestrator model
//! backend subrequest (Checkpoint 4).
//!
//! Validates that the orchestrator calls the configured
//! model backend via an internal HTTP subrequest and
//! returns the backend's response to the client.

use praxis_core::config::Config;
use praxis_test_utils::{
    ProxyGuard,
    agentic::{
        ResponsesFixture, ResponsesMockConfig, ResponsesMockServerGuard, start_responses_mock_server_with_config,
    },
    free_port, http_send,
    net::http_client::json_post,
    parse_body, parse_status, start_proxy,
};

// -----------------------------------------------------------------------------
// Model Subrequest Tests
// -----------------------------------------------------------------------------

#[test]
fn orchestrator_calls_model_backend_once() {
    let (mock_a, _mock_b, proxy) = setup_cp4();

    let body = r#"{"model":"model-a","input":"Hello"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "should return 200 from model backend");

    assert_eq!(
        mock_a.responses_request_count(),
        1,
        "model backend should receive exactly one request"
    );
}

#[test]
fn orchestrator_returns_model_backend_response() {
    let (_mock_a, _mock_b, proxy) = setup_cp4();

    let body = r#"{"model":"model-a","input":"Hello"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "should return 200");

    let response_body = parse_body(&raw);
    let response: serde_json::Value = serde_json::from_str(&response_body).expect("response should be valid JSON");

    assert_eq!(
        response["backend"].as_str(),
        Some("backend-a"),
        "response should come from backend-a: {response_body}"
    );
    assert_eq!(
        response["object"].as_str(),
        Some("response"),
        "should be a Responses object"
    );
    assert_eq!(
        response["status"].as_str(),
        Some("completed"),
        "status should be completed"
    );

    assert!(
        !response_body.contains("resp_orchestrator_placeholder"),
        "should not be the placeholder response"
    );
}

#[test]
fn orchestrator_preserves_request_body_to_model_backend() {
    let (mock_a, _mock_b, proxy) = setup_cp4();

    let body = r#"{"model":"model-a","input":"test body","store":false,"tools":[{"type":"function","name":"get_weather"}],"custom_field":42}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "should return 200");

    let recorded = mock_a
        .last_responses_request()
        .expect("backend should record a request");

    assert_eq!(
        recorded.body, body,
        "model backend should receive exact original body byte-for-byte"
    );
    assert_eq!(recorded.http_method, "POST", "should be a POST request");
    assert_eq!(recorded.path, "/v1/responses", "should POST to /v1/responses");
}

#[test]
fn orchestrator_routes_model_b_to_backend_b() {
    let (mock_a, mock_b, proxy) = setup_cp4();

    let body = r#"{"model":"model-b","input":"Hello B"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "should return 200 from model-b backend");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("backend-b"),
        "response should come from backend-b: {response_body}"
    );

    assert_eq!(
        mock_a.responses_request_count(),
        0,
        "backend-a should receive zero requests when model-b is requested"
    );
    assert_eq!(
        mock_b.responses_request_count(),
        1,
        "backend-b should receive exactly one request"
    );
}

#[test]
fn orchestrator_unknown_model_returns_error_without_backend_request() {
    let (mock_a, mock_b, proxy) = setup_cp4();

    let body = r#"{"model":"unknown-model","input":"test"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 404, "unknown model should return 404");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("unknown model"),
        "error should mention unknown model: {response_body}"
    );

    let a_posts = mock_a
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(a_posts, 0, "backend-a should not receive any POST");

    let b_posts = mock_b
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(b_posts, 0, "backend-b should not receive any POST");
}

#[test]
fn orchestrator_missing_model_returns_error_without_backend_request() {
    let (mock_a, mock_b, proxy) = setup_cp4();

    let body = r#"{"input":"no model field"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 400, "missing model should return 400");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("missing required field: model"),
        "error should mention missing field: {response_body}"
    );

    let a_posts = mock_a
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(a_posts, 0, "backend-a should not receive any POST");

    let b_posts = mock_b
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(b_posts, 0, "backend-b should not receive any POST");
}

#[test]
fn orchestrator_cp4_non_responses_path_not_handled() {
    let (mock_a, mock_b, proxy) = setup_cp4();

    let body = r#"{"model":"model-a","messages":[{"role":"user","content":"hi"}]}"#;
    let req = json_post("/v1/chat/completions", body);
    let raw = http_send(proxy.addr(), &req);

    let response_body = parse_body(&raw);
    assert!(
        !response_body.contains("resp_orchestrator_placeholder"),
        "non-Responses path must not receive orchestrator response: {response_body}"
    );
    assert!(
        !response_body.contains("backend-a"),
        "non-Responses path must not receive model backend response: {response_body}"
    );

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

struct Cp4Setup {
    _proxy: ProxyGuard,
    addr: String,
}

impl Cp4Setup {
    fn addr(&self) -> &str {
        &self.addr
    }
}

fn setup_cp4() -> (ResponsesMockServerGuard, ResponsesMockServerGuard, Cp4Setup) {
    let mock_a = named_mock("backend-a");
    let mock_b = named_mock("backend-b");
    let proxy_port = free_port();

    let yaml = format!(
        r#"
listeners:
  - name: gateway
    address: "127.0.0.1:{proxy_port}"
    filter_chains: [orchestrator]
filter_chains:
  - name: orchestrator
    filters:
      - filter: responses_orchestrator
        timeout_ms: 5000
        models:
          model-a:
            endpoint: "127.0.0.1:{port_a}"
          model-b:
            endpoint: "127.0.0.1:{port_b}"
        conditions:
          - when:
              path: "/v1/responses"
              methods: [POST]
"#,
        port_a = mock_a.port(),
        port_b = mock_b.port(),
    );

    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    (mock_a, mock_b, Cp4Setup { _proxy: proxy, addr })
}

fn send_responses_request(addr: &str, body: &str) -> String {
    let req = json_post("/v1/responses", body);
    http_send(addr, &req)
}
