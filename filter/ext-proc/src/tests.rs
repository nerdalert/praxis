// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests"
)]

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

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
fn request_to_proto_headers_preserves_query_string() {
    let req = make_request(Method::GET, "/search?q=secret&page=1");
    let ctx = make_ctx(&req);

    let proto = mutations::request_to_proto_headers(&ctx);
    let headers = proto.headers.unwrap().headers;

    let path = headers.iter().find(|h| h.key == ":path").expect("should include :path");
    assert_eq!(
        path.value, "/search?q=secret&page=1",
        "path pseudo-header should include query string"
    );
}

#[test]
fn request_to_proto_headers_includes_scheme() {
    let req = make_request(Method::GET, "/");
    let ctx = make_ctx(&req);

    let proto = mutations::request_to_proto_headers(&ctx);
    let headers = proto.headers.unwrap().headers;

    let scheme = headers
        .iter()
        .find(|h| h.key == ":scheme")
        .expect("should include :scheme");
    assert_eq!(scheme.value, "http", "scheme should default to http");
}

#[test]
fn request_to_proto_headers_includes_https_scheme() {
    let req = make_request(Method::GET, "/");
    let mut ctx = make_ctx(&req);
    ctx.downstream_tls = true;

    let proto = mutations::request_to_proto_headers(&ctx);
    let headers = proto.headers.unwrap().headers;

    let scheme = headers
        .iter()
        .find(|h| h.key == ":scheme")
        .expect("should include :scheme");
    assert_eq!(scheme.value, "https", "scheme should be https when TLS is active");
}

#[test]
fn request_to_proto_headers_includes_authority() {
    let mut req = make_request(Method::GET, "/");
    req.headers.insert("host", "example.com".parse().unwrap());
    let ctx = make_ctx(&req);

    let proto = mutations::request_to_proto_headers(&ctx);
    let headers = proto.headers.unwrap().headers;

    let authority = headers
        .iter()
        .find(|h| h.key == ":authority")
        .expect("should include :authority");
    assert_eq!(authority.value, "example.com", "authority should match host header");
}

