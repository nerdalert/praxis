// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Configuration types for the llm-d endpoint picker filter.

use serde::Deserialize;

use super::{
    disaggregation::{self, DisaggregationConfig, EndpointRole},
    inference_objective::InferenceObjectiveConfig,
    model_rewrite::ModelRewriteConfig,
    prefix_cache::PrefixCacheConfig,
    saturation::SaturationGateConfig,
};
use crate::{FilterError, body::DEFAULT_JSON_BODY_MAX_BYTES};

// -----------------------------------------------------------------------------
// Defaults
// -----------------------------------------------------------------------------

/// Default logical pool name used in metadata.
pub(super) const DEFAULT_POOL_NAME: &str = "llmd";
/// Default weight for queue-depth scoring.
pub(super) const DEFAULT_QUEUE_WEIGHT: f64 = 2.0;
/// Default weight for KV-cache pressure scoring.
pub(super) const DEFAULT_KV_CACHE_WEIGHT: f64 = 2.0;
/// Default metrics refresh interval in milliseconds.
const DEFAULT_METRICS_REFRESH_MS: u64 = 1000;
/// Default metrics scrape timeout in milliseconds.
const DEFAULT_METRICS_TIMEOUT_MS: u64 = 500;

// -----------------------------------------------------------------------------
// Top-Level Config
// -----------------------------------------------------------------------------

/// YAML configuration for the native llm-d endpoint picker filter.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LlmdEndpointPickerConfig {
    /// Logical pool name to expose in request metadata and metrics.
    #[serde(default = "default_pool_name")]
    pub pool_name: String,

    /// Maximum request body size in bytes for `StreamBuffer` mode.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,

    /// Weight applied to inverse queue-depth scoring.
    #[serde(default = "default_queue_weight")]
    pub queue_weight: f64,

    /// Weight applied to inverse KV-cache pressure scoring.
    #[serde(default = "default_kv_cache_weight")]
    pub kv_cache_weight: f64,

    /// Metrics refresh interval in milliseconds.
    #[serde(default = "default_metrics_refresh_ms")]
    pub metrics_refresh_ms: u64,

    /// Metrics scrape timeout in milliseconds.
    #[serde(default = "default_metrics_timeout_ms")]
    pub metrics_timeout_ms: u64,

    /// Static endpoint definitions.
    #[serde(default)]
    pub endpoints: Vec<EndpointConfig>,

    /// Optional Kubernetes `InferencePool` discovery configuration.
    #[serde(default)]
    pub inference_pool: Option<InferencePoolConfig>,

    /// Optional Gateway API `HTTPRoute` -> `InferencePool` discovery.
    #[serde(default)]
    pub gateway_api: Option<GatewayApiConfig>,

    /// Optional prefix-cache scoring configuration.
    #[serde(default)]
    pub prefix_cache: Option<PrefixCacheConfig>,

    /// Optional saturation/admission gate configuration.
    #[serde(default)]
    pub saturation_gate: Option<SaturationGateConfig>,

    /// Optional prefill/decode disaggregation configuration.
    #[serde(default)]
    pub disaggregation: Option<DisaggregationConfig>,

    /// Optional `InferenceModelRewrite` configuration.
    #[serde(default)]
    pub model_rewrite: Option<ModelRewriteConfig>,

    /// Optional `InferenceObjective` priority configuration.
    #[serde(default)]
    pub inference_objective: Option<InferenceObjectiveConfig>,
}

// -----------------------------------------------------------------------------
// InferencePool Config
// -----------------------------------------------------------------------------

/// Default metrics scrape path.
const DEFAULT_METRICS_PATH: &str = "/metrics";
/// Default API version for `InferencePool`.
const DEFAULT_API_VERSION: &str = "inference.networking.k8s.io/v1";

