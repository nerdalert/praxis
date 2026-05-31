// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests"
)]

use std::{collections::HashMap, time::Instant};

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode, Uri};
use praxis_filter::parse_filter_config;
use praxis_proto::envoy::service::{
    common::v3::{HeaderValue, HeaderValueOption, HttpStatus},
    ext_proc::v3::{CommonResponse, HeaderMutation, HeadersResponse, ImmediateResponse},
};

use super::*;

// -----------------------------------------------------------------------------
// Config Parsing
// -----------------------------------------------------------------------------

#[tokio::test]
async fn parse_valid_config() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
message_timeout_ms: 500
"#,
    )
    .unwrap();

    let filter = ExtProcFilter::from_config(&yaml).unwrap();
    assert_eq!(filter.name(), "ext_proc");
}

#[tokio::test]
async fn parse_minimal_config() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(r#"target: "http://127.0.0.1:50051""#).unwrap();
    let filter = ExtProcFilter::from_config(&yaml).unwrap();
    assert_eq!(filter.name(), "ext_proc");
}

#[tokio::test]
async fn parse_full_config_with_processing_mode() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
message_timeout_ms: 500
max_message_timeout_ms: 5000
processing_mode:
  request_header_mode: send
  response_header_mode: send
  request_body_mode: none
  response_body_mode: none
  request_trailer_mode: skip
  response_trailer_mode: skip
"#,
    )
    .unwrap();

    let filter = ExtProcFilter::from_config(&yaml).unwrap();
    assert_eq!(filter.name(), "ext_proc");
}

#[test]
fn defaults_core_fields() {
    let cfg = minimal_config();

    assert_eq!(
        cfg.message_timeout_ms, DEFAULT_MESSAGE_TIMEOUT_MS,
        "default message_timeout_ms should be {DEFAULT_MESSAGE_TIMEOUT_MS}"
    );
    assert_eq!(
        cfg.status_on_error, DEFAULT_STATUS_ON_ERROR,
        "default status_on_error should be {DEFAULT_STATUS_ON_ERROR}"
    );
    assert!(
        cfg.max_message_timeout_ms.is_none(),
        "default max_message_timeout_ms should be None"
    );
    assert_eq!(
        cfg.deferred_close_timeout_ms, DEFAULT_DEFERRED_CLOSE_TIMEOUT_MS,
        "default deferred_close_timeout_ms should be {DEFAULT_DEFERRED_CLOSE_TIMEOUT_MS}"
    );
}

#[test]
fn defaults_processing_mode() {
    let pm = minimal_config().processing_mode;
    assert_eq!(
        pm.request_header_mode,
        HeaderSendMode::Send,
        "default request_header_mode"
    );
    assert_eq!(
        pm.response_header_mode,
        HeaderSendMode::Send,
        "default response_header_mode"
    );
    assert_eq!(pm.request_body_mode, BodySendMode::None, "default request_body_mode");
    assert_eq!(pm.response_body_mode, BodySendMode::None, "default response_body_mode");
    assert_eq!(
        pm.request_trailer_mode,
        HeaderSendMode::Skip,
        "default request_trailer_mode"
    );
    assert_eq!(
        pm.response_trailer_mode,
        HeaderSendMode::Skip,
        "default response_trailer_mode"
    );
}

#[test]
fn defaults_feature_flags() {
    let cfg = minimal_config();

    assert!(!cfg.allow_mode_override, "default allow_mode_override should be false");
    assert!(!cfg.observability_mode, "default observability_mode should be false");
    assert!(
        !cfg.disable_immediate_response,
        "default disable_immediate_response should be false"
    );
    assert!(
        !cfg.allow_content_length_header,
        "default allow_content_length_header should be false"
    );
    assert!(
        !cfg.send_body_without_waiting_for_header_response,
        "default send_body_without_waiting should be false"
    );
    assert!(
        cfg.allowed_override_modes.is_empty(),
        "default allowed_override_modes should be empty"
    );
    assert!(cfg.mutation_rules.is_none(), "default mutation_rules should be None");
    assert!(cfg.forward_rules.is_none(), "default forward_rules should be None");
}

#[tokio::test]
async fn missing_target_errors() {
    let yaml: serde_yaml::Value = serde_yaml::from_str("message_timeout_ms: 500").unwrap();
    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("target"),
        "error should mention missing target field: {err}"
    );
}

