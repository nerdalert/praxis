// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Integration tests for tool output guardrails inside
//! the orchestrator loop (Checkpoint 9).
//!
//! Validates that tool output is checked against guardrail
//! patterns before reinference, and that blocked content
//! is not blindly injected into the model's next turn.

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
// Guardrail Tests
// -----------------------------------------------------------------------------

#[test]
fn guardrail_blocks_tool_output_with_blocked_pattern() {
    let (model_mock, tool_mock, proxy) = setup_guardrail_blocked();

    let body = r#"{"model":"guard-model","input":"get secret data","tools":[{"type":"function","name":"get_data"}]}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 400, "blocked tool output should return 400");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("guardrail_violation"),
        "should indicate guardrail violation: {response_body}"
    );
    assert!(
        response_body.contains("tool_output_blocked"),
        "should include error code: {response_body}"
    );
    assert!(
        response_body.contains("CONFIDENTIAL"),
        "should mention the matched pattern: {response_body}"
    );

    assert_eq!(
        model_mock.responses_request_count(),
        1,
        "model should be called once (no reinference after block)"
    );
    assert_eq!(tool_mock.tool_request_count(), 1, "tool should be called once");
}

#[test]
fn guardrail_does_not_reinject_blocked_content() {
    let (model_mock, _tool_mock, proxy) = setup_guardrail_blocked();

    let body = r#"{"model":"guard-model","input":"get secret","tools":[{"type":"function","name":"get_data"}]}"#;
    let _raw = send_responses_request(proxy.addr(), body);

    let requests = model_mock.received_requests();
    let model_posts: Vec<_> = requests
        .iter()
        .filter(|r| r.http_method == "POST" && r.path == "/v1/responses")
        .collect();

    assert_eq!(
        model_posts.len(),
        1,
        "second model call should NOT happen after guardrail block"
    );

    assert!(
        !model_posts[0].body.contains("CONFIDENTIAL"),
        "first model request should not contain blocked content"
    );
}

#[test]
fn guardrail_allows_clean_tool_output() {
    let (model_mock, tool_mock, proxy) = setup_guardrail_clean();

    let body = r#"{"model":"guard-model","input":"get weather","tools":[{"type":"function","name":"get_weather"}]}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "clean tool output should return 200");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("Weather is fine"),
        "should return final model response: {response_body}"
    );
    assert!(
        !response_body.contains("guardrail_violation"),
        "should not indicate guardrail violation"
    );

    assert_eq!(
        model_mock.responses_request_count(),
        2,
        "model should be called twice (reinference happened)"
    );
    assert_eq!(tool_mock.tool_request_count(), 1, "tool should be called once");
}

#[test]
fn guardrail_no_patterns_configured_allows_all() {
    let (model_mock, tool_mock, proxy) = setup_no_guardrails();

    let body = r#"{"model":"guard-model","input":"get data","tools":[{"type":"function","name":"get_data"}]}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "no guardrails should return 200");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("Final answer with secret"),
        "should return final response: {response_body}"
    );

    assert_eq!(model_mock.responses_request_count(), 2, "model should be called twice");
    assert_eq!(tool_mock.tool_request_count(), 1, "tool should be called once");
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

struct Cp9Setup {
    _proxy: ProxyGuard,
    addr: String,
}

impl Cp9Setup {
    fn addr(&self) -> &str {
        &self.addr
    }
}

fn setup_guardrail_blocked() -> (ResponsesMockServerGuard, ToolHttpMockServerGuard, Cp9Setup) {
    let fc_resp = function_call_response("call_001", "get_data", r#"{}"#);
    let final_resp = final_text_response("Should not reach here.");

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
        response_body: r#"{"data":"CONFIDENTIAL: secret value"}"#.to_owned(),
        ..ToolHttpMockConfig::default()
    });

    let proxy_port = free_port();
    let yaml = guardrail_yaml(proxy_port, model_mock.port(), tool_mock.port(), &["CONFIDENTIAL"]);
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    (model_mock, tool_mock, Cp9Setup { _proxy: proxy, addr })
}

fn setup_guardrail_clean() -> (ResponsesMockServerGuard, ToolHttpMockServerGuard, Cp9Setup) {
    let fc_resp = function_call_response("call_001", "get_weather", r#"{"city":"Boston"}"#);
    let final_resp = final_text_response("Weather is fine.");

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
    let yaml = guardrail_yaml(
        proxy_port,
        model_mock.port(),
        tool_mock.port(),
        &["CONFIDENTIAL", "SECRET"],
    );
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    (model_mock, tool_mock, Cp9Setup { _proxy: proxy, addr })
}

fn setup_no_guardrails() -> (ResponsesMockServerGuard, ToolHttpMockServerGuard, Cp9Setup) {
    let fc_resp = function_call_response("call_001", "get_data", r#"{}"#);
    let final_resp = final_text_response("Final answer with secret data.");

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
        response_body: r#"{"data":"CONFIDENTIAL data here"}"#.to_owned(),
        ..ToolHttpMockConfig::default()
    });

    let proxy_port = free_port();
    let yaml = no_guardrail_yaml(proxy_port, model_mock.port(), tool_mock.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    (model_mock, tool_mock, Cp9Setup { _proxy: proxy, addr })
}

fn guardrail_yaml(proxy_port: u16, model_port: u16, tool_port: u16, patterns: &[&str]) -> String {
    let patterns_yaml: String = patterns
        .iter()
        .map(|p| format!("            - \"{p}\""))
        .collect::<Vec<_>>()
        .join("\n");
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
          guard-model:
            endpoint: "127.0.0.1:{model_port}"
        tools:
          get_data:
            endpoint: "127.0.0.1:{tool_port}"
          get_weather:
            endpoint: "127.0.0.1:{tool_port}"
        tool_output_guardrails:
          blocked_patterns:
{patterns_yaml}
        conditions:
          - when:
              path: "/v1/responses"
              methods: [POST]
"#
    )
}

fn no_guardrail_yaml(proxy_port: u16, model_port: u16, tool_port: u16) -> String {
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
          guard-model:
            endpoint: "127.0.0.1:{model_port}"
        tools:
          get_data:
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
