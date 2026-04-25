// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! HTTP external auth filter: validates bearer tokens via an HTTP callout.

mod config;

#[cfg(test)]
mod tests;

use std::{borrow::Cow, collections::HashMap, time::Duration};

use async_trait::async_trait;
use tracing::{debug, info, warn};

use self::config::HttpExtAuthConfig;
use crate::{
    FilterAction, FilterError, Rejection,
    factory::parse_filter_config,
    filter::{HttpFilter, HttpFilterContext},
};

// -----------------------------------------------------------------------------
// HttpExtAuthFilter
// -----------------------------------------------------------------------------

/// HTTP external auth filter that validates bearer tokens via an HTTP
/// callout to an auth service.
///
/// Extracts the bearer token from the `Authorization` header, sends
/// it to a configured endpoint, and writes validated identity
/// descriptors into [`HttpFilterContext::filter_metadata`].
///
/// # YAML configuration
///
/// ```yaml
/// filter: http_ext_auth
/// endpoint: "http://maas-api.opendatahub.svc:8080/internal/v1/api-keys/validate"
/// timeout_ms: 500
/// response:
///   metadata:
///     user: userId
///     subscription: selected_subscription
///   upstream_headers:
///     x-maas-subscription: selected_subscription
/// strip:
///   request_headers:
///     - authorization
/// ```
///
/// [`HttpFilterContext::filter_metadata`]: crate::context::HttpFilterContext::filter_metadata
pub struct HttpExtAuthFilter {
    /// Auth service endpoint URL.
    endpoint: String,

    /// Request timeout.
    timeout: Duration,

    /// HTTP client for auth callouts.
    client: reqwest::Client,

    /// Map from metadata key to JSON response field name.
    metadata_map: HashMap<String, String>,

    /// Map from upstream header name to JSON response field name.
    upstream_header_map: HashMap<String, String>,

    /// Request headers to strip before upstream.
    strip_headers: Vec<String>,
}

impl HttpExtAuthFilter {
    /// Create an HTTP ext-auth filter from parsed YAML config.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if the config is invalid.
    pub fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: HttpExtAuthConfig = parse_filter_config("http_ext_auth", config)?;

        if cfg.endpoint.is_empty() {
            return Err("http_ext_auth: endpoint is required".into());
        }

        if cfg.timeout_ms == 0 {
            return Err("http_ext_auth: timeout_ms must be greater than 0".into());
        }

        if cfg.tls_skip_verify {
            tracing::warn!("http_ext_auth: TLS verification disabled — do not use in production");
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .danger_accept_invalid_certs(cfg.tls_skip_verify)
            .build()
            .map_err(|e| FilterError::from(format!("http_ext_auth: failed to build HTTP client: {e}")))?;

        Ok(Box::new(Self {
            endpoint: cfg.endpoint,
            timeout: Duration::from_millis(cfg.timeout_ms),
            client,
            metadata_map: cfg.response.metadata,
            upstream_header_map: cfg.response.upstream_headers,
            strip_headers: cfg.strip.request_headers,
        }))
    }
}

#[async_trait]
impl HttpFilter for HttpExtAuthFilter {
    fn name(&self) -> &'static str {
        "http_ext_auth"
    }

    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        let token = extract_bearer_token(ctx);

        let Some(token) = token else {
            info!("http_ext_auth: missing bearer token, rejecting 401");
            metrics::counter!("praxis_auth_rejected_total", "filter" => "http_ext_auth", "reason" => "missing_token")
                .increment(1);
            return Ok(FilterAction::Reject(Rejection::status(401)));
        };

        let body = serde_json::json!({ "key": token });

        let result = self
            .client
            .post(&self.endpoint)
            .json(&body)
            .timeout(self.timeout)
            .send()
            .await;

        let response = match result {
            Ok(resp) => resp,
            Err(e) => {
                warn!(error = %e, "http_ext_auth: auth callout failed");
                metrics::counter!("praxis_auth_error_total", "filter" => "http_ext_auth", "reason" => "callout_error")
                    .increment(1);
                for header in &self.strip_headers {
                    ctx.remove_request_headers.push(Cow::Owned(header.clone()));
                }
                return Err(format!("http_ext_auth: auth callout failed: {e}").into());
            },
        };

        let status = response.status().as_u16();

        if status == 200 {
            let json: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);

            match json.get("valid").and_then(|v| v.as_bool()) {
                Some(true) => {
                    debug!("http_ext_auth: auth allowed (valid=true)");
                    metrics::counter!("praxis_auth_allowed_total", "filter" => "http_ext_auth").increment(1);

                    for (meta_key, json_field) in &self.metadata_map {
                        if let Some(val) = json.get(json_field).and_then(|v| v.as_str()) {
                            ctx.set_metadata(meta_key, val);
                        } else if let Some(val) = json.get(json_field) {
                            ctx.set_metadata(meta_key, &val.to_string());
                        }
                    }

                    for (header_name, json_field) in &self.upstream_header_map {
                        if let Some(val) = json.get(json_field).and_then(|v| v.as_str()) {
                            ctx.extra_request_headers
                                .push((Cow::Owned(header_name.clone()), val.to_owned()));
                        }
                    }

                    for header in &self.strip_headers {
                        ctx.remove_request_headers.push(Cow::Owned(header.clone()));
                    }

                    return Ok(FilterAction::Continue);
                },
                Some(false) => {
                    info!("http_ext_auth: auth service returned valid=false, rejecting 403");
                    metrics::counter!(
                        "praxis_auth_rejected_total",
                        "filter" => "http_ext_auth",
                        "reason" => "invalid_key"
                    )
                    .increment(1);
                    return Ok(FilterAction::Reject(Rejection::status(403)));
                },
                None => {
                    warn!("http_ext_auth: auth response missing valid field, fail-closed");
                    metrics::counter!(
                        "praxis_auth_error_total",
                        "filter" => "http_ext_auth",
                        "reason" => "missing_valid_field"
                    )
                    .increment(1);
                    for header in &self.strip_headers {
                        ctx.remove_request_headers.push(Cow::Owned(header.clone()));
                    }
                    return Err("http_ext_auth: auth response missing 'valid' field".into());
                },
            }
        }

        if status == 401 || status == 403 {
            info!(status, "http_ext_auth: auth denied");
            metrics::counter!("praxis_auth_rejected_total", "filter" => "http_ext_auth", "reason" => "denied")
                .increment(1);
            return Ok(FilterAction::Reject(Rejection::status(status)));
        }

        warn!(status, "http_ext_auth: unexpected auth response status");
        metrics::counter!("praxis_auth_error_total", "filter" => "http_ext_auth", "reason" => "unexpected_status")
            .increment(1);
        Err(format!("http_ext_auth: unexpected auth response status {status}").into())
    }
}

// -----------------------------------------------------------------------------
// Utilities
// -----------------------------------------------------------------------------

/// Extract a bearer token from the Authorization header.
fn extract_bearer_token<'a>(ctx: &'a HttpFilterContext<'_>) -> Option<&'a str> {
    let auth = ctx.request.headers.get("authorization")?.to_str().ok()?;
    let token = auth.strip_prefix("Bearer ")?;
    if token.is_empty() {
        return None;
    }
    Some(token)
}