#[tokio::test]
async fn invalid_target_uri_errors() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "not a valid uri"
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("invalid target URI"),
        "error should mention invalid target URI: {err}"
    );
}

#[tokio::test]
async fn unknown_field_errors() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
bogus_field: true
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("unknown field"),
        "error should mention unknown field: {err}"
    );
}

// -----------------------------------------------------------------------------
// Unsupported Feature Validation
// -----------------------------------------------------------------------------

#[tokio::test]
async fn rejects_request_header_mode_skip() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
processing_mode:
  request_header_mode: skip
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("request_header_mode"),
        "error should mention request_header_mode: {err}"
    );
}

#[tokio::test]
async fn rejects_response_header_mode_skip() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
processing_mode:
  response_header_mode: skip
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("response_header_mode"),
        "error should mention response_header_mode: {err}"
    );
}

#[tokio::test]
async fn rejects_request_trailer_mode_send() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
processing_mode:
  request_trailer_mode: send
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("request_trailer_mode"),
        "error should mention request_trailer_mode: {err}"
    );
}

#[tokio::test]
async fn rejects_response_trailer_mode_send() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
processing_mode:
  response_trailer_mode: send
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("response_trailer_mode"),
        "error should mention response_trailer_mode: {err}"
    );
}

#[tokio::test]
async fn rejects_allow_mode_override() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
allow_mode_override: true
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("allow_mode_override"),
        "error should mention allow_mode_override: {err}"
    );
}

#[tokio::test]
async fn rejects_observability_mode() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
observability_mode: true
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("observability_mode"),
        "error should mention observability_mode: {err}"
    );
}

#[tokio::test]
async fn rejects_disable_immediate_response() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
disable_immediate_response: true
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("disable_immediate_response"),
        "error should mention disable_immediate_response: {err}"
    );
}

#[tokio::test]
async fn rejects_mutation_rules() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
mutation_rules:
  allow: ["x-custom"]
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("mutation_rules"),
        "error should mention mutation_rules: {err}"
    );
}

#[tokio::test]
async fn rejects_forward_rules() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
forward_rules:
  allowed_headers: ["content-type"]
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("forward_rules"),
        "error should mention forward_rules: {err}"
    );
}

#[tokio::test]
async fn rejects_allow_content_length_header() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
allow_content_length_header: true
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("allow_content_length_header"),
        "error should mention allow_content_length_header: {err}"
    );
}

#[tokio::test]
async fn rejects_send_body_without_waiting() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
send_body_without_waiting_for_header_response: true
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string()
            .contains("send_body_without_waiting_for_header_response"),
        "error should mention send_body_without_waiting_for_header_response: {err}"
    );
}

#[test]
fn accepts_custom_status_on_error() {
    let cfg: ExtProcConfig = parse_filter_config(
        "ext_proc",
        &serde_yaml::from_str(
            r#"target: "http://127.0.0.1:50051"
status_on_error: 503"#,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(cfg.status_on_error, 503, "custom status_on_error should be preserved");
}

#[test]
fn accepts_custom_deferred_close_timeout() {
    let cfg: ExtProcConfig = parse_filter_config(
        "ext_proc",
        &serde_yaml::from_str(
            r#"target: "http://127.0.0.1:50051"
deferred_close_timeout_ms: 10000"#,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        cfg.deferred_close_timeout_ms, 10000,
        "custom deferred_close_timeout_ms should be preserved"
    );
}

#[tokio::test]
async fn rejects_all_request_body_send_mode_variants() {
    for mode in ["streamed", "buffered", "buffered_partial", "full_duplex_streamed"] {
        let yaml: serde_yaml::Value = serde_yaml::from_str(&format!(
            r#"
target: "http://127.0.0.1:50051"
processing_mode:
  request_body_mode: {mode}
"#,
        ))
        .unwrap();

        let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
        assert!(
            err.to_string().contains("request_body_mode"),
            "{mode} error should mention request_body_mode: {err}"
        );
        assert!(
            err.to_string().contains("not yet supported"),
            "{mode} should parse but fail validation: {err}"
        );
    }
}

#[tokio::test]
async fn rejects_all_response_body_send_mode_variants() {
    for mode in ["streamed", "buffered", "buffered_partial", "full_duplex_streamed"] {
        let yaml: serde_yaml::Value = serde_yaml::from_str(&format!(
            r#"
target: "http://127.0.0.1:50051"
processing_mode:
  response_body_mode: {mode}
"#,
        ))
        .unwrap();

        let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
        assert!(
            err.to_string().contains("response_body_mode"),
            "{mode} error should mention response_body_mode: {err}"
        );
        assert!(
            err.to_string().contains("not yet supported"),
            "{mode} should parse but fail validation: {err}"
        );
    }
}

#[tokio::test]
async fn rejects_status_on_error_zero() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
status_on_error: 0
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("status_on_error"),
        "error should mention status_on_error: {err}"
    );
}

#[tokio::test]
async fn rejects_status_on_error_out_of_range() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
status_on_error: 600
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("status_on_error"),
        "error should mention status_on_error: {err}"
    );
}

