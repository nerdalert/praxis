// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Configuration types for the llm-d endpoint picker filter.

use serde::Deserialize;

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

// -----------------------------------------------------------------------------
// Top-Level Config
// -----------------------------------------------------------------------------

/// YAML configuration for the llm-d endpoint picker filter.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LlmdEndpointPickerConfig {
    /// Static endpoint definitions (at least one required).
    pub endpoints: Vec<EndpointConfig>,

    /// Weight applied to inverse KV-cache pressure scoring.
    #[serde(default = "default_kv_cache_weight")]
    pub kv_cache_weight: f64,

    /// Maximum request body size in bytes for `StreamBuffer` mode.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,

    /// Logical pool name for request metadata.
    #[serde(default = "default_pool_name")]
    pub pool_name: String,

    /// Weight applied to inverse queue-depth scoring.
    #[serde(default = "default_queue_weight")]
    pub queue_weight: f64,
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

    /// Whether this endpoint is eligible for new requests.
    #[serde(default = "default_healthy")]
    pub healthy: bool,

    /// Current KV-cache utilization percentage, 0-100.
    #[serde(default)]
    pub kv_cache_usage_percent: f64,

    /// Models served by this endpoint.
    pub models: Vec<String>,

    /// Current running request count.
    #[serde(default)]
    pub running_requests: u64,

    /// Current waiting request count.
    #[serde(default)]
    pub waiting_requests: u64,
}

// -----------------------------------------------------------------------------
// Default Helpers
// -----------------------------------------------------------------------------

/// Returns [`DEFAULT_POOL_NAME`].
fn default_pool_name() -> String {
    DEFAULT_POOL_NAME.to_owned()
}

/// Returns [`DEFAULT_JSON_BODY_MAX_BYTES`].
fn default_max_body_bytes() -> usize {
    DEFAULT_JSON_BODY_MAX_BYTES
}

/// Returns [`DEFAULT_QUEUE_WEIGHT`].
fn default_queue_weight() -> f64 {
    DEFAULT_QUEUE_WEIGHT
}

/// Returns [`DEFAULT_KV_CACHE_WEIGHT`].
fn default_kv_cache_weight() -> f64 {
    DEFAULT_KV_CACHE_WEIGHT
}

/// Returns `true`.
fn default_healthy() -> bool {
    true
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
    validate_endpoints_present(cfg)?;
    validate_scoring_weights(cfg)?;
    Ok(())
}

/// Validate that at least one endpoint is configured.
fn validate_endpoints_present(cfg: &LlmdEndpointPickerConfig) -> Result<(), FilterError> {
    if cfg.endpoints.is_empty() {
        return Err("llmd_endpoint_picker: endpoints must not be empty".into());
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
            "llmd_endpoint_picker: endpoint '{name}' has non-finite kv_cache_usage_percent",
            name = cfg.name,
        )
        .into());
    }
    if cfg.kv_cache_usage_percent < 0.0 {
        return Err(format!(
            "llmd_endpoint_picker: endpoint '{name}' has negative kv_cache_usage_percent",
            name = cfg.name,
        )
        .into());
    }
    if cfg.kv_cache_usage_percent > 100.0 {
        return Err(format!(
            "llmd_endpoint_picker: endpoint '{name}' has kv_cache_usage_percent > 100",
            name = cfg.name,
        )
        .into());
    }
    Ok(())
}

/// Validate the models list for a single endpoint.
fn validate_endpoint_models(cfg: &EndpointConfig) -> Result<(), FilterError> {
    if cfg.models.is_empty() {
        return Err(format!(
            "llmd_endpoint_picker: endpoint '{name}' must serve at least one model",
            name = cfg.name,
        )
        .into());
    }
    if cfg.models.iter().any(|m| m.trim().is_empty()) {
        return Err(format!(
            "llmd_endpoint_picker: endpoint '{name}' has an empty model",
            name = cfg.name,
        )
        .into());
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

    #[test]
    fn accepts_valid_config() {
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
        assert!(validate_config(&cfg).is_ok(), "valid config should be accepted");
    }

    #[test]
    fn rejects_empty_endpoints() {
        let yaml: serde_yaml::Value = serde_yaml::from_str("endpoints: []").unwrap();
        let cfg: LlmdEndpointPickerConfig = serde_yaml::from_value(yaml).unwrap();
        let err = validate_config(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("endpoints must not be empty"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_empty_pool_name() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
pool_name: ""
endpoints:
  - name: ep
    address: "127.0.0.1:8000"
    models: ["model"]
"#,
        )
        .unwrap();
        let cfg: LlmdEndpointPickerConfig = serde_yaml::from_value(yaml).unwrap();
        assert!(validate_config(&cfg).is_err(), "empty pool_name should be rejected");
    }

    #[test]
    fn rejects_zero_max_body_bytes() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
max_body_bytes: 0
endpoints:
  - name: ep
    address: "127.0.0.1:8000"
    models: ["model"]
"#,
        )
        .unwrap();
        let cfg: LlmdEndpointPickerConfig = serde_yaml::from_value(yaml).unwrap();
        assert!(validate_config(&cfg).is_err(), "zero max_body_bytes should be rejected");
    }

    #[test]
    fn rejects_negative_queue_weight() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
queue_weight: -1.0
endpoints:
  - name: ep
    address: "127.0.0.1:8000"
    models: ["model"]
"#,
        )
        .unwrap();
        let cfg: LlmdEndpointPickerConfig = serde_yaml::from_value(yaml).unwrap();
        assert!(
            validate_config(&cfg).is_err(),
            "negative queue_weight should be rejected"
        );
    }

    #[test]
    fn rejects_infinite_kv_cache_weight() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
