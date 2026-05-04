// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Unit tests for the MCP filter.

use bytes::Bytes;

use super::{
    McpFilter,
    config::{McpConfig, build_config},
};
use crate::{FilterAction, filter::HttpFilter};

// -----------------------------------------------------------------------------
// Config Tests
// -----------------------------------------------------------------------------

#[test]
fn parse_minimal_config() {
    let yaml: serde_yaml::Value = serde_yaml::from_str("{}").unwrap();
    let filter = McpFilter::from_config(&yaml).unwrap();
    assert_eq!(filter.name(), "mcp");
}

#[test]
fn parse_full_config() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
        max_body_bytes: 131072
        on_invalid: continue
        header_validation:
          mismatch: ignore
          missing: synthesize
        headers:
          method: x-mcp-method
          name: x-mcp-name
          kind: x-mcp-kind
          session_present: x-mcp-session
        "#,
    )
    .unwrap();
    let filter = McpFilter::from_config(&yaml).unwrap();
    assert_eq!(filter.name(), "mcp");
}

#[test]
fn reject_zero_max_body_bytes() {
    let yaml: serde_yaml::Value = serde_yaml::from_str("max_body_bytes: 0").unwrap();
    let err = McpFilter::from_config(&yaml).err().expect("should fail");
    assert!(err.to_string().contains("must be greater than 0"));
}

#[test]
fn reject_empty_header_names() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
        headers:
          method: ""
        "#,
    )
    .unwrap();
    let err = McpFilter::from_config(&yaml).err().expect("should fail");
    assert!(err.to_string().contains("must not be empty"));
}

#[test]
fn reject_invalid_header_names() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
        headers:
          method: "bad header"
        "#,
    )
    .unwrap();
    let err = McpFilter::from_config(&yaml).err().expect("should fail");
    assert!(err.to_string().contains("not a valid HTTP header name"));
}

// -----------------------------------------------------------------------------
// Filter Behavior Tests
// -----------------------------------------------------------------------------

#[tokio::test]
async fn tools_call_extracts_method_and_name() {
    let filter = make_filter("{}");
    let body_str =
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_weather"}}"#;
    let req = make_mcp_request(&[
        ("mcp-method", "tools/call"),
        ("mcp-name", "get_weather"),
    ]);
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .unwrap();

    assert!(matches!(action, FilterAction::Release));
    assert_eq!(ctx.get_metadata("mcp.method"), Some("tools/call"));
    assert_eq!(ctx.get_metadata("mcp.name"), Some("get_weather"));
}

