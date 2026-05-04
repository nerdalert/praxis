// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Integration tests for the `mcp_gateway` filter.

use praxis_core::config::Config;
use praxis_test_utils::{
    free_port, http_send, json_post, parse_body, parse_header, parse_status,
    start_backend_with_shutdown, start_echo_backend_with_shutdown,
    start_uri_echo_backend_with_shutdown, start_proxy,
};

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[test]
fn gateway_initialize_returns_session() {
    let weather_guard = start_backend_with_shutdown("weather-response");
    let proxy_port = free_port();

    let yaml = gateway_yaml(proxy_port, weather_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{}}}"#,
        ),
    );

    assert_eq!(parse_status(&raw), 200, "initialize should return 200");
    let body = parse_body(&raw);
    assert!(body.contains("protocolVersion"), "should contain protocol version: {body}");
    assert!(body.contains("praxis-mcp-gateway"), "should contain server info: {body}");

    let session_header = parse_header(&raw, "mcp-session-id");
    assert!(session_header.is_some(), "should return MCP-Session-Id header");
}

#[test]
fn gateway_tools_list_returns_prefixed_tools() {
    let weather_guard = start_backend_with_shutdown("unused");
    let proxy_port = free_port();

    let yaml = gateway_yaml(proxy_port, weather_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/mcp",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        ),
    );

    assert_eq!(parse_status(&raw), 200, "tools/list should return 200");
    let body = parse_body(&raw);
    assert!(body.contains("weather_get_weather"), "should contain prefixed weather tool: {body}");
    assert!(body.contains("weather_forecast"), "should contain prefixed forecast tool: {body}");
}

#[test]
fn gateway_tools_call_routes_and_strips_prefix() {
    let weather_guard = start_backend_with_shutdown("weather-result");
    let proxy_port = free_port();

    let yaml = gateway_yaml(proxy_port, weather_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/mcp",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"weather_get_weather","arguments":{"city":"London"}}}"#,
        ),
    );

    assert_eq!(parse_status(&raw), 200, "tools/call should route successfully");
    assert_eq!(parse_body(&raw), "weather-result", "should reach weather backend");
}

#[test]
fn gateway_tools_call_unknown_tool_rejected() {
    let weather_guard = start_backend_with_shutdown("unused");
    let proxy_port = free_port();

    let yaml = gateway_yaml(proxy_port, weather_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/mcp",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nonexistent_tool","arguments":{}}}"#,
        ),
    );

    assert_eq!(parse_status(&raw), 400, "unknown tool should return 400");
    let body = parse_body(&raw);
    assert!(body.contains("unknown tool"), "should mention unknown tool: {body}");
}

#[test]
fn gateway_ping_returns_empty_result() {
    let weather_guard = start_backend_with_shutdown("unused");
    let proxy_port = free_port();

    let yaml = gateway_yaml(proxy_port, weather_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/mcp",
            r#"{"jsonrpc":"2.0","id":5,"method":"ping"}"#,
        ),
    );

    assert_eq!(parse_status(&raw), 200, "ping should return 200");
    let body = parse_body(&raw);
    assert!(body.contains(r#""result":{}"#), "ping should have empty result: {body}");
}

#[test]
fn gateway_notifications_initialized_returns_204() {
    let weather_guard = start_backend_with_shutdown("unused");
    let proxy_port = free_port();

    let yaml = gateway_yaml(proxy_port, weather_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/mcp",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        ),
    );

    assert_eq!(parse_status(&raw), 204, "notifications/initialized should return 204");
}

#[test]
fn gateway_multi_backend_routes_by_tool() {
    let weather_guard = start_backend_with_shutdown("weather-result");
    let calendar_guard = start_backend_with_shutdown("calendar-result");
    let proxy_port = free_port();

    let yaml = multi_backend_gateway_yaml(proxy_port, weather_guard.port(), calendar_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let weather_raw = http_send(
        proxy.addr(),
        &json_post(
            "/mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"weather_get_weather","arguments":{"city":"London"}}}"#,
        ),
    );
    assert_eq!(parse_status(&weather_raw), 200, "weather tools/call should succeed");
    assert_eq!(
        parse_body(&weather_raw),
        "weather-result",
        "weather tool should reach weather backend"
    );

    let calendar_raw = http_send(
        proxy.addr(),
        &json_post(
            "/mcp",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"cal_create_event","arguments":{}}}"#,
        ),
    );
    assert_eq!(parse_status(&calendar_raw), 200, "calendar tools/call should succeed");
    assert_eq!(
        parse_body(&calendar_raw),
        "calendar-result",
        "calendar tool should reach calendar backend, not weather"
    );
}

#[test]
fn gateway_tools_call_strips_prefix_in_forwarded_body() {
    let weather_guard = start_echo_backend_with_shutdown();
    let calendar_guard = start_echo_backend_with_shutdown();
    let proxy_port = free_port();

    let yaml = multi_backend_gateway_yaml(proxy_port, weather_guard.port(), calendar_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"cal_create_event","arguments":{"title":"meeting"}}}"#,
        ),
    );

    assert_eq!(parse_status(&raw), 200, "tools/call should succeed");
    let echoed_body = parse_body(&raw);
    assert!(
        echoed_body.contains(r#""name":"create_event"#),
        "backend should see stripped tool name 'create_event', not 'cal_create_event', got:\n{echoed_body}"
    );
    assert!(
        !echoed_body.contains("cal_create_event"),
        "backend should NOT see prefixed tool name, got:\n{echoed_body}"
    );
}