#[test]
fn request_to_proto_headers_omits_authority_when_no_host() {
    let req = make_request(Method::GET, "/");
    let ctx = make_ctx(&req);

    let proto = mutations::request_to_proto_headers(&ctx);
    let headers = proto.headers.unwrap().headers;

    assert!(
        headers.iter().all(|h| h.key != ":authority"),
        "should not include :authority when host header is absent"
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
fn apply_request_header_mutation_removes_header() {
    let mut req = make_request(Method::GET, "/");
    req.headers.insert("x-remove-me", "gone".parse().unwrap());
    let mut ctx = make_ctx(&req);

    let mutation = HeaderMutation {
        set_headers: vec![],
        remove_headers: vec!["x-remove-me".to_owned()],
    };

    mutations::apply_request_header_mutation(&mutation, &mut ctx);

    assert_eq!(
        ctx.request_headers_to_remove.len(),
        1,
        "should queue one header for removal"
    );
    assert_eq!(
        ctx.request_headers_to_remove[0].as_str(),
        "x-remove-me",
        "removed header name should match"
    );
}

#[test]
fn apply_request_header_mutation_removal_skips_pseudo_headers() {
    let req = make_request(Method::GET, "/");
    let mut ctx = make_ctx(&req);

    let mutation = HeaderMutation {
        set_headers: vec![],
        remove_headers: vec![":method".to_owned(), ":path".to_owned()],
    };

    mutations::apply_request_header_mutation(&mutation, &mut ctx);

    assert!(
        ctx.request_headers_to_remove.is_empty(),
        "pseudo-header removals should be skipped"
    );
}

#[test]
fn apply_request_header_mutation_overwrite_uses_set_queue() {
    use praxis_proto::envoy::service::common::v3::header_value_option::HeaderAppendAction;

    let mut req = make_request(Method::GET, "/");
    req.headers.insert("x-existing", "old".parse().unwrap());
    let mut ctx = make_ctx(&req);

    let hvo = make_hvo_with_append(
        "x-existing",
        "new",
        HeaderAppendAction::OverwriteIfExistsOrAdd as i32,
        None,
    );
    let mutation = HeaderMutation {
        set_headers: vec![hvo],
        remove_headers: vec![],
    };

    mutations::apply_request_header_mutation(&mutation, &mut ctx);

    assert!(
        ctx.extra_request_headers.is_empty(),
        "overwrite should not use extra_request_headers"
    );
    assert_eq!(
        ctx.request_headers_to_set.len(),
        1,
        "overwrite should use request_headers_to_set"
    );
    assert_eq!(
        ctx.request_headers_to_set[0].0.as_str(),
        "x-existing",
        "name should match"
    );
    assert_eq!(ctx.request_headers_to_set[0].1, "new", "value should match");
}

#[test]
fn apply_request_header_mutation_overwrite_if_exists_skips_absent() {
    use praxis_proto::envoy::service::common::v3::header_value_option::HeaderAppendAction;

    let req = make_request(Method::GET, "/");
    let mut ctx = make_ctx(&req);

    let hvo = make_hvo_with_append("x-absent", "value", HeaderAppendAction::OverwriteIfExists as i32, None);
    let mutation = HeaderMutation {
        set_headers: vec![hvo],
        remove_headers: vec![],
    };

    mutations::apply_request_header_mutation(&mutation, &mut ctx);

    assert!(
        ctx.request_headers_to_set.is_empty(),
        "overwrite-if-exists should skip absent headers"
    );
    assert!(
        ctx.extra_request_headers.is_empty(),
        "should not fall through to append"
    );
}

#[test]
fn apply_request_header_mutation_add_if_absent_skips_existing() {
    use praxis_proto::envoy::service::common::v3::header_value_option::HeaderAppendAction;

    let mut req = make_request(Method::GET, "/");
    req.headers.insert("x-existing", "old".parse().unwrap());
    let mut ctx = make_ctx(&req);

    let hvo = make_hvo_with_append("x-existing", "new", HeaderAppendAction::AddIfAbsent as i32, None);
    let mutation = HeaderMutation {
        set_headers: vec![hvo],
        remove_headers: vec![],
    };

    mutations::apply_request_header_mutation(&mutation, &mut ctx);

    assert!(
        ctx.extra_request_headers.is_empty(),
        "add-if-absent should skip existing headers"
    );
}

#[test]
fn apply_request_header_mutation_add_if_absent_adds_missing() {
    use praxis_proto::envoy::service::common::v3::header_value_option::HeaderAppendAction;

    let req = make_request(Method::GET, "/");
    let mut ctx = make_ctx(&req);

    let hvo = make_hvo_with_append("x-new", "value", HeaderAppendAction::AddIfAbsent as i32, None);
    let mutation = HeaderMutation {
        set_headers: vec![hvo],
        remove_headers: vec![],
    };

    mutations::apply_request_header_mutation(&mutation, &mut ctx);

    assert_eq!(
        ctx.extra_request_headers.len(),
        1,
        "add-if-absent should add missing headers"
    );
    assert_eq!(ctx.extra_request_headers[0].0, "x-new", "header name should match");
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
fn apply_response_header_mutation_remove_absent_does_not_mark_modified() {
    let req = make_request(Method::GET, "/");
    let mut resp = make_response();
    let mut ctx = make_ctx(&req);
    ctx.response_header = Some(&mut resp);

    let mutation = HeaderMutation {
        set_headers: vec![],
        remove_headers: vec!["x-nonexistent".to_owned()],
    };

    mutations::apply_response_header_mutation(&mutation, &mut ctx);

    assert!(
        !ctx.response_headers_modified,
        "removing an absent header should not mark response as modified"
    );
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
// Mutation: should_append (via set_response_headers)
// -----------------------------------------------------------------------------

#[test]
fn response_header_default_action_appends() {
    use praxis_proto::envoy::service::common::v3::header_value_option::HeaderAppendAction;

    let req = make_request(Method::GET, "/");
    let mut resp = make_response();
    resp.headers.insert("x-existing", "original".parse().unwrap());
    let mut ctx = make_ctx(&req);
    ctx.response_header = Some(&mut resp);

    let hvo = make_hvo_with_append(
        "x-existing",
        "appended",
        HeaderAppendAction::AppendIfExistsOrAdd as i32,
        None,
    );
    let mutation = HeaderMutation {
        set_headers: vec![hvo],
        remove_headers: vec![],
    };

    mutations::apply_response_header_mutation(&mutation, &mut ctx);

    let resp = ctx.response_header.unwrap();
    let values: Vec<&str> = resp
        .headers
        .get_all("x-existing")
        .iter()
        .map(|v| v.to_str().unwrap_or_default())
        .collect();
    assert_eq!(values, vec!["original", "appended"], "default action should append");
}

#[test]
fn response_header_overwrite_action_replaces() {
    use praxis_proto::envoy::service::common::v3::header_value_option::HeaderAppendAction;

    let req = make_request(Method::GET, "/");
    let mut resp = make_response();
    resp.headers.insert("x-existing", "original".parse().unwrap());
    let mut ctx = make_ctx(&req);
    ctx.response_header = Some(&mut resp);

    let hvo = make_hvo_with_append(
        "x-existing",
        "replaced",
        HeaderAppendAction::OverwriteIfExistsOrAdd as i32,
        None,
    );
    let mutation = HeaderMutation {
        set_headers: vec![hvo],
        remove_headers: vec![],
    };

    mutations::apply_response_header_mutation(&mutation, &mut ctx);

    let resp = ctx.response_header.unwrap();
    assert_eq!(
        resp.headers.get("x-existing").unwrap(),
        "replaced",
        "overwrite action should replace the existing value"
    );
}

#[test]
fn response_header_zero_action_with_append_true_appends() {
    let req = make_request(Method::GET, "/");
    let mut resp = make_response();
    resp.headers.insert("x-existing", "original".parse().unwrap());
    let mut ctx = make_ctx(&req);
    ctx.response_header = Some(&mut resp);

    let mutation = HeaderMutation {
        set_headers: vec![make_hvo_with_append("x-existing", "appended", 0, Some(true))],
        remove_headers: vec![],
    };

    mutations::apply_response_header_mutation(&mutation, &mut ctx);

    let resp = ctx.response_header.unwrap();
    let values: Vec<&str> = resp
        .headers
        .get_all("x-existing")
        .iter()
        .map(|v| v.to_str().unwrap_or_default())
        .collect();
    assert_eq!(
        values,
        vec!["original", "appended"],
        "deprecated append=true should append"
    );
}

#[test]
fn response_header_zero_action_with_append_false_overwrites() {
    let req = make_request(Method::GET, "/");
    let mut resp = make_response();
    resp.headers.insert("x-existing", "original".parse().unwrap());
    let mut ctx = make_ctx(&req);
    ctx.response_header = Some(&mut resp);

    let mutation = HeaderMutation {
        set_headers: vec![make_hvo_with_append("x-existing", "replaced", 0, Some(false))],
        remove_headers: vec![],
    };

    mutations::apply_response_header_mutation(&mutation, &mut ctx);

    let resp = ctx.response_header.unwrap();
    assert_eq!(
        resp.headers.get("x-existing").unwrap(),
        "replaced",
        "deprecated append=false should overwrite"
    );
}

#[test]
fn response_header_both_unset_defaults_to_append() {
    let req = make_request(Method::GET, "/");
    let mut resp = make_response();
    resp.headers.insert("x-existing", "original".parse().unwrap());
    let mut ctx = make_ctx(&req);
    ctx.response_header = Some(&mut resp);

    let mutation = HeaderMutation {
        set_headers: vec![make_hvo("x-existing", "appended")],
        remove_headers: vec![],
    };

    mutations::apply_response_header_mutation(&mutation, &mut ctx);

    let resp = ctx.response_header.unwrap();
    let values: Vec<&str> = resp
        .headers
        .get_all("x-existing")
        .iter()
        .map(|v| v.to_str().unwrap_or_default())
        .collect();
    assert_eq!(
        values,
        vec!["original", "appended"],
        "both fields unset should default to append per proto3 spec"
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
// gRPC Callout Integration
// -----------------------------------------------------------------------------

#[tokio::test]
async fn grpc_request_headers_round_trip_applies_mutation() {
    let (addr, _guard) = start_mock_processor(MockBehavior::AddHeader {
        name: "x-injected".to_owned(),
        value: "from-processor".to_owned(),
    })
    .await;

    let channel = connect_channel(addr).await;

    let req = make_request(Method::GET, "/test");
    let mut ctx = make_ctx(&req);
    let timeout = Duration::from_secs(5);

    let action = callout::process_request_headers(channel, &addr.to_string(), timeout, &mut ctx)
        .await
        .expect("callout should succeed");

    assert!(
        matches!(action, FilterAction::Continue),
        "action should be Continue after header mutation"
    );
    let injected = ctx.extra_request_headers.iter().find(|(k, _)| k == "x-injected");
    assert!(injected.is_some(), "processor-injected header should be present");
    assert_eq!(
        injected.unwrap().1,
        "from-processor",
        "injected header value should match"
    );
}

#[tokio::test]
async fn grpc_response_headers_round_trip_applies_mutation() {
    let (addr, _guard) = start_mock_processor(MockBehavior::AddHeader {
        name: "x-resp-injected".to_owned(),
        value: "from-processor".to_owned(),
    })
    .await;

    let channel = connect_channel(addr).await;

    let req = make_request(Method::GET, "/");
    let mut resp = make_response();
    let mut ctx = make_ctx(&req);
    ctx.response_header = Some(&mut resp);
    let timeout = Duration::from_secs(5);

    let action = callout::process_response_headers(channel, &addr.to_string(), timeout, &mut ctx)
        .await
        .expect("callout should succeed");

    assert!(
        matches!(action, FilterAction::Continue),
        "action should be Continue after response header mutation"
    );
    assert!(ctx.response_headers_modified, "response_headers_modified should be set");
    let resp = ctx.response_header.unwrap();
    assert_eq!(
        resp.headers.get("x-resp-injected").unwrap(),
        "from-processor",
        "response header should be mutated"
    );
}

#[tokio::test]
async fn grpc_immediate_response_returns_rejection() {
    let (addr, _guard) = start_mock_processor(MockBehavior::ImmediateReject {
        status: 403,
        body: "blocked".to_owned(),
    })
    .await;

    let channel = connect_channel(addr).await;

    let req = make_request(Method::GET, "/secret");
    let mut ctx = make_ctx(&req);
    let timeout = Duration::from_secs(5);

    let action = callout::process_request_headers(channel, &addr.to_string(), timeout, &mut ctx)
        .await
        .expect("callout should succeed");

    let rejection = match action {
        FilterAction::Reject(r) => r,
        other => panic!("expected Reject, got {other:?}"),
    };
    assert_eq!(rejection.status, 403, "rejection status should match");
    assert_eq!(
        rejection.body.unwrap(),
        Bytes::from("blocked"),
        "rejection body should match"
    );
}

#[tokio::test]
async fn grpc_noop_response_returns_continue() {
    let (addr, _guard) = start_mock_processor(MockBehavior::Noop).await;

    let channel = connect_channel(addr).await;

    let req = make_request(Method::GET, "/");
    let mut ctx = make_ctx(&req);
    let timeout = Duration::from_secs(5);

    let action = callout::process_request_headers(channel, &addr.to_string(), timeout, &mut ctx)
        .await
        .expect("callout should succeed");

    assert!(
        matches!(action, FilterAction::Continue),
        "no-op response should produce Continue"
    );
    assert!(
        ctx.extra_request_headers.is_empty(),
        "no headers should be added for no-op response"
    );
}

#[tokio::test]
async fn grpc_unexpected_response_type_returns_error() {
    let (addr, _guard) = start_mock_processor(MockBehavior::UnexpectedBodyResponse).await;

    let channel = connect_channel(addr).await;

    let req = make_request(Method::GET, "/");
    let mut ctx = make_ctx(&req);
    let timeout = Duration::from_secs(5);

    let result = callout::process_request_headers(channel, &addr.to_string(), timeout, &mut ctx).await;

    assert!(result.is_err(), "unexpected response type should return Err");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("RequestBody"),
        "error should name the unexpected variant: {err}"
    );
    assert!(err.contains("request"), "error should mention the phase: {err}");
}

#[tokio::test]
async fn grpc_phase_mismatched_response_returns_error() {
    let (addr, _guard) = start_mock_processor(MockBehavior::AlwaysResponseHeaders).await;

    let channel = connect_channel(addr).await;

    let req = make_request(Method::GET, "/");
    let mut ctx = make_ctx(&req);
    let timeout = Duration::from_secs(5);

    let result = callout::process_request_headers(channel, &addr.to_string(), timeout, &mut ctx).await;

    assert!(result.is_err(), "phase-mismatched response should return Err");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("ResponseHeaders"),
        "error should name the mismatched variant: {err}"
    );
}

#[tokio::test]
async fn grpc_timeout_returns_error() {
    let (addr, _guard) = start_mock_processor(MockBehavior::Hang).await;

    let channel = connect_channel(addr).await;

    let req = make_request(Method::GET, "/");
    let mut ctx = make_ctx(&req);
    let timeout = Duration::from_millis(50);

    let result = callout::process_request_headers(channel, &addr.to_string(), timeout, &mut ctx).await;

    assert!(result.is_err(), "timed-out callout should return Err");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("timeout"), "error should mention timeout: {err}");
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
        request_body_mode: BodyMode::Stream,
        request_start: Instant::now(),
        response_body_bytes: 0,
        response_body_mode: BodyMode::Stream,
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

/// Build a [`HeaderValueOption`] with explicit append control.
fn make_hvo_with_append(key: &str, value: &str, append_action: i32, append: Option<bool>) -> HeaderValueOption {
    HeaderValueOption {
        header: Some(HeaderValue {
            key: key.to_owned(),
            value: value.to_owned(),
            raw_value: Vec::new(),
        }),
        append,
        append_action,
    }
}

/// Connect a tonic [`Channel`] to the given address.
async fn connect_channel(addr: SocketAddr) -> Channel {
    Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap()
}

/// Parse a minimal valid config for default-checking tests.
fn minimal_config() -> ExtProcConfig {
    let yaml: serde_yaml::Value = serde_yaml::from_str(r#"target: "http://127.0.0.1:50051""#).unwrap();
    parse_filter_config("ext_proc", &yaml).unwrap()
}

// -----------------------------------------------------------------------------
// Mock gRPC Server
// -----------------------------------------------------------------------------

use std::{
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use praxis_proto::envoy::service::ext_proc::v3::{
    BodyMutation, BodyResponse, ProcessingRequest, ProcessingResponse, body_mutation,
    external_processor_server::{ExternalProcessor, ExternalProcessorServer},
    processing_request, processing_response,
};
use tokio::sync::oneshot;
use tokio_stream::Stream;

/// Configurable behavior for the mock external processor.
#[derive(Clone)]
enum MockBehavior {
    /// Add a header to the response mutation.
    AddHeader { name: String, value: String },

    /// Return an `ImmediateResponse` rejection.
    ImmediateReject { status: i32, body: String },

    /// Return a response with no mutations.
    Noop,

    /// Never respond (for timeout testing).
    Hang,

    /// Return an unexpected `RequestBody` response type.
    UnexpectedBodyResponse,

    /// Return `ResponseHeaders` regardless of request phase.
    AlwaysResponseHeaders,
}

/// Mock implementation of the Envoy `ExternalProcessor` gRPC service.
struct MockProcessor {
    behavior: MockBehavior,
}

#[async_trait]
impl ExternalProcessor for MockProcessor {
    type ProcessStream = Pin<Box<dyn Stream<Item = Result<ProcessingResponse, tonic::Status>> + Send>>;

    async fn process(
        &self,
        request: tonic::Request<tonic::Streaming<ProcessingRequest>>,
    ) -> Result<tonic::Response<Self::ProcessStream>, tonic::Status> {
        let mut stream = request.into_inner();
        let msg = stream
            .message()
            .await?
            .ok_or_else(|| tonic::Status::internal("empty request stream"))?;

        let response = match &self.behavior {
            MockBehavior::Hang => {
                futures::future::pending::<()>().await;
                unreachable!("pending future should never resolve");
            },
            MockBehavior::Noop => build_noop_response(&msg),
            MockBehavior::AddHeader { name, value } => build_add_header_response(&msg, name, value),
            MockBehavior::ImmediateReject { status, body } => build_immediate_response(*status, body),
            MockBehavior::UnexpectedBodyResponse => build_unexpected_body_response(),
            MockBehavior::AlwaysResponseHeaders => build_always_response_headers(),
        };

        let output = futures::stream::once(async { Ok(response) });
        Ok(tonic::Response::new(Box::pin(output)))
    }
}

/// Build a response that echoes back the phase with no mutations.
fn build_noop_response(req: &ProcessingRequest) -> ProcessingResponse {
    let response = match &req.request {
        Some(processing_request::Request::RequestHeaders(_)) => {
            processing_response::Response::RequestHeaders(HeadersResponse { response: None })
        },
        Some(processing_request::Request::ResponseHeaders(_)) => {
            processing_response::Response::ResponseHeaders(HeadersResponse { response: None })
        },
        _ => processing_response::Response::RequestHeaders(HeadersResponse { response: None }),
    };
    ProcessingResponse {
        response: Some(response),
        ..Default::default()
    }
}

/// Build a response that adds a single header via [`HeaderMutation`].
fn build_add_header_response(req: &ProcessingRequest, name: &str, value: &str) -> ProcessingResponse {
    let mutation = Some(HeaderMutation {
        set_headers: vec![make_hvo(name, value)],
        remove_headers: vec![],
    });
    let common = Some(CommonResponse {
        status: 0,
        header_mutation: mutation,
        body_mutation: None,
        trailers: None,
        clear_route_cache: false,
    });
    let response = match &req.request {
        Some(processing_request::Request::ResponseHeaders(_)) => {
            processing_response::Response::ResponseHeaders(HeadersResponse { response: common })
        },
        _ => processing_response::Response::RequestHeaders(HeadersResponse { response: common }),
    };
    ProcessingResponse {
        response: Some(response),
        ..Default::default()
    }
}

/// Build an [`ImmediateResponse`] rejection.
fn build_immediate_response(status: i32, body: &str) -> ProcessingResponse {
    ProcessingResponse {
        response: Some(processing_response::Response::ImmediateResponse(ImmediateResponse {
            status: Some(HttpStatus { code: status }),
            headers: None,
            body: body.to_owned(),
            grpc_status: None,
            details: String::new(),
        })),
        ..Default::default()
    }
}

/// Build a `RequestBody` response to trigger the unexpected-type error path.
fn build_unexpected_body_response() -> ProcessingResponse {
    ProcessingResponse {
        response: Some(processing_response::Response::RequestBody(BodyResponse {
            response: None,
        })),
        ..Default::default()
    }
}

/// Build a `ResponseHeaders` response regardless of the request phase.
fn build_always_response_headers() -> ProcessingResponse {
    ProcessingResponse {
        response: Some(processing_response::Response::ResponseHeaders(HeadersResponse {
            response: None,
        })),
        ..Default::default()
    }
}

/// RAII guard that shuts down the mock gRPC server on drop.
struct MockServerGuard {
    shutdown: Option<oneshot::Sender<()>>,
}

impl Drop for MockServerGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Start a mock `ExternalProcessor` gRPC server on a random port.
///
/// Returns the listen address and an RAII guard that shuts down
/// the server when dropped.
async fn start_mock_processor(behavior: MockBehavior) -> (SocketAddr, MockServerGuard) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let svc = ExternalProcessorServer::new(MockProcessor { behavior });

    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming_shutdown(tokio_stream::wrappers::TcpListenerStream::new(listener), async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    wait_for_server(addr).await;

    let guard = MockServerGuard {
        shutdown: Some(shutdown_tx),
    };
    (addr, guard)
}

/// Poll until the server accepts a TCP connection.
async fn wait_for_server(addr: SocketAddr) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("mock server at {addr} did not become ready");
}

// -----------------------------------------------------------------------------
// Request Phase Tests
// -----------------------------------------------------------------------------

use praxis_proto::envoy::service::ext_proc::v3::{HttpHeaders, StreamedBodyResponse};
use tonic::transport::Endpoint;

use crate::request_phase::{self, DESTINATION_ENDPOINT_HEADER, RequestPhaseError};

/// Build minimal [`HttpHeaders`] for request-phase tests.
fn make_proto_headers() -> HttpHeaders {
    HttpHeaders {
        headers: Some(praxis_proto::envoy::service::ext_proc::v3::HeaderMap {
            headers: vec![
                HeaderValue {
                    key: ":method".to_owned(),
                    value: "POST".to_owned(),
                    raw_value: Vec::new(),
                },
                HeaderValue {
                    key: ":path".to_owned(),
                    value: "/v1/completions".to_owned(),
                    raw_value: Vec::new(),
                },
            ],
        }),
        end_of_stream: false,
    }
}

// -----------------------------------------------------------------------------
// Request Phase Mock (multi-message bidirectional stream)
// -----------------------------------------------------------------------------

/// Scenario-driven behavior for the request-phase mock processor.
#[derive(Clone)]
enum RequestPhaseScenario {
    /// Respond to headers with optional endpoint, then respond to
    /// body with optional mutation.
    Normal {
        endpoint: Option<String>,
        body_mutation: Option<Vec<u8>>,
    },

    /// Send `ImmediateResponse` on header request.
    ImmediateOnHeaders { status: i32, body: String },

    /// Respond normally to headers, send `ImmediateResponse` on body
    /// request.
    ImmediateOnBody { status: i32, body: String },

    /// Close stream without sending any response.
    EmptyStream,

    /// Respond to headers, then close stream without body response.
    CloseAfterHeaders { endpoint: Option<String> },

    /// Never respond (timeout testing).
    Hang,

    /// Send wrong response type (`ResponseHeaders`) for header
    /// request.
    WrongTypeOnHeaders,

    /// Respond normally to headers, send wrong response type
    /// (`ResponseHeaders`) for body request.
    WrongTypeOnBody,

    /// Body response with `ClearBody` mutation.
    ClearBody,

    /// Verify the mock receives the request body bytes.
    EchoBodyInMutation,

    /// Single `StreamedResponse` chunk with `end_of_stream=true`.
    StreamedSingleChunk { body: Vec<u8> },

    /// Multiple `StreamedResponse` chunks reassembled in order.
    StreamedMultiChunk { chunks: Vec<Vec<u8>> },

    /// Stream closes after streamed chunks without `end_of_stream`.
    StreamedCloseBeforeEos { chunks: Vec<Vec<u8>> },

    /// Headers with endpoint, then streamed body chunks.
    NormalStreamed { endpoint: String, chunks: Vec<Vec<u8>> },

    /// One streamed chunk, then `ImmediateResponse`.
    StreamedThenImmediate { chunk: Vec<u8>, status: i32, body: String },

    /// One streamed chunk without EOS, then a direct `Body` mutation.
    StreamedThenBody { chunk: Vec<u8>, replacement: Vec<u8> },

    /// One streamed chunk without EOS, then a `ClearBody` mutation.
    StreamedThenClearBody { chunk: Vec<u8> },

    /// One streamed chunk without EOS, then a no-mutation `RequestBody`.
    StreamedThenNoMutation { chunk: Vec<u8> },

    /// Normal response with extra trailing no-op responses after body.
    NormalWithTrailing { endpoint: String, trailing_count: usize },

    /// Normal response, then server keeps stream open (never closes).
    NormalThenHangStream { endpoint: String },
}

/// Multi-message mock implementing the full request-phase protocol.
struct RequestPhaseMock {
    /// Per-`Process` call counter (shared with test for cardinality assertions).
    call_count: Arc<AtomicUsize>,

    /// Scenario to execute.
    scenario: RequestPhaseScenario,
}

#[async_trait]
impl ExternalProcessor for RequestPhaseMock {
    type ProcessStream = Pin<Box<dyn Stream<Item = Result<ProcessingResponse, tonic::Status>> + Send>>;

    async fn process(
        &self,
        request: tonic::Request<tonic::Streaming<ProcessingRequest>>,
    ) -> Result<tonic::Response<Self::ProcessStream>, tonic::Status> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let req_stream = request.into_inner();
        let scenario = self.scenario.clone();

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ProcessingResponse, tonic::Status>>(4);

        tokio::spawn(async move {
            run_request_phase_scenario(scenario, req_stream, tx).await;
        });

        let output = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(tonic::Response::new(Box::pin(output)))
    }
}

/// Execute a request-phase scenario in the mock's background task.
#[allow(
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    reason = "dispatch match over test scenarios"
)]
async fn run_request_phase_scenario(
    scenario: RequestPhaseScenario,
    mut req_stream: tonic::Streaming<ProcessingRequest>,
    tx: tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
) {
    match scenario {
        RequestPhaseScenario::Hang => run_hang(&mut req_stream).await,
        RequestPhaseScenario::EmptyStream => {},
        RequestPhaseScenario::Normal {
            endpoint,
            body_mutation,
        } => run_normal(&mut req_stream, &tx, endpoint, body_mutation).await,
        RequestPhaseScenario::ImmediateOnHeaders { status, body } => {
            run_immediate_on_headers(&mut req_stream, &tx, status, &body).await;
        },
        RequestPhaseScenario::ImmediateOnBody { status, body } => {
            run_immediate_on_body(&mut req_stream, &tx, status, &body).await;
        },
        RequestPhaseScenario::CloseAfterHeaders { endpoint } => {
            run_close_after_headers(&mut req_stream, &tx, endpoint).await;
        },
        RequestPhaseScenario::WrongTypeOnHeaders => {
            run_wrong_type_on_headers(&mut req_stream, &tx).await;
        },
        RequestPhaseScenario::WrongTypeOnBody => {
            run_wrong_type_on_body(&mut req_stream, &tx).await;
        },
        RequestPhaseScenario::ClearBody => run_clear_body(&mut req_stream, &tx).await,
        RequestPhaseScenario::EchoBodyInMutation => run_echo_body(&mut req_stream, &tx).await,
        RequestPhaseScenario::NormalStreamed { endpoint, chunks } => {
            run_normal_streamed(&mut req_stream, &tx, endpoint, chunks).await;
        },
        RequestPhaseScenario::StreamedSingleChunk { body } => {
            run_streamed_single(&mut req_stream, &tx, body).await;
        },
        RequestPhaseScenario::StreamedMultiChunk { chunks } => {
            run_streamed_multi(&mut req_stream, &tx, chunks).await;
        },
        RequestPhaseScenario::StreamedCloseBeforeEos { chunks } => {
            run_streamed_close_before_eos(&mut req_stream, &tx, chunks).await;
        },
        RequestPhaseScenario::StreamedThenImmediate { chunk, status, body } => {
            run_streamed_then_immediate(&mut req_stream, &tx, chunk, status, &body).await;
        },
        RequestPhaseScenario::StreamedThenBody { chunk, replacement } => {
            run_streamed_then_body(&mut req_stream, &tx, chunk, replacement).await;
        },
        RequestPhaseScenario::StreamedThenClearBody { chunk } => {
            run_streamed_then_clear(&mut req_stream, &tx, chunk).await;
        },
        RequestPhaseScenario::StreamedThenNoMutation { chunk } => {
            run_streamed_then_no_mutation(&mut req_stream, &tx, chunk).await;
        },
        RequestPhaseScenario::NormalWithTrailing {
            endpoint,
            trailing_count,
        } => {
            run_normal_with_trailing(&mut req_stream, &tx, endpoint, trailing_count).await;
        },
        RequestPhaseScenario::NormalThenHangStream { endpoint } => {
            run_normal_then_hang(&mut req_stream, &tx, endpoint).await;
        },
    }
}

/// Hang scenario: read one message then block forever.
async fn run_hang(req_stream: &mut tonic::Streaming<ProcessingRequest>) {
    drop(req_stream.message().await);
    futures::future::pending::<()>().await;
}

/// Normal scenario: headers response then body response.
async fn run_normal(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
    endpoint: Option<String>,
    body_mutation: Option<Vec<u8>>,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_headers_phase_response(endpoint))).await);
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_body_phase_response(body_mutation))).await);
}

