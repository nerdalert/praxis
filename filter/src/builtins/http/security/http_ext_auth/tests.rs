// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Tests for the HTTP external auth filter.

use std::io::Write;

use super::*;

// -----------------------------------------------------------------------------
// Config Parsing Tests
// -----------------------------------------------------------------------------

#[test]
fn from_config_parses_minimal() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
endpoint: "http://auth-svc:8080/validate"
"#,
    )
    .unwrap();
    let filter = HttpExtAuthFilter::from_config(&yaml).unwrap();
    assert_eq!(filter.name(), "http_ext_auth");
}

#[test]
fn from_config_parses_full() {
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
endpoint: "http://maas-api:8080/internal/v1/api-keys/validate"
timeout_ms: 1000
response:
  metadata:
    user: userId
    subscription: subscription
  upstream_headers:
    x-maas-subscription: subscription
strip:
  request_headers:
    - authorization
"#,
    )
    .unwrap();
    let filter = HttpExtAuthFilter::from_config(&yaml).unwrap();
    assert_eq!(filter.name(), "http_ext_auth");
}

#[test]
fn from_config_rejects_empty_endpoint() {
    let yaml: serde_yaml::Value = serde_yaml::from_str("endpoint: \"\"").unwrap();
    assert!(HttpExtAuthFilter::from_config(&yaml).is_err());
}

#[test]
fn from_config_rejects_zero_timeout() {
    let yaml: serde_yaml::Value = serde_yaml::from_str("endpoint: \"http://a:8080/v\"\ntimeout_ms: 0").unwrap();
    assert!(HttpExtAuthFilter::from_config(&yaml).is_err());
}

// -----------------------------------------------------------------------------
// Token Extraction Tests
// -----------------------------------------------------------------------------

#[tokio::test]
async fn missing_auth_header_rejects_401() {
    let filter = make_filter("http://unused:8080/v");
    let req = crate::test_utils::make_request(http::Method::GET, "/");
    let mut ctx = crate::test_utils::make_filter_context(&req);

    let result = filter.on_request(&mut ctx).await.unwrap();
    assert!(matches!(result, FilterAction::Reject(r) if r.status == 401));
}

#[tokio::test]
async fn empty_bearer_rejects_401() {
    let filter = make_filter("http://unused:8080/v");
    let mut req = crate::test_utils::make_request(http::Method::GET, "/");
    req.headers.insert("authorization", "Bearer ".parse().unwrap());
    let mut ctx = crate::test_utils::make_filter_context(&req);

    let result = filter.on_request(&mut ctx).await.unwrap();
    assert!(matches!(result, FilterAction::Reject(r) if r.status == 401));
}

#[tokio::test]
async fn non_bearer_auth_rejects_401() {
    let filter = make_filter("http://unused:8080/v");
    let mut req = crate::test_utils::make_request(http::Method::GET, "/");
    req.headers
        .insert("authorization", "Basic dXNlcjpwYXNz".parse().unwrap());
    let mut ctx = crate::test_utils::make_filter_context(&req);

    let result = filter.on_request(&mut ctx).await.unwrap();
    assert!(matches!(result, FilterAction::Reject(r) if r.status == 401));
}

#[test]
fn extract_bearer_token_valid() {
    let mut req = crate::test_utils::make_request(http::Method::GET, "/");
    req.headers
        .insert("authorization", "Bearer sk-oai-test123".parse().unwrap());
    let ctx = crate::test_utils::make_filter_context(&req);
    assert_eq!(extract_bearer_token(&ctx), Some("sk-oai-test123"));
}

#[test]
fn extract_bearer_token_missing() {
    let req = crate::test_utils::make_request(http::Method::GET, "/");
    let ctx = crate::test_utils::make_filter_context(&req);
    assert_eq!(extract_bearer_token(&ctx), None);
}

