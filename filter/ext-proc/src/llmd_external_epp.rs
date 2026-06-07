// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Track B `llmd_external_epp` filter: narrow llm-d EPP picker.
//!
//! Buffers the request body, sends headers and body to the Go EPP
//! via the `ext_proc` request-phase helper, then sets `ctx.upstream`
//! from the EPP's `x-gateway-destination-endpoint` header, applies
//! request header/body mutations, and releases the buffered body.
//!
//! This is a narrow Track B filter — not full Envoy `ext_proc` parity.
//! It does not implement response-phase processing.
//!
//! # Failure mode
//!
//! All EPP operational errors (transport, timeout, missing/invalid
//! endpoint) produce [`FilterAction::Reject`] with the configured
//! `status_on_error`. This means the filter is always fail-closed:
//! even if the surrounding pipeline sets `failure_mode: open`, EPP
//! failures become explicit rejections, not `FilterError` values
//! that the pipeline could swallow.
//!
//! # Cancellation
//!
//! If `request_timeout_ms` expires, the tonic response stream
//! receiver is dropped. The server observes this as the response
//! channel closing (verified by
//! `epp_timeout_causes_server_observed_response_stream_cancellation`
//! which awaits `Sender::closed()`). No automatic retry is
//! performed. Client-disconnect cancellation is not separately
//! tested.
//!
//! # EPP call cardinality
//!
//! Each request produces exactly one `process_request_phase` call
//! (at `end_of_stream = true`). There is no retry, no hedging, and
//! no speculative pre-call during body buffering.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use praxis_core::connectivity::{ConnectionOptions, Upstream};
use praxis_filter::{
    BodyAccess, BodyMode, FilterAction, FilterError, HttpFilter, HttpFilterContext, Rejection, parse_filter_config,
};
use serde::Deserialize;
use tokio::sync::OnceCell;
use tonic::transport::{Channel, Endpoint};

use crate::{
    Phase,
    mutations::{apply_headers_response, immediate_to_rejection, request_to_proto_headers},
    request_phase,
};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Default per-request timeout in milliseconds.
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 5000;

/// Default maximum request body size in bytes (4 MiB).
const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 4_194_304;

/// Default HTTP status code returned on EPP errors.
const DEFAULT_STATUS_ON_ERROR: u16 = 500;

// -----------------------------------------------------------------------------
// LlmdExternalEppConfig
// -----------------------------------------------------------------------------

/// YAML configuration for the `llmd_external_epp` filter.
///
/// ```yaml
/// filter: llmd_external_epp
/// target: "http://127.0.0.1:9002"
/// request_timeout_ms: 5000
/// max_request_body_bytes: 4194304
/// status_on_error: 500
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LlmdExternalEppConfig {
    /// gRPC endpoint URI of the Go EPP.
    target: String,

    /// Per-request timeout in milliseconds covering the full
    /// `ext_proc` exchange (headers + body). Default: 5000.
    #[serde(default = "default_request_timeout_ms")]
    request_timeout_ms: u64,

    /// Maximum request body size in bytes. Requests exceeding this
    /// limit receive a 413 response. Default: 4 MiB.
    #[serde(default = "default_max_request_body_bytes")]
    max_request_body_bytes: usize,

    /// HTTP status code returned when the EPP returns an error,
    /// fails to respond, or the endpoint is missing. Default: 500.
    #[serde(default = "default_status_on_error")]
    status_on_error: u16,
}

/// Returns the default request timeout in milliseconds.
fn default_request_timeout_ms() -> u64 {
    DEFAULT_REQUEST_TIMEOUT_MS
}

/// Returns the default max request body bytes.
fn default_max_request_body_bytes() -> usize {
    DEFAULT_MAX_REQUEST_BODY_BYTES
}

/// Returns the default HTTP status on EPP error.
fn default_status_on_error() -> u16 {
    DEFAULT_STATUS_ON_ERROR
}

// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

/// Validate configuration values.
fn validate_config(cfg: &LlmdExternalEppConfig) -> Result<(), FilterError> {
    if cfg.request_timeout_ms == 0 {
        return Err("llmd_external_epp: request_timeout_ms must be greater than 0".into());
    }
    if cfg.max_request_body_bytes == 0 {
        return Err("llmd_external_epp: max_request_body_bytes must be greater than 0".into());
    }
    if !(100..=599).contains(&cfg.status_on_error) {
        let code = cfg.status_on_error;
        return Err(
            format!("llmd_external_epp: status_on_error {code} is not a valid HTTP status code (100..=599)").into(),
        );
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// LlmdExternalEppFilter
// -----------------------------------------------------------------------------

/// Track B external EPP picker filter for llm-d.
///
/// Buffers the full request body, calls the Go EPP via `ext_proc`,
/// sets the upstream endpoint, and applies request mutations.
///
/// # YAML configuration
///
/// ```yaml
/// filter: llmd_external_epp
/// target: "http://127.0.0.1:9002"
/// request_timeout_ms: 5000
/// max_request_body_bytes: 4194304
/// ```
/// Track B external EPP picker filter for llm-d.
///
/// The gRPC channel is created lazily on first request (inside the
/// tokio runtime) rather than in `from_config`, because tonic's
/// `connect_lazy` requires a reactor context.
pub struct LlmdExternalEppFilter {
    /// Lazily-initialized gRPC channel to the Go EPP.
    channel: OnceCell<Channel>,

    /// Parsed endpoint for lazy channel creation.
    endpoint: Endpoint,

    /// Maximum request body size in bytes.
    max_request_body_bytes: usize,

    /// Per-request timeout for the full `ext_proc` exchange.
    request_timeout: Duration,

    /// HTTP status code on EPP errors.
    status_on_error: u16,

    /// gRPC target URI (retained for diagnostics).
    target: String,
}

impl std::fmt::Debug for LlmdExternalEppFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmdExternalEppFilter")
            .field("target", &self.target)
            .field("request_timeout", &self.request_timeout)
            .field("max_request_body_bytes", &self.max_request_body_bytes)
            .field("status_on_error", &self.status_on_error)
            .finish()
    }
}