/// Configuration for Kubernetes `InferencePool` endpoint discovery.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InferencePoolConfig {
    /// Name of the `InferencePool` resource.
    pub name: String,

    /// Namespace of the `InferencePool` resource.
    #[serde(default)]
    pub namespace: Option<String>,

    /// API version including group (e.g. `inference.networking.k8s.io/v1`).
    #[serde(default = "default_api_version")]
    pub api_version: String,

    /// Models served by endpoints in this pool.
    pub models: Vec<String>,

    /// Metrics scrape path appended to each discovered pod address.
    #[serde(default = "default_metrics_path")]
    pub metrics_path: String,
}

// -----------------------------------------------------------------------------
// Gateway API Config
// -----------------------------------------------------------------------------

/// Configuration for Gateway API `HTTPRoute` -> `InferencePool` discovery.
///
/// Reads an `HTTPRoute` resource, finds the first `backendRef` pointing to
/// an `InferencePool`, and then performs standard pool-based discovery.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GatewayApiConfig {
    /// Reference to the `HTTPRoute` resource to read.
    pub http_route: HttpRouteRef,

    /// Models served by the discovered pool endpoints.
    pub models: Vec<String>,

    /// Metrics scrape path appended to each discovered pod address.
    #[serde(default = "default_metrics_path")]
    pub metrics_path: String,

    /// API version for the `InferencePool` found via the route.
    #[serde(default = "default_api_version")]
    pub inference_pool_api_version: String,
}

/// Reference to a Kubernetes `HTTPRoute` resource.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HttpRouteRef {
    /// Name of the `HTTPRoute` resource.
    pub name: String,

    /// Namespace of the `HTTPRoute` resource.
    #[serde(default)]
    pub namespace: Option<String>,
}

impl GatewayApiConfig {
    /// Return the effective namespace, reading the service account
    /// namespace file if not explicitly configured.
    pub fn effective_namespace(&self) -> String {
        if let Some(ref ns) = self.http_route.namespace {
            return ns.clone();
        }
        std::fs::read_to_string(super::kubernetes::sa_namespace_path())
            .ok()
            .map_or_else(|| "default".to_owned(), |s| s.trim().to_owned())
    }
}

impl InferencePoolConfig {
    /// Return the effective namespace, reading the service account
    /// namespace file if not explicitly configured.
    pub fn effective_namespace(&self) -> String {
        if let Some(ref ns) = self.namespace {
            return ns.clone();
        }
        std::fs::read_to_string(super::kubernetes::sa_namespace_path())
            .ok()
            .map_or_else(|| "default".to_owned(), |s| s.trim().to_owned())
    }
}

/// Default API version.
fn default_api_version() -> String {
    DEFAULT_API_VERSION.to_owned()
}

/// Default metrics path.
fn default_metrics_path() -> String {
    DEFAULT_METRICS_PATH.to_owned()
}

// -----------------------------------------------------------------------------
// Endpoint Config
// -----------------------------------------------------------------------------

/// YAML configuration for one inference endpoint.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EndpointConfig {
    /// Stable endpoint name used in metadata and logs.
    pub name: String,

    /// Upstream address in `host:port` form.
    pub address: String,

    /// Models served by this endpoint.
    pub models: Vec<String>,

    /// Current running request count.
    #[serde(default)]
    pub running_requests: u64,

    /// Current waiting request count.
    #[serde(default)]
    pub waiting_requests: u64,

    /// Current KV-cache utilization percentage, 0-100.
    #[serde(default)]
    pub kv_cache_usage_percent: f64,

    /// Whether this endpoint is eligible for new requests.
    #[serde(default = "default_healthy")]
    pub healthy: bool,

    /// Optional metrics scrape URL for dynamic state updates.
    #[serde(default)]
    pub metrics_url: Option<String>,

    /// Role this endpoint plays in a disaggregated serving topology.
    #[serde(default = "default_endpoint_role")]
    pub role: EndpointRole,
}

// -----------------------------------------------------------------------------
// Default Helpers
// -----------------------------------------------------------------------------

/// Default pool name for omitted `pool_name`.
fn default_pool_name() -> String {
    DEFAULT_POOL_NAME.to_owned()
}

