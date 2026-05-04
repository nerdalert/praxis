// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Integration tests for the `mcp` classifier filter.

use praxis_core::config::Config;
use praxis_test_utils::{
    free_port, http_send, parse_body, parse_status,
    start_backend_with_shutdown, start_header_echo_backend_with_shutdown, start_proxy,
};

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[test]
fn mcp_tools_call_routes_by_name() {
    let weather_guard = start_backend_with_shutdown("weather-backend");
    let calendar_guard = start_backend_with_shutdown("calendar-backend");
    let proxy_port = free_port();

    let yaml = mcp_routing_yaml(proxy_port, weather_guard.port(), calendar_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post_with_mcp_headers(
            "/mcp/",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_weather"}}"#,
            "tools/call",
            Some("get_weather"),
        ),
    );
    assert_eq!(parse_status(&raw), 200);
    assert_eq!(parse_body(&raw), "weather-backend");

    let raw2 = http_send(
        proxy.addr(),
        &json_post_with_mcp_headers(
            "/mcp/",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"create_event"}}"#,
            "tools/call",
            Some("create_event"),
        ),
    );
    assert_eq!(parse_status(&raw2), 200);
    assert_eq!(parse_body(&raw2), "calendar-backend");
}

#[test]
fn mcp_classifier_promotes_headers() {
    let backend_guard = start_header_echo_backend_with_shutdown();
    let proxy_port = free_port();

    let yaml = mcp_echo_yaml(proxy_port, backend_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post_with_mcp_headers(
            "/mcp/",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"test_tool"}}"#,
            "tools/call",
            Some("test_tool"),
        ),
    );

    assert_eq!(parse_status(&raw), 200);
    let body = parse_body(&raw);
    assert!(
        body.to_lowercase().contains("x-praxis-mcp-method: tools/call"),
        "expected MCP method header echoed by backend, got:\n{body}"
    );
    assert!(
        body.to_lowercase().contains("x-praxis-mcp-name: test_tool"),
        "expected MCP name header echoed by backend, got:\n{body}"
    );
}

#[test]
fn mcp_header_body_mismatch_rejected() {
    let backend_guard = start_backend_with_shutdown("should-not-reach");
    let proxy_port = free_port();

    let yaml = mcp_echo_yaml(proxy_port, backend_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post_with_mcp_headers(
            "/mcp/",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"test"}}"#,
            "tools/list",
            Some("test"),
        ),
    );

    assert_eq!(parse_status(&raw), 400, "mismatch should return 400");
    let body = parse_body(&raw);
    assert!(body.contains("HeaderMismatch"), "should contain HeaderMismatch error: {body}");
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

fn json_post_with_mcp_headers(path: &str, body: &str, method: &str, name: Option<&str>) -> String {
    let mut headers = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Mcp-Method: {method}\r\n",
        body.len()
    );
    if let Some(n) = name {
        headers.push_str(&format!("Mcp-Name: {n}\r\n"));
    }
    headers.push_str(&format!("\r\n{body}"));
    headers
}

fn mcp_routing_yaml(proxy_port: u16, weather_port: u16, calendar_port: u16) -> String {
    format!(
        r#"
listeners:
  - name: default
    address: "127.0.0.1:{proxy_port}"
    filter_chains: [main]
filter_chains:
  - name: main
    filters:
      - filter: mcp
        max_body_bytes: 65536
        header_validation:
          mismatch: reject
          missing: synthesize
      - filter: router
        routes:
          - path_prefix: "/mcp/"
            headers:
              x-praxis-mcp-name: "get_weather"
            cluster: "weather"
          - path_prefix: "/mcp/"
            headers:
              x-praxis-mcp-name: "create_event"
            cluster: "calendar"
          - path_prefix: "/"
            cluster: "weather"
      - filter: load_balancer
        clusters:
          - name: "weather"
            endpoints:
              - "127.0.0.1:{weather_port}"
          - name: "calendar"
            endpoints:
              - "127.0.0.1:{calendar_port}"
"#
    )
}

fn mcp_echo_yaml(proxy_port: u16, backend_port: u16) -> String {
    format!(
        r#"
listeners:
  - name: default
    address: "127.0.0.1:{proxy_port}"
    filter_chains: [main]
filter_chains:
  - name: main
    filters:
      - filter: mcp
        max_body_bytes: 65536
        header_validation:
          mismatch: reject
          missing: synthesize
      - filter: router
        routes:
          - path_prefix: "/"
            cluster: "backend"
      - filter: load_balancer
        clusters:
          - name: "backend"
            endpoints:
              - "127.0.0.1:{backend_port}"
"#
    )
}