impl LlmdExternalEppFilter {
    /// Create from parsed YAML config.
    ///
    /// Validates the target URI at construction time but defers gRPC
    /// channel creation to the first request (requires tokio runtime).
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if the config is malformed or the
    /// target URI is invalid.
    pub fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: LlmdExternalEppConfig = parse_filter_config("llmd_external_epp", config)?;
        validate_config(&cfg)?;

        let endpoint: Endpoint = cfg.target.parse().map_err(|e| -> FilterError {
            let target = &cfg.target;
            format!("llmd_external_epp: invalid target URI '{target}': {e}").into()
        })?;

        Ok(Box::new(Self {
            channel: OnceCell::new(),
            endpoint,
            max_request_body_bytes: cfg.max_request_body_bytes,
            request_timeout: Duration::from_millis(cfg.request_timeout_ms),
            status_on_error: cfg.status_on_error,
            target: cfg.target,
        }))
    }

    /// Get or lazily create the gRPC channel.
    async fn channel(&self) -> Channel {
        self.channel
            .get_or_init(|| async { self.endpoint.connect_lazy() })
            .await
            .clone()
    }
}

#[async_trait]
impl HttpFilter for LlmdExternalEppFilter {
    fn name(&self) -> &'static str {
        "llmd_external_epp"
    }

    fn request_body_access(&self) -> BodyAccess {
        BodyAccess::ReadWrite
    }

    fn request_body_mode(&self) -> BodyMode {
        BodyMode::StreamBuffer {
            max_bytes: Some(self.max_request_body_bytes),
        }
    }

    async fn on_request(&self, _ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        Ok(FilterAction::Continue)
    }

    async fn on_request_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        if !end_of_stream {
            return Ok(FilterAction::Continue);
        }

        let body_bytes = body.clone().unwrap_or_default();
        let headers = request_to_proto_headers(ctx);

        let result = match self.call_epp(headers, body_bytes).await {
            Ok(r) => r,
            Err(e) => return Ok(self.reject_with_error(&e)),
        };

        if let Some(imm) = &result.immediate_response {
            return Ok(immediate_to_rejection(imm));
        }

        let endpoint = match validate_endpoint(result.selected_endpoint.as_deref()) {
            Ok(ep) => ep,
            Err(e) => return Ok(self.reject_with_error(&e)),
        };

        set_upstream(ctx, endpoint);
        apply_request_mutations(ctx, &result, body);

        Ok(FilterAction::Release)
    }
}

impl LlmdExternalEppFilter {
    /// Call the Go EPP and return the result or a loggable error.
    async fn call_epp(
        &self,
        headers: praxis_proto::envoy::service::ext_proc::v3::HttpHeaders,
        body: Bytes,
    ) -> Result<request_phase::RequestPhaseResult, String> {
        let channel = self.channel().await;
        request_phase::process_request_phase(channel, headers, body, self.request_timeout)
            .await
            .map_err(|e| {
                tracing::warn!(target = %self.target, error = %e, "llmd_external_epp EPP call failed");
                e.to_string()
            })
    }

    /// Build a rejection with the configured `status_on_error`.
    fn reject_with_error(&self, reason: &str) -> FilterAction {
        tracing::warn!(
            status = self.status_on_error,
            reason = %reason,
            "llmd_external_epp rejecting request"
        );
        FilterAction::Reject(Rejection::status(self.status_on_error))
    }
}

// -----------------------------------------------------------------------------
// Private helpers
// -----------------------------------------------------------------------------

/// Validate the selected endpoint from the EPP result.
///
/// Accepts `host:port` where host is an IPv4 address, a DNS name,
/// or a bracketed IPv6 address, and port is a valid number (1-65535).
/// Rejects URIs with schemes, missing ports, non-numeric ports,
/// comma-separated multi-endpoints, empty hosts, whitespace, and
/// malformed IPv6.
pub(crate) fn validate_endpoint(endpoint: Option<&str>) -> Result<&str, String> {
    let endpoint = endpoint.unwrap_or_default();

    if endpoint.is_empty() {
        return Err("EPP did not return x-gateway-destination-endpoint".to_owned());
    }

    reject_invalid_format(endpoint)?;
    let (host, port_str, bracketed) = split_host_port(endpoint)?;
    validate_host(host, bracketed)?;
    validate_port(port_str)?;

    Ok(endpoint)
}