/// Default buffered request body ceiling.
fn default_max_body_bytes() -> usize {
    DEFAULT_JSON_BODY_MAX_BYTES
}

/// Default queue scorer weight.
fn default_queue_weight() -> f64 {
    DEFAULT_QUEUE_WEIGHT
}

/// Default KV-cache scorer weight.
fn default_kv_cache_weight() -> f64 {
    DEFAULT_KV_CACHE_WEIGHT
}

/// Default endpoint health value.
fn default_healthy() -> bool {
    true
}

/// Default metrics refresh interval.
fn default_metrics_refresh_ms() -> u64 {
    DEFAULT_METRICS_REFRESH_MS
}

/// Default metrics scrape timeout.
fn default_metrics_timeout_ms() -> u64 {
    DEFAULT_METRICS_TIMEOUT_MS
}

/// Default endpoint role for backward compatibility.
fn default_endpoint_role() -> EndpointRole {
    disaggregation::default_endpoint_role()
}

// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

/// Validate top-level endpoint picker config.
pub(super) fn validate_config(cfg: &LlmdEndpointPickerConfig) -> Result<(), FilterError> {
    if cfg.pool_name.trim().is_empty() {
        return Err("llmd_endpoint_picker: pool_name must not be empty".into());
    }
    if cfg.max_body_bytes == 0 {
        return Err("llmd_endpoint_picker: max_body_bytes must be greater than zero".into());
    }
    validate_discovery_sources(cfg)?;
    if let Some(ref pool) = cfg.inference_pool {
        validate_inference_pool_config(pool)?;
    }
    if let Some(ref gw) = cfg.gateway_api {
        validate_gateway_api_config(gw)?;
    }
    validate_scoring_weights(cfg)?;
    validate_metrics_intervals(cfg)?;
    validate_optional_features(cfg)?;
    validate_disaggregation(cfg)?;
    validate_model_rewrite(cfg)?;
    validate_inference_objective(cfg)?;
    Ok(())
}

/// Validate that exactly one discovery source is configured.
fn validate_discovery_sources(cfg: &LlmdEndpointPickerConfig) -> Result<(), FilterError> {
    if cfg.inference_pool.is_some() && cfg.gateway_api.is_some() {
        return Err("llmd_endpoint_picker: inference_pool and gateway_api are mutually exclusive".into());
    }
    let has_source = !cfg.endpoints.is_empty() || cfg.inference_pool.is_some() || cfg.gateway_api.is_some();
    if !has_source {
        return Err("llmd_endpoint_picker: either endpoints, inference_pool, or gateway_api must be configured".into());
    }
    Ok(())
}

/// Validate scoring weight fields.
fn validate_scoring_weights(cfg: &LlmdEndpointPickerConfig) -> Result<(), FilterError> {
    if !cfg.queue_weight.is_finite() || cfg.queue_weight < 0.0 {
        return Err("llmd_endpoint_picker: queue_weight must be a finite non-negative number".into());
    }
    if !cfg.kv_cache_weight.is_finite() || cfg.kv_cache_weight < 0.0 {
        return Err("llmd_endpoint_picker: kv_cache_weight must be a finite non-negative number".into());
    }
    Ok(())
}

/// Validate metrics interval fields.
fn validate_metrics_intervals(cfg: &LlmdEndpointPickerConfig) -> Result<(), FilterError> {
    if cfg.metrics_refresh_ms == 0 {
        return Err("llmd_endpoint_picker: metrics_refresh_ms must be greater than zero".into());
    }
    if cfg.metrics_timeout_ms == 0 {
        return Err("llmd_endpoint_picker: metrics_timeout_ms must be greater than zero".into());
    }
    Ok(())
}

/// Validate optional feature sub-configs (prefix cache, saturation gate).
fn validate_optional_features(cfg: &LlmdEndpointPickerConfig) -> Result<(), FilterError> {
    if let Some(ref pc) = cfg.prefix_cache
        && pc.enabled
    {
        super::prefix_cache::validate_prefix_cache_config(pc)?;
    }
    if let Some(ref sg) = cfg.saturation_gate
        && sg.enabled
    {
        super::saturation::validate_saturation_gate_config(sg)?;
    }
    Ok(())
}

