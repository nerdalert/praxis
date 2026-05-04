// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Unit tests for MCP gateway filter.

use bytes::Bytes;

use super::*;
use super::config::McpGatewayConfig;

// -----------------------------------------------------------------------------
// Config Tests
// -----------------------------------------------------------------------------

#[test]
fn parse_minimal_config() {
    let yaml: serde_yaml::Value = serde_yaml::from_str("{}").unwrap();
    let filter = McpGatewayFilter::from_config(&yaml).unwrap();
    assert_eq!(filter.name(), "mcp_gateway");
}

#[test]
fn reject_zero_max_body_bytes() {
    let yaml: serde_yaml::Value = serde_yaml::from_str("max_body_bytes: 0").unwrap();
    let err = McpGatewayFilter::from_config(&yaml).err().expect("should fail");
    assert!(err.to_string().contains("must be greater than 0"));
}

#[test]
fn duplicate_server_names_rejected() {
    let yaml = r#"
servers:
  - name: weather
    cluster: weather-mcp
    tools:
      - name: get_weather
  - name: weather
    cluster: weather2-mcp
    tools:
      - name: forecast
"#;
    let cfg: McpGatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let result = build_config(cfg);
    assert!(result.is_err());
    assert!(result.err().unwrap().to_string().contains("duplicate server name"));
}

#[test]
fn duplicate_tool_names_rejected() {
    let yaml = r#"
servers:
  - name: server1
    cluster: cluster1
    tools:
      - name: search
  - name: server2
    cluster: cluster2
    tools:
      - name: search
"#;
    let cfg: McpGatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let result = build_config(cfg);
    assert!(result.is_err());
    assert!(result.err().unwrap().to_string().contains("duplicate exposed tool name"));
}

#[test]
fn empty_server_name_rejected() {
    let yaml = r#"
servers:
  - name: ""
    cluster: cluster1
    tools: []
"#;
    let cfg: McpGatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let result = build_config(cfg);
    assert!(result.is_err());
    assert!(result.err().unwrap().to_string().contains("must not be empty"));
}

#[test]
fn server_path_must_start_with_slash() {
    let yaml = r#"
servers:
  - name: bad
    cluster: c
    path: "no-leading-slash"
    tools: []
"#;
    let cfg: McpGatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let result = build_config(cfg);
    assert!(result.is_err());
    assert!(result.err().unwrap().to_string().contains("must start with /"));
}

#[test]
fn server_path_rejects_double_slash() {
    let yaml = r#"
servers:
  - name: bad
    cluster: c
    path: "//evil"
    tools: []
"#;
    let cfg: McpGatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let result = build_config(cfg);
    assert!(result.is_err());
    assert!(result.err().unwrap().to_string().contains("must not start with //"));
}

#[test]
fn server_path_rejects_traversal() {
    let yaml = r#"
servers:
  - name: bad
    cluster: c
    path: "/backend/../etc/passwd"
    tools: []
"#;
    let cfg: McpGatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let result = build_config(cfg);
    assert!(result.is_err());
    assert!(result.err().unwrap().to_string().contains("traversal"));
}

#[test]
fn valid_server_path_accepted() {
    let yaml = r#"
servers:
  - name: ok
    cluster: c
    path: "/backend/v1/mcp"
    tools: []
"#;
    let cfg: McpGatewayConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(build_config(cfg).is_ok());
}

// -----------------------------------------------------------------------------
// Filter Behavior Tests
// -----------------------------------------------------------------------------

#[tokio::test]
async fn initialize_returns_session() {
    let filter = make_gateway_filter();
    let body_str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{}}}"#;
    let req = make_mcp_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    match action {
        FilterAction::Reject(rejection) => {
            assert_eq!(rejection.status, 200);
            let body_str = std::str::from_utf8(rejection.body.as_ref().unwrap()).unwrap();
            assert!(body_str.contains("protocolVersion"), "should contain protocolVersion: {body_str}");
            assert!(body_str.contains("praxis-mcp-gateway"), "should contain server name: {body_str}");
            assert!(
                rejection.headers.iter().any(|(k, _)| k == "mcp-session-id"),
                "should contain mcp-session-id header"
            );
        },
        _ => panic!("expected Reject with 200"),
    }
}

#[tokio::test]
async fn initialize_extracts_protocol_version() {
    let filter = make_gateway_filter();
    let body_str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{}}}"#;
    let req = make_mcp_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert_eq!(ctx.get_metadata("mcp.protocol_version"), Some("2025-03-26"));
}

#[tokio::test]
async fn tools_list_returns_aggregated_catalog() {
    let filter = make_gateway_filter();
    let body_str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
    let req = make_mcp_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    match action {
        FilterAction::Reject(rejection) => {
            assert_eq!(rejection.status, 200);
            let body_str = std::str::from_utf8(rejection.body.as_ref().unwrap()).unwrap();
            assert!(body_str.contains("weather_get_weather"), "should contain prefixed tool: {body_str}");
            assert!(body_str.contains("cal_create_event"), "should contain prefixed tool: {body_str}");
        },
        _ => panic!("expected Reject with 200"),
    }
}