#[tokio::test]
async fn initialize_extracts_protocol_version() {
    let filter = make_filter(r#"{"header_validation": {"missing": "ignore"}}"#);
    let body_str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#;
    let req = make_mcp_request(&[]);
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .unwrap();

    assert!(matches!(action, FilterAction::Release));
    assert_eq!(ctx.get_metadata("mcp.method"), Some("initialize"));
    assert_eq!(
        ctx.get_metadata("mcp.protocol_version"),
        Some("2025-03-26")
    );
}

#[tokio::test]
async fn header_body_mismatch_rejected() {
    let filter = make_filter("{}");
    let body_str =
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_weather"}}"#;
    let req = make_mcp_request(&[
        ("mcp-method", "tools/list"),
        ("mcp-name", "get_weather"),
    ]);
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .unwrap();

    assert!(matches!(action, FilterAction::Reject(_)));
}

#[tokio::test]
async fn session_id_detected() {
    let filter = make_filter(r#"{"header_validation": {"missing": "ignore"}}"#);
    let body_str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
    let mut req = make_mcp_request(&[]);
    req.headers
        .insert("mcp-session-id", "gw-123".parse().unwrap());
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .unwrap();

    assert!(matches!(action, FilterAction::Release));
    assert_eq!(ctx.get_metadata("mcp.session_id"), Some("gw-123"));
}

#[tokio::test]
async fn non_json_rpc_continues_when_configured() {
    let filter = make_filter(
        r#"{"on_invalid": "continue", "header_validation": {"missing": "ignore"}}"#,
    );
    let body_str = r#"{"message":"hello"}"#;
    let req = make_mcp_request(&[]);
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .unwrap();

    assert!(matches!(action, FilterAction::Continue));
}

#[tokio::test]
async fn non_json_rpc_rejected_by_default() {
    let filter = make_filter(r#"{"header_validation": {"missing": "ignore"}}"#);
    let body_str = r#"{"message":"hello"}"#;
    let req = make_mcp_request(&[]);
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .unwrap();

    assert!(matches!(action, FilterAction::Reject(_)));
}

#[tokio::test]
async fn resources_read_extracts_uri() {
    let filter = make_filter(r#"{"header_validation": {"missing": "ignore"}}"#);
    let body_str =
        r#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"file:///data.csv"}}"#;
    let req = make_mcp_request(&[]);
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .unwrap();

    assert!(matches!(action, FilterAction::Release));
    assert_eq!(ctx.get_metadata("mcp.method"), Some("resources/read"));
    assert_eq!(ctx.get_metadata("mcp.name"), Some("file:///data.csv"));
}

#[tokio::test]
async fn missing_headers_rejected_by_default() {
    let filter = make_filter("{}");
    let body_str =
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"test"}}"#;
    let req = make_mcp_request(&[]);
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .unwrap();

    assert!(matches!(action, FilterAction::Reject(_)));
}

#[tokio::test]
async fn missing_headers_synthesized_when_configured() {
    let filter = make_filter(r#"{"header_validation": {"missing": "synthesize"}}"#);
    let body_str =
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"test"}}"#;
    let req = make_mcp_request(&[]);
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .unwrap();

    assert!(matches!(action, FilterAction::Release));
    assert_eq!(ctx.get_metadata("mcp.method"), Some("tools/call"));
    assert_eq!(ctx.get_metadata("mcp.name"), Some("test"));
}

#[tokio::test]
async fn on_request_is_noop() {
    let filter = make_filter("{}");
    let req = make_mcp_request(&[]);
    let mut ctx = crate::test_utils::make_filter_context(&req);

    let action = filter.on_request(&mut ctx).await.unwrap();

    assert!(matches!(action, FilterAction::Continue));
}

#[tokio::test]
async fn returns_continue_on_none_body() {
    let filter = make_filter("{}");
    let req = make_mcp_request(&[]);
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body: Option<Bytes> = None;

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .unwrap();

    assert!(matches!(action, FilterAction::Continue));
}

#[test]
fn body_access_is_read_only() {
    let filter = make_filter("{}");
    assert_eq!(
        filter.request_body_access(),
        crate::body::BodyAccess::ReadOnly
    );
}

#[test]
fn body_mode_is_stream_buffer() {
    let filter = make_filter("{}");
    assert_eq!(
        filter.request_body_mode(),
        crate::body::BodyMode::StreamBuffer {
            max_bytes: Some(super::config::DEFAULT_MAX_BODY_BYTES)
        }
    );
}

#[tokio::test]
async fn promotes_filter_results() {
    let filter = make_filter(r#"{"header_validation": {"missing": "ignore"}}"#);
    let body_str =
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_weather"}}"#;
    let req = make_mcp_request(&[]);
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .unwrap();

    assert!(matches!(action, FilterAction::Release));

    let results = ctx.filter_results.get("mcp").unwrap();
    assert_eq!(results.get("method"), Some("tools/call"));
    assert_eq!(results.get("name"), Some("get_weather"));
    assert_eq!(results.get("kind"), Some("request"));
    assert_eq!(results.get("session_present"), Some("false"));
}

#[tokio::test]
async fn promotes_internal_headers() {
    let filter = make_filter(r#"{"header_validation": {"missing": "ignore"}}"#);
    let body_str =
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_weather"}}"#;
    let req = make_mcp_request(&[]);
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .unwrap();

    assert!(matches!(action, FilterAction::Release));

    let headers: std::collections::HashMap<_, _> = ctx
        .extra_request_headers
        .iter()
        .map(|(k, v)| (k.as_ref(), v.as_str()))
        .collect();

    assert_eq!(headers.get("x-praxis-mcp-method"), Some(&"tools/call"));
    assert_eq!(headers.get("x-praxis-mcp-name"), Some(&"get_weather"));
    assert_eq!(headers.get("x-praxis-mcp-kind"), Some(&"request"));
    assert_eq!(headers.get("x-praxis-mcp-session-present"), Some(&"false"));
}

#[tokio::test]
async fn session_present_true_in_results() {
    let filter = make_filter(r#"{"header_validation": {"missing": "ignore"}}"#);
    let body_str = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let mut req = make_mcp_request(&[]);
    req.headers
        .insert("mcp-session-id", "sess-456".parse().unwrap());
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .unwrap();

    assert!(matches!(action, FilterAction::Release));

    let results = ctx.filter_results.get("mcp").unwrap();
    assert_eq!(results.get("session_present"), Some("true"));
}

#[tokio::test]
async fn notification_sets_kind() {
    let filter = make_filter(r#"{"header_validation": {"missing": "ignore"}}"#);
    let body_str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let req = make_mcp_request(&[]);
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .unwrap();

    assert!(matches!(action, FilterAction::Release));
    assert_eq!(ctx.get_metadata("json_rpc.kind"), Some("notification"));
    assert_eq!(
        ctx.get_metadata("mcp.method"),
        Some("notifications/initialized")
    );
}

// -----------------------------------------------------------------------------
// Envelope Tests
// -----------------------------------------------------------------------------

#[test]
fn mcp_method_round_trips() {
    use super::envelope::McpMethod;

    let cases = [
        "initialize",
        "notifications/initialized",
        "tools/list",
        "tools/call",
        "resources/read",
        "resources/list",
        "prompts/get",
        "prompts/list",
        "ping",
        "logging/setLevel",
        "completion/complete",
        "notifications/tools/list_changed",
        "notifications/resources/list_changed",
        "notifications/prompts/list_changed",
    ];

    for method_str in &cases {
        let method = McpMethod::from_method_str(method_str);
        assert_eq!(
            method.as_str(),
            *method_str,
            "round-trip failed for {method_str}"
        );
    }
}

#[test]
fn mcp_method_other_preserves_string() {
    use super::envelope::McpMethod;

    let method = McpMethod::from_method_str("custom/method");
    assert_eq!(method.as_str(), "custom/method");
    assert!(matches!(method, McpMethod::Other(_)));
}

#[test]
fn tools_call_requires_name() {
    use super::envelope::McpMethod;
    assert!(McpMethod::ToolsCall.requires_name());
    assert!(!McpMethod::ToolsCall.requires_uri());
}

#[test]
fn resources_read_requires_uri() {
    use super::envelope::McpMethod;
    assert!(McpMethod::ResourcesRead.requires_uri());
    assert!(!McpMethod::ResourcesRead.requires_name());
}

#[test]
fn prompts_get_requires_name() {
    use super::envelope::McpMethod;
    assert!(McpMethod::PromptsGet.requires_name());
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

fn make_filter(yaml: &str) -> McpFilter {
    let cfg: McpConfig = serde_yaml::from_str(yaml).unwrap();
    let (max_body_bytes, validated_config) = build_config(cfg).unwrap();
    let json_rpc_config = super::build_json_rpc_config(max_body_bytes);
    McpFilter {
        max_body_bytes,
        config: validated_config,
        json_rpc_config,
    }
}

fn make_mcp_request(extra_headers: &[(&str, &str)]) -> crate::context::Request {
    let mut req = crate::test_utils::make_request(http::Method::POST, "/mcp");
    req.headers
        .insert("content-type", "application/json".parse().unwrap());
    for (name, value) in extra_headers {
        req.headers.insert(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    req
}