#[tokio::test]
async fn rejects_message_timeout_ms_zero() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
message_timeout_ms: 0
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("message_timeout_ms"),
        "error should reject message_timeout_ms set to 0: {err}"
    );
}

#[tokio::test]
async fn rejects_max_message_timeout_ms_zero() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
max_message_timeout_ms: 0
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("max_message_timeout_ms"),
        "error should reject max_message_timeout_ms set to 0: {err}"
    );
}

#[tokio::test]
async fn rejects_max_message_timeout_ms_less_than_message_timeout_ms() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
message_timeout_ms: 500
max_message_timeout_ms: 100
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("max_message_timeout_ms"),
        "error should reject max_message_timeout_ms less than message_timeout_ms: {err}"
    );
}

#[tokio::test]
async fn rejects_deferred_close_timeout_less_than_message_timeout() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
message_timeout_ms: 500
deferred_close_timeout_ms: 100
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("deferred_close_timeout_ms"),
        "error should reject deferred_close_timeout_ms < message_timeout_ms: {err}"
    );
}

#[tokio::test]
async fn rejects_allowed_override_modes_with_entries() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
allowed_override_modes:
  - request_header_mode: send
    response_header_mode: send
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("allowed_override_modes"),
        "error should mention allowed_override_modes: {err}"
    );
}

// -----------------------------------------------------------------------------
// Pipeline-Level failure_mode
// -----------------------------------------------------------------------------

#[tokio::test]
async fn failure_mode_in_yaml_is_stripped_by_parse() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
failure_mode: open
"#,
    )
    .unwrap();

    let filter = ExtProcFilter::from_config(&yaml).unwrap();
    assert_eq!(
        filter.name(),
        "ext_proc",
        "failure_mode should be stripped as a structural key and not cause an unknown-field error"
    );
}

#[tokio::test]
async fn filter_entry_captures_failure_mode_open() {
    let entry: praxis_filter::FilterEntry = serde_yaml::from_str(
        r#"
filter: ext_proc
failure_mode: open
target: "http://127.0.0.1:50051"
"#,
    )
    .unwrap();

    assert_eq!(
        entry.failure_mode,
        praxis_filter::FailureMode::Open,
        "FilterEntry should capture failure_mode: open"
    );

    let filter = ExtProcFilter::from_config(&entry.config).unwrap();
    assert_eq!(
        filter.name(),
        "ext_proc",
        "filter should build from the entry config after structural key stripping"
    );
}

#[tokio::test]
async fn filter_entry_captures_failure_mode_closed() {
    let entry: praxis_filter::FilterEntry = serde_yaml::from_str(
        r#"
filter: ext_proc
failure_mode: closed
target: "http://127.0.0.1:50051"
message_timeout_ms: 300
"#,
    )
    .unwrap();

    assert_eq!(
        entry.failure_mode,
        praxis_filter::FailureMode::Closed,
        "FilterEntry should capture failure_mode: closed"
    );

    let filter = ExtProcFilter::from_config(&entry.config).unwrap();
    assert_eq!(
        filter.name(),
        "ext_proc",
        "filter should build from the entry config after structural key stripping"
    );
}

