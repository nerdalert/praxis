// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Integration tests for streaming buffering in the
//! orchestrator (Checkpoint 11).
//!
//! Validates that SSE function-call arguments split across
//! chunks are buffered, tool executes once with complete
//! arguments, and the loop completes.

use praxis_core::config::Config;
use praxis_test_utils::{
    ProxyGuard,
    agentic::{
        ResponsesFixture, ResponsesMockConfig, ResponsesMockServerGuard, ToolHttpMockConfig, ToolHttpMockServerGuard,
        responses::{final_text_response, streaming_function_call_response},
        start_responses_mock_server_with_config, start_tool_http_mock_server_with_config,
    },
    free_port, http_send,
    net::http_client::json_post,
    parse_body, parse_status, start_proxy,
};

// -----------------------------------------------------------------------------
// Streaming Tests
// -----------------------------------------------------------------------------

#[test]
fn streaming_function_call_arguments_split_across_chunks_executes_once() {
    let (_model_mock, tool_mock, proxy) = setup_streaming();

    let body = r#"{"model":"stream-model","input":"Weather?","tools":[{"type":"function","name":"get_weather"}],"stream":true}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "should return 200");

    assert_eq!(tool_mock.tool_request_count(), 1, "tool should execute exactly once");

    let recorded = tool_mock.last_tool_request().expect("tool should record request");
    assert_eq!(
        recorded.body, r#"{"city":"Boston"}"#,
        "tool should receive complete arguments"
    );
}

#[test]
fn streaming_loop_reinjects_function_call_output() {
    let (model_mock, _tool_mock, proxy) = setup_streaming();

    let body = r#"{"model":"stream-model","input":"Weather?","tools":[{"type":"function","name":"get_weather"}],"stream":true}"#;
    let _raw = send_responses_request(proxy.addr(), body);

    let requests = model_mock.received_requests();
    let model_posts: Vec<_> = requests
        .iter()
        .filter(|r| r.http_method == "POST" && r.path == "/v1/responses")
        .collect();

    assert_eq!(model_posts.len(), 2, "model should be called twice");

    let second_body = &model_posts[1].body;
    assert!(
        second_body.contains("function_call_output"),
        "second model request should contain function_call_output: {second_body}"
    );
    assert!(
        second_body.contains("call_weather_001"),
        "second model request should contain call_id: {second_body}"
    );
}

