// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Unit tests for the A2A filter.

use bytes::Bytes;

use super::{
    A2aFilter,
    config::{A2aConfig, build_config},
};
use crate::{FilterAction, filter::HttpFilter};

// -----------------------------------------------------------------------------
// Config Tests
// -----------------------------------------------------------------------------

#[test]
fn parse_minimal_config() {
    let yaml: serde_yaml::Value = serde_yaml::from_str("{}").unwrap();
    let filter = A2aFilter::from_config(&yaml).unwrap();
    assert_eq!(filter.name(), "a2a");
}

#[test]
fn parse_full_config() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
        max_body_bytes: 131072
        on_invalid: continue
        method_aliases:
          "message/send": "SendMessage"
          "tasks/get": "GetTask"
        headers:
          method: x-a2a-method
          family: x-a2a-family
          task_present: x-a2a-task
          streaming: x-a2a-stream
        "#,
    )
    .unwrap();
    let filter = A2aFilter::from_config(&yaml).unwrap();
    assert_eq!(filter.name(), "a2a");
}

#[test]
fn reject_zero_max_body_bytes() {
    let yaml: serde_yaml::Value = serde_yaml::from_str("max_body_bytes: 0").unwrap();
    let err = A2aFilter::from_config(&yaml).err().expect("should fail");
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
    let err = A2aFilter::from_config(&yaml).err().expect("should fail");
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
    let err = A2aFilter::from_config(&yaml).err().expect("should fail");
    assert!(err.to_string().contains("not a valid HTTP header name"));
}

// -----------------------------------------------------------------------------
// Filter Behavior Tests
// -----------------------------------------------------------------------------

#[tokio::test]
async fn send_message_classifies_correctly() {
    let filter = make_filter("{}");
    let body_str = r#"{"jsonrpc":"2.0","id":"msg-1","method":"SendMessage","params":{"message":{}}}"#;
    let req = make_a2a_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Release));
    assert_eq!(ctx.get_metadata("a2a.method"), Some("SendMessage"));
    assert_eq!(ctx.get_metadata("a2a.family"), Some("message"));
    assert_eq!(ctx.get_metadata("a2a.streaming"), Some("false"));
}

#[tokio::test]
async fn send_streaming_message_detected() {
    let filter = make_filter("{}");
    let body_str =
        r#"{"jsonrpc":"2.0","id":"msg-2","method":"SendStreamingMessage","params":{"message":{}}}"#;
    let req = make_a2a_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Release));
    assert_eq!(ctx.get_metadata("a2a.method"), Some("SendStreamingMessage"));
    assert_eq!(ctx.get_metadata("a2a.streaming"), Some("true"));
}

#[tokio::test]
async fn get_task_extracts_task_id() {
    let filter = make_filter("{}");
    let body_str = r#"{"jsonrpc":"2.0","id":"req-3","method":"GetTask","params":{"id":"task-123"}}"#;
    let req = make_a2a_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Release));
    assert_eq!(ctx.get_metadata("a2a.method"), Some("GetTask"));
    assert_eq!(ctx.get_metadata("a2a.family"), Some("task"));
    assert_eq!(ctx.get_metadata("a2a.task_id"), Some("task-123"));
}

#[tokio::test]
async fn cancel_task_extracts_task_id() {
    let filter = make_filter("{}");
    let body_str = r#"{"jsonrpc":"2.0","id":"req-4","method":"CancelTask","params":{"id":"task-456"}}"#;
    let req = make_a2a_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Release));
    assert_eq!(ctx.get_metadata("a2a.method"), Some("CancelTask"));
    assert_eq!(ctx.get_metadata("a2a.task_id"), Some("task-456"));
}

#[tokio::test]
async fn v03_alias_resolves() {
    let filter = make_filter(
        r#"{"method_aliases": {"message/send": "SendMessage", "tasks/get": "GetTask"}}"#,
    );
    let body_str = r#"{"jsonrpc":"2.0","id":"req-5","method":"message/send","params":{"message":{}}}"#;
    let req = make_a2a_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Release));
    assert_eq!(ctx.get_metadata("a2a.method"), Some("SendMessage"));
    assert_eq!(ctx.get_metadata("a2a.family"), Some("message"));
}

#[tokio::test]
async fn non_json_rpc_continues() {
    let filter = make_filter(r#"{"on_invalid": "continue"}"#);
    let body_str = r#"{"message":"hello"}"#;
    let req = make_a2a_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Continue));
}

#[tokio::test]
async fn non_json_rpc_rejects_by_default() {
    let filter = make_filter("{}");
    let body_str = r#"{"message":"hello"}"#;
    let req = make_a2a_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Reject(r) if r.status == 400));
}

#[tokio::test]
async fn extended_agent_card_classifies() {
    let filter = make_filter("{}");
    let body_str = r#"{"jsonrpc":"2.0","id":"req-6","method":"GetExtendedAgentCard","params":{}}"#;
    let req = make_a2a_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Release));
    assert_eq!(ctx.get_metadata("a2a.method"), Some("GetExtendedAgentCard"));
    assert_eq!(ctx.get_metadata("a2a.family"), Some("agent_card"));
}