#[tokio::test]
async fn filter_entry_defaults_failure_mode_to_closed() {
    let entry: praxis_filter::FilterEntry = serde_yaml::from_str(
        r#"
filter: ext_proc
target: "http://127.0.0.1:50051"
"#,
    )
    .unwrap();

    assert_eq!(
        entry.failure_mode,
        praxis_filter::FailureMode::Closed,
        "FilterEntry should default failure_mode to Closed"
    );

    let filter = ExtProcFilter::from_config(&entry.config).unwrap();
    assert_eq!(
        filter.name(),
        "ext_proc",
        "filter should build from the entry config without failure_mode"
    );
}

#[tokio::test]
async fn pipeline_builds_with_ext_proc_and_failure_mode() {
    let mut registry = praxis_filter::FilterRegistry::with_builtins();
    registry
        .register("ext_proc", praxis_filter::http_builtin(ExtProcFilter::from_config))
        .unwrap();

    let mut entries: Vec<praxis_filter::FilterEntry> = serde_yaml::from_str(
        r#"
- filter: ext_proc
  failure_mode: open
  target: "http://127.0.0.1:50051"
- filter: ext_proc
  failure_mode: closed
  target: "http://127.0.0.1:50052"
"#,
    )
    .unwrap();

    let pipeline = praxis_filter::FilterPipeline::build(&mut entries, &registry).unwrap();
    assert_eq!(pipeline.len(), 2, "pipeline should contain both ext_proc filters");
}

#[tokio::test]
async fn rejects_negative_max_message_timeout_ms() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:50051"
max_message_timeout_ms: -1
"#,
    )
    .unwrap();

    let err = ExtProcFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("max_message_timeout_ms") || err.to_string().contains("integer"),
        "error should reject negative max_message_timeout_ms: {err}"
    );
}

// -----------------------------------------------------------------------------
// Proto Conversion: request_to_proto_headers
// -----------------------------------------------------------------------------

#[test]
fn request_to_proto_headers_includes_method_and_path() {
    let req = make_request(Method::POST, "/api/v1/users");
    let ctx = make_ctx(&req);

    let proto = mutations::request_to_proto_headers(&ctx);
    let headers = proto.headers.unwrap().headers;

    let method = headers
        .iter()
        .find(|h| h.key == ":method")
        .expect("should include :method");
    assert_eq!(method.value, "POST", "method pseudo-header should match request method");

    let path = headers.iter().find(|h| h.key == ":path").expect("should include :path");
    assert_eq!(
        path.value, "/api/v1/users",
        "path pseudo-header should match request URI"
    );
}

#[test]
fn request_to_proto_headers_includes_request_headers() {
    let mut req = make_request(Method::GET, "/");
    req.headers.insert("content-type", "application/json".parse().unwrap());
    req.headers.insert("x-request-id", "abc-123".parse().unwrap());
    let ctx = make_ctx(&req);

    let proto = mutations::request_to_proto_headers(&ctx);
    let headers = proto.headers.unwrap().headers;

    let ct = headers
        .iter()
        .find(|h| h.key == "content-type")
        .expect("should include content-type");
    assert_eq!(ct.value, "application/json", "content-type should match");

    let rid = headers
        .iter()
        .find(|h| h.key == "x-request-id")
        .expect("should include x-request-id");
    assert_eq!(rid.value, "abc-123", "x-request-id should match");
}

// -----------------------------------------------------------------------------
// Proto Conversion: response_to_proto_headers
// -----------------------------------------------------------------------------

#[test]
fn response_to_proto_headers_includes_status() {
    let req = make_request(Method::GET, "/");
    let mut resp = make_response();
    resp.status = StatusCode::NOT_FOUND;
    let mut ctx = make_ctx(&req);
    ctx.response_header = Some(&mut resp);

    let proto = mutations::response_to_proto_headers(&ctx);
    let headers = proto.headers.unwrap().headers;

    let status = headers
        .iter()
        .find(|h| h.key == ":status")
        .expect("should include :status");
    assert_eq!(status.value, "404", "status pseudo-header should match response status");
}

#[test]
fn response_to_proto_headers_includes_response_headers() {
    let req = make_request(Method::GET, "/");
    let mut resp = make_response();
    resp.headers.insert("x-powered-by", "praxis".parse().unwrap());
    let mut ctx = make_ctx(&req);
    ctx.response_header = Some(&mut resp);

    let proto = mutations::response_to_proto_headers(&ctx);
    let headers = proto.headers.unwrap().headers;

    let hdr = headers
        .iter()
        .find(|h| h.key == "x-powered-by")
        .expect("should include x-powered-by");
    assert_eq!(hdr.value, "praxis", "x-powered-by value should match");
}