#[tokio::test]
async fn tools_call_routes_to_correct_backend() {
    let filter = make_gateway_filter();
    let body_str = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"weather_get_weather","arguments":{"city":"London"}}}"#;
    let req = make_mcp_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Release));
    assert_eq!(ctx.cluster_name(), Some("weather-mcp"));
    assert_eq!(ctx.get_metadata("mcp.server"), Some("weather"));

    let new_body: serde_json::Value = serde_json::from_slice(body.unwrap().as_ref()).unwrap();
    assert_eq!(new_body["params"]["name"], "get_weather");
}

#[tokio::test]
async fn tools_call_sets_rewritten_path() {
    let filter = make_gateway_filter();
    let body_str = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"weather_get_weather","arguments":{}}}"#;
    let req = make_mcp_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert_eq!(ctx.rewritten_path.as_deref(), Some("/mcp"));
}

#[tokio::test]
async fn tools_call_unknown_tool_rejected() {
    let filter = make_gateway_filter();
    let body_str = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"unknown_tool","arguments":{}}}"#;
    let req = make_mcp_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    match action {
        FilterAction::Reject(rejection) => {
            assert_eq!(rejection.status, 400);
            let body_str = std::str::from_utf8(rejection.body.as_ref().unwrap()).unwrap();
            assert!(body_str.contains("unknown tool"), "error should mention unknown tool: {body_str}");
        },
        _ => panic!("expected Reject with 400"),
    }
}

#[tokio::test]
async fn tools_call_missing_name_rejected() {
    let filter = make_gateway_filter();
    let body_str = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"arguments":{}}}"#;
    let req = make_mcp_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    match action {
        FilterAction::Reject(rejection) => {
            assert_eq!(rejection.status, 400);
            let body_str = std::str::from_utf8(rejection.body.as_ref().unwrap()).unwrap();
            assert!(body_str.contains("missing params.name"), "error should mention missing name: {body_str}");
        },
        _ => panic!("expected Reject with 400"),
    }
}

#[tokio::test]
async fn tools_call_no_prefix_passes_name_unchanged() {
    let filter = make_gateway_filter_no_prefix();
    let body_str = r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"get_weather","arguments":{}}}"#;
    let req = make_mcp_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Release));

    let body_bytes = body.unwrap();
    let new_body: serde_json::Value = serde_json::from_slice(body_bytes.as_ref()).unwrap();
    assert_eq!(new_body["params"]["name"], "get_weather");
}

#[tokio::test]
async fn delete_session_returns_204() {
    let filter = make_gateway_filter();
    let mut req = crate::test_utils::make_request(http::Method::DELETE, "/mcp");
    req.headers.insert("mcp-session-id", "gw-test-123".parse().unwrap());
    let mut ctx = crate::test_utils::make_filter_context(&req);

    filter.state.put_gateway_session(GatewaySession {
        session_id: "gw-test-123".to_owned(),
        created_at: Instant::now(),
        last_used: Instant::now(),
        protocol_version: None,
    });

    let action = filter.on_request(&mut ctx).await.unwrap();

    match action {
        FilterAction::Reject(rejection) => {
            assert_eq!(rejection.status, 204);
        },
        _ => panic!("expected Reject with 204"),
    }

    assert!(filter.state.get_gateway_session("gw-test-123").is_none());
}

#[tokio::test]
async fn delete_without_session_returns_400() {
    let filter = make_gateway_filter();
    let req = crate::test_utils::make_request(http::Method::DELETE, "/mcp");
    let mut ctx = crate::test_utils::make_filter_context(&req);

    let action = filter.on_request(&mut ctx).await.unwrap();

    match action {
        FilterAction::Reject(rejection) => {
            assert_eq!(rejection.status, 400);
        },
        _ => panic!("expected Reject with 400"),
    }
}

#[tokio::test]
async fn ping_returns_empty_result() {
    let filter = make_gateway_filter();
    let body_str = r#"{"jsonrpc":"2.0","id":5,"method":"ping"}"#;
    let req = make_mcp_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    match action {
        FilterAction::Reject(rejection) => {
            assert_eq!(rejection.status, 200);
            let body_str = std::str::from_utf8(rejection.body.as_ref().unwrap()).unwrap();
            assert!(body_str.contains(r#""result":{}"#), "ping should return empty result: {body_str}");
        },
        _ => panic!("expected Reject with 200"),
    }
}

#[tokio::test]
async fn notifications_initialized_returns_204() {
    let filter = make_gateway_filter();
    let body_str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let req = make_mcp_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    match action {
        FilterAction::Reject(rejection) => {
            assert_eq!(rejection.status, 204);
        },
        _ => panic!("expected Reject with 204"),
    }
}

#[tokio::test]
async fn unknown_method_passes_through() {
    let filter = make_gateway_filter();
    let body_str = r#"{"jsonrpc":"2.0","id":7,"method":"resources/list"}"#;
    let req = make_mcp_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Release));
}

