// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Integration tests for agentic protocol example configurations.

use std::collections::HashMap;

use praxis_test_utils::{
    free_port, http_send, json_post, load_example_config, parse_body, parse_status, start_backend_with_shutdown,
    start_proxy,
};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn json_rpc_routing_routes_mcp_tools() {
    let mcp_tools_guard = start_backend_with_shutdown("mcp-tools-response");
    let mcp_tools_port = mcp_tools_guard.port();
    let mcp_discovery_guard = start_backend_with_shutdown("mcp-discovery-response");
    let mcp_discovery_port = mcp_discovery_guard.port();
    let a2a_send_guard = start_backend_with_shutdown("a2a-send-response");
    let a2a_send_port = a2a_send_guard.port();
    let a2a_tasks_guard = start_backend_with_shutdown("a2a-tasks-response");
    let a2a_tasks_port = a2a_tasks_guard.port();
    let default_guard = start_backend_with_shutdown("default-response");
    let default_port = default_guard.port();
    let proxy_port = free_port();

    let config = load_example_config(
        "ai/json-rpc-routing.yaml",
        proxy_port,
        HashMap::from([
            ("127.0.0.1:9001", mcp_tools_port),
            ("127.0.0.1:9002", mcp_tools_port),
            ("127.0.0.1:9101", mcp_discovery_port),
            ("127.0.0.1:9201", a2a_send_port),
            ("127.0.0.1:9202", a2a_send_port),
            ("127.0.0.1:9301", a2a_tasks_port),
            ("127.0.0.1:9000", default_port),
        ]),
    );
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/mcp/",
            r#"{"jsonrpc":"2.0","id":"req-1","method":"tools/call","params":{"name":"calculator"}}"#,
        ),
    );
    assert_eq!(parse_status(&raw), 200, "MCP tools/call should return 200");
    assert_eq!(
        parse_body(&raw),
        "mcp-tools-response",
        "JSON-RPC method=tools/call should route to mcp-tools cluster"
    );
}

#[test]
fn json_rpc_routing_routes_a2a_send() {
    let mcp_tools_guard = start_backend_with_shutdown("mcp-tools-response");
    let mcp_tools_port = mcp_tools_guard.port();
    let mcp_discovery_guard = start_backend_with_shutdown("mcp-discovery-response");
    let mcp_discovery_port = mcp_discovery_guard.port();
    let a2a_send_guard = start_backend_with_shutdown("a2a-send-response");
    let a2a_send_port = a2a_send_guard.port();
    let a2a_tasks_guard = start_backend_with_shutdown("a2a-tasks-response");
    let a2a_tasks_port = a2a_tasks_guard.port();
    let default_guard = start_backend_with_shutdown("default-response");
    let default_port = default_guard.port();
    let proxy_port = free_port();

    let config = load_example_config(
        "ai/json-rpc-routing.yaml",
        proxy_port,
        HashMap::from([
            ("127.0.0.1:9001", mcp_tools_port),
            ("127.0.0.1:9002", mcp_tools_port),
            ("127.0.0.1:9101", mcp_discovery_port),
            ("127.0.0.1:9201", a2a_send_port),
            ("127.0.0.1:9202", a2a_send_port),
            ("127.0.0.1:9301", a2a_tasks_port),
            ("127.0.0.1:9000", default_port),
        ]),
    );
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/a2a/",
            r#"{"jsonrpc":"2.0","id":"msg-123","method":"SendMessage","params":{"recipient":"agent-42"}}"#,
        ),
    );
    assert_eq!(parse_status(&raw), 200, "A2A SendMessage should return 200");
    assert_eq!(
        parse_body(&raw),
        "a2a-send-response",
        "JSON-RPC method=SendMessage should route to a2a-send cluster"
    );
}

