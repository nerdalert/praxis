// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Transport-agnostic HTTP request/response metadata and per-request filter context.

use std::{borrow::Cow, collections::HashMap, net::IpAddr, sync::Arc, time::Instant};

use http::{HeaderMap, Method, StatusCode, Uri};
use praxis_core::{connectivity::Upstream, health::HealthRegistry};

// -----------------------------------------------------------------------------
// HttpFilterContext
// -----------------------------------------------------------------------------

/// Per-request mutable state shared across all HTTP filters.
///
/// Created by the protocol layer for each incoming request. Filters read
/// and mutate it to select clusters, choose upstreams, and inject headers.
pub struct HttpFilterContext<'a> {
    /// Downstream client IP address (from the TCP connection).
    pub client_addr: Option<IpAddr>,

    /// The cluster name selected by the router filter.
    pub cluster: Option<Arc<str>>,

    /// Per-request metadata set by filters for downstream consumption.
    ///
    /// Filters use this bag to pass validated identity, subscription,
    /// model, and other descriptors to later filters in the pipeline.
    /// For example, an auth filter stores `"user"` and `"subscription"`,
    /// and a rate-limit filter reads them to build a composite key.
    ///
    /// Values are strings. Filters write with [`set_metadata`] and
    /// read with [`metadata`].
    ///
    /// [`set_metadata`]: HttpFilterContext::set_metadata
    /// [`metadata`]: HttpFilterContext::metadata
    pub filter_metadata: HashMap<String, String>,

    /// Extra headers to inject into the upstream request.
    pub extra_request_headers: Vec<(Cow<'static, str>, String)>,

    /// Header names to remove from the upstream request.
    pub remove_request_headers: Vec<Cow<'static, str>>,

    /// Shared health registry for endpoint health lookups.
    pub health_registry: Option<&'a HealthRegistry>,

    /// Transport-agnostic request headers, URI, and method.
    pub request: &'a Request,

    /// Accumulated request body bytes seen so far.
    pub request_body_bytes: u64,

    /// When the request was received; available in all phases.
    pub request_start: Instant,

    /// Accumulated response body bytes seen so far.
    pub response_body_bytes: u64,

    /// The upstream response headers, available during `on_response`.
    /// `None` during the request phase.
    pub response_header: Option<&'a mut Response>,

    /// Whether any filter modified the response headers during
    /// `on_response`. Used to skip unnecessary work.
    pub response_headers_modified: bool,

    /// Rewritten URI path for the upstream request.
    ///
    /// Set by the `path_rewrite` or `url_rewrite` filter during
    /// `on_request`. Applied to the upstream [`RequestHeader`] in the
    /// protocol layer.
    ///
    /// The router checks this field before the original request URI.
    /// If a preceding filter sets `rewritten_path`, the router
    /// matches against it, enabling "rewrite then route" pipelines.
    ///
    /// If both `path_rewrite` and `url_rewrite` appear in the same
    /// pipeline, only the last writer's value takes effect.
    /// Pipeline validation rejects this by default; set
    /// `allow_rewrite_override: true` on the later filter to
    /// permit it. Or, better yet, don't.
    ///
    /// [`RequestHeader`]: pingora_http::RequestHeader
    pub rewritten_path: Option<String>,

    /// The upstream peer selected by the load balancer filter.
    pub upstream: Option<Upstream>,
}

impl HttpFilterContext<'_> {
    /// Selected cluster name, if any.
    pub fn cluster_name(&self) -> Option<&str> {
        self.cluster.as_deref()
    }

    /// Upstream peer address, if selected.
    pub fn upstream_addr(&self) -> Option<&str> {
        self.upstream.as_ref().map(|u| &*u.address)
    }

    /// Retrieve a metadata value set by a preceding filter.
    pub fn metadata(&self, key: &str) -> Option<&str> {
        self.filter_metadata.get(key).map(|v| v.as_str())
    }

    /// X-Request-ID header value, if present and valid UTF-8.
    pub fn request_id(&self) -> Option<&str> {
        self.request.headers.get("x-request-id").and_then(|v| v.to_str().ok())
    }

    /// Store a metadata value for consumption by later filters.
    pub fn set_metadata(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.filter_metadata.insert(key.into(), value.into());
    }
}

// -----------------------------------------------------------------------------
// Request
// -----------------------------------------------------------------------------

/// HTTP request metadata.
///
/// ```
/// use http::{HeaderMap, Method, Uri};
/// use praxis_filter::Request;
///
/// let req = Request {
///     method: Method::GET,
///     uri: Uri::from_static("/api/users"),
///     headers: HeaderMap::new(),
/// };
/// assert_eq!(req.uri.path(), "/api/users");
/// ```
#[derive(Debug, Clone)]
pub struct Request {
    /// HTTP header map.
    pub headers: HeaderMap,