#[test]
fn extract_bearer_token_empty() {
    let mut req = crate::test_utils::make_request(http::Method::GET, "/");
    req.headers.insert("authorization", "Bearer ".parse().unwrap());
    let ctx = crate::test_utils::make_filter_context(&req);
    assert_eq!(extract_bearer_token(&ctx), None);
}

// -----------------------------------------------------------------------------
// Mock Server HTTP Callout Tests
// -----------------------------------------------------------------------------

#[tokio::test]
async fn valid_true_allows_and_injects_metadata() {
    let (addr, _guard) = start_mock_server(200, r#"{"valid":true,"userId":"alice","subscription":"sub-1"}"#).await;
    let filter = make_filter_with_metadata(&format!("http://{addr}/validate"));

    let mut req = crate::test_utils::make_request(http::Method::POST, "/");
    req.headers
        .insert("authorization", "Bearer sk-oai-good".parse().unwrap());
    let mut ctx = crate::test_utils::make_filter_context(&req);

    let result = filter.on_request(&mut ctx).await.unwrap();
    assert!(matches!(result, FilterAction::Continue));
    assert_eq!(ctx.metadata("user"), Some("alice"));
    assert_eq!(ctx.metadata("subscription"), Some("sub-1"));
}

#[tokio::test]
async fn valid_false_rejects_403() {
    let (addr, _guard) = start_mock_server(200, r#"{"valid":false}"#).await;
    let filter = make_filter(&format!("http://{addr}/validate"));

    let mut req = crate::test_utils::make_request(http::Method::POST, "/");
    req.headers
        .insert("authorization", "Bearer sk-oai-bad".parse().unwrap());
    let mut ctx = crate::test_utils::make_filter_context(&req);

    let result = filter.on_request(&mut ctx).await.unwrap();
    assert!(matches!(result, FilterAction::Reject(r) if r.status == 403));
}

#[tokio::test]
async fn missing_valid_field_returns_error() {
    let (addr, _guard) = start_mock_server(200, r#"{"userId":"alice"}"#).await;
    let filter = make_filter(&format!("http://{addr}/validate"));

    let mut req = crate::test_utils::make_request(http::Method::POST, "/");
    req.headers
        .insert("authorization", "Bearer sk-oai-nofield".parse().unwrap());
    let mut ctx = crate::test_utils::make_filter_context(&req);

    let result = filter.on_request(&mut ctx).await;
    assert!(result.is_err(), "missing valid field should return error");
}

#[tokio::test]
async fn auth_401_rejects() {
    let (addr, _guard) = start_mock_server(401, r#"{"error":"unauthorized"}"#).await;
    let filter = make_filter(&format!("http://{addr}/validate"));

    let mut req = crate::test_utils::make_request(http::Method::POST, "/");
    req.headers
        .insert("authorization", "Bearer sk-oai-expired".parse().unwrap());
    let mut ctx = crate::test_utils::make_filter_context(&req);

    let result = filter.on_request(&mut ctx).await.unwrap();
    assert!(matches!(result, FilterAction::Reject(r) if r.status == 401));
}

#[tokio::test]
async fn auth_403_rejects() {
    let (addr, _guard) = start_mock_server(403, r#"{"error":"forbidden"}"#).await;
    let filter = make_filter(&format!("http://{addr}/validate"));

    let mut req = crate::test_utils::make_request(http::Method::POST, "/");
    req.headers
        .insert("authorization", "Bearer sk-oai-noaccess".parse().unwrap());
    let mut ctx = crate::test_utils::make_filter_context(&req);

    let result = filter.on_request(&mut ctx).await.unwrap();
    assert!(matches!(result, FilterAction::Reject(r) if r.status == 403));
}

#[tokio::test]
async fn callout_error_returns_err() {
    let filter = make_filter("http://127.0.0.1:1/will-not-connect");

    let mut req = crate::test_utils::make_request(http::Method::POST, "/");
    req.headers
        .insert("authorization", "Bearer sk-oai-test".parse().unwrap());
    let mut ctx = crate::test_utils::make_filter_context(&req);

    let result = filter.on_request(&mut ctx).await;
    assert!(result.is_err(), "connection failure should return error");
}

#[tokio::test]
async fn upstream_headers_injected() {
    let (addr, _guard) = start_mock_server(200, r#"{"valid":true,"subscription":"premium-sub"}"#).await;

    let yaml: serde_yaml::Value = serde_yaml::from_str(&format!(
        r#"
endpoint: "http://{addr}/validate"
response:
  upstream_headers:
    x-maas-subscription: subscription
"#
    ))
    .unwrap();
    let filter = HttpExtAuthFilter::from_config(&yaml).unwrap();

    let mut req = crate::test_utils::make_request(http::Method::POST, "/");
    req.headers
        .insert("authorization", "Bearer sk-oai-test".parse().unwrap());
    let mut ctx = crate::test_utils::make_filter_context(&req);

    let result = filter.on_request(&mut ctx).await.unwrap();
    assert!(matches!(result, FilterAction::Continue));
    assert!(
        ctx.extra_request_headers
            .iter()
            .any(|(n, v)| n.as_ref() == "x-maas-subscription" && v == "premium-sub"),
        "upstream header should be injected"
    );
}

#[tokio::test]
async fn strip_headers_applied() {
    let (addr, _guard) = start_mock_server(200, r#"{"valid":true}"#).await;

    let yaml: serde_yaml::Value = serde_yaml::from_str(&format!(
        r#"
endpoint: "http://{addr}/validate"
strip:
  request_headers:
    - authorization
    - x-internal
"#
    ))
    .unwrap();
    let filter = HttpExtAuthFilter::from_config(&yaml).unwrap();

    let mut req = crate::test_utils::make_request(http::Method::POST, "/");
    req.headers
        .insert("authorization", "Bearer sk-oai-test".parse().unwrap());
    let mut ctx = crate::test_utils::make_filter_context(&req);

    let result = filter.on_request(&mut ctx).await.unwrap();
    assert!(matches!(result, FilterAction::Continue));
    assert_eq!(ctx.remove_request_headers.len(), 2);
    assert!(ctx.remove_request_headers.iter().any(|h| h.as_ref() == "authorization"));
    assert!(ctx.remove_request_headers.iter().any(|h| h.as_ref() == "x-internal"));
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

/// Create a minimal filter for testing.
fn make_filter(endpoint: &str) -> Box<dyn HttpFilter> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(&format!("endpoint: \"{endpoint}\"")).unwrap();
    HttpExtAuthFilter::from_config(&yaml).unwrap()
}

/// Create a filter with metadata mapping for testing.
fn make_filter_with_metadata(endpoint: &str) -> Box<dyn HttpFilter> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(&format!(
        r#"
endpoint: "{endpoint}"
response:
  metadata:
    user: userId
    subscription: subscription
"#
    ))
    .unwrap();
    HttpExtAuthFilter::from_config(&yaml).unwrap()
}

/// Guard that shuts down the mock server when dropped.
#[allow(dead_code, reason = "sender drop triggers shutdown")]
struct MockGuard(tokio::sync::oneshot::Sender<()>);

/// Start a mock HTTP server that returns a fixed status and body.
async fn start_mock_server(status: u16, body: &str) -> (std::net::SocketAddr, MockGuard) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    let body = body.to_owned();
    let status_line = format!("HTTP/1.1 {status} OK");

    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        loop {
            tokio::select! {
                accept = listener.accept() => {
                    if let Ok((stream, _)) = accept {
                        let mut std_stream = stream.into_std().unwrap();
                        std_stream.set_nonblocking(false).unwrap();
                        let mut buf = [0u8; 4096];
                        drop(std::io::Read::read(&mut std_stream, &mut buf));
                        let response = format!(
                            "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body,
                        );
                        drop(std_stream.write_all(response.as_bytes()));
                        drop(std_stream.flush());
                    }
                }
                _ = &mut rx => break,
            }
        }
    });

    (addr, MockGuard(tx))
}