/// Immediate response on headers scenario.
async fn run_immediate_on_headers(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
    status: i32,
    body: &str,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_immediate_response(status, body))).await);
}

/// Immediate response on body scenario.
async fn run_immediate_on_body(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
    status: i32,
    body: &str,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_headers_phase_response(None))).await);
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_immediate_response(status, body))).await);
}

/// Close after headers scenario.
async fn run_close_after_headers(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
    endpoint: Option<String>,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_headers_phase_response(endpoint))).await);
}

/// Wrong type on headers scenario.
async fn run_wrong_type_on_headers(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_always_response_headers())).await);
}

/// Wrong type on body scenario.
async fn run_wrong_type_on_body(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_headers_phase_response(None))).await);
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_always_response_headers())).await);
}

/// Clear body scenario.
async fn run_clear_body(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_headers_phase_response(None))).await);
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_clear_body_response())).await);
}

/// Echo body scenario: returns the received body as a body mutation.
async fn run_echo_body(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_headers_phase_response(None))).await);

    if let Ok(Some(msg)) = req_stream.message().await
        && let Some(processing_request::Request::RequestBody(body)) = msg.request
    {
        drop(tx.send(Ok(build_body_phase_response(Some(body.body)))).await);
    }
}

/// Normal with endpoint + streamed body chunks.
async fn run_normal_streamed(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
    endpoint: String,
    chunks: Vec<Vec<u8>>,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_headers_phase_response(Some(endpoint)))).await);
    drop(req_stream.message().await);
    let last = chunks.len().saturating_sub(1);
    for (i, chunk) in chunks.into_iter().enumerate() {
        drop(tx.send(Ok(build_streamed_body_response(&chunk, i == last))).await);
    }
}