/// Validate disaggregation config when enabled.
fn validate_disaggregation(cfg: &LlmdEndpointPickerConfig) -> Result<(), FilterError> {
    if let Some(ref dc) = cfg.disaggregation
        && dc.enabled
    {
        disaggregation::validate_disaggregation_config(dc)?;
    }
    Ok(())
}

/// Validate model rewrite config when enabled.
fn validate_model_rewrite(cfg: &LlmdEndpointPickerConfig) -> Result<(), FilterError> {
    if let Some(ref mr) = cfg.model_rewrite
        && mr.enabled
    {
        super::model_rewrite::validate_model_rewrite_config(mr)?;
    }
    Ok(())
}

/// Validate inference objective config when enabled.
fn validate_inference_objective(cfg: &LlmdEndpointPickerConfig) -> Result<(), FilterError> {
    if let Some(ref io) = cfg.inference_objective
        && io.enabled
    {
        super::inference_objective::validate_objective_config(io)?;
    }
    Ok(())
}

/// Validate one static endpoint config.
pub(super) fn validate_endpoint_config(cfg: &EndpointConfig) -> Result<(), FilterError> {
    if cfg.name.trim().is_empty() {
        return Err("llmd_endpoint_picker: endpoint name must not be empty".into());
    }
    if cfg.address.trim().is_empty() {
        return Err("llmd_endpoint_picker: endpoint address must not be empty".into());
    }
    validate_endpoint_models(cfg)?;
    if !cfg.kv_cache_usage_percent.is_finite() {
        return Err(format!(
            "llmd_endpoint_picker: endpoint '{}' has non-finite kv_cache_usage_percent",
            cfg.name
        )
        .into());
    }
    if let Some(ref url) = cfg.metrics_url {
        validate_metrics_url(&cfg.name, url)?;
    }
    Ok(())
}

/// Validate a metrics URL has valid structure without performing DNS
/// resolution.
fn validate_metrics_url(endpoint_name: &str, url: &str) -> Result<(), FilterError> {
    let Some(parsed) = super::metrics::parse_http_url(url) else {
        return Err(format!("llmd_endpoint_picker: endpoint '{endpoint_name}' has invalid metrics_url '{url}'").into());
    };
    if parsed.host_port.is_empty() {
        return Err(format!("llmd_endpoint_picker: endpoint '{endpoint_name}' metrics_url has empty host").into());
    }
    Ok(())
}

/// Validate `InferencePool` discovery config.
fn validate_inference_pool_config(cfg: &InferencePoolConfig) -> Result<(), FilterError> {
    if cfg.name.trim().is_empty() {
        return Err("llmd_endpoint_picker: inference_pool.name must not be empty".into());
    }
    if cfg.models.is_empty() {
        return Err("llmd_endpoint_picker: inference_pool.models must not be empty".into());
    }
    if cfg.models.iter().any(|m| m.trim().is_empty()) {
        return Err("llmd_endpoint_picker: inference_pool.models contains an empty entry".into());
    }
    if !cfg.metrics_path.starts_with('/') {
        return Err("llmd_endpoint_picker: inference_pool.metrics_path must start with /".into());
    }
    match cfg.api_version.split_once('/') {
        None => {
            return Err("llmd_endpoint_picker: inference_pool.api_version must contain group/version".into());
        },
        Some((group, version)) => {
            if group.is_empty() || version.is_empty() || version.contains('/') {
                return Err("llmd_endpoint_picker: inference_pool.api_version must be exactly group/version".into());
            }
        },
    }
    Ok(())
}

