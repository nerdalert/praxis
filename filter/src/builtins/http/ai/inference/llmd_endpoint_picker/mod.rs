// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! llm-d native endpoint picker filter.

mod config;
mod state;

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use config::LlmdEndpointPickerConfig;
use praxis_core::connectivity::{ConnectionOptions, Upstream};
use state::{EndpointSnapshot, EndpointState};
use tracing::debug;

use crate::{
    FilterAction, FilterError, Rejection,
    body::{BodyAccess, BodyMode},
    factory::parse_filter_config,
    filter::{HttpFilter, HttpFilterContext},
};

// -----------------------------------------------------------------------------
// LlmdEndpointPickerFilter
// -----------------------------------------------------------------------------

/// Selects an upstream endpoint for OpenAI-compatible inference requests
/// based on model affinity, health, queue depth, and KV-cache pressure.
pub struct LlmdEndpointPickerFilter {
    /// Plain HTTP connection options for selected upstreams.
    connection: Arc<ConnectionOptions>,

    /// Weight applied to inverse KV-cache pressure scoring.
    kv_cache_weight: f64,

    /// Request body size limit for `StreamBuffer`.
    max_body_bytes: usize,

    /// Logical pool name used for metadata and cluster accounting.
    pool_name: Arc<str>,

    /// Weight applied to inverse queue-depth scoring.
    queue_weight: f64,

    /// Static endpoint snapshot.
    snapshot: Arc<EndpointSnapshot>,
}

impl LlmdEndpointPickerFilter {
    /// Build from parsed YAML config.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] when endpoint config is invalid.
    pub fn from_config(yaml: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: LlmdEndpointPickerConfig = parse_filter_config("llmd_endpoint_picker", yaml)?;
        config::validate_config(&cfg)?;
        build_filter(cfg)
    }

    /// Select the highest-scored healthy endpoint for the given model.
    fn select_endpoint<'a>(&self, snapshot: &'a EndpointSnapshot, model: &str) -> Option<(&'a EndpointState, f64)> {
        build_candidates(snapshot, model)
            .into_iter()
            .map(|ep| (ep, self.score(ep)))
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
    }

    /// Score one endpoint using queue depth and KV-cache pressure.
    fn score(&self, endpoint: &EndpointState) -> f64 {
        let queue_depth = endpoint.running_requests.saturating_add(endpoint.waiting_requests);
        let bounded = u32::try_from(queue_depth).unwrap_or(u32::MAX);
        let queue_score = 1.0 / (1.0 + f64::from(bounded));
        // defensive clamp; config validation rejects out-of-range values
        let kv_pressure = endpoint.kv_cache_usage_percent.clamp(0.0, 100.0);
        let kv_score = 1.0 - (kv_pressure / 100.0);
        (self.queue_weight * queue_score) + (self.kv_cache_weight * kv_score)
    }

    /// Select an endpoint from a buffered request body and apply it.
    fn pick_from_body(&self, ctx: &mut HttpFilterContext<'_>, body: &Option<Bytes>) -> FilterAction {
        let raw = match body.as_ref() {
            Some(b) => b.as_ref(),
            None => return reject(400, "llmd_endpoint_picker: missing request body"),
        };
        let model = match extract_model(raw) {
            Ok(m) => m,
            Err(action) => return action,
        };

        let snapshot = &self.snapshot;
        let Some((endpoint, score)) = self.select_endpoint(snapshot, &model) else {
            debug!(model = %model, "llm-d endpoint picker found no eligible endpoint");
            return reject(503, "llmd_endpoint_picker: no eligible endpoint");
        };

        self.apply_selection(ctx, &model, endpoint, score);
        FilterAction::Release
    }

    /// Store the selected endpoint in the request context.
    fn apply_selection(&self, ctx: &mut HttpFilterContext<'_>, model: &str, endpoint: &EndpointState, score: f64) {
        debug!(
            model = %model,
            pool = %self.pool_name,
            endpoint = %endpoint.name,
            upstream = %endpoint.address,
            score,
            "llm-d endpoint selected"
        );

        ctx.cluster = Some(Arc::clone(&self.pool_name));
        ctx.set_metadata("llmd.model", model.to_owned());
        ctx.set_metadata("llmd.endpoint", endpoint.name.as_ref().to_owned());
        ctx.upstream = Some(Upstream {
            address: Arc::clone(&endpoint.address),
            connection: Arc::clone(&self.connection),
            tls: None,
        });
    }
}

#[async_trait]
impl HttpFilter for LlmdEndpointPickerFilter {
    fn name(&self) -> &'static str {
        "llmd_endpoint_picker"
    }

    fn request_body_access(&self) -> BodyAccess {
        BodyAccess::ReadOnly
    }

    fn request_body_mode(&self) -> BodyMode {
        BodyMode::StreamBuffer {
            max_bytes: Some(self.max_body_bytes),
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

        Ok(self.pick_from_body(ctx, body))
    }
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// Build a filter instance from validated config.
fn build_filter(cfg: LlmdEndpointPickerConfig) -> Result<Box<dyn HttpFilter>, FilterError> {
    let static_endpoints: Vec<EndpointState> = cfg
        .endpoints
        .into_iter()
        .map(EndpointState::try_from)
        .collect::<Result<_, _>>()?;

    let snapshot = Arc::new(EndpointSnapshot {
        endpoints: static_endpoints,
    });

    Ok(Box::new(LlmdEndpointPickerFilter {
        connection: Arc::new(ConnectionOptions::default()),
        kv_cache_weight: cfg.kv_cache_weight,
        max_body_bytes: cfg.max_body_bytes,
        pool_name: Arc::from(cfg.pool_name),
        queue_weight: cfg.queue_weight,
        snapshot,
    }))
}

/// Return healthy endpoints that serve `model`.
fn build_candidates<'a>(snapshot: &'a EndpointSnapshot, model: &str) -> Vec<&'a EndpointState> {
    snapshot
        .endpoints
        .iter()
        .filter(|ep| ep.healthy && ep.models.iter().any(|m| m.as_ref() == model))
        .collect()
}