/// Single streamed chunk scenario.
async fn run_streamed_single(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
    body: Vec<u8>,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_headers_phase_response(None))).await);
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_streamed_body_response(&body, true))).await);
}

/// Multiple streamed chunks scenario.
async fn run_streamed_multi(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
    chunks: Vec<Vec<u8>>,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_headers_phase_response(None))).await);
    drop(req_stream.message().await);
    let last = chunks.len().saturating_sub(1);
    for (i, chunk) in chunks.into_iter().enumerate() {
        drop(tx.send(Ok(build_streamed_body_response(&chunk, i == last))).await);
    }
}

/// Streamed chunks then stream close without `end_of_stream`.
async fn run_streamed_close_before_eos(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
    chunks: Vec<Vec<u8>>,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_headers_phase_response(None))).await);
    drop(req_stream.message().await);
    for chunk in chunks {
        drop(tx.send(Ok(build_streamed_body_response(&chunk, false))).await);
    }
}

/// Streamed chunk then `ImmediateResponse`.
async fn run_streamed_then_immediate(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
    chunk: Vec<u8>,
    status: i32,
    body: &str,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_headers_phase_response(None))).await);
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_streamed_body_response(&chunk, false))).await);
    drop(tx.send(Ok(build_immediate_response(status, body))).await);
}

/// Streamed chunk then `Body` mutation.
async fn run_streamed_then_body(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
    chunk: Vec<u8>,
    replacement: Vec<u8>,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_headers_phase_response(None))).await);
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_streamed_body_response(&chunk, false))).await);
    drop(tx.send(Ok(build_body_phase_response(Some(replacement)))).await);
}

/// Streamed chunk then `ClearBody` mutation.
async fn run_streamed_then_clear(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
    chunk: Vec<u8>,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_headers_phase_response(None))).await);
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_streamed_body_response(&chunk, false))).await);
    drop(tx.send(Ok(build_clear_body_response())).await);
}

/// Streamed chunk then no-mutation `RequestBody`.
async fn run_streamed_then_no_mutation(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
    chunk: Vec<u8>,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_headers_phase_response(None))).await);
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_streamed_body_response(&chunk, false))).await);
    drop(tx.send(Ok(build_body_phase_response(None))).await);
}

/// Normal response then extra trailing no-op responses.
async fn run_normal_with_trailing(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
    endpoint: String,
    trailing_count: usize,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_headers_phase_response(Some(endpoint)))).await);
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_body_phase_response(None))).await);
    for _ in 0..trailing_count {
        drop(tx.send(Ok(build_body_phase_response(None))).await);
    }
}

/// Normal response then server keeps stream open indefinitely.
async fn run_normal_then_hang(
    req_stream: &mut tonic::Streaming<ProcessingRequest>,
    tx: &tokio::sync::mpsc::Sender<Result<ProcessingResponse, tonic::Status>>,
    endpoint: String,
) {
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_headers_phase_response(Some(endpoint)))).await);
    drop(req_stream.message().await);
    drop(tx.send(Ok(build_body_phase_response(None))).await);
    futures::future::pending::<()>().await;
}

/// Build a `RequestHeaders` response with optional endpoint header.
fn build_headers_phase_response(endpoint: Option<String>) -> ProcessingResponse {
    let mut set_headers = Vec::new();
    if let Some(ep) = endpoint {
        set_headers.push(make_hvo(DESTINATION_ENDPOINT_HEADER, &ep));
    }

    let common = if set_headers.is_empty() {
        None
    } else {
        Some(CommonResponse {
            status: 0,
            header_mutation: Some(HeaderMutation {
                set_headers,
                remove_headers: vec![],
            }),
            body_mutation: None,
            trailers: None,
            clear_route_cache: false,
        })
    };

    ProcessingResponse {
        response: Some(processing_response::Response::RequestHeaders(HeadersResponse {
            response: common,
        })),
        ..Default::default()
    }
}

/// Build a `RequestBody` response with optional body replacement.
fn build_body_phase_response(body_mutation: Option<Vec<u8>>) -> ProcessingResponse {
    let common = body_mutation.map(|data| CommonResponse {
        status: 0,
        header_mutation: None,
        body_mutation: Some(BodyMutation {
            mutation: Some(body_mutation::Mutation::Body(data)),
        }),
        trailers: None,
        clear_route_cache: false,
    });

    ProcessingResponse {
        response: Some(processing_response::Response::RequestBody(BodyResponse {
            response: common,
        })),
        ..Default::default()
    }
}