/// Validate `GatewayApiConfig` fields.
fn validate_gateway_api_config(cfg: &GatewayApiConfig) -> Result<(), FilterError> {
    if cfg.http_route.name.trim().is_empty() {
        return Err("llmd_endpoint_picker: gateway_api.http_route.name must not be empty".into());
    }
    if cfg.models.is_empty() {
        return Err("llmd_endpoint_picker: gateway_api.models must not be empty".into());
    }
    if cfg.models.iter().any(|m| m.trim().is_empty()) {
        return Err("llmd_endpoint_picker: gateway_api.models contains an empty entry".into());
    }
    if !cfg.metrics_path.starts_with('/') {
        return Err("llmd_endpoint_picker: gateway_api.metrics_path must start with /".into());
    }
    validate_gateway_api_version(&cfg.inference_pool_api_version)
}

/// Validate the inference pool API version format for gateway API config.
fn validate_gateway_api_version(api_version: &str) -> Result<(), FilterError> {
    match api_version.split_once('/') {
        None => Err("llmd_endpoint_picker: gateway_api.inference_pool_api_version must contain group/version".into()),
        Some((group, version)) => {
            if group.is_empty() || version.is_empty() || version.contains('/') {
                return Err(
                    "llmd_endpoint_picker: gateway_api.inference_pool_api_version must be exactly group/version".into(),
                );
            }
            Ok(())
        },
    }
}

