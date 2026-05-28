// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Integration tests for the full model → tool → model
//! loop (Checkpoint 7).
//!
//! Validates that the orchestrator calls the model, detects
//! a function_call, executes the tool, injects
//! function_call_output, calls the model again, and returns
//! the final response.

use praxis_core::config::Config;
use praxis_test_utils::{
    ProxyGuard,
    agentic::{
        ResponsesFixture, ResponsesMockConfig, ResponsesMockServerGuard, ToolHttpMockConfig, ToolHttpMockServerGuard,
        responses::{final_text_response, function_call_response},
        start_responses_mock_server_with_config, start_tool_http_mock_server_with_config,
    },
    free_port, http_send,
    net::http_client::json_post,
    parse_body, parse_status, start_proxy,
};

// -----------------------------------------------------------------------------
// Full Loop Tests
// -----------------------------------------------------------------------------

#[test]
fn orchestrator_full_loop_returns_final_response() {
    let (model_mock, _tool_mock, proxy) = setup_cp7();

    let body = r#"{"model":"loop-model","input":"What is the weather in Boston?","tools":[{"type":"function","name":"get_weather"}]}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "should return 200");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("It is sunny and 72F in Boston"),
        "should return final model answer: {response_body}"
    );
    assert!(
        !response_body.contains("tool_executed_pending"),
        "should not be a diagnostic response"
    );

    assert_eq!(
        model_mock.responses_request_count(),
        2,
        "model backend should receive exactly two requests"
    );
}

#[test]
fn orchestrator_full_loop_tool_called_once() {
    let (_model_mock, tool_mock, proxy) = setup_cp7();

    let body = r#"{"model":"loop-model","input":"weather","tools":[{"type":"function","name":"get_weather"}]}"#;
    let _raw = send_responses_request(proxy.addr(), body);

    assert_eq!(
        tool_mock.tool_request_count(),
        1,
        "tool backend should receive exactly one request"
    );
}

#[test]
fn orchestrator_second_model_request_includes_function_call_output() {
    let (model_mock, _tool_mock, proxy) = setup_cp7();

    let body =
        r#"{"model":"loop-model","input":"weather in Boston","tools":[{"type":"function","name":"get_weather"}]}"#;
    let _raw = send_responses_request(proxy.addr(), body);

    let requests = model_mock.received_requests();
    let model_posts: Vec<_> = requests
        .iter()
        .filter(|r| r.http_method == "POST" && r.path == "/v1/responses")
        .collect();

    assert_eq!(model_posts.len(), 2, "should have two model requests");

    let second_body = &model_posts[1].body;
    assert!(
        second_body.contains("function_call_output"),
        "second model request should contain function_call_output: {second_body}"
    );
    assert!(
        second_body.contains("call_weather_001"),
        "second model request should contain the call_id: {second_body}"
    );
    assert!(
        second_body.contains("sunny, 72F"),
        "second model request should contain the tool result: {second_body}"
    );
}

#[test]
fn orchestrator_first_model_request_is_original_body() {
    let (model_mock, _tool_mock, proxy) = setup_cp7();

    let body =
        r#"{"model":"loop-model","input":"weather in Boston","tools":[{"type":"function","name":"get_weather"}]}"#;
    let _raw = send_responses_request(proxy.addr(), body);

    let requests = model_mock.received_requests();
    let model_posts: Vec<_> = requests
        .iter()
        .filter(|r| r.http_method == "POST" && r.path == "/v1/responses")
        .collect();

    assert_eq!(
        model_posts[0].body, body,
        "first model request should be the original body"
    );
}

#[test]
fn orchestrator_no_tool_call_returns_directly() {
    let (model_mock, tool_mock, proxy) = setup_cp7_final_only();

    let body = r#"{"model":"final-model","input":"Hello"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "should return 200");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("Direct final answer"),
        "should return final response directly: {response_body}"
    );

    assert_eq!(model_mock.responses_request_count(), 1, "model should be called once");

    let tool_posts = tool_mock
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(tool_posts, 0, "tool should not be called");
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

struct Cp7Setup {
    _proxy: ProxyGuard,
    addr: String,
}

impl Cp7Setup {
    fn addr(&self) -> &str {
        &self.addr
    }
}

fn setup_cp7() -> (ResponsesMockServerGuard, ToolHttpMockServerGuard, Cp7Setup) {
    let fc_resp = function_call_response("call_weather_001", "get_weather", r#"{"city":"Boston"}"#);
    let final_resp = final_text_response("It is sunny and 72F in Boston.");

    let model_mock = start_responses_mock_server_with_config(ResponsesMockConfig {
        fixtures: vec![
            ResponsesFixture {
                content_type: None,
                matches: Box::new(|body| body.contains("function_call_output")),
                response_body: final_resp,
            },
            ResponsesFixture {
                content_type: None,
                matches: Box::new(|_| true),
                response_body: fc_resp,
            },
        ],
        ..ResponsesMockConfig::default()
    });

    let tool_mock = start_tool_http_mock_server_with_config(ToolHttpMockConfig {
        response_body: r#"{"weather":"sunny, 72F"}"#.to_owned(),
        ..ToolHttpMockConfig::default()
    });

    let proxy_port = free_port();
    let yaml = loop_yaml(proxy_port, model_mock.port(), tool_mock.port(), "loop-model");
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    (model_mock, tool_mock, Cp7Setup { _proxy: proxy, addr })
}

fn setup_cp7_final_only() -> (ResponsesMockServerGuard, ToolHttpMockServerGuard, Cp7Setup) {
    let final_resp = final_text_response("Direct final answer.");

    let model_mock = start_responses_mock_server_with_config(ResponsesMockConfig {
        fixtures: vec![ResponsesFixture {
            content_type: None,
            matches: Box::new(|_| true),
            response_body: final_resp,
        }],
        ..ResponsesMockConfig::default()
    });

    let tool_mock = start_tool_http_mock_server_with_config(ToolHttpMockConfig::default());

    let proxy_port = free_port();
    let yaml = loop_yaml(proxy_port, model_mock.port(), tool_mock.port(), "final-model");
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    (model_mock, tool_mock, Cp7Setup { _proxy: proxy, addr })
}

fn loop_yaml(proxy_port: u16, model_port: u16, tool_port: u16, model_name: &str) -> String {
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
        max_iterations: 5
        models:
          {model_name}:
            endpoint: "127.0.0.1:{model_port}"
        tools:
          get_weather:
            endpoint: "127.0.0.1:{tool_port}"
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
