// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Integration tests for responses_orchestrator state
//! load/persist behavior (Checkpoint 10).
//!
//! Validates `store`, `previous_response_id`, and
//! `conversation` behavior.

use praxis_core::config::Config;
use praxis_test_utils::{
    ProxyGuard,
    agentic::{
        ResponsesFixture, ResponsesMockConfig, ResponsesMockServerGuard, responses::final_text_response,
        start_responses_mock_server_with_config,
    },
    free_port, http_send,
    net::http_client::json_post,
    parse_body, parse_status, start_proxy,
};

// -----------------------------------------------------------------------------
// store: true / false
// -----------------------------------------------------------------------------

#[test]
fn store_true_persists_completed_response() {
    let (mock, proxy) = setup_state();

    let body1 = r#"{"model":"state-model","input":"My name is Ada","store":true}"#;
    let raw1 = send_responses_request(proxy.addr(), body1);
    assert_eq!(parse_status(&raw1), 200, "first request should succeed");

    let body2 = r#"{"model":"state-model","input":"What is my name?","previous_response_id":"resp_mock_test_001"}"#;
    let raw2 = send_responses_request(proxy.addr(), body2);
    assert_eq!(
        parse_status(&raw2),
        200,
        "second request using previous_response_id should succeed"
    );

    assert_eq!(mock.responses_request_count(), 2, "model should be called twice");
}

#[test]
fn previous_response_id_loads_prior_output_into_model_request() {
    let (mock, proxy) = setup_state();

    let body1 = r#"{"model":"state-model","input":"My name is Ada","store":true}"#;
    let _raw1 = send_responses_request(proxy.addr(), body1);

    let body2 = r#"{"model":"state-model","input":"What is my name?","previous_response_id":"resp_mock_test_001"}"#;
    let _raw2 = send_responses_request(proxy.addr(), body2);

    let requests = mock.received_requests();
    let model_posts: Vec<_> = requests
        .iter()
        .filter(|r| r.http_method == "POST" && r.path == "/v1/responses")
        .collect();

    assert_eq!(model_posts.len(), 2, "should have two model requests");

    let second_body = &model_posts[1].body;
    assert!(
        second_body.contains("My name is Ada"),
        "second model request should include prior user input: {second_body}"
    );
    assert!(
        second_body.contains("This is a mock response"),
        "second model request should include prior assistant output: {second_body}"
    );
    assert!(
        second_body.contains("What is my name?"),
        "second model request should include new input: {second_body}"
    );
    assert!(
        !second_body.contains("previous_response_id"),
        "previous_response_id should be removed from upstream request: {second_body}"
    );
}

#[test]
fn store_false_does_not_persist() {
    let (_mock, proxy) = setup_state();

    let body1 = r#"{"model":"state-model","input":"Do not store","store":false}"#;
    let raw1 = send_responses_request(proxy.addr(), body1);
    assert_eq!(parse_status(&raw1), 200, "first request should succeed");

    let body2 = r#"{"model":"state-model","input":"continue","previous_response_id":"resp_mock_test_001"}"#;
    let raw2 = send_responses_request(proxy.addr(), body2);
    assert_eq!(parse_status(&raw2), 404, "referencing store:false response should fail");

    let response_body = parse_body(&raw2);
    assert!(
        response_body.contains("previous response not found"),
        "error should mention not found: {response_body}"
    );
}

#[test]
fn missing_previous_response_id_returns_error_without_backend_calls() {
    let (mock, proxy) = setup_state();

    let body = r#"{"model":"state-model","input":"continue","previous_response_id":"resp_missing"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 404, "missing previous response should return 404");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("previous response not found"),
        "error should mention not found: {response_body}"
    );

    let model_posts = mock
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(
        model_posts, 0,
        "no model call should happen for missing previous response"
    );
}

// -----------------------------------------------------------------------------
// conversation
// -----------------------------------------------------------------------------

#[test]
fn conversation_id_uses_latest_response() {
    let (mock, proxy) = setup_state();

    let body1 = r#"{"model":"state-model","conversation":"conv_1","input":"Remember blue","store":true}"#;
    let raw1 = send_responses_request(proxy.addr(), body1);
    assert_eq!(parse_status(&raw1), 200, "first conversation request should succeed");

    let body2 = r#"{"model":"state-model","conversation":"conv_1","input":"What color?"}"#;
    let raw2 = send_responses_request(proxy.addr(), body2);
    assert_eq!(parse_status(&raw2), 200, "second conversation request should succeed");

    let requests = mock.received_requests();
    let model_posts: Vec<_> = requests
        .iter()
        .filter(|r| r.http_method == "POST" && r.path == "/v1/responses")
        .collect();

    assert_eq!(model_posts.len(), 2, "should have two model requests");

    let second_body = &model_posts[1].body;
    assert!(
        second_body.contains("This is a mock response"),
        "second request should include prior context from conversation: {second_body}"
    );
    assert!(
        second_body.contains("What color?"),
        "second request should include new input: {second_body}"
    );
}

// -----------------------------------------------------------------------------
// Regression
// -----------------------------------------------------------------------------

#[test]
fn no_previous_response_no_conversation_keeps_original_body() {
    let (mock, proxy) = setup_state();

    let body = r#"{"model":"state-model","input":"fresh request"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "fresh request should succeed");

    let requests = mock.received_requests();
    let model_posts: Vec<_> = requests
        .iter()
        .filter(|r| r.http_method == "POST" && r.path == "/v1/responses")
        .collect();

    assert_eq!(model_posts.len(), 1, "should have one model request");
    assert_eq!(
        model_posts[0].body, body,
        "model should receive original body unchanged"
    );
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

struct StateSetup {
    _proxy: ProxyGuard,
    addr: String,
}

impl StateSetup {
    fn addr(&self) -> &str {
        &self.addr
    }
}

fn setup_state() -> (ResponsesMockServerGuard, StateSetup) {
    let final_resp = final_text_response("This is a mock response.");

    let mock = start_responses_mock_server_with_config(ResponsesMockConfig {
        fixtures: vec![ResponsesFixture {
            content_type: None,
            matches: Box::new(|_| true),
            response_body: final_resp,
        }],
        ..ResponsesMockConfig::default()
    });

    let proxy_port = free_port();
    let yaml = state_yaml(proxy_port, mock.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    (mock, StateSetup { _proxy: proxy, addr })
}

fn state_yaml(proxy_port: u16, model_port: u16) -> String {
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
          state-model:
            endpoint: "127.0.0.1:{model_port}"
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