/// Build a `RequestBody` response with a `ClearBody` mutation.
fn build_clear_body_response() -> ProcessingResponse {
    ProcessingResponse {
        response: Some(processing_response::Response::RequestBody(BodyResponse {
            response: Some(CommonResponse {
                status: 0,
                header_mutation: None,
                body_mutation: Some(BodyMutation {
                    mutation: Some(body_mutation::Mutation::ClearBody(true)),
                }),
                trailers: None,
                clear_route_cache: false,
            }),
        })),
        ..Default::default()
    }
}

/// Build a `RequestBody` response with a `StreamedResponse` chunk.
fn build_streamed_body_response(body: &[u8], end_of_stream: bool) -> ProcessingResponse {
    ProcessingResponse {
        response: Some(processing_response::Response::RequestBody(BodyResponse {
            response: Some(CommonResponse {
                status: 0,
                header_mutation: None,
                body_mutation: Some(BodyMutation {
                    mutation: Some(body_mutation::Mutation::StreamedResponse(StreamedBodyResponse {
                        body: body.to_vec(),
                        end_of_stream,
                    })),
                }),
                trailers: None,
                clear_route_cache: false,
            }),
        })),
        ..Default::default()
    }
}

/// Start a request-phase mock server and return address, guard, and call counter.
async fn start_request_phase_mock(scenario: RequestPhaseScenario) -> (SocketAddr, MockServerGuard, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let call_count = Arc::new(AtomicUsize::new(0));

    let svc = ExternalProcessorServer::new(RequestPhaseMock {
        call_count: Arc::clone(&call_count),
        scenario,
    });

    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming_shutdown(tokio_stream::wrappers::TcpListenerStream::new(listener), async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    wait_for_server(addr).await;

    let guard = MockServerGuard {
        shutdown: Some(shutdown_tx),
    };
    (addr, guard, call_count)
}

// -----------------------------------------------------------------------------
// Request Phase: headers + body round trip
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_headers_then_body_succeeds() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::Normal {
        endpoint: Some("10.0.0.1:8080".to_owned()),
        body_mutation: None,
    })
    .await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from(r#"{"model":"llama","prompt":"hi"}"#);

    let result = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect("request phase should succeed");

    assert!(result.headers_response.is_some(), "should have a headers response");
    assert_eq!(
        result.selected_endpoint.as_deref(),
        Some("10.0.0.1:8080"),
        "should extract the selected endpoint"
    );
    assert!(result.body_response.is_some(), "should have a body response");
    assert!(result.mutated_body.is_none(), "no body mutation requested");
    assert!(result.immediate_response.is_none(), "no immediate response expected");
}

// -----------------------------------------------------------------------------
// Request Phase: body mutation replacement
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_body_mutation_replaces_body() {
    let replacement = b"replaced-body-content".to_vec();
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::Normal {
        endpoint: None,
        body_mutation: Some(replacement.clone()),
    })
    .await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("original");

    let result = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect("request phase should succeed");

    assert_eq!(
        result.mutated_body.as_deref(),
        Some(replacement.as_slice()),
        "should contain the replacement body"
    );
}

// -----------------------------------------------------------------------------
// Request Phase: body mutation echo (verifies body is received)
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_mock_receives_body_bytes() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::EchoBodyInMutation).await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("echo-this-payload");

    let result = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect("request phase should succeed");

    assert_eq!(
        result.mutated_body.as_deref(),
        Some(b"echo-this-payload".as_slice()),
        "mock should echo back the request body as body mutation"
    );
}

// -----------------------------------------------------------------------------
// Request Phase: clear body mutation
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_clear_body_produces_empty_bytes() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::ClearBody).await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("some-content");

    let result = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect("request phase should succeed");

    assert_eq!(
        result.mutated_body.as_deref(),
        Some(b"".as_slice()),
        "clear_body should produce empty bytes"
    );
}

// -----------------------------------------------------------------------------
// Request Phase: immediate response on headers
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_immediate_response_on_headers() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::ImmediateOnHeaders {
        status: 429,
        body: "rate limited".to_owned(),
    })
    .await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("payload");

    let result = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect("request phase should succeed");

    let imm = result.immediate_response.expect("should have immediate response");
    assert_eq!(
        imm.status.expect("should have status").code,
        429,
        "immediate response status should match"
    );
    assert_eq!(imm.body, "rate limited", "immediate response body should match");
    assert!(result.headers_response.is_none(), "no headers response when immediate");
    assert!(
        result.body_response.is_none(),
        "no body response when immediate on headers"
    );
}

// -----------------------------------------------------------------------------
// Request Phase: immediate response on body
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_immediate_response_on_body() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::ImmediateOnBody {
        status: 413,
        body: "too large".to_owned(),
    })
    .await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("huge-payload");

    let result = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect("request phase should succeed");

    assert!(
        result.headers_response.is_some(),
        "should have headers response before body immediate"
    );
    let imm = result.immediate_response.expect("should have immediate response");
    assert_eq!(
        imm.status.expect("should have status").code,
        413,
        "body immediate response status should match"
    );
}

// -----------------------------------------------------------------------------
// Request Phase: timeout
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_timeout_surfaces_error() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::Hang).await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("payload");

    let err = request_phase::process_request_phase(channel, headers, body, Duration::from_millis(50))
        .await
        .expect_err("should timeout");

    assert!(
        matches!(err, RequestPhaseError::Timeout),
        "error should be Timeout, got: {err}"
    );
}

// -----------------------------------------------------------------------------
// Request Phase: empty stream
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_empty_stream_surfaces_error() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::EmptyStream).await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("payload");

    let err = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect_err("should get empty stream error");

    assert!(
        matches!(err, RequestPhaseError::EmptyStream),
        "error should be EmptyStream, got: {err}"
    );
}

// -----------------------------------------------------------------------------
// Request Phase: unexpected response type on headers
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_unexpected_type_on_headers() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::WrongTypeOnHeaders).await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("payload");

    let err = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect_err("should get unexpected response error");

    match &err {
        RequestPhaseError::UnexpectedResponse(variant) => {
            assert_eq!(variant, "ResponseHeaders", "should name the unexpected variant");
        },
        _ => panic!("expected UnexpectedResponse, got: {err}"),
    }
}

// -----------------------------------------------------------------------------
// Request Phase: unexpected response type on body
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_unexpected_type_on_body() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::WrongTypeOnBody).await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("payload");

    let err = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect_err("should get unexpected response error");

    match &err {
        RequestPhaseError::UnexpectedResponse(variant) => {
            assert_eq!(variant, "ResponseHeaders", "should name the unexpected variant");
        },
        _ => panic!("expected UnexpectedResponse, got: {err}"),
    }
}

// -----------------------------------------------------------------------------
// Request Phase: missing endpoint is None
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_missing_endpoint_is_none() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::Normal {
        endpoint: None,
        body_mutation: None,
    })
    .await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("payload");

    let result = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect("request phase should succeed");

    assert!(
        result.selected_endpoint.is_none(),
        "selected_endpoint should be None when EPP does not set the header"
    );
}

// -----------------------------------------------------------------------------
// Request Phase: close after headers (no body response)
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_close_after_headers_succeeds() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::CloseAfterHeaders {
        endpoint: Some("10.0.0.2:9090".to_owned()),
    })
    .await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("payload");

    let result = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect("request phase should succeed even without body response");

    assert_eq!(
        result.selected_endpoint.as_deref(),
        Some("10.0.0.2:9090"),
        "endpoint should be extracted from header response"
    );
    assert!(
        result.body_response.is_none(),
        "no body response when stream closes after headers"
    );
    assert!(result.immediate_response.is_none(), "no immediate response");
}

// -----------------------------------------------------------------------------
// Request Phase: single StreamedResponse chunk
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_streamed_single_chunk() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::StreamedSingleChunk {
        body: b"streamed-body".to_vec(),
    })
    .await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("original");

    let result = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect("single streamed chunk should succeed");

    assert_eq!(
        result.mutated_body.as_deref(),
        Some(b"streamed-body".as_slice()),
        "should reassemble single streamed chunk"
    );
    assert!(result.body_response.is_some(), "should have a body response");
}

// -----------------------------------------------------------------------------
// Request Phase: multiple StreamedResponse chunks reassembled
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_streamed_multi_chunk_reassembly() {
    let chunks = vec![b"chunk-1-".to_vec(), b"chunk-2-".to_vec(), b"chunk-3".to_vec()];
    let (addr, _guard, _call_count) =
        start_request_phase_mock(RequestPhaseScenario::StreamedMultiChunk { chunks }).await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("original");

    let result = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect("multi-chunk streamed should succeed");

    assert_eq!(
        result.mutated_body.as_deref(),
        Some(b"chunk-1-chunk-2-chunk-3".as_slice()),
        "should reassemble chunks in order"
    );
}

// -----------------------------------------------------------------------------
// Request Phase: empty StreamedResponse body
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_streamed_empty_body() {
    let (addr, _guard, _call_count) =
        start_request_phase_mock(RequestPhaseScenario::StreamedSingleChunk { body: Vec::new() }).await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("original");

    let result = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect("empty streamed chunk should succeed");

    assert_eq!(
        result.mutated_body.as_deref(),
        Some(b"".as_slice()),
        "empty streamed body should produce empty bytes"
    );
}

