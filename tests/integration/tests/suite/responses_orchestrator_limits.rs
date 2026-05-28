// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Integration tests for orchestrator loop controls and
//! failure modes (Checkpoint 8).
//!
//! Validates max iterations, unknown tool fail-closed,
//! and tool non-2xx behavior.

use praxis_core::config::Config;
use praxis_test_utils::{
    ProxyGuard,
    agentic::{
        ResponsesFixture, ResponsesMockConfig, ResponsesMockServerGuard, ToolHttpMockConfig, ToolHttpMockServerGuard,
        responses::function_call_response, start_responses_mock_server_with_config,
        start_tool_http_mock_server_with_config,
    },
    free_port, http_send,
    net::http_client::json_post,
    parse_body, parse_status, start_proxy,
};

// -----------------------------------------------------------------------------
// Max Iterations
// -----------------------------------------------------------------------------

#[test]
fn orchestrator_max_iterations_returns_incomplete() {
    let (model_mock, tool_mock, proxy) = setup_always_tool_call(2);

    let body = r#"{"model":"loop-model","input":"infinite loop","tools":[{"type":"function","name":"get_weather"}]}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "max iterations should return 200");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("incomplete"),
        "response should indicate incomplete: {response_body}"
    );
    assert!(
        response_body.contains("max iterations"),
        "response should mention max iterations: {response_body}"
    );

    assert_eq!(
        model_mock.responses_request_count(),
        2,
        "model should be called max_iterations times"
    );
    assert_eq!(
        tool_mock.tool_request_count(),
        2,
        "tool should be called once per iteration"
    );
}

#[test]
fn orchestrator_max_iterations_one_stops_after_first_tool() {
    let (model_mock, tool_mock, proxy) = setup_always_tool_call(1);

    let body = r#"{"model":"loop-model","input":"test","tools":[{"type":"function","name":"get_weather"}]}"#;
    let raw = send_responses_request(proxy.addr(), body);

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("incomplete"),
        "should return incomplete: {response_body}"
    );

    assert_eq!(
        model_mock.responses_request_count(),
        1,
        "model should be called exactly once"
    );
    assert_eq!(tool_mock.tool_request_count(), 1, "tool should be called exactly once");
}

// -----------------------------------------------------------------------------
// Unknown Tool
// -----------------------------------------------------------------------------

#[test]
fn orchestrator_unknown_tool_returns_error() {
    let fc_response = function_call_response("call_001", "nonexistent_tool", r#"{}"#);

    let model_mock = start_responses_mock_server_with_config(ResponsesMockConfig {
        fixtures: vec![ResponsesFixture {
            content_type: None,
            matches: Box::new(|_| true),
            response_body: fc_response,
        }],
        ..ResponsesMockConfig::default()
    });

    let tool_mock = start_tool_http_mock_server_with_config(ToolHttpMockConfig::default());

    let proxy_port = free_port();
    let yaml = limits_yaml(proxy_port, model_mock.port(), tool_mock.port(), 5, "get_weather");
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let body = r#"{"model":"loop-model","input":"test","tools":[{"type":"function","name":"nonexistent_tool"}]}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 400, "unknown tool should return 400");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("unknown tool"),
        "should mention unknown tool: {response_body}"
    );
    assert!(
        response_body.contains("nonexistent_tool"),
        "should name the tool: {response_body}"
    );

    let tool_posts = tool_mock
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(tool_posts, 0, "no tool backend should receive any POST");
}

// -----------------------------------------------------------------------------
// Tool Non-2xx
// -----------------------------------------------------------------------------

#[test]
fn orchestrator_tool_non_2xx_returns_error() {
    let fc_response = function_call_response("call_001", "failing_tool", r#"{}"#);

    let model_mock = start_responses_mock_server_with_config(ResponsesMockConfig {
        fixtures: vec![ResponsesFixture {
            content_type: None,
            matches: Box::new(|_| true),
            response_body: fc_response,
        }],
        ..ResponsesMockConfig::default()
    });

    let tool_mock = start_tool_http_mock_server_with_config(ToolHttpMockConfig::default());

    let proxy_port = free_port();
    let yaml = limits_yaml_tool_path(
        proxy_port,
        model_mock.port(),
        tool_mock.port(),
        5,
        "failing_tool",
        "/nonexistent",
    );
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let body = r#"{"model":"loop-model","input":"test","tools":[{"type":"function","name":"failing_tool"}]}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 502, "tool non-2xx should return 502");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("tool backend error"),
        "should mention tool backend error: {response_body}"
    );

    assert_eq!(
        model_mock.responses_request_count(),
        1,
        "model should be called once before tool failure"
    );
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

struct LimitsSetup {
    _proxy: ProxyGuard,
    addr: String,
}

impl LimitsSetup {
    fn addr(&self) -> &str {
        &self.addr
    }
}

fn setup_always_tool_call(max_iterations: u32) -> (ResponsesMockServerGuard, ToolHttpMockServerGuard, LimitsSetup) {
    let fc_response = function_call_response("call_001", "get_weather", r#"{"city":"Boston"}"#);

    let model_mock = start_responses_mock_server_with_config(ResponsesMockConfig {
        fixtures: vec![ResponsesFixture {
            content_type: None,
            matches: Box::new(|_| true),
            response_body: fc_response,
        }],
        ..ResponsesMockConfig::default()
    });

    let tool_mock = start_tool_http_mock_server_with_config(ToolHttpMockConfig {
        response_body: r#"{"weather":"sunny"}"#.to_owned(),
        ..ToolHttpMockConfig::default()
    });

    let proxy_port = free_port();
    let yaml = limits_yaml_with_max(proxy_port, model_mock.port(), tool_mock.port(), max_iterations);
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    (model_mock, tool_mock, LimitsSetup { _proxy: proxy, addr })
}

fn limits_yaml_with_max(proxy_port: u16, model_port: u16, tool_port: u16, max_iterations: u32) -> String {
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
        max_iterations: {max_iterations}
        models:
          loop-model:
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

fn limits_yaml(proxy_port: u16, model_port: u16, tool_port: u16, max_iterations: u32, tool_name: &str) -> String {
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
        max_iterations: {max_iterations}
        models:
          loop-model:
            endpoint: "127.0.0.1:{model_port}"
        tools:
          {tool_name}:
            endpoint: "127.0.0.1:{tool_port}"
        conditions:
          - when:
              path: "/v1/responses"
              methods: [POST]
"#
    )
}

fn limits_yaml_tool_path(
    proxy_port: u16,
    model_port: u16,
    tool_port: u16,
    max_iterations: u32,
    tool_name: &str,
    tool_path: &str,
) -> String {
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
        max_iterations: {max_iterations}
        models:
          loop-model:
            endpoint: "127.0.0.1:{model_port}"
        tools:
          {tool_name}:
            endpoint: "127.0.0.1:{tool_port}"
            path: "{tool_path}"
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
