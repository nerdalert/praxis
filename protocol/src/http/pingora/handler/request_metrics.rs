// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Request-level Prometheus metrics recorded during the `logging()` hook.

use pingora_proxy::Session;

use super::super::context::PingoraRequestCtx;

// -----------------------------------------------------------------------------
// Request Metrics
// -----------------------------------------------------------------------------

/// Record HTTP request metrics from the completed request lifecycle.
///
/// Called from the `logging()` hook in both `with_body` and `no_body`
/// handlers, after the response has been sent (or failed).
pub(super) fn record_request_metrics(session: &Session, _error: Option<&pingora_core::Error>, ctx: &PingoraRequestCtx) {
    let method = session.req_header().method.as_str();
    let status = status_label(session);
    let cluster = ctx.cluster.as_deref().unwrap_or("none");
    let duration = ctx.request_start.elapsed().as_secs_f64();

    metrics::counter!(
        "praxis_http_requests_total",
        "method" => method.to_owned(),
        "status" => status.clone(),
        "cluster" => cluster.to_owned(),
    )
    .increment(1);

    metrics::histogram!(
        "praxis_http_request_duration_seconds",
        "method" => method.to_owned(),
        "status" => status,
        "cluster" => cluster.to_owned(),
    )
    .record(duration);
}

// -----------------------------------------------------------------------------
// Utilities
// -----------------------------------------------------------------------------

/// Extract the HTTP status code as a label string.
///
/// Prefers the actual response status written to the client. Falls back
/// to `"unknown"` when no response was sent (e.g. early connection close).
fn status_label(session: &Session) -> String {
    session
        .response_written()
        .map(|resp| resp.status.as_u16().to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}