// -----------------------------------------------------------------------------
// Request Phase: stream close before end_of_stream (fail-closed)
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_streamed_close_before_eos_returns_error() {
    let chunks = vec![b"partial-1-".to_vec(), b"partial-2".to_vec()];
    let (addr, _guard, _call_count) =
        start_request_phase_mock(RequestPhaseScenario::StreamedCloseBeforeEos { chunks }).await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("original");

    let err = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect_err("close before EOS should be an error");

    assert!(
        matches!(err, RequestPhaseError::IncompleteBodyStream(_)),
        "should be IncompleteBodyStream, got: {err}"
    );
}

// -----------------------------------------------------------------------------
// Request Phase: ImmediateResponse after streamed chunk
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_streamed_then_immediate() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::StreamedThenImmediate {
        chunk: b"partial-data".to_vec(),
        status: 503,
        body: "overloaded".to_owned(),
    })
    .await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("payload");

    let result = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect("streamed then immediate should succeed");

    let imm = result.immediate_response.expect("should have immediate response");
    assert_eq!(
        imm.status.expect("should have status").code,
        503,
        "immediate response status should match"
    );
    assert!(
        result.mutated_body.is_none(),
        "partial chunks should not be assembled when immediate response takes precedence"
    );
}

// -----------------------------------------------------------------------------
// Request Phase: streamed chunk then Body mutation (fail-closed)
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_streamed_then_body_returns_error() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::StreamedThenBody {
        chunk: b"partial".to_vec(),
        replacement: b"replaced".to_vec(),
    })
    .await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("original");

    let err = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect_err("streamed then Body should be an error");

    assert!(
        matches!(err, RequestPhaseError::IncompleteBodyStream(_)),
        "should be IncompleteBodyStream, got: {err}"
    );
}

// -----------------------------------------------------------------------------
// Request Phase: streamed chunk then ClearBody (fail-closed)
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_streamed_then_clear_body_returns_error() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::StreamedThenClearBody {
        chunk: b"partial".to_vec(),
    })
    .await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("original");

    let err = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect_err("streamed then ClearBody should be an error");

    assert!(
        matches!(err, RequestPhaseError::IncompleteBodyStream(_)),
        "should be IncompleteBodyStream, got: {err}"
    );
}

// -----------------------------------------------------------------------------
// Request Phase: streamed chunk then no-mutation RequestBody (fail-closed)
// -----------------------------------------------------------------------------

#[tokio::test]
async fn request_phase_streamed_then_no_mutation_returns_error() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::StreamedThenNoMutation {
        chunk: b"partial".to_vec(),
    })
    .await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("original");

    let err = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect_err("streamed then no-mutation should be an error");

    assert!(
        matches!(err, RequestPhaseError::IncompleteBodyStream(_)),
        "should be IncompleteBodyStream, got: {err}"
    );
}

// =============================================================================
// B02: llmd_external_epp filter tests
// =============================================================================

use praxis_filter::{BodyAccess, BodyMode};

use crate::llmd_external_epp::LlmdExternalEppFilter;

// -----------------------------------------------------------------------------
// llmd_external_epp: Config Parsing
// -----------------------------------------------------------------------------

#[tokio::test]
async fn epp_filter_parse_minimal_config() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(r#"target: "http://127.0.0.1:9002""#).unwrap();
    let filter = LlmdExternalEppFilter::from_config(&yaml).unwrap();
    assert_eq!(filter.name(), "llmd_external_epp", "filter name should match");
}

#[tokio::test]
async fn epp_filter_parse_full_config() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:9002"
request_timeout_ms: 10000
max_request_body_bytes: 8388608
status_on_error: 503
"#,
    )
    .unwrap();
    let filter = LlmdExternalEppFilter::from_config(&yaml).unwrap();
    assert_eq!(filter.name(), "llmd_external_epp", "filter name should match");
}

#[tokio::test]
async fn epp_filter_missing_target_errors() {
    let yaml: serde_yaml::Value = serde_yaml::from_str("request_timeout_ms: 5000").unwrap();
    let err = LlmdExternalEppFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("target"),
        "error should mention missing target: {err}"
    );
}

#[tokio::test]
async fn epp_filter_invalid_target_errors() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(r#"target: "not valid""#).unwrap();
    let err = LlmdExternalEppFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("invalid target URI"),
        "error should mention invalid URI: {err}"
    );
}

#[tokio::test]
async fn epp_filter_zero_timeout_errors() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:9002"
request_timeout_ms: 0
"#,
    )
    .unwrap();
    let err = LlmdExternalEppFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("request_timeout_ms"),
        "error should mention timeout: {err}"
    );
}

#[tokio::test]
async fn epp_filter_zero_max_body_errors() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:9002"
max_request_body_bytes: 0
"#,
    )
    .unwrap();
    let err = LlmdExternalEppFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("max_request_body_bytes"),
        "error should mention max body: {err}"
    );
}

#[tokio::test]
async fn epp_filter_invalid_status_on_error() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:9002"
status_on_error: 999
"#,
    )
    .unwrap();
    let err = LlmdExternalEppFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("status_on_error"),
        "error should mention status_on_error: {err}"
    );
}

#[tokio::test]
async fn epp_filter_unknown_field_errors() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:9002"
bogus: true
"#,
    )
    .unwrap();
    let err = LlmdExternalEppFilter::from_config(&yaml).err().expect("should error");
    assert!(
        err.to_string().contains("unknown field"),
        "error should mention unknown field: {err}"
    );
}

// -----------------------------------------------------------------------------
// llmd_external_epp: Body Access / Mode
// -----------------------------------------------------------------------------

#[tokio::test]
async fn epp_filter_requests_bounded_body_buffering() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:9002"
max_request_body_bytes: 1048576
"#,
    )
    .unwrap();
    let filter = LlmdExternalEppFilter::from_config(&yaml).unwrap();

    assert_eq!(
        filter.request_body_access(),
        BodyAccess::ReadWrite,
        "should request ReadWrite body access"
    );
    assert!(
        matches!(
            filter.request_body_mode(),
            BodyMode::StreamBuffer {
                max_bytes: Some(1_048_576)
            }
        ),
        "should request StreamBuffer with configured max_bytes"
    );
}

// -----------------------------------------------------------------------------
// llmd_external_epp: Fake EPP Integration
// -----------------------------------------------------------------------------

/// Build an [`LlmdExternalEppFilter`] connected to a mock server.
fn make_epp_filter(addr: SocketAddr) -> Box<dyn HttpFilter> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(&format!(r#"target: "http://{addr}""#)).unwrap();
    LlmdExternalEppFilter::from_config(&yaml).unwrap()
}

/// Build an [`LlmdExternalEppFilter`] with a short timeout.
fn make_epp_filter_with_timeout(addr: SocketAddr, timeout_ms: u64) -> Box<dyn HttpFilter> {
    let yaml: serde_yaml::Value =
        serde_yaml::from_str(&format!("target: \"http://{addr}\"\nrequest_timeout_ms: {timeout_ms}",)).unwrap();
    LlmdExternalEppFilter::from_config(&yaml).unwrap()
}

#[tokio::test]
async fn epp_filter_selected_endpoint_sets_upstream() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::Normal {
        endpoint: Some("10.0.0.1:8080".to_owned()),
        body_mutation: None,
    })
    .await;

    let filter = make_epp_filter(addr);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from(r#"{"model":"llama"}"#));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .expect("should succeed");

    assert!(matches!(action, FilterAction::Release), "should return Release");
    let upstream = ctx.upstream.expect("upstream should be set");
    assert_eq!(
        &*upstream.address, "10.0.0.1:8080",
        "upstream address should match EPP endpoint"
    );
    assert!(upstream.tls.is_none(), "TLS should be None for plain endpoint");
}

#[tokio::test]
async fn epp_filter_streamed_body_mutation_replaces_body() {
    let chunks = vec![b"chunk-a-".to_vec(), b"chunk-b".to_vec()];
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::NormalStreamed {
        endpoint: "10.0.0.2:8080".to_owned(),
        chunks,
    })
    .await;

    let filter = make_epp_filter(addr);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("original"));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .expect("should succeed");

    assert!(matches!(action, FilterAction::Release), "should return Release");
    assert_eq!(
        body.as_deref(),
        Some(b"chunk-a-chunk-b".as_slice()),
        "body should be replaced with reassembled streamed chunks"
    );
}

#[tokio::test]
async fn epp_filter_immediate_response_returns_reject() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::ImmediateOnHeaders {
        status: 429,
        body: "rate limited".to_owned(),
    })
    .await;

    let filter = make_epp_filter(addr);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("payload"));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .expect("should succeed");

    let rejection = match action {
        FilterAction::Reject(r) => r,
        other => panic!("expected Reject, got {other:?}"),
    };
    assert_eq!(rejection.status, 429, "rejection status should match");
    assert_eq!(
        rejection.body.unwrap(),
        Bytes::from("rate limited"),
        "rejection body should match"
    );
}

#[tokio::test]
async fn epp_filter_missing_endpoint_rejects_with_status_on_error() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::Normal {
        endpoint: None,
        body_mutation: None,
    })
    .await;

    let filter = make_epp_filter(addr);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("payload"));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .expect("should return Reject, not Err");

    let rejection = match action {
        FilterAction::Reject(r) => r,
        other => panic!("expected Reject, got {other:?}"),
    };
    assert_eq!(rejection.status, 500, "default status_on_error should be 500");
}

