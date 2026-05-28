// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Integration tests for responses_orchestrator function_call
//! detection (Checkpoint 5).
//!
//! Validates that the orchestrator parses model responses and
//! detects function_call items without executing them.

use praxis_core::config::Config;
use praxis_test_utils::{
    ProxyGuard,
    agentic::{
        ResponsesFixture, ResponsesMockConfig, ResponsesMockServerGuard,
        responses::{final_text_response, function_call_response},
        start_responses_mock_server_with_config,
    },
    free_port, http_send,
    net::http_client::json_post,
    parse_body, parse_status, start_proxy,
};

// -----------------------------------------------------------------------------
// Function Call Detection
// -----------------------------------------------------------------------------

#[test]
fn orchestrator_rejects_unadvertised_tool() {
    let (_mock, proxy) = setup_function_call_mock();

    let body = r#"{"model":"tool-model","input":"What is the weather in Boston?"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(
        parse_status(&raw),
        400,
        "function_call with no advertised tools should return 400"
    );

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("not advertised in request"),
        "error should mention tool not advertised: {response_body}"
    );
    assert!(
        response_body.contains("get_weather"),
        "error should name the tool: {response_body}"
    );
}

#[test]
fn orchestrator_model_called_once_for_function_call() {
    let (mock, proxy) = setup_function_call_mock();

    let body = r#"{"model":"tool-model","input":"weather please"}"#;
    let _raw = send_responses_request(proxy.addr(), body);

    assert_eq!(
        mock.responses_request_count(),
        1,
        "model backend should receive exactly one request"
    );
}

#[test]
fn orchestrator_preserves_body_to_model_for_function_call() {
    let (mock, proxy) = setup_function_call_mock();

    let body =
        r#"{"model":"tool-model","input":"weather in Boston","tools":[{"type":"function","name":"get_weather"}]}"#;
    let _raw = send_responses_request(proxy.addr(), body);

    let recorded = mock.last_responses_request().expect("backend should record a request");

    assert_eq!(recorded.body, body, "model backend should receive exact original body");
}

#[test]
fn orchestrator_no_tool_response_returns_backend_response() {
    let (mock, proxy) = setup_final_response_mock();

    let body = r#"{"model":"final-model","input":"Hello"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "should return 200");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("The weather in Boston is sunny"),
        "should return backend final response: {response_body}"
    );
    assert!(
        !response_body.contains("tool_calls_detected"),
        "should not contain tool detection diagnostic"
    );

    assert_eq!(
        mock.responses_request_count(),
        1,
        "model backend should receive exactly one request"
    );
}

#[test]
fn orchestrator_cp5_unknown_model_no_backend_request() {
    let (mock, proxy) = setup_function_call_mock();

    let body = r#"{"model":"unknown","input":"test"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 404, "unknown model should return 404");

    let posts = mock
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(posts, 0, "no POST should reach backend for unknown model");
}

#[test]
fn orchestrator_cp5_non_responses_path_not_handled() {
    let (_mock, proxy) = setup_function_call_mock();

    let body = r#"{"model":"tool-model","messages":[{"role":"user","content":"hi"}]}"#;
    let req = json_post("/v1/chat/completions", body);
    let raw = http_send(proxy.addr(), &req);

    let response_body = parse_body(&raw);
    assert!(
        !response_body.contains("tool_calls_detected"),
        "non-Responses path must not get orchestrator response: {response_body}"
    );
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

struct Cp5Setup {
    _proxy: ProxyGuard,
    addr: String,
}

impl Cp5Setup {
    fn addr(&self) -> &str {
        &self.addr
    }
}

fn setup_function_call_mock() -> (ResponsesMockServerGuard, Cp5Setup) {
    let fc_response = function_call_response("call_weather_001", "get_weather", r#"{"city":"Boston"}"#);

    let mock = start_responses_mock_server_with_config(ResponsesMockConfig {
        fixtures: vec![ResponsesFixture {
            content_type: None,
            matches: Box::new(|_| true),
            response_body: fc_response,
        }],
        ..ResponsesMockConfig::default()
    });

    let proxy_port = free_port();
    let yaml = orchestrator_yaml(proxy_port, mock.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    (mock, Cp5Setup { _proxy: proxy, addr })
}

fn setup_final_response_mock() -> (ResponsesMockServerGuard, Cp5Setup) {
    let final_resp = final_text_response("The weather in Boston is sunny.");

    let mock = start_responses_mock_server_with_config(ResponsesMockConfig {
        fixtures: vec![ResponsesFixture {
            content_type: None,
            matches: Box::new(|_| true),
            response_body: final_resp,
        }],
        ..ResponsesMockConfig::default()
    });

    let proxy_port = free_port();
    let yaml = orchestrator_yaml_model(proxy_port, mock.port(), "final-model");
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    (mock, Cp5Setup { _proxy: proxy, addr })
}

fn orchestrator_yaml(proxy_port: u16, backend_port: u16) -> String {
    orchestrator_yaml_model(proxy_port, backend_port, "tool-model")
}

fn orchestrator_yaml_model(proxy_port: u16, backend_port: u16, model_name: &str) -> String {
    format!(
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
          {model_name}:
            endpoint: "127.0.0.1:{backend_port}"
        conditions:
          - when:
              path: "/v1/responses"
              methods: [POST]
"#
    )
}

fn send_responses_request(addr: &str, body: &str) -> String {
    let req = json_post("/v1/responses", body);
    http_send(addr, &req)
}
