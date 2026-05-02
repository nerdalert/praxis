// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Prometheus metrics: recorder installation, HTTP request metric recording, and scrape rendering.

use std::sync::OnceLock;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

// -----------------------------------------------------------------------------
// Recorder Installation
// -----------------------------------------------------------------------------

/// Global handle to the Prometheus exporter.
static PROMETHEUS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the global Prometheus metrics recorder.
///
/// Must be called exactly once during server startup. Subsequent
/// calls are no-ops and return the existing handle.
///
/// # Panics
///
/// Panics if the global recorder cannot be installed (another
/// recorder was already set by a different subsystem).
pub fn install_prometheus_recorder() -> &'static PrometheusHandle {
    #[allow(
        clippy::expect_used,
        reason = "recorder installation is a one-time startup operation"
    )]
    PROMETHEUS_HANDLE.get_or_init(|| {
        let builder = PrometheusBuilder::new();
        builder
            .install_recorder()
            .expect("failed to install Prometheus recorder")
    })
}

/// Render all collected metrics in Prometheus text exposition format.
///
/// Returns `None` if the recorder has not been installed.
pub fn render_prometheus() -> Option<String> {
    PROMETHEUS_HANDLE.get().map(PrometheusHandle::render)
}

// -----------------------------------------------------------------------------
// Status Class
// -----------------------------------------------------------------------------

/// Map an HTTP status code to its class label (`"1xx"`, `"2xx"`, etc.).
///
/// Returns `"unknown"` for zero (no response written) or codes
/// outside the 100–599 range.
///
/// ```
/// use praxis_protocol::http::pingora::metrics::status_class;
///
/// assert_eq!(status_class(200), "2xx");
/// assert_eq!(status_class(404), "4xx");
/// assert_eq!(status_class(0), "unknown");
/// ```
pub fn status_class(code: u16) -> &'static str {
    match code {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "unknown",
    }
}

/// Map an HTTP method to a bounded label value.
///
/// Returns the method string for the nine standard methods
/// defined in [RFC 9110]; all others collapse to `"OTHER"`.
///
/// ```
/// use praxis_protocol::http::pingora::metrics::method_label;
///
/// assert_eq!(method_label("GET"), "GET");
/// assert_eq!(method_label("PURGE"), "OTHER");
/// ```
///
/// [RFC 9110]: https://datatracker.ietf.org/doc/html/rfc9110#section-9.1
pub fn method_label(method: &str) -> &'static str {
    match method {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "DELETE" => "DELETE",
        "PATCH" => "PATCH",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        "TRACE" => "TRACE",
        "CONNECT" => "CONNECT",
        _ => "OTHER",
    }
}