#[test]
fn streaming_unknown_tool_fails_closed() {
    let sse_resp = streaming_function_call_response("call_001", "nonexistent_tool", r#"{"x":1}"#);

    let model_mock = start_responses_mock_server_with_config(ResponsesMockConfig {
        fixtures: vec![ResponsesFixture {
            content_type: Some("text/event-stream".to_owned()),
            matches: Box::new(|_| true),
            response_body: sse_resp,
        }],
        ..ResponsesMockConfig::default()
    });

    let tool_mock = start_tool_http_mock_server_with_config(ToolHttpMockConfig::default());

    let proxy_port = free_port();
    let yaml = streaming_yaml(proxy_port, model_mock.port(), tool_mock.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let body =
        r#"{"model":"stream-model","input":"test","tools":[{"type":"function","name":"get_weather"}],"stream":true}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 400, "unknown tool should return 400");

    let tool_posts = tool_mock
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(tool_posts, 0, "no tool should be called");
}

#[test]
fn streaming_final_without_tool_returns_synthesized_json() {
    let sse_resp = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello \"}\n\
                    data: {\"type\":\"response.output_text.delta\",\"delta\":\"world\"}\n\
                    data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_sse_001\",\"model\":\"stream-model\"}}\n\
                    data: [DONE]\n";

    let model_mock = start_responses_mock_server_with_config(ResponsesMockConfig {
        fixtures: vec![ResponsesFixture {
            content_type: Some("text/event-stream".to_owned()),
            matches: Box::new(|_| true),
            response_body: sse_resp.to_owned(),
        }],
        ..ResponsesMockConfig::default()
    });

    let tool_mock = start_tool_http_mock_server_with_config(ToolHttpMockConfig::default());

    let proxy_port = free_port();
    let yaml = streaming_yaml(proxy_port, model_mock.port(), tool_mock.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let body = r#"{"model":"stream-model","input":"hello","stream":true}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 200, "should return 200");

    let response_body = parse_body(&raw);
    let response: serde_json::Value = serde_json::from_str(&response_body).expect("response should be valid JSON");

    assert_eq!(
        response["id"].as_str(),
        Some("resp_sse_001"),
        "id should come from stream metadata: {response_body}"
    );
    assert_eq!(
        response["model"].as_str(),
        Some("stream-model"),
        "model should come from stream metadata: {response_body}"
    );
    assert_eq!(
        response["status"].as_str(),
        Some("completed"),
        "status should be completed"
    );
    assert!(
        response_body.contains("hello world"),
        "should contain synthesized text: {response_body}"
    );
    assert!(
        !response_body.contains("data:"),
        "should not contain raw SSE lines: {response_body}"
    );

    let tool_posts = tool_mock
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(tool_posts, 0, "no tool should be called");

    assert_eq!(model_mock.responses_request_count(), 1, "model should be called once");
}

#[test]
fn streaming_incomplete_function_call_fails_closed() {
    let sse_resp = "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_001\",\"name\":\"get_weather\"}}\n\
                    data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"partial\"}\n\
                    data: {\"type\":\"response.completed\"}\n\
                    data: [DONE]\n";

    let model_mock = start_responses_mock_server_with_config(ResponsesMockConfig {
        fixtures: vec![ResponsesFixture {
            content_type: Some("text/event-stream".to_owned()),
            matches: Box::new(|_| true),
            response_body: sse_resp.to_owned(),
        }],
        ..ResponsesMockConfig::default()
    });

    let tool_mock = start_tool_http_mock_server_with_config(ToolHttpMockConfig::default());

    let proxy_port = free_port();
    let yaml = streaming_yaml(proxy_port, model_mock.port(), tool_mock.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let body =
        r#"{"model":"stream-model","input":"test","tools":[{"type":"function","name":"get_weather"}],"stream":true}"#;
    let raw = send_responses_request(proxy.addr(), body);

    assert_eq!(parse_status(&raw), 502, "incomplete function call should return 502");

    let response_body = parse_body(&raw);
    assert!(
        response_body.contains("incomplete function calls"),
        "should mention incomplete: {response_body}"
    );

    let tool_posts = tool_mock
        .received_requests()
        .iter()
        .filter(|r| r.http_method == "POST")
        .count();
    assert_eq!(tool_posts, 0, "no tool should execute for incomplete function call");
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

struct StreamingSetup {
    _proxy: ProxyGuard,
    addr: String,
}

impl StreamingSetup {
    fn addr(&self) -> &str {
        &self.addr
    }
}

fn setup_streaming() -> (ResponsesMockServerGuard, ToolHttpMockServerGuard, StreamingSetup) {
    let sse_resp = streaming_function_call_response("call_weather_001", "get_weather", r#"{"city":"Boston"}"#);
    let final_resp = final_text_response("It is sunny in Boston.");

    let model_mock = start_responses_mock_server_with_config(ResponsesMockConfig {
        fixtures: vec![
            ResponsesFixture {
                content_type: None,
                matches: Box::new(|body| body.contains("function_call_output")),
                response_body: final_resp,
            },
            ResponsesFixture {
                content_type: Some("text/event-stream".to_owned()),
                matches: Box::new(|_| true),
                response_body: sse_resp,
            },
        ],
        ..ResponsesMockConfig::default()
    });

    let tool_mock = start_tool_http_mock_server_with_config(ToolHttpMockConfig {
        response_body: r#"{"weather":"sunny, 72F"}"#.to_owned(),
        ..ToolHttpMockConfig::default()
    });

    let proxy_port = free_port();
    let yaml = streaming_yaml(proxy_port, model_mock.port(), tool_mock.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);
    let addr = proxy.addr().to_owned();

    (model_mock, tool_mock, StreamingSetup { _proxy: proxy, addr })
}

fn streaming_yaml(proxy_port: u16, model_port: u16, tool_port: u16) -> String {
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
          stream-model:
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