#[tokio::test]
async fn epp_filter_invalid_endpoint_rejects_with_status_on_error() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::Normal {
        endpoint: Some("no-port-here".to_owned()),
        body_mutation: None,
    })
    .await;

    let filter = make_epp_filter(addr);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("payload"));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .expect("should return Reject, not Err");

    let rejection = match action {
        FilterAction::Reject(r) => r,
        other => panic!("expected Reject, got {other:?}"),
    };
    assert_eq!(rejection.status, 500, "default status_on_error should be 500");
}

#[tokio::test]
async fn epp_filter_timeout_rejects_with_status_on_error() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::Hang).await;

    let filter = make_epp_filter_with_timeout(addr, 50);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("payload"));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .expect("should return Reject, not Err");

    let rejection = match action {
        FilterAction::Reject(r) => r,
        other => panic!("expected Reject, got {other:?}"),
    };
    assert_eq!(rejection.status, 500, "default status_on_error should be 500");
}

#[tokio::test]
async fn epp_filter_custom_status_on_error_for_timeout() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::Hang).await;

    let yaml: serde_yaml::Value = serde_yaml::from_str(&format!(
        "target: \"http://{addr}\"\nrequest_timeout_ms: 50\nstatus_on_error: 503",
    ))
    .unwrap();
    let filter = LlmdExternalEppFilter::from_config(&yaml).unwrap();

    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("payload"));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .expect("should return Reject, not Err");

    let rejection = match action {
        FilterAction::Reject(r) => r,
        other => panic!("expected Reject, got {other:?}"),
    };
    assert_eq!(rejection.status, 503, "custom status_on_error should be 503");
}

#[tokio::test]
async fn epp_filter_immediate_response_ignores_status_on_error() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::ImmediateOnHeaders {
        status: 429,
        body: "rate limited".to_owned(),
    })
    .await;

    let yaml: serde_yaml::Value =
        serde_yaml::from_str(&format!("target: \"http://{addr}\"\nstatus_on_error: 503",)).unwrap();
    let filter = LlmdExternalEppFilter::from_config(&yaml).unwrap();

    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("payload"));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .expect("should return Reject");

    let rejection = match action {
        FilterAction::Reject(r) => r,
        other => panic!("expected Reject, got {other:?}"),
    };
    assert_eq!(
        rejection.status, 429,
        "ImmediateResponse should use EPP status, not status_on_error"
    );
}

#[tokio::test]
async fn epp_filter_header_mutation_applied() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::Normal {
        endpoint: Some("10.0.0.1:8080".to_owned()),
        body_mutation: None,
    })
    .await;

    let filter = make_epp_filter(addr);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("payload"));

    let _action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .expect("should succeed");

    let has_endpoint_header = ctx
        .extra_request_headers
        .iter()
        .any(|(k, _)| k == "x-gateway-destination-endpoint");
    assert!(
        has_endpoint_header,
        "EPP-set endpoint header should be in extra_request_headers"
    );
}

#[tokio::test]
async fn epp_filter_not_eos_continues() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::Hang).await;

    let filter = make_epp_filter(addr);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("chunk"));

    let action = filter
        .on_request_body(&mut ctx, &mut body, false)
        .await
        .expect("should succeed");

    assert!(
        matches!(action, FilterAction::Continue),
        "non-EOS should return Continue"
    );
}

#[tokio::test]
async fn epp_filter_max_body_config_propagated() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:9002"
max_request_body_bytes: 2097152
"#,
    )
    .unwrap();
    let filter = LlmdExternalEppFilter::from_config(&yaml).unwrap();

    assert!(
        matches!(
            filter.request_body_mode(),
            BodyMode::StreamBuffer {
                max_bytes: Some(2_097_152)
            }
        ),
        "max_bytes should match configured value"
    );
}

// -----------------------------------------------------------------------------
// llmd_external_epp: Endpoint Validation
// -----------------------------------------------------------------------------

use crate::llmd_external_epp::validate_endpoint;

#[test]
fn endpoint_accepts_ipv4() {
    assert!(
        validate_endpoint(Some("10.0.0.1:8080")).is_ok(),
        "IPv4:port should be valid"
    );
}

#[test]
fn endpoint_accepts_dns() {
    assert!(
        validate_endpoint(Some("my-backend.svc.cluster.local:8080")).is_ok(),
        "DNS:port should be valid"
    );
}

#[test]
fn endpoint_accepts_bracketed_ipv6() {
    assert!(
        validate_endpoint(Some("[::1]:8080")).is_ok(),
        "bracketed IPv6:port should be valid"
    );
}

#[test]
fn endpoint_rejects_missing_port() {
    assert!(
        validate_endpoint(Some("10.0.0.1")).is_err(),
        "missing port should be rejected"
    );
}

#[test]
fn endpoint_rejects_non_numeric_port() {
    assert!(
        validate_endpoint(Some("10.0.0.1:not-a-port")).is_err(),
        "non-numeric port should be rejected"
    );
}

#[test]
fn endpoint_rejects_uri_with_scheme() {
    assert!(
        validate_endpoint(Some("http://backend:8080")).is_err(),
        "URI with scheme should be rejected"
    );
}

#[test]
fn endpoint_rejects_comma_separated() {
    assert!(
        validate_endpoint(Some("10.0.0.1:8080,10.0.0.2:8080")).is_err(),
        "comma-separated endpoints should be rejected"
    );
}

#[test]
fn endpoint_rejects_empty() {
    assert!(validate_endpoint(None).is_err(), "None endpoint should be rejected");
    assert!(
        validate_endpoint(Some("")).is_err(),
        "empty endpoint should be rejected"
    );
}

#[test]
fn endpoint_rejects_port_zero() {
    assert!(
        validate_endpoint(Some("10.0.0.1:0")).is_err(),
        "port 0 should be rejected"
    );
}

#[test]
fn endpoint_rejects_empty_host() {
    assert!(
        validate_endpoint(Some(":8080")).is_err(),
        "empty host should be rejected"
    );
}

#[test]
fn endpoint_rejects_whitespace() {
    assert!(
        validate_endpoint(Some("bad host:8080")).is_err(),
        "whitespace in endpoint should be rejected"
    );
}

#[test]
fn endpoint_rejects_unbracketed_ipv6() {
    assert!(
        validate_endpoint(Some("::1:8080")).is_err(),
        "unbracketed IPv6 should be rejected"
    );
}

#[test]
fn endpoint_rejects_empty_bracketed_ipv6() {
    assert!(
        validate_endpoint(Some("[]:8080")).is_err(),
        "empty brackets should be rejected"
    );
}

#[test]
fn endpoint_accepts_ipv6_full() {
    assert!(
        validate_endpoint(Some("[2001:db8::1]:8080")).is_ok(),
        "full bracketed IPv6:port should be valid"
    );
}

#[test]
fn endpoint_rejects_port_out_of_range() {
    assert!(
        validate_endpoint(Some("10.0.0.1:99999")).is_err(),
        "port >65535 should be rejected"
    );
}

// =============================================================================
// B04: Failure-mode hardening tests
// =============================================================================

#[tokio::test]
async fn epp_filter_errors_are_reject_not_filter_error() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::Normal {
        endpoint: None,
        body_mutation: None,
    })
    .await;

    let filter = make_epp_filter(addr);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("payload"));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .expect("should return Ok(Reject), not Err");

    assert!(
        matches!(action, FilterAction::Reject(_)),
        "EPP errors must produce Reject (fail-closed via filter action), not FilterError"
    );
}

#[tokio::test]
async fn epp_filter_immediate_response_preserves_body() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::ImmediateOnHeaders {
        status: 429,
        body: "retry after 60s".to_owned(),
    })
    .await;

    let filter = make_epp_filter(addr);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("payload"));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .expect("should succeed");

    let rejection = match action {
        FilterAction::Reject(r) => r,
        other => panic!("expected Reject, got {other:?}"),
    };
    assert_eq!(rejection.status, 429, "ImmediateResponse status preserved");
    assert_eq!(
        rejection.body.as_deref(),
        Some(b"retry after 60s".as_slice()),
        "ImmediateResponse body preserved"
    );
}

#[tokio::test]
async fn epp_filter_max_body_enforced_at_capability_level() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
target: "http://127.0.0.1:9002"
max_request_body_bytes: 100
"#,
    )
    .unwrap();
    let filter = LlmdExternalEppFilter::from_config(&yaml).unwrap();

    assert!(
        matches!(
            filter.request_body_mode(),
            BodyMode::StreamBuffer { max_bytes: Some(100) }
        ),
        "StreamBuffer max_bytes=100 enforces 413 at pipeline level before EPP is called"
    );
}

#[tokio::test]
async fn epp_filter_no_epp_call_before_eos() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::Hang).await;

    let filter = make_epp_filter(addr);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("chunk"));

    let action = filter
        .on_request_body(&mut ctx, &mut body, false)
        .await
        .expect("should succeed");

    assert!(
        matches!(action, FilterAction::Continue),
        "no EPP call should happen before end_of_stream"
    );
    assert!(ctx.upstream.is_none(), "upstream should not be set before EOS");
}

#[tokio::test]
async fn epp_filter_comma_endpoint_rejects() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::Normal {
        endpoint: Some("10.0.0.1:8080,10.0.0.2:8080".to_owned()),
        body_mutation: None,
    })
    .await;

    let filter = make_epp_filter(addr);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("payload"));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .expect("should return Reject");

    assert!(
        matches!(action, FilterAction::Reject(_)),
        "comma-separated endpoints should reject"
    );
}

// =============================================================================
// B04: EPP call cardinality
// =============================================================================

#[tokio::test]
async fn epp_filter_exactly_one_process_call_at_eos() {
    let (addr, _guard, call_count) = start_request_phase_mock(RequestPhaseScenario::Normal {
        endpoint: Some("10.0.0.1:8080".to_owned()),
        body_mutation: None,
    })
    .await;

    let filter = make_epp_filter(addr);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("payload"));

    let _action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .expect("should succeed");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "exactly one Process call should occur at EOS"
    );
}