#[test]
fn response_to_proto_headers_empty_when_no_response() {
    let req = make_request(Method::GET, "/");
    let ctx = make_ctx(&req);

    let proto = mutations::response_to_proto_headers(&ctx);
    let headers = proto.headers.unwrap().headers;
    assert!(
        headers.is_empty(),
        "headers should be empty when response_header is None"
    );
}

// -----------------------------------------------------------------------------
// Mutation: apply_request_header_mutation
// -----------------------------------------------------------------------------

#[test]
fn apply_request_header_mutation_adds_to_extra_headers() {
    let req = make_request(Method::GET, "/");
    let mut ctx = make_ctx(&req);

    let mutation = HeaderMutation {
        set_headers: vec![make_hvo("x-custom", "value1")],
        remove_headers: vec![],
    };

    mutations::apply_request_header_mutation(&mutation, &mut ctx);

    assert_eq!(ctx.extra_request_headers.len(), 1, "should add one header");
    assert_eq!(ctx.extra_request_headers[0].0, "x-custom", "header name should match");
    assert_eq!(ctx.extra_request_headers[0].1, "value1", "header value should match");
}

#[test]
fn apply_request_header_mutation_skips_pseudo_headers() {
    let req = make_request(Method::GET, "/");
    let mut ctx = make_ctx(&req);

    let mutation = HeaderMutation {
        set_headers: vec![
            make_hvo(":method", "POST"),
            make_hvo(":path", "/new"),
            make_hvo("x-real", "kept"),
        ],
        remove_headers: vec![],
    };

    mutations::apply_request_header_mutation(&mutation, &mut ctx);

    assert_eq!(ctx.extra_request_headers.len(), 1, "should skip pseudo-headers");
    assert_eq!(
        ctx.extra_request_headers[0].0, "x-real",
        "only non-pseudo header should be added"
    );
}

#[test]
fn apply_request_header_mutation_skips_removal() {
    let req = make_request(Method::GET, "/");
    let mut ctx = make_ctx(&req);

    let mutation = HeaderMutation {
        set_headers: vec![],
        remove_headers: vec!["content-type".to_owned()],
    };

    mutations::apply_request_header_mutation(&mutation, &mut ctx);

    assert!(
        ctx.extra_request_headers.is_empty(),
        "removal should be skipped without error"
    );
}

// -----------------------------------------------------------------------------
// Mutation: apply_response_header_mutation
// -----------------------------------------------------------------------------

#[test]
fn apply_response_header_mutation_modifies_response() {
    let req = make_request(Method::GET, "/");
    let mut resp = make_response();
    let mut ctx = make_ctx(&req);
    ctx.response_header = Some(&mut resp);

    let mutation = HeaderMutation {
        set_headers: vec![make_hvo("x-added", "new-value")],
        remove_headers: vec![],
    };

    mutations::apply_response_header_mutation(&mutation, &mut ctx);

    assert!(ctx.response_headers_modified, "should set response_headers_modified");
    let resp = ctx.response_header.unwrap();
    assert_eq!(
        resp.headers.get("x-added").unwrap(),
        "new-value",
        "header should be inserted"
    );
}

#[test]
fn apply_response_header_mutation_removes_header() {
    let req = make_request(Method::GET, "/");
    let mut resp = make_response();
    resp.headers.insert("x-remove-me", "gone".parse().unwrap());
    let mut ctx = make_ctx(&req);
    ctx.response_header = Some(&mut resp);

    let mutation = HeaderMutation {
        set_headers: vec![],
        remove_headers: vec!["x-remove-me".to_owned()],
    };

    mutations::apply_response_header_mutation(&mutation, &mut ctx);

    assert!(ctx.response_headers_modified, "should set response_headers_modified");
    let resp = ctx.response_header.unwrap();
    assert!(resp.headers.get("x-remove-me").is_none(), "header should be removed");
}

#[test]
fn apply_response_header_mutation_skips_pseudo_headers() {
    let req = make_request(Method::GET, "/");
    let mut resp = make_response();
    let mut ctx = make_ctx(&req);
    ctx.response_header = Some(&mut resp);

    let mutation = HeaderMutation {
        set_headers: vec![make_hvo(":status", "404")],
        remove_headers: vec![":status".to_owned()],
    };

    mutations::apply_response_header_mutation(&mutation, &mut ctx);

    assert!(
        !ctx.response_headers_modified,
        "pseudo-header mutations should not mark headers as modified"
    );
}