/// Validate the models list for a single endpoint.
fn validate_endpoint_models(cfg: &EndpointConfig) -> Result<(), FilterError> {
    if cfg.models.is_empty() {
        return Err(format!(
            "llmd_endpoint_picker: endpoint '{}' must serve at least one model",
            cfg.name
        )
        .into());
    }
    if cfg.models.iter().any(|m| m.trim().is_empty()) {
        return Err(format!("llmd_endpoint_picker: endpoint '{}' has an empty model", cfg.name).into());
    }
    Ok(())
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

    fn make_valid_endpoint(metrics_url: Option<&str>) -> EndpointConfig {
        EndpointConfig {
            name: "test".to_owned(),
            address: "127.0.0.1:8000".to_owned(),
            models: vec!["model".to_owned()],
            running_requests: 0,
            waiting_requests: 0,
            kv_cache_usage_percent: 0.0,
            healthy: true,
            metrics_url: metrics_url.map(str::to_owned),
            role: default_endpoint_role(),
        }
    }

    #[test]
    fn accepts_ip_metrics_url() {
        let cfg = make_valid_endpoint(Some("http://127.0.0.1:8000/metrics"));
        assert!(
            validate_endpoint_config(&cfg).is_ok(),
            "IP-based URL should be accepted"
        );
    }

    #[test]
    fn accepts_localhost_metrics_url() {
        let cfg = make_valid_endpoint(Some("http://localhost:8000/metrics"));
        assert!(
            validate_endpoint_config(&cfg).is_ok(),
            "localhost URL should be accepted"
        );
    }

    #[test]
    fn accepts_k8s_service_metrics_url() {
        let cfg = make_valid_endpoint(Some("http://vllm-a.default.svc:8000/metrics"));
        assert!(
            validate_endpoint_config(&cfg).is_ok(),
            "K8s service DNS URL should be accepted"
        );
    }

    #[test]
    fn rejects_https_metrics_url() {
        let cfg = make_valid_endpoint(Some("https://127.0.0.1:8000/metrics"));
        assert!(validate_endpoint_config(&cfg).is_err(), "https URL should be rejected");
    }

    #[test]
    fn rejects_empty_host_metrics_url() {
        let cfg = make_valid_endpoint(Some("http:///metrics"));
        assert!(
            validate_endpoint_config(&cfg).is_err(),
            "empty host URL should be rejected"
        );
    }

    #[test]
    fn rejects_bare_scheme_metrics_url() {
        let cfg = make_valid_endpoint(Some("http://"));
        assert!(
            validate_endpoint_config(&cfg).is_err(),
            "bare scheme URL should be rejected"
        );
    }

    #[test]
    fn rejects_empty_metrics_url() {
        let cfg = make_valid_endpoint(Some(""));
        assert!(
            validate_endpoint_config(&cfg).is_err(),
            "empty string URL should be rejected"
        );
    }

    #[test]
    fn accepts_no_metrics_url() {
        let cfg = make_valid_endpoint(None);
        assert!(
            validate_endpoint_config(&cfg).is_ok(),
            "no metrics_url should be accepted"
        );
    }

    // -- Top-level config validation tests --

    #[test]
    fn accepts_inference_pool_without_endpoints() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
inference_pool:
  name: sim-pool
  models: ["fake-model"]
"#,
        )
        .unwrap();
        let cfg: LlmdEndpointPickerConfig = serde_yaml::from_value(yaml).unwrap();
        assert!(
            validate_config(&cfg).is_ok(),
            "inference_pool without endpoints should be accepted"
        );
    }

    #[test]
    fn rejects_empty_endpoints_without_inference_pool() {
        let yaml: serde_yaml::Value = serde_yaml::from_str("endpoints: []").unwrap();
        let cfg: LlmdEndpointPickerConfig = serde_yaml::from_value(yaml).unwrap();
        let err = validate_config(&cfg).unwrap_err();
        assert!(
            err.to_string()
                .contains("either endpoints, inference_pool, or gateway_api must be configured"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_inference_pool_empty_name() {
        let cfg = InferencePoolConfig {
            name: String::new(),
            namespace: None,
            api_version: "inference.networking.k8s.io/v1".to_owned(),
            models: vec!["model".to_owned()],
            metrics_path: "/metrics".to_owned(),
        };
        assert!(
            validate_inference_pool_config(&cfg).is_err(),
            "empty pool name should be rejected"
        );
    }

    #[test]
    fn rejects_inference_pool_empty_models() {
        let cfg = InferencePoolConfig {
            name: "pool".to_owned(),
            namespace: None,
            api_version: "inference.networking.k8s.io/v1".to_owned(),
            models: vec![],
            metrics_path: "/metrics".to_owned(),
        };
        assert!(
            validate_inference_pool_config(&cfg).is_err(),
            "empty models should be rejected"
        );
    }

    #[test]
    fn rejects_inference_pool_bad_metrics_path() {
        let cfg = InferencePoolConfig {
            name: "pool".to_owned(),
            namespace: None,
            api_version: "inference.networking.k8s.io/v1".to_owned(),
            models: vec!["model".to_owned()],
            metrics_path: "metrics".to_owned(),
        };
        assert!(
            validate_inference_pool_config(&cfg).is_err(),
            "metrics_path without leading / should be rejected"
        );
    }

    #[test]
    fn rejects_inference_pool_bad_api_version() {
        let cfg = InferencePoolConfig {
            name: "pool".to_owned(),
            namespace: None,
            api_version: "v1".to_owned(),
            models: vec!["model".to_owned()],
            metrics_path: "/metrics".to_owned(),
        };
        assert!(
            validate_inference_pool_config(&cfg).is_err(),
            "api_version without group/version should be rejected"
        );
    }

    #[test]
    fn rejects_inference_pool_api_version_extra_slash() {
        let cfg = InferencePoolConfig {
            name: "pool".to_owned(),
            namespace: None,
            api_version: "group/version/extra".to_owned(),
            models: vec!["model".to_owned()],
            metrics_path: "/metrics".to_owned(),
        };
        assert!(
            validate_inference_pool_config(&cfg).is_err(),
            "api_version with multiple slashes should be rejected"
        );
    }

    #[test]
    fn rejects_inference_pool_api_version_empty_group() {
        let cfg = InferencePoolConfig {
            name: "pool".to_owned(),
            namespace: None,
            api_version: "/v1".to_owned(),
            models: vec!["model".to_owned()],
            metrics_path: "/metrics".to_owned(),
        };
        assert!(
            validate_inference_pool_config(&cfg).is_err(),
            "api_version with empty group should be rejected"
        );
    }

    #[test]
    fn rejects_inference_pool_api_version_empty_version() {
        let cfg = InferencePoolConfig {
            name: "pool".to_owned(),
            namespace: None,
            api_version: "group/".to_owned(),
            models: vec!["model".to_owned()],
            metrics_path: "/metrics".to_owned(),
        };
        assert!(
            validate_inference_pool_config(&cfg).is_err(),
            "api_version with empty version should be rejected"
        );
    }

    // -- Gateway API config validation tests --

    #[test]
    fn accepts_gateway_api_without_endpoints_or_inference_pool() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
gateway_api:
  http_route:
    name: my-route
  models: ["llama3"]
"#,
        )
        .unwrap();
        let cfg: LlmdEndpointPickerConfig = serde_yaml::from_value(yaml).unwrap();
        assert!(
            validate_config(&cfg).is_ok(),
            "gateway_api without endpoints or inference_pool should be accepted"
        );
    }

    #[test]
    fn rejects_gateway_api_with_inference_pool() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
gateway_api:
  http_route:
    name: my-route
  models: ["llama3"]
inference_pool:
  name: pool
  models: ["llama3"]
"#,
        )
        .unwrap();
        let cfg: LlmdEndpointPickerConfig = serde_yaml::from_value(yaml).unwrap();
        let err = validate_config(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("mutually exclusive"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_gateway_api_empty_http_route_name() {
        let cfg = GatewayApiConfig {
            http_route: HttpRouteRef {
                name: String::new(),
                namespace: None,
            },
            models: vec!["model".to_owned()],
            metrics_path: "/metrics".to_owned(),
            inference_pool_api_version: "inference.networking.k8s.io/v1".to_owned(),
        };
        assert!(
            validate_gateway_api_config(&cfg).is_err(),
            "empty http_route name should be rejected"
        );
    }

    #[test]
    fn rejects_gateway_api_empty_models() {
        let cfg = GatewayApiConfig {
            http_route: HttpRouteRef {
                name: "route".to_owned(),
                namespace: None,
            },
            models: vec![],
            metrics_path: "/metrics".to_owned(),
            inference_pool_api_version: "inference.networking.k8s.io/v1".to_owned(),
        };
        assert!(
            validate_gateway_api_config(&cfg).is_err(),
            "empty models should be rejected"
        );
    }

    #[test]
    fn rejects_gateway_api_bad_metrics_path() {
        let cfg = GatewayApiConfig {
            http_route: HttpRouteRef {
                name: "route".to_owned(),
                namespace: None,
            },
            models: vec!["model".to_owned()],
            metrics_path: "metrics".to_owned(),
            inference_pool_api_version: "inference.networking.k8s.io/v1".to_owned(),
        };
        assert!(
            validate_gateway_api_config(&cfg).is_err(),
            "metrics_path without leading / should be rejected"
        );
    }

    #[test]
    fn rejects_gateway_api_bad_api_version() {
        let cfg = GatewayApiConfig {
            http_route: HttpRouteRef {
                name: "route".to_owned(),
                namespace: None,
            },
            models: vec!["model".to_owned()],
            metrics_path: "/metrics".to_owned(),
            inference_pool_api_version: "v1".to_owned(),
        };
        assert!(
            validate_gateway_api_config(&cfg).is_err(),
            "api_version without group/version should be rejected"
        );
    }

    #[test]
    fn static_endpoints_config_still_works() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
endpoints:
  - name: ep1
    address: "127.0.0.1:8000"
    models: ["model"]
"#,
        )
        .unwrap();
        let cfg: LlmdEndpointPickerConfig = serde_yaml::from_value(yaml).unwrap();
        assert!(
            validate_config(&cfg).is_ok(),
            "static endpoints config should still work"
        );
    }

    #[test]
    fn direct_inference_pool_config_still_works() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
inference_pool:
  name: pool
  models: ["model"]
"#,
        )
        .unwrap();
        let cfg: LlmdEndpointPickerConfig = serde_yaml::from_value(yaml).unwrap();
        assert!(
            validate_config(&cfg).is_ok(),
            "direct inference_pool config should still work"
        );
    }
}