/// Extract the `model` field from a JSON request body.
///
/// Returns a 400 rejection for invalid JSON or a missing model field.
fn extract_model(body: &[u8]) -> Result<String, FilterAction> {
    let value = serde_json::from_slice::<serde_json::Value>(body)
        .map_err(|_err| reject(400, "llmd_endpoint_picker: invalid JSON request body"))?;

    value
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| reject(400, "llmd_endpoint_picker: missing model field"))
}

/// Build a plain-text rejection response.
fn reject(status: u16, message: &'static str) -> FilterAction {
    FilterAction::Reject(
        Rejection::status(status)
            .with_header("content-type", "text/plain; charset=utf-8")
            .with_body(Bytes::from_static(message.as_bytes())),
    )
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests"
)]
mod tests {
    use super::*;

    #[test]
    fn from_config_rejects_empty_endpoints() {
        let yaml: serde_yaml::Value = serde_yaml::from_str("endpoints: []").unwrap();
        let Err(err) = LlmdEndpointPickerFilter::from_config(&yaml) else {
            panic!("empty endpoints should fail");
        };

        assert!(
            err.to_string().contains("endpoints must not be empty"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn body_access_uses_stream_buffer() {
        let filter = make_filter();

        assert_eq!(filter.request_body_access(), BodyAccess::ReadOnly);
        assert_eq!(
            filter.request_body_mode(),
            BodyMode::StreamBuffer {
                max_bytes: Some(crate::body::DEFAULT_JSON_BODY_MAX_BYTES)
            }
        );
    }

    #[tokio::test]
    async fn selects_lowest_pressure_matching_endpoint() {
        let filter = make_filter();
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"fake-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release));
        assert_eq!(ctx.upstream_addr(), Some("127.0.0.1:18082"));
        assert_eq!(ctx.get_metadata("llmd.model"), Some("fake-model"));
        assert_eq!(ctx.get_metadata("llmd.endpoint"), Some("less-loaded"));
    }

    #[tokio::test]
    async fn filters_by_requested_model() {
        let filter = make_filter();
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"other-model","prompt":"hi"}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release));
        assert_eq!(ctx.upstream_addr(), Some("127.0.0.1:18083"));
        assert_eq!(ctx.get_metadata("llmd.endpoint"), Some("other-model"));
    }

    #[tokio::test]
    async fn rejects_when_model_is_missing() {
        let filter = make_filter();
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"prompt":"hi"}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Reject(r) if r.status == 400));
        assert!(ctx.upstream.is_none(), "missing model must not select an upstream");
    }

    #[tokio::test]
    async fn rejects_when_no_endpoint_serves_model() {
        let filter = make_filter();
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"missing","prompt":"hi"}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Reject(r) if r.status == 503));
        assert!(
            ctx.upstream.is_none(),
            "no eligible endpoint must not select an upstream"
        );
    }

    #[tokio::test]
    async fn unhealthy_endpoint_is_skipped() {
        let mut unhealthy = make_endpoint("unhealthy-best", "127.0.0.1:9001");
        unhealthy.healthy = false;

        let mut healthy = make_endpoint("healthy-worse", "127.0.0.1:9002");
        healthy.kv_cache_usage_percent = 80.0;
        healthy.running_requests = 10;
        healthy.waiting_requests = 5;

        let filter = make_filter_with_endpoints(vec![unhealthy, healthy]);
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release));
        assert_eq!(
            ctx.upstream_addr(),
            Some("127.0.0.1:9002"),
            "unhealthy endpoint should be skipped even when it has a better score"
        );
        assert_eq!(
            ctx.get_metadata("llmd.endpoint"),
            Some("healthy-worse"),
            "healthy endpoint should be selected"
        );
    }

    // Test Utilities

    fn make_endpoint(name: &str, address: &str) -> EndpointState {
        EndpointState {
            name: Arc::from(name),
            address: Arc::from(address),
            healthy: true,
            kv_cache_usage_percent: 0.0,
            models: vec![Arc::from("test-model")],
            running_requests: 0,
            waiting_requests: 0,
        }
    }

    fn make_filter_with_endpoints(endpoints: Vec<EndpointState>) -> LlmdEndpointPickerFilter {
        LlmdEndpointPickerFilter {
            connection: Arc::new(ConnectionOptions::default()),
            kv_cache_weight: 2.0,
            max_body_bytes: 1_048_576, // 1 MiB
            pool_name: Arc::from("test"),
            queue_weight: 2.0,
            snapshot: Arc::new(EndpointSnapshot { endpoints }),
        }
    }

    fn make_filter() -> Box<dyn HttpFilter> {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
endpoints:
  - name: loaded
    address: "127.0.0.1:18081"
    models: ["fake-model"]
    running_requests: 8
    waiting_requests: 4
    kv_cache_usage_percent: 92.0
  - name: less-loaded
    address: "127.0.0.1:18082"
    models: ["fake-model"]
    running_requests: 1
    waiting_requests: 0
    kv_cache_usage_percent: 10.0
  - name: other-model
    address: "127.0.0.1:18083"
    models: ["other-model"]
    running_requests: 0
    waiting_requests: 0
    kv_cache_usage_percent: 0.0
"#,
        )
        .unwrap();
        LlmdEndpointPickerFilter::from_config(&yaml).unwrap()
    }
}