#[test]
fn apply_response_header_mutation_noop_when_no_response() {
    let req = make_request(Method::GET, "/");
    let mut ctx = make_ctx(&req);

    let mutation = HeaderMutation {
        set_headers: vec![make_hvo("x-added", "value")],
        remove_headers: vec![],
    };

    mutations::apply_response_header_mutation(&mutation, &mut ctx);

    assert!(
        !ctx.response_headers_modified,
        "should not modify when response_header is None"
    );
}

// -----------------------------------------------------------------------------
// Mutation: immediate_to_rejection
// -----------------------------------------------------------------------------

#[test]
fn immediate_to_rejection_maps_status_body_headers() {
    let imm = ImmediateResponse {
        status: Some(HttpStatus { code: 403 }),
        headers: Some(HeaderMutation {
            set_headers: vec![make_hvo("x-reason", "blocked")],
            remove_headers: vec![],
        }),
        body: "forbidden".to_owned(),
        grpc_status: None,
        details: String::new(),
    };

    let action = mutations::immediate_to_rejection(&imm);
    let rejection = match action {
        FilterAction::Reject(r) => r,
        other => panic!("expected Reject, got {other:?}"),
    };

    assert_eq!(rejection.status, 403, "status should match");
    assert_eq!(rejection.body.unwrap(), Bytes::from("forbidden"), "body should match");
    assert_eq!(rejection.headers.len(), 1, "should have one header");
    assert_eq!(rejection.headers[0].0, "x-reason", "header name should match");
    assert_eq!(rejection.headers[0].1, "blocked", "header value should match");
}

#[test]
fn immediate_to_rejection_defaults_status_to_200() {
    let imm = ImmediateResponse {
        status: None,
        headers: None,
        body: String::new(),
        grpc_status: None,
        details: String::new(),
    };

    let action = mutations::immediate_to_rejection(&imm);
    let rejection = match action {
        FilterAction::Reject(r) => r,
        other => panic!("expected Reject, got {other:?}"),
    };

    assert_eq!(rejection.status, 200, "should default to 200 when status absent");
    assert!(rejection.body.is_none(), "empty body should be None");
    assert!(rejection.headers.is_empty(), "should have no headers");
}

#[test]
fn immediate_to_rejection_clamps_invalid_status() {
    let imm = ImmediateResponse {
        status: Some(HttpStatus { code: 999 }),
        headers: None,
        body: String::new(),
        grpc_status: None,
        details: String::new(),
    };

    let action = mutations::immediate_to_rejection(&imm);
    let rejection = match action {
        FilterAction::Reject(r) => r,
        other => panic!("expected Reject, got {other:?}"),
    };

    assert_eq!(rejection.status, 500, "out-of-range status should clamp to 500");
}

// -----------------------------------------------------------------------------
// Utility: header_value_string
// -----------------------------------------------------------------------------

#[test]
fn header_value_string_prefers_raw_value() {
    let hv = HeaderValue {
        key: "x-test".to_owned(),
        value: "text-value".to_owned(),
        raw_value: b"raw-value".to_vec(),
    };

    assert_eq!(
        mutations::header_value_string(&hv),
        "raw-value",
        "should prefer raw_value when non-empty"
    );
}

#[test]
fn header_value_string_falls_back_to_value() {
    let hv = HeaderValue {
        key: "x-test".to_owned(),
        value: "text-value".to_owned(),
        raw_value: Vec::new(),
    };

    assert_eq!(
        mutations::header_value_string(&hv),
        "text-value",
        "should fall back to value when raw_value is empty"
    );
}

// -----------------------------------------------------------------------------
// Utility: is_pseudo_header
// -----------------------------------------------------------------------------

#[test]
fn is_pseudo_header_detects_colon_prefix() {
    assert!(mutations::is_pseudo_header(":method"), ":method is a pseudo-header");
    assert!(mutations::is_pseudo_header(":path"), ":path is a pseudo-header");
    assert!(mutations::is_pseudo_header(":status"), ":status is a pseudo-header");
    assert!(
        !mutations::is_pseudo_header("content-type"),
        "content-type is not a pseudo-header"
    );
    assert!(
        !mutations::is_pseudo_header("x-custom"),
        "x-custom is not a pseudo-header"
    );
}