#[tokio::test]
async fn epp_filter_zero_process_calls_before_eos() {
    let (addr, _guard, call_count) = start_request_phase_mock(RequestPhaseScenario::Hang).await;

    let filter = make_epp_filter(addr);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("chunk1"));

    let _action = filter
        .on_request_body(&mut ctx, &mut body, false)
        .await
        .expect("should succeed");

    let _action = filter
        .on_request_body(&mut ctx, &mut body, false)
        .await
        .expect("should succeed");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        0,
        "no Process call should occur before end_of_stream"
    );
}

#[tokio::test]
async fn epp_filter_no_retry_after_timeout() {
    let (addr, _guard, call_count) = start_request_phase_mock(RequestPhaseScenario::Hang).await;

    let filter = make_epp_filter_with_timeout(addr, 50);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("payload"));

    let _action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .expect("should return Reject");

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "timeout should not cause a retry — exactly one Process call"
    );
}

#[tokio::test]
async fn epp_filter_no_retry_after_epp_error() {
    let (addr, _guard, call_count) = start_request_phase_mock(RequestPhaseScenario::EmptyStream).await;

    let filter = make_epp_filter(addr);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("payload"));

    let _action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .expect("should return Reject");

    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "empty stream error should not cause a retry"
    );
}

// =============================================================================
// B04: Strengthened endpoint validation
// =============================================================================

#[test]
fn endpoint_rejects_not_ipv6_in_brackets() {
    assert!(
        validate_endpoint(Some("[not-ipv6]:8080")).is_err(),
        "non-IPv6 in brackets should be rejected"
    );
}

#[test]
fn endpoint_rejects_slash_in_host() {
    assert!(
        validate_endpoint(Some("bad/host:8080")).is_err(),
        "slash in host should be rejected"
    );
}

#[test]
fn endpoint_rejects_leading_dash_label() {
    assert!(
        validate_endpoint(Some("-bad.example.com:8080")).is_err(),
        "DNS label starting with dash should be rejected"
    );
}

#[test]
fn endpoint_rejects_trailing_dash_label() {
    assert!(
        validate_endpoint(Some("bad-.example.com:8080")).is_err(),
        "DNS label ending with dash should be rejected"
    );
}

#[test]
fn endpoint_rejects_empty_dns_label() {
    assert!(
        validate_endpoint(Some("bad..example.com:8080")).is_err(),
        "empty DNS label (double dot) should be rejected"
    );
}

#[test]
fn endpoint_accepts_hyphenated_dns() {
    assert!(
        validate_endpoint(Some("my-backend.svc.cluster.local:8080")).is_ok(),
        "valid hyphenated DNS should be accepted"
    );
}

#[test]
fn endpoint_accepts_single_label_dns() {
    assert!(
        validate_endpoint(Some("localhost:8080")).is_ok(),
        "single-label DNS should be accepted"
    );
}

#[test]
fn endpoint_rejects_label_over_63_bytes() {
    let long_label = "a".repeat(64);
    let endpoint = format!("{long_label}.example.com:8080");
    assert!(
        validate_endpoint(Some(&endpoint)).is_err(),
        "DNS label >63 bytes should be rejected"
    );
}

#[test]
fn endpoint_rejects_hostname_over_253_bytes() {
    let label = "a".repeat(50);
    let host = format!("{label}.{label}.{label}.{label}.{label}.x");
    let endpoint = format!("{host}:8080");
    assert!(
        validate_endpoint(Some(&endpoint)).is_err(),
        "hostname >253 bytes should be rejected"
    );
}

// =============================================================================
// B04: Server-observed response-stream cancellation on timeout
// =============================================================================

/// Mock that holds the response channel open and signals when the
/// client drops the response receiver (proving the gRPC exchange
/// was abandoned after timeout).
///
/// The inbound request half-stream closes naturally after
/// `stream::iter` delivers its messages — that is NOT cancellation
/// evidence. The response `Sender::closed()` future resolves only
/// when the client drops its end of the response stream, which
/// happens when `process_request_phase` times out and drops the
/// tonic `Streaming`.
struct ResponseCancellationMock {
    /// Notified when the response channel receiver is dropped.
    response_abandoned: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ExternalProcessor for ResponseCancellationMock {
    type ProcessStream = Pin<Box<dyn Stream<Item = Result<ProcessingResponse, tonic::Status>> + Send>>;

    async fn process(
        &self,
        _request: tonic::Request<tonic::Streaming<ProcessingRequest>>,
    ) -> Result<tonic::Response<Self::ProcessStream>, tonic::Status> {
        let abandoned = Arc::clone(&self.response_abandoned);

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ProcessingResponse, tonic::Status>>(1);

        tokio::spawn(async move {
            tx.closed().await;
            abandoned.notify_one();
        });

        let output = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(tonic::Response::new(Box::pin(output)))
    }
}

/// Start a response-cancellation mock.
async fn start_response_cancellation_mock() -> (SocketAddr, MockServerGuard, Arc<tokio::sync::Notify>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let response_abandoned = Arc::new(tokio::sync::Notify::new());

    let svc = ExternalProcessorServer::new(ResponseCancellationMock {
        response_abandoned: Arc::clone(&response_abandoned),
    });
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming_shutdown(tokio_stream::wrappers::TcpListenerStream::new(listener), async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    wait_for_server(addr).await;

    let guard = MockServerGuard {
        shutdown: Some(shutdown_tx),
    };
    (addr, guard, response_abandoned)
}

#[tokio::test]
async fn epp_timeout_causes_server_observed_response_stream_cancellation() {
    let (addr, _guard, response_abandoned) = start_response_cancellation_mock().await;

    let filter = make_epp_filter_with_timeout(addr, 100);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("payload"));

    let action = filter
        .on_request_body(&mut ctx, &mut body, true)
        .await
        .expect("should return Reject on timeout");

    assert!(
        matches!(action, FilterAction::Reject(_)),
        "timeout should produce Reject"
    );

    let abandoned = tokio::time::timeout(Duration::from_secs(2), response_abandoned.notified()).await;
    assert!(
        abandoned.is_ok(),
        "server should observe response-stream receiver drop within 2s after client timeout"
    );
}

// =============================================================================
// B04: failure_mode=open pipeline test
// =============================================================================

/// Build a pipeline with `llmd_external_epp` and `failure_mode: open`.
fn build_epp_pipeline(addr: SocketAddr) -> praxis_filter::FilterPipeline {
    let yaml_str = format!(
        r#"
- filter: llmd_external_epp
  failure_mode: open
  target: "http://{addr}"
  request_timeout_ms: 5000
"#,
    );
    let mut entries: Vec<praxis_filter::FilterEntry> = serde_yaml::from_str(&yaml_str).unwrap();
    let mut registry = praxis_filter::FilterRegistry::with_builtins();
    registry
        .register(
            "llmd_external_epp",
            praxis_filter::http_builtin(LlmdExternalEppFilter::from_config),
        )
        .unwrap();
    praxis_filter::FilterPipeline::build(&mut entries, &registry).unwrap()
}

#[tokio::test]
async fn epp_filter_reject_survives_failure_mode_open_pipeline() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::EmptyStream).await;

    let pipeline = build_epp_pipeline(addr);
    let req = make_request(Method::POST, "/v1/completions");
    let mut ctx = make_ctx(&req);
    let mut body = Some(Bytes::from("payload"));

    let action = pipeline.execute_http_request_body(&mut ctx, &mut body, true).await;

    let rejection = match action {
        Ok(FilterAction::Reject(r)) => r,
        other => panic!("expected Ok(Reject) even with failure_mode: open, got {other:?}"),
    };
    assert_eq!(
        rejection.status, 500,
        "default status_on_error should be 500 even with failure_mode: open"
    );
}

// =============================================================================
// B06: Stream drain tests
// =============================================================================

#[tokio::test]
async fn request_phase_drains_trailing_responses() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::NormalWithTrailing {
        endpoint: "10.0.0.1:8080".to_owned(),
        trailing_count: 3,
    })
    .await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("payload");

    let result = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect("should succeed despite trailing responses");

    assert_eq!(
        result.selected_endpoint.as_deref(),
        Some("10.0.0.1:8080"),
        "endpoint should be extracted normally"
    );
}

#[tokio::test]
async fn request_phase_drain_timeout_does_not_hang() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::NormalThenHangStream {
        endpoint: "10.0.0.1:8080".to_owned(),
    })
    .await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("payload");

    let start = Instant::now();
    let result = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect("should succeed — drain timeout prevents hang");

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "should complete quickly (drain timeout is 5ms), took {elapsed:?}"
    );
    assert_eq!(
        result.selected_endpoint.as_deref(),
        Some("10.0.0.1:8080"),
        "endpoint should be extracted before drain"
    );
}

#[tokio::test]
async fn request_phase_normal_still_succeeds_with_drain() {
    let (addr, _guard, _call_count) = start_request_phase_mock(RequestPhaseScenario::Normal {
        endpoint: Some("10.0.0.1:8080".to_owned()),
        body_mutation: None,
    })
    .await;

    let channel = connect_channel(addr).await;
    let headers = make_proto_headers();
    let body = Bytes::from("payload");

    let result = request_phase::process_request_phase(channel, headers, body, Duration::from_secs(5))
        .await
        .expect("normal flow should still succeed with drain");

    assert_eq!(
        result.selected_endpoint.as_deref(),
        Some("10.0.0.1:8080"),
        "endpoint should be extracted"
    );
}