    /// HTTP method.
    pub method: Method,

    /// Request URI.
    pub uri: Uri,
}

// -----------------------------------------------------------------------------
// Response
// -----------------------------------------------------------------------------

/// HTTP response metadata.
///
/// ```
/// use http::{HeaderMap, StatusCode};
/// use praxis_filter::Response;
///
/// let mut resp = Response {
///     status: StatusCode::OK,
///     headers: HeaderMap::new(),
/// };
/// resp.headers.insert("x-custom", "value".parse().unwrap());
/// assert_eq!(resp.status, StatusCode::OK);
/// ```
#[derive(Debug)]
pub struct Response {
    /// HTTP header map.
    pub headers: HeaderMap,

    /// HTTP status code.
    pub status: StatusCode,
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_fields_are_accessible() {
        let req = Request {
            method: Method::POST,
            uri: "/submit".parse().unwrap(),
            headers: HeaderMap::new(),
        };
        assert_eq!(req.method, Method::POST);
        assert_eq!(req.uri.path(), "/submit");
        assert!(req.headers.is_empty(), "new request should have no headers");
    }

    #[test]
    fn response_header_mutation() {
        let mut resp = Response {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        };
        resp.headers.insert("x-powered-by", "praxis".parse().unwrap());
        assert_eq!(resp.headers["x-powered-by"], "praxis");
    }

    #[test]
    fn response_status_codes() {
        for code in [200u16, 404, 500] {
            let resp = Response {
                status: StatusCode::from_u16(code).unwrap(),
                headers: HeaderMap::new(),
            };
            assert_eq!(resp.status.as_u16(), code);
        }
    }

    #[test]
    fn cluster_name_returns_none_when_unset() {
        let req = crate::test_utils::make_request(Method::GET, "/");
        let ctx = crate::test_utils::make_filter_context(&req);
        assert!(ctx.cluster_name().is_none(), "cluster name should be None when unset");
    }

    #[test]
    fn cluster_name_returns_value_when_set() {
        let req = crate::test_utils::make_request(Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.cluster = Some(Arc::from("backend"));
        assert_eq!(
            ctx.cluster_name(),
            Some("backend"),
            "cluster name should return set value"
        );
    }

    #[test]
    fn upstream_addr_returns_none_when_unset() {
        let req = crate::test_utils::make_request(Method::GET, "/");
        let ctx = crate::test_utils::make_filter_context(&req);
        assert!(ctx.upstream_addr().is_none(), "upstream addr should be None when unset");
    }

    #[test]
    fn upstream_addr_returns_value_when_set() {
        let req = crate::test_utils::make_request(Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.upstream = Some(Upstream {
            address: Arc::from("10.0.0.1:8080"),
            tls: None,
            connection: Arc::new(praxis_core::connectivity::ConnectionOptions::default()),
        });
        assert_eq!(
            ctx.upstream_addr(),
            Some("10.0.0.1:8080"),
            "upstream addr should return set address"
        );
    }

    #[test]
    fn metadata_returns_none_when_absent() {
        let req = crate::test_utils::make_request(Method::GET, "/");
        let ctx = crate::test_utils::make_filter_context(&req);
        assert!(ctx.metadata("user").is_none(), "missing key should return None");
    }

    #[test]
    fn set_metadata_and_retrieve() {
        let req = crate::test_utils::make_request(Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.set_metadata("user", "alice");
        ctx.set_metadata("subscription", "gpt-4o-sub");
        assert_eq!(ctx.metadata("user"), Some("alice"));
        assert_eq!(ctx.metadata("subscription"), Some("gpt-4o-sub"));
    }

    #[test]
    fn set_metadata_overwrites_existing() {
        let req = crate::test_utils::make_request(Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.set_metadata("model", "qwen");
        ctx.set_metadata("model", "mistral");
        assert_eq!(ctx.metadata("model"), Some("mistral"));
    }

    #[test]
    fn request_id_returns_none_when_absent() {
        let req = crate::test_utils::make_request(Method::GET, "/");
        let ctx = crate::test_utils::make_filter_context(&req);
        assert!(
            ctx.request_id().is_none(),
            "request ID should be None when header absent"
        );
    }

    #[test]
    fn request_id_returns_value_when_present() {
        let mut req = crate::test_utils::make_request(Method::GET, "/");
        req.headers.insert("x-request-id", "abc-123".parse().unwrap());
        let ctx = crate::test_utils::make_filter_context(&req);
        assert_eq!(
            ctx.request_id(),
            Some("abc-123"),
            "request ID should return header value"
        );
    }
}