// -----------------------------------------------------------------------------
// Mutation: apply_headers_response delegates by phase
// -----------------------------------------------------------------------------

#[test]
fn apply_headers_response_delegates_to_request_phase() {
    let req = make_request(Method::GET, "/");
    let mut ctx = make_ctx(&req);

    let hr = HeadersResponse {
        response: Some(CommonResponse {
            status: 0,
            header_mutation: Some(HeaderMutation {
                set_headers: vec![make_hvo("x-from-proc", "req")],
                remove_headers: vec![],
            }),
            body_mutation: None,
            trailers: None,
            clear_route_cache: false,
        }),
    };

    mutations::apply_headers_response(&hr, &mut ctx, Phase::Request);

    assert_eq!(
        ctx.extra_request_headers.len(),
        1,
        "should add to extra request headers"
    );
    assert_eq!(
        ctx.extra_request_headers[0].0, "x-from-proc",
        "header name should match"
    );
}

#[test]
fn apply_headers_response_delegates_to_response_phase() {
    let req = make_request(Method::GET, "/");
    let mut resp = make_response();
    let mut ctx = make_ctx(&req);
    ctx.response_header = Some(&mut resp);

    let hr = HeadersResponse {
        response: Some(CommonResponse {
            status: 0,
            header_mutation: Some(HeaderMutation {
                set_headers: vec![make_hvo("x-from-proc", "resp")],
                remove_headers: vec![],
            }),
            body_mutation: None,
            trailers: None,
            clear_route_cache: false,
        }),
    };

    mutations::apply_headers_response(&hr, &mut ctx, Phase::Response);

    assert!(ctx.response_headers_modified, "should set response_headers_modified");
    let resp = ctx.response_header.unwrap();
    assert_eq!(
        resp.headers.get("x-from-proc").unwrap(),
        "resp",
        "header should be set on response"
    );
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

/// Build a minimal [`praxis_filter::Request`].
fn make_request(method: Method, path: &str) -> praxis_filter::Request {
    praxis_filter::Request {
        method,
        uri: path.parse::<Uri>().expect("invalid URI in test"),
        headers: HeaderMap::new(),
    }
}

/// Build a minimal OK [`praxis_filter::Response`].
fn make_response() -> praxis_filter::Response {
    praxis_filter::Response {
        headers: HeaderMap::new(),
        status: StatusCode::OK,
    }
}

/// Build a minimal [`HttpFilterContext`] for unit tests.
fn make_ctx(req: &praxis_filter::Request) -> HttpFilterContext<'_> {
    HttpFilterContext {
        body_done_indices: Vec::new(),
        branch_iterations: HashMap::new(),
        client_addr: None,
        cluster: None,
        downstream_tls: false,
        executed_filter_indices: Vec::new(),
        extra_request_headers: Vec::new(),
        request_headers_to_remove: Vec::new(),
        request_headers_to_set: Vec::new(),
        filter_metadata: HashMap::new(),
        filter_results: HashMap::new(),
        health_registry: None,
        kv_stores: None,
        request: req,
        request_body_bytes: 0,
        request_body_mode: praxis_filter::BodyMode::Stream,
        request_start: Instant::now(),
        response_body_bytes: 0,
        response_body_mode: praxis_filter::BodyMode::Stream,
        response_header: None,
        response_headers_modified: false,
        rewritten_path: None,
        selected_endpoint_index: None,
        upstream: None,
    }
}

/// Build a [`HeaderValueOption`] with the given key and value.
fn make_hvo(key: &str, value: &str) -> HeaderValueOption {
    HeaderValueOption {
        header: Some(HeaderValue {
            key: key.to_owned(),
            value: value.to_owned(),
            raw_value: Vec::new(),
        }),
        append: None,
        append_action: 0,
    }
}

/// Parse a minimal valid config for default-checking tests.
fn minimal_config() -> ExtProcConfig {
    let yaml: serde_yaml::Value = serde_yaml::from_str(r#"target: "http://127.0.0.1:50051""#).unwrap();
    parse_filter_config("ext_proc", &yaml).unwrap()
}
