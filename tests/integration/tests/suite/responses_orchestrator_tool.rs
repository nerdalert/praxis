// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Integration tests for responses_orchestrator local HTTP
//! tool execution (Checkpoint 6).
//!
//! Validates that the orchestrator executes an allowed local
//! tool via an HTTP subrequest when the model emits a
//! function_call, and rejects unknown tools.

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
// Tool Execution Tests
// -----------------------------------------------------------------------------

#[test]
fn orchestrator_executes_local_tool() {
    let (model_mock, tool_mock, proxy) = setup_cp6();

    let body = r#"{"model":"tool-model","input":"What is the weather in Boston?","tools":[{"type":"function","name":"get_weather"}]}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "should return 200");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("Tool executed successfully"),
        "should return final model response after tool execution: {response_body}"
    );

    assert_eq!(
        model_mock.responses_request_count(),
        2,
        "model backend should receive two requests (initial + reinference)"
    );
    assert_eq!(
        tool_mock.tool_request_count(),
        1,
        "tool backend should receive exactly one request"
    );
}

#[test]
fn orchestrator_sends_arguments_to_tool() {
    let (_model_mock, tool_mock, proxy) = setup_cp6();

    let body = r#"{"model":"tool-model","input":"weather","tools":[{"type":"function","name":"get_weather"}]}"#;
    let _raw = send_responses_request(proxy.addr(), body);

    let recorded = tool_mock.last_tool_request().expect("tool should record a request");

    assert_eq!(
        recorded.body, r#"{"city":"Boston"}"#,
        "tool backend should receive the function_call arguments"
    );
    assert_eq!(recorded.http_method, "POST", "tool request should be POST");
    assert_eq!(recorded.path, "/tool", "tool request should hit /tool path");
}

#[test]
fn orchestrator_tool_result_reaches_model() {
    let (model_mock, _tool_mock, proxy) = setup_cp6();

    let body = r#"{"model":"tool-model","input":"weather","tools":[{"type":"function","name":"get_weather"}]}"#;
    let _raw = send_responses_request(proxy.addr(), body);

    let requests = model_mock.received_requests();
    let model_posts: Vec<_> = requests
        .iter()
        .filter(|r| r.http_method == "POST" && r.path == "/v1/responses")
        .collect();

    assert_eq!(model_posts.len(), 2, "should have two model requests");

    assert!(
        model_posts[1].body.contains("sunny, 72F"),
        "second model request should contain tool output: {}",
        model_posts[1].body
    );
}

#[test]
fn orchestrator_unknown_tool_fails_closed() {
    let (model_mock, tool_mock, proxy) = setup_cp6_unknown_tool();

    let body = r#"{"model":"tool-model","input":"test","tools":[{"type":"function","name":"get_weather"}]}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 400, "unknown tool should return 400");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("not advertised in request"),
        "error should mention tool not advertised: {response_body}"
    );

    assert_eq!(
        model_mock.responses_request_count(),
        1,
        "model backend should still be called once"
    );

    let tool_posts = tool_mock
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(tool_posts, 0, "no tool backend should receive any POST");
}

#[test]
fn orchestrator_no_tool_call_preserves_cp4_behavior() {
    let (model_mock, tool_mock, proxy) = setup_cp6_final_response();

    let body = r#"{"model":"final-model","input":"Hello"}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "should return 200");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("backend-final"),
        "should return model backend response: {response_body}"
    );
    assert!(
        !response_body.contains("tool_executed"),
        "should not contain tool execution diagnostic"
    );

    assert_eq!(
        model_mock.responses_request_count(),
        1,
        "model backend should receive one request"
    );

    let tool_posts = tool_mock
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(tool_posts, 0, "tool backend should not receive any POST");
}

#[test]
fn orchestrator_cp6_non_responses_path_not_handled() {
    let (model_mock, tool_mock, proxy) = setup_cp6();

    let body = r#"{"model":"tool-model","messages":[{"role":"user","content":"hi"}]}"#;
    let req = json_post("/v1/chat/completions", body);
    let raw = http_send(proxy.addr(), &req);

    let response_body = parse_body(&raw);
    assert!(
        !response_body.contains("tool_executed"),
        "non-Responses path must not get orchestrator response: {response_body}"
    );

    let model_posts = model_mock
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(model_posts, 0, "model backend should not receive any POST");

    let tool_posts = tool_mock
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(tool_posts, 0, "tool backend should not receive any POST");
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

struct Cp6Setup {
    _proxy: ProxyGuard,
    addr: String,
}

impl Cp6Setup {
    fn addr(&self) -> &str {
        &self.addr
    }
}

fn setup_cp6() -> (ResponsesMockServerGuard, ToolHttpMockServerGuard, Cp6Setup) {
    let fc_response = function_call_response("call_weather_001", "get_weather", r#"{"city":"Boston"}"#);
    let final_resp = final_text_response("Tool executed successfully.");

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
                response_body: fc_response,
            },
        ],
        ..ResponsesMockConfig::default()
    });

    let tool_mock = start_tool_http_mock_server_with_config(ToolHttpMockConfig {
        response_body: r#"{"weather":"sunny, 72F"}"#.to_owned(),
        ..ToolHttpMockConfig::default()
    });

    let proxy_port = free_port();
    let yaml = cp6_yaml(
        proxy_port,
        model_mock.port(),
        tool_mock.port(),
        "tool-model",
        "get_weather",
    );
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    (model_mock, tool_mock, Cp6Setup { _proxy: proxy, addr })
}

fn setup_cp6_unknown_tool() -> (ResponsesMockServerGuard, ToolHttpMockServerGuard, Cp6Setup) {
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
    let yaml = cp6_yaml(
        proxy_port,
        model_mock.port(),
        tool_mock.port(),
        "tool-model",
        "get_weather",
    );
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    (model_mock, tool_mock, Cp6Setup { _proxy: proxy, addr })
}

fn setup_cp6_final_response() -> (ResponsesMockServerGuard, ToolHttpMockServerGuard, Cp6Setup) {
    let model_mock = start_responses_mock_server_with_config(ResponsesMockConfig {
        fixtures: vec![ResponsesFixture {
            content_type: None,
            matches: Box::new(|_| true),
            response_body: serde_json::json!({
                "id": "resp_mock_001",
                "object": "response",
                "created_at": 1_700_000_000_i64,
                "status": "completed",
                "model": "final-model",
                "backend": "backend-final",
                "output": [{
                    "type": "message",
                    "id": "msg_001",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "Final answer", "annotations": []}]
                }]
            })
            .to_string(),
        }],
        ..ResponsesMockConfig::default()
    });

    let tool_mock = start_tool_http_mock_server_with_config(ToolHttpMockConfig::default());

    let proxy_port = free_port();
    let yaml = cp6_yaml(
        proxy_port,
        model_mock.port(),
        tool_mock.port(),
        "final-model",
        "get_weather",
    );
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    (model_mock, tool_mock, Cp6Setup { _proxy: proxy, addr })
}

fn cp6_yaml(proxy_port: u16, model_port: u16, tool_port: u16, model_name: &str, tool_name: &str) -> String {
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

fn send_responses_request(addr: &str, body: &str) -> String {
    let req = json_post("/v1/responses", body);
    http_send(addr, &req)
}