#[test]
fn json_rpc_routing_falls_through_to_default() {
    let mcp_tools_guard = start_backend_with_shutdown("mcp-tools-response");
    let mcp_tools_port = mcp_tools_guard.port();
    let mcp_discovery_guard = start_backend_with_shutdown("mcp-discovery-response");
    let mcp_discovery_port = mcp_discovery_guard.port();
    let a2a_send_guard = start_backend_with_shutdown("a2a-send-response");
    let a2a_send_port = a2a_send_guard.port();
    let a2a_tasks_guard = start_backend_with_shutdown("a2a-tasks-response");
    let a2a_tasks_port = a2a_tasks_guard.port();
    let default_guard = start_backend_with_shutdown("default-response");
    let default_port = default_guard.port();
    let proxy_port = free_port();

    let config = load_example_config(
        "ai/json-rpc-routing.yaml",
        proxy_port,
        HashMap::from([
            ("127.0.0.1:9001", mcp_tools_port),
            ("127.0.0.1:9002", mcp_tools_port),
            ("127.0.0.1:9101", mcp_discovery_port),
            ("127.0.0.1:9201", a2a_send_port),
            ("127.0.0.1:9202", a2a_send_port),
            ("127.0.0.1:9301", a2a_tasks_port),
            ("127.0.0.1:9000", default_port),
        ]),
    );
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/unknown/",
            r#"{"jsonrpc":"2.0","id":"unknown-1","method":"UnknownMethod"}"#,
        ),
    );
    assert_eq!(parse_status(&raw), 200, "unknown method should return 200");
    assert_eq!(
        parse_body(&raw),
        "default-response",
        "unknown method should route to default cluster"
    );
}

#[test]
fn mcp_classifier_routing_routes_by_tool_name() {
    let weather_guard = start_backend_with_shutdown("weather-response");
    let weather_port = weather_guard.port();
    let calendar_guard = start_backend_with_shutdown("calendar-response");
    let calendar_port = calendar_guard.port();
    let default_guard = start_backend_with_shutdown("default-response");
    let default_port = default_guard.port();
    let proxy_port = free_port();

    let config = load_example_config(
        "ai/mcp-classifier-routing.yaml",
        proxy_port,
        HashMap::from([
            ("127.0.0.1:9001", weather_port),
            ("127.0.0.1:9002", calendar_port),
            ("127.0.0.1:9003", default_port),
        ]),
    );
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &mcp_json_post(
            "/",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_weather"}}"#,
            &[("Mcp-Method", "tools/call"), ("Mcp-Name", "get_weather")],
        ),
    );
    assert_eq!(parse_status(&raw), 200, "tools/call get_weather should return 200");
    assert_eq!(
        parse_body(&raw),
        "weather-response",
        "tools/call with name=get_weather should route to weather-tools cluster"
    );
}

#[test]
fn mcp_classifier_routing_default_fallback() {
    let weather_guard = start_backend_with_shutdown("weather-response");
    let weather_port = weather_guard.port();
    let calendar_guard = start_backend_with_shutdown("calendar-response");
    let calendar_port = calendar_guard.port();
    let default_guard = start_backend_with_shutdown("default-response");
    let default_port = default_guard.port();
    let proxy_port = free_port();

    let config = load_example_config(
        "ai/mcp-classifier-routing.yaml",
        proxy_port,
        HashMap::from([
            ("127.0.0.1:9001", weather_port),
            ("127.0.0.1:9002", calendar_port),
            ("127.0.0.1:9003", default_port),
        ]),
    );
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &mcp_json_post(
            "/",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            &[("Mcp-Method", "tools/list")],
        ),
    );
    assert_eq!(parse_status(&raw), 200, "tools/list should return 200");
    assert_eq!(
        parse_body(&raw),
        "default-response",
        "tools/list should route to default-mcp cluster"
    );
}

#[test]
fn mcp_classifier_routing_routes_calendar_without_client_mcp_headers() {
    let weather_guard = start_backend_with_shutdown("weather-response");
    let weather_port = weather_guard.port();
    let calendar_guard = start_backend_with_shutdown("calendar-response");
    let calendar_port = calendar_guard.port();
    let default_guard = start_backend_with_shutdown("default-response");
    let default_port = default_guard.port();
    let proxy_port = free_port();

    let config = load_example_config(
        "ai/mcp-classifier-routing.yaml",
        proxy_port,
        HashMap::from([
            ("127.0.0.1:9001", weather_port),
            ("127.0.0.1:9002", calendar_port),
            ("127.0.0.1:9003", default_port),
        ]),
    );
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &mcp_json_post(
            "/",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_calendar"}}"#,
            &[],
        ),
    );
    assert_eq!(parse_status(&raw), 200, "tools/call get_calendar should return 200");
    assert_eq!(
        parse_body(&raw),
        "calendar-response",
        "tools/call with name=get_calendar should route to calendar-tools cluster without client Mcp-* headers"
    );
}

// ---------------------------------------------------------------------------
// Test Utilities
// ---------------------------------------------------------------------------

fn mcp_json_post(path: &str, body: &str, headers: &[(&str, &str)]) -> String {
    let mut extra = String::new();
    for (name, value) in headers {
        extra.push_str(&format!("{name}: {value}\r\n"));
    }
    format!(
        "POST {path} HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         {extra}\
         \r\n\
         {body}",
        body.len(),
    )
}