#[tokio::test]
async fn invalid_json_rejected() {
    let filter = make_gateway_filter();
    let body_str = "not json";
    let req = make_mcp_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Reject(_)));
}

#[tokio::test]
async fn none_body_continues() {
    let filter = make_gateway_filter();
    let req = make_mcp_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body: Option<Bytes> = None;

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Continue));
}

#[tokio::test]
async fn non_post_request_continues() {
    let filter = make_gateway_filter();
    let req = crate::test_utils::make_request(http::Method::GET, "/mcp");
    let mut ctx = crate::test_utils::make_filter_context(&req);

    let action = filter.on_request(&mut ctx).await.unwrap();

    assert!(matches!(action, FilterAction::Continue));
}

#[test]
fn body_access_is_read_write() {
    let filter = make_gateway_filter();
    assert_eq!(filter.request_body_access(), BodyAccess::ReadWrite);
}

#[test]
fn body_mode_is_stream_buffer() {
    let filter = make_gateway_filter();
    assert_eq!(
        filter.request_body_mode(),
        BodyMode::StreamBuffer {
            max_bytes: Some(config::DEFAULT_MAX_BODY_BYTES)
        }
    );
}

#[test]
fn static_catalog_builds_correctly() {
    let filter = make_gateway_filter();
    assert_eq!(filter.catalog.len(), 2);
    assert_eq!(filter.catalog[0].exposed_name, "weather_get_weather");
    assert_eq!(filter.catalog[0].original_name, "get_weather");
    assert_eq!(filter.catalog[0].server_name, "weather");
    assert_eq!(filter.catalog[1].exposed_name, "cal_create_event");
    assert_eq!(filter.catalog[1].original_name, "create_event");
    assert_eq!(filter.catalog[1].server_name, "calendar");
}

#[tokio::test]
async fn tools_call_with_string_id() {
    let filter = make_gateway_filter();
    let body_str = r#"{"jsonrpc":"2.0","id":"req-42","method":"tools/call","params":{"name":"weather_get_weather","arguments":{}}}"#;
    let req = make_mcp_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Release));
}

#[tokio::test]
async fn initialize_with_string_id() {
    let filter = make_gateway_filter();
    let body_str = r#"{"jsonrpc":"2.0","id":"init-1","method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{}}}"#;
    let req = make_mcp_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    match action {
        FilterAction::Reject(rejection) => {
            assert_eq!(rejection.status, 200);
            let body_str = std::str::from_utf8(rejection.body.as_ref().unwrap()).unwrap();
            assert!(body_str.contains(r#""id":"init-1""#), "should preserve string id: {body_str}");
        },
        _ => panic!("expected Reject with 200"),
    }
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

fn make_gateway_filter() -> McpGatewayFilter {
    let yaml = r#"
servers:
  - name: weather
    cluster: weather-mcp
    path: /mcp
    tool_prefix: "weather_"
    tools:
      - name: get_weather
        description: Get current weather
        schema: {"type": "object", "properties": {"city": {"type": "string"}}}
  - name: calendar
    cluster: calendar-mcp
    path: /mcp
    tool_prefix: "cal_"
    tools:
      - name: create_event
        description: Create a calendar event
"#;
    let cfg: McpGatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let (max_body_bytes, validated_config) = build_config(cfg).unwrap();
    let json_rpc_config = build_json_rpc_config(max_body_bytes);
    let catalog = build_static_catalog(&validated_config.servers);
    McpGatewayFilter {
        config: validated_config,
        json_rpc_config,
        max_body_bytes,
        catalog,
        state: LocalStateStore::new(),
    }
}

fn make_gateway_filter_no_prefix() -> McpGatewayFilter {
    let yaml = r#"
servers:
  - name: weather
    cluster: weather-mcp
    path: /mcp
    tools:
      - name: get_weather
        description: Get current weather
"#;
    let cfg: McpGatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let (max_body_bytes, validated_config) = build_config(cfg).unwrap();
    let json_rpc_config = build_json_rpc_config(max_body_bytes);
    let catalog = build_static_catalog(&validated_config.servers);
    McpGatewayFilter {
        config: validated_config,
        json_rpc_config,
        max_body_bytes,
        catalog,
        state: LocalStateStore::new(),
    }
}

fn make_mcp_request() -> crate::context::Request {
    let mut req = crate::test_utils::make_request(http::Method::POST, "/mcp");
    req.headers.insert("content-type", "application/json".parse().unwrap());
    req
}