#[tokio::test]
async fn subscribe_to_task_is_streaming() {
    let filter = make_filter("{}");
    let body_str =
        r#"{"jsonrpc":"2.0","id":"req-7","method":"SubscribeToTask","params":{"id":"task-789"}}"#;
    let req = make_a2a_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Release));
    assert_eq!(ctx.get_metadata("a2a.method"), Some("SubscribeToTask"));
    assert_eq!(ctx.get_metadata("a2a.streaming"), Some("true"));
    assert_eq!(ctx.get_metadata("a2a.task_id"), Some("task-789"));
}

#[tokio::test]
async fn promotes_headers() {
    let filter = make_filter("{}");
    let body_str = r#"{"jsonrpc":"2.0","id":"req-8","method":"SendMessage","params":{"message":{}}}"#;
    let req = make_a2a_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    let headers: std::collections::HashMap<_, _> = ctx
        .extra_request_headers
        .iter()
        .map(|(k, v)| (k.as_ref(), v.as_str()))
        .collect();
    assert_eq!(headers.get("x-praxis-a2a-method"), Some(&"SendMessage"));
    assert_eq!(headers.get("x-praxis-a2a-family"), Some(&"message"));
    assert_eq!(headers.get("x-praxis-a2a-task-present"), Some(&"false"));
    assert_eq!(headers.get("x-praxis-a2a-streaming"), Some(&"false"));
}

#[tokio::test]
async fn promotes_filter_results() {
    let filter = make_filter("{}");
    let body_str = r#"{"jsonrpc":"2.0","id":"req-9","method":"GetTask","params":{"id":"t-1"}}"#;
    let req = make_a2a_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    let results = ctx.filter_results.get("a2a").unwrap();
    assert_eq!(results.get("method"), Some("GetTask"));
    assert_eq!(results.get("family"), Some("task"));
    assert_eq!(results.get("task_present"), Some("true"));
    assert_eq!(results.get("streaming"), Some("false"));
}

#[tokio::test]
async fn on_request_is_noop() {
    let filter = make_filter("{}");
    let req = make_a2a_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);

    let action = filter.on_request(&mut ctx).await.unwrap();

    assert!(matches!(action, FilterAction::Continue));
}

#[tokio::test]
async fn returns_continue_on_none_body() {
    let filter = make_filter(r#"{"on_invalid": "continue"}"#);
    let req = make_a2a_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body: Option<Bytes> = None;

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Continue));
}

#[test]
fn body_access_is_read_only() {
    let filter = make_filter("{}");
    assert_eq!(filter.request_body_access(), crate::body::BodyAccess::ReadOnly);
}

#[test]
fn body_mode_is_stream_buffer() {
    use super::config::DEFAULT_MAX_BODY_BYTES;

    let filter = make_filter("{}");
    assert_eq!(
        filter.request_body_mode(),
        crate::body::BodyMode::StreamBuffer {
            max_bytes: Some(DEFAULT_MAX_BODY_BYTES)
        }
    );
}

#[tokio::test]
async fn push_notification_extracts_task_id() {
    let filter = make_filter("{}");
    let body_str = r#"{"jsonrpc":"2.0","id":"req-10","method":"CreateTaskPushNotificationConfig","params":{"taskId":"task-abc"}}"#;
    let req = make_a2a_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Release));
    assert_eq!(ctx.get_metadata("a2a.method"), Some("CreateTaskPushNotificationConfig"));
    assert_eq!(ctx.get_metadata("a2a.family"), Some("push_notification"));
    assert_eq!(ctx.get_metadata("a2a.task_id"), Some("task-abc"));
}

#[tokio::test]
async fn unknown_method_classifies_as_unknown_family() {
    let filter = make_filter("{}");
    let body_str = r#"{"jsonrpc":"2.0","id":"req-11","method":"CustomMethod","params":{}}"#;
    let req = make_a2a_request();
    let mut ctx = crate::test_utils::make_filter_context(&req);
    let mut body = Some(Bytes::from(body_str));

    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

    assert!(matches!(action, FilterAction::Release));
    assert_eq!(ctx.get_metadata("a2a.method"), Some("CustomMethod"));
    assert_eq!(ctx.get_metadata("a2a.family"), Some("unknown"));
    assert_eq!(ctx.get_metadata("a2a.streaming"), Some("false"));
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

fn make_filter(yaml: &str) -> A2aFilter {
    let cfg: A2aConfig = serde_yaml::from_str(yaml).unwrap();
    let (max_body_bytes, validated_config) = build_config(cfg).unwrap();
    let json_rpc_config = super::super::json_rpc::config::JsonRpcConfig {
        max_body_bytes,
        batch_policy: super::super::json_rpc::config::BatchPolicy::Reject,
        on_invalid: super::super::json_rpc::config::InvalidJsonRpcBehavior::Continue,
        headers: super::super::json_rpc::config::JsonRpcHeaders {
            method: None,
            id: None,
            kind: None,
        },
    };
    A2aFilter {
        config: validated_config,
        json_rpc_config,
        max_body_bytes,
    }
}

fn make_a2a_request() -> crate::context::Request {
    let mut req = crate::test_utils::make_request(http::Method::POST, "/a2a");
    req.headers.insert("content-type", "application/json".parse().unwrap());
    req
}