#[test]
fn gateway_tools_call_rewrites_backend_path() {
    let backend_guard = start_uri_echo_backend_with_shutdown();
    let proxy_port = free_port();

    let yaml = format!(
        r#"
listeners:
  - name: default
    address: "127.0.0.1:{proxy_port}"
    filter_chains: [main]
filter_chains:
  - name: main
    filters:
      - filter: mcp_gateway
        path: /mcp
        max_body_bytes: 65536
        servers:
          - name: backend
            cluster: backend-mcp
            path: /backend/v1/mcp
            tools:
              - name: ping_tool
      - filter: load_balancer
        clusters:
          - name: backend-mcp
            endpoints:
              - "127.0.0.1:{}"
"#,
        backend_guard.port()
    );
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"ping_tool","arguments":{}}}"#,
        ),
    );

    assert_eq!(parse_status(&raw), 200, "tools/call should succeed");
    let uri = parse_body(&raw);
    assert_eq!(
        uri, "/backend/v1/mcp",
        "backend should see rewritten path, not the client-facing /mcp"
    );
}

#[test]
fn gateway_delete_session_returns_204() {
    let proxy_port = free_port();
    let weather_port = start_backend_with_shutdown("unused").port();

    let yaml = gateway_yaml(proxy_port, weather_port);
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let init_raw = http_send(
        proxy.addr(),
        &json_post(
            "/mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{}}}"#,
        ),
    );
    let session_id = parse_header(&init_raw, "mcp-session-id").expect("should have session ID");

    let delete_req = format!(
        "DELETE /mcp HTTP/1.1\r\n\
         Host: localhost\r\n\
         MCP-Session-Id: {session_id}\r\n\
         Content-Length: 0\r\n\
         \r\n"
    );
    let raw = http_send(proxy.addr(), &delete_req);
    assert_eq!(parse_status(&raw), 204, "DELETE session should return 204");
}

#[test]
fn gateway_full_mcp_flow() {
    let weather_guard = start_backend_with_shutdown("weather-data");
    let proxy_port = free_port();

    let yaml = gateway_yaml(proxy_port, weather_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let init_raw = http_send(
        proxy.addr(),
        &json_post(
            "/mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{}}}"#,
        ),
    );
    assert_eq!(parse_status(&init_raw), 200, "initialize should succeed");
    let session_id = parse_header(&init_raw, "mcp-session-id").expect("should have session ID");

    let notif_body = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let notif_req = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         MCP-Session-Id: {session_id}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {notif_body}",
        notif_body.len(),
    );
    let notif_raw = http_send(proxy.addr(), &notif_req);
    assert_eq!(parse_status(&notif_raw), 204, "notifications/initialized should return 204");

    let list_body = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
    let list_req = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         MCP-Session-Id: {session_id}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {list_body}",
        list_body.len(),
    );
    let list_raw = http_send(proxy.addr(), &list_req);
    assert_eq!(parse_status(&list_raw), 200, "tools/list should succeed");
    let list_response = parse_body(&list_raw);
    assert!(
        list_response.contains("weather_get_weather"),
        "tools/list should contain weather tool: {list_response}"
    );

    let call_body = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"weather_get_weather","arguments":{"city":"Paris"}}}"#;
    let call_req = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         MCP-Session-Id: {session_id}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {call_body}",
        call_body.len(),
    );
    let call_raw = http_send(proxy.addr(), &call_req);
    assert_eq!(parse_status(&call_raw), 200, "tools/call should succeed");
    assert_eq!(parse_body(&call_raw), "weather-data", "should reach weather backend");
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

fn gateway_yaml(proxy_port: u16, weather_port: u16) -> String {
    format!(
        r#"
listeners:
  - name: default
    address: "127.0.0.1:{proxy_port}"
    filter_chains: [main]
filter_chains:
  - name: main
    filters:
      - filter: mcp_gateway
        path: /mcp
        max_body_bytes: 65536
        servers:
          - name: weather
            cluster: weather-mcp
            path: /mcp
            tool_prefix: "weather_"
            tools:
              - name: get_weather
                description: "Get current weather"
                schema:
                  type: object
                  properties:
                    city:
                      type: string
              - name: forecast
                description: "Get weather forecast"
      - filter: load_balancer
        clusters:
          - name: "weather-mcp"
            endpoints:
              - "127.0.0.1:{weather_port}"
"#
    )
}

fn multi_backend_gateway_yaml(proxy_port: u16, weather_port: u16, calendar_port: u16) -> String {
    format!(
        r#"
listeners:
  - name: default
    address: "127.0.0.1:{proxy_port}"
    filter_chains: [main]
filter_chains:
  - name: main
    filters:
      - filter: mcp_gateway
        path: /mcp
        max_body_bytes: 65536
        servers:
          - name: weather
            cluster: weather-mcp
            path: /mcp
            tool_prefix: "weather_"
            tools:
              - name: get_weather
                description: "Get current weather"
              - name: forecast
                description: "Get weather forecast"
          - name: calendar
            cluster: calendar-mcp
            path: /mcp
            tool_prefix: "cal_"
            tools:
              - name: create_event
                description: "Create a calendar event"
      - filter: load_balancer
        clusters:
          - name: "weather-mcp"
            endpoints:
              - "127.0.0.1:{weather_port}"
          - name: "calendar-mcp"
            endpoints:
              - "127.0.0.1:{calendar_port}"
"#
    )
}