kv_cache_weight: .inf
endpoints:
  - name: ep
    address: "127.0.0.1:8000"
    models: ["model"]
"#,
        )
        .unwrap();
        let cfg: LlmdEndpointPickerConfig = serde_yaml::from_value(yaml).unwrap();
        assert!(
            validate_config(&cfg).is_err(),
            "infinite kv_cache_weight should be rejected"
        );
    }

    #[test]
    fn endpoint_rejects_empty_name() {
        let cfg = EndpointConfig {
            name: String::new(),
            address: "127.0.0.1:8000".to_owned(),
            models: vec!["model".to_owned()],
            running_requests: 0,
            waiting_requests: 0,
            kv_cache_usage_percent: 0.0,
            healthy: true,
        };
        assert!(validate_endpoint_config(&cfg).is_err(), "empty name should be rejected");
    }

    #[test]
    fn endpoint_rejects_empty_address() {
        let cfg = EndpointConfig {
            name: "ep".to_owned(),
            address: String::new(),
            models: vec!["model".to_owned()],
            running_requests: 0,
            waiting_requests: 0,
            kv_cache_usage_percent: 0.0,
            healthy: true,
        };
        assert!(
            validate_endpoint_config(&cfg).is_err(),
            "empty address should be rejected"
        );
    }

    #[test]
    fn endpoint_rejects_empty_models() {
        let cfg = EndpointConfig {
            name: "ep".to_owned(),
            address: "127.0.0.1:8000".to_owned(),
            models: vec![],
            running_requests: 0,
            waiting_requests: 0,
            kv_cache_usage_percent: 0.0,
            healthy: true,
        };
        assert!(
            validate_endpoint_config(&cfg).is_err(),
            "empty models should be rejected"
        );
    }

    #[test]
    fn endpoint_rejects_non_finite_kv_cache() {
        let cfg = EndpointConfig {
            name: "ep".to_owned(),
            address: "127.0.0.1:8000".to_owned(),
            models: vec!["model".to_owned()],
            running_requests: 0,
            waiting_requests: 0,
            kv_cache_usage_percent: f64::INFINITY,
            healthy: true,
        };
        assert!(
            validate_endpoint_config(&cfg).is_err(),
            "non-finite kv_cache should be rejected"
        );
    }

    #[test]
    fn endpoint_rejects_negative_kv_cache() {
        let cfg = EndpointConfig {
            name: "ep".to_owned(),
            address: "127.0.0.1:8000".to_owned(),
            models: vec!["model".to_owned()],
            running_requests: 0,
            waiting_requests: 0,
            kv_cache_usage_percent: -1.0,
            healthy: true,
        };
        assert!(
            validate_endpoint_config(&cfg).is_err(),
            "negative kv_cache should be rejected"
        );
    }

    #[test]
    fn endpoint_rejects_kv_cache_over_100() {
        let cfg = EndpointConfig {
            name: "ep".to_owned(),
            address: "127.0.0.1:8000".to_owned(),
            models: vec!["model".to_owned()],
            running_requests: 0,
            waiting_requests: 0,
            kv_cache_usage_percent: 100.1,
            healthy: true,
        };
        assert!(
            validate_endpoint_config(&cfg).is_err(),
            "kv_cache > 100 should be rejected"
        );
    }

    #[test]
    fn defaults_applied_correctly() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
endpoints:
  - name: ep
    address: "127.0.0.1:8000"
    models: ["m"]
"#,
        )
        .unwrap();
        let cfg: LlmdEndpointPickerConfig = serde_yaml::from_value(yaml).unwrap();

        assert_eq!(cfg.pool_name, DEFAULT_POOL_NAME, "default pool_name");
        assert_eq!(cfg.queue_weight, DEFAULT_QUEUE_WEIGHT, "default queue_weight");
        assert_eq!(cfg.kv_cache_weight, DEFAULT_KV_CACHE_WEIGHT, "default kv_cache_weight");
        assert_eq!(
            cfg.max_body_bytes, DEFAULT_JSON_BODY_MAX_BYTES,
            "default max_body_bytes"
        );
    }

    #[test]
    fn endpoint_defaults_applied() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
name: ep
address: "127.0.0.1:8000"
models: ["m"]
"#,
        )
        .unwrap();
        let cfg: EndpointConfig = serde_yaml::from_value(yaml).unwrap();

        assert_eq!(cfg.running_requests, 0, "default running_requests");
        assert_eq!(cfg.waiting_requests, 0, "default waiting_requests");
        assert_eq!(cfg.kv_cache_usage_percent, 0.0, "default kv_cache");
        assert!(cfg.healthy, "default healthy");
    }
}