/// Reject endpoints with invalid format markers.
fn reject_invalid_format(endpoint: &str) -> Result<(), String> {
    if endpoint.contains(',') {
        return Err(format!("multiple endpoints not supported: '{endpoint}'"));
    }
    if endpoint.contains("://") {
        return Err(format!("endpoint must be host:port, not a URI: '{endpoint}'"));
    }
    if endpoint.bytes().any(|b| b == b' ' || b == b'\t') {
        return Err(format!("endpoint contains whitespace: '{endpoint}'"));
    }
    Ok(())
}

/// Split `host:port` or `[ipv6]:port` into (host, port, bracketed).
fn split_host_port(endpoint: &str) -> Result<(&str, &str, bool), String> {
    if endpoint.starts_with('[') {
        let bracket_end = endpoint
            .find(']')
            .ok_or_else(|| format!("invalid IPv6 bracket in '{endpoint}'"))?;
        let host = &endpoint[1..bracket_end];
        let rest = &endpoint[bracket_end + 1..];
        if !rest.starts_with(':') {
            return Err(format!("missing port after IPv6 address in '{endpoint}'"));
        }
        Ok((host, &rest[1..], true))
    } else {
        let colon = endpoint
            .rfind(':')
            .ok_or_else(|| format!("missing port in '{endpoint}'"))?;
        Ok((&endpoint[..colon], &endpoint[colon + 1..], false))
    }
}

/// Validate the host portion of the endpoint.
fn validate_host(host: &str, bracketed: bool) -> Result<(), String> {
    if host.is_empty() {
        return Err("empty host in endpoint".to_owned());
    }
    if bracketed {
        validate_ipv6_host(host)
    } else {
        validate_unbracketed_host(host)
    }
}

/// Validate a bracketed host as a real IPv6 address.
fn validate_ipv6_host(host: &str) -> Result<(), String> {
    host.parse::<std::net::Ipv6Addr>()
        .map_err(|_parse_err| format!("invalid IPv6 address '{host}'"))?;
    Ok(())
}

/// Validate an unbracketed host as IPv4 or DNS hostname.
fn validate_unbracketed_host(host: &str) -> Result<(), String> {
    if host.contains(':') {
        return Err(format!("unbracketed IPv6 address '{host}' — use [addr]:port"));
    }
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return Ok(());
    }
    validate_dns_hostname(host)
}

/// Validate DNS hostname syntax (RFC 952/1123): alphanumeric labels
/// separated by dots, no leading/trailing dashes, labels <= 63
/// bytes, total hostname <= 253 bytes.
fn validate_dns_hostname(host: &str) -> Result<(), String> {
    if host.len() > 253 {
        return Err(format!("hostname exceeds 253 bytes: '{host}'"));
    }
    for label in host.split('.') {
        validate_dns_label(label, host)?;
    }
    Ok(())
}

/// Validate a single DNS label.
fn validate_dns_label(label: &str, host: &str) -> Result<(), String> {
    if label.is_empty() {
        return Err(format!("empty DNS label in '{host}'"));
    }
    if label.len() > 63 {
        return Err(format!("DNS label '{label}' exceeds 63 bytes"));
    }
    if label.starts_with('-') || label.ends_with('-') {
        return Err(format!("DNS label '{label}' must not start or end with '-'"));
    }
    if !label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(format!("invalid character in DNS label '{label}'"));
    }
    Ok(())
}

/// Validate that the port is a numeric value in 1-65535.
fn validate_port(port_str: &str) -> Result<(), String> {
    let port: u16 = port_str
        .parse()
        .map_err(|_parse_err| format!("non-numeric port '{port_str}'"))?;
    if port == 0 {
        return Err("port 0 is not valid".to_owned());
    }
    Ok(())
}

/// Set `ctx.upstream` from the EPP-selected endpoint.
fn set_upstream(ctx: &mut HttpFilterContext<'_>, endpoint: &str) {
    ctx.upstream = Some(Upstream {
        address: Arc::from(endpoint),
        tls: None,
        connection: Arc::new(ConnectionOptions::default()),
    });
}

/// Apply header mutations and body replacement from the EPP result.
///
/// Content-Length is NOT set here: the protocol layer's
/// `apply_mutated_content_length` in `upstream_request_filter`
/// handles it authoritatively using `mutated_request_body_len`,
/// which `stream_buffer::pre_read_body` sets for any body-writing
/// filter.
fn apply_request_mutations(
    ctx: &mut HttpFilterContext<'_>,
    result: &request_phase::RequestPhaseResult,
    body: &mut Option<Bytes>,
) {
    if let Some(hr) = &result.headers_response {
        apply_headers_response(hr, ctx, Phase::Request);
    }

    if let Some(new_body) = &result.mutated_body {
        *body = Some(new_body.clone());
    }
}
