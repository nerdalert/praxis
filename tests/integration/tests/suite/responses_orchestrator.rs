// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Integration tests for the responses_orchestrator skeleton.
//!
//! Validates that the orchestrator returns a local
//! Responses-shaped response without contacting any
//! upstream backend.

use std::collections::HashMap;

use praxis_test_utils::{
    agentic::start_responses_mock_server, free_port, http_send, load_example_config, net::http_client::json_post,
    parse_body, parse_status, start_proxy,
};

// -----------------------------------------------------------------------------
// Orchestrator Skeleton
// -----------------------------------------------------------------------------

#[test]
fn orchestrator_returns_local_response() {
    let proxy = setup_orchestrator();

    let body = r#"{"model":"gpt-4.1-mini","input":"Hello"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "orchestrator should return 200");

    let response_body = parse_body(&raw);
    let response: serde_json::Value = serde_json::from_str(&response_body).expect("response should be valid JSON");

    assert_eq!(
        response["id"].as_str(),
        Some("resp_orchestrator_placeholder"),
        "response should contain orchestrator placeholder ID"
    );
    assert_eq!(
        response["object"].as_str(),
        Some("response"),
        "object should be 'response'"
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
fn orchestrator_does_not_contact_upstream() {
    let mock = start_responses_mock_server();
    let proxy_port = free_port();

    let yaml = format!(
        r#"
listeners:
  - name: gateway
    address: "127.0.0.1:{proxy_port}"
    filter_chains: [orchestrator, fallback]
filter_chains:
  - name: orchestrator
    filters:
      - filter: responses_orchestrator
        conditions:
          - when:
              path: "/v1/responses"
              methods: [POST]
  - name: fallback
    filters:
      - filter: router
        routes:
          - path_prefix: "/"
            cluster: backend
      - filter: load_balancer
        clusters:
          - name: backend
            endpoints:
              - "127.0.0.1:{backend_port}"
"#,
        backend_port = mock.port()
    );

    let config = praxis_core::config::Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let body = r#"{"model":"gpt-4.1-mini","input":"test"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "orchestrator should return 200");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("resp_orchestrator_placeholder"),
        "response should be from orchestrator, not upstream: {response_body}"
    );

    assert_eq!(
        mock.responses_request_count(),
        0,
        "upstream mock should receive zero /v1/responses requests"
    );
}

#[test]
fn orchestrator_echoes_model_from_body() {
    let proxy = setup_orchestrator();

    let body = r#"{"model":"custom-model-xyz","input":"test"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "should return 200");

    let response_body = parse_body(&raw);
    let response: serde_json::Value = serde_json::from_str(&response_body).expect("response should be valid JSON");

    assert_eq!(
        response["model"].as_str(),
        Some("custom-model-xyz"),
        "response model should echo request model"
    );
}

#[test]
fn orchestrator_handles_missing_model() {
    let proxy = setup_orchestrator();

    let body = r#"{"input":"no model field"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "should return 200 even without model");

    let response_body = parse_body(&raw);
    let response: serde_json::Value = serde_json::from_str(&response_body).expect("response should be valid JSON");

    assert_eq!(
        response["model"].as_str(),
        Some("unknown"),
        "missing model should produce 'unknown'"
    );
}

#[test]
fn orchestrator_non_responses_path_not_handled() {
    let proxy = setup_orchestrator();

    let body = r#"{"model":"gpt-4.1-mini","messages":[{"role":"user","content":"hi"}]}"#;
    let req = json_post("/v1/chat/completions", body);
    let raw = http_send(proxy.addr(), &req);

    let response_body = parse_body(&raw);
    assert!(
        !response_body.contains("resp_orchestrator_placeholder"),
        "non-Responses path must not receive orchestrator response: {response_body}"
    );
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

struct OrchestratorSetup {
    _proxy: praxis_test_utils::ProxyGuard,
    addr: String,
}

impl OrchestratorSetup {
    fn addr(&self) -> &str {
        &self.addr
    }
}

fn setup_orchestrator() -> OrchestratorSetup {
    let proxy_port = free_port();

    let config = load_example_config("ai/responses-orchestrator-skeleton.yaml", proxy_port, HashMap::new());
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    OrchestratorSetup { _proxy: proxy, addr }
}

fn send_responses_request(addr: &str, body: &str) -> String {
    let req = json_post("/v1/responses", body);
    http_send(addr, &req)
}
