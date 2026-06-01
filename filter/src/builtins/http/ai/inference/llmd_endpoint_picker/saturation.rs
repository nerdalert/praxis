// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Saturation/admission gate for the llm-d endpoint picker.
//!
//! Provides deterministic saturation scoring based on queue depth and
//! KV-cache utilization. When the pool-level saturation exceeds a
//! configured threshold the gate rejects the request, preventing
//! cascading overload.

use serde::Deserialize;

use super::state::EndpointState;
use crate::FilterError;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Default queue depth threshold for saturation scoring.
const DEFAULT_QUEUE_DEPTH_THRESHOLD: u64 = 5;

/// Default KV-cache utilization threshold (fraction, 0-1).
const DEFAULT_KV_CACHE_UTIL_THRESHOLD: f64 = 0.8;

/// Default pool saturation threshold.
const DEFAULT_POOL_SATURATION_THRESHOLD: f64 = 1.0;

/// Default headroom factor for endpoint filtering.
const DEFAULT_HEADROOM: f64 = 0.2;

/// Default HTTP status code for rejection.
const DEFAULT_REJECT_STATUS: u16 = 429; // Too Many Requests

/// Default priority headroom per level (no effect).
const DEFAULT_PRIORITY_HEADROOM_PER_LEVEL: f64 = 0.0;

// -----------------------------------------------------------------------------
// Config
// -----------------------------------------------------------------------------

/// Configuration for the saturation/admission gate.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SaturationGateConfig {
    /// Whether the saturation gate is enabled.
    pub enabled: bool,

    /// Queue depth at which an endpoint is considered fully saturated.
    #[serde(default = "default_queue_depth_threshold")]
    pub queue_depth_threshold: u64,

    /// KV-cache utilization fraction (0-1) at which an endpoint is
    /// considered fully saturated.
    #[serde(default = "default_kv_cache_util_threshold")]
    pub kv_cache_util_threshold: f64,

    /// Pool-level saturation threshold above which requests are rejected.
    #[serde(default = "default_pool_saturation_threshold")]
    pub pool_saturation_threshold: f64,

    /// Headroom factor added to per-endpoint thresholds when filtering
    /// individual overloaded endpoints.
    #[serde(default = "default_headroom")]
    pub headroom: f64,

    /// HTTP status code returned when the gate rejects a request.
    #[serde(default = "default_reject_status")]
    pub reject_status: u16,

    /// Priority headroom per priority level. Each unit of priority
    /// adds this much to the pool saturation threshold.
    #[serde(default = "default_priority_headroom_per_level")]
    pub priority_headroom_per_level: f64,
}

// -----------------------------------------------------------------------------
// Default Helpers
// -----------------------------------------------------------------------------

/// Default queue depth threshold.
fn default_queue_depth_threshold() -> u64 {
    DEFAULT_QUEUE_DEPTH_THRESHOLD
}

/// Default KV-cache utilization threshold.
fn default_kv_cache_util_threshold() -> f64 {
    DEFAULT_KV_CACHE_UTIL_THRESHOLD
}

/// Default pool saturation threshold.
fn default_pool_saturation_threshold() -> f64 {
    DEFAULT_POOL_SATURATION_THRESHOLD
}

/// Default headroom factor.
fn default_headroom() -> f64 {
    DEFAULT_HEADROOM
}

/// Default rejection HTTP status code.
fn default_reject_status() -> u16 {
    DEFAULT_REJECT_STATUS
}

/// Default priority headroom per level.
fn default_priority_headroom_per_level() -> f64 {
    DEFAULT_PRIORITY_HEADROOM_PER_LEVEL
}

// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

/// Validate saturation gate configuration.
///
/// # Errors
///
/// Returns [`FilterError`] when any field has an invalid value.
pub(super) fn validate_saturation_gate_config(cfg: &SaturationGateConfig) -> Result<(), FilterError> {
    validate_queue_depth(cfg)?;
    validate_kv_cache_threshold(cfg)?;
    validate_pool_saturation(cfg)?;
    validate_headroom(cfg)?;
    validate_reject_status(cfg)?;
    validate_priority_headroom(cfg)?;
    Ok(())
}

/// Validate queue depth threshold.
fn validate_queue_depth(cfg: &SaturationGateConfig) -> Result<(), FilterError> {
    if cfg.queue_depth_threshold == 0 {
        return Err("llmd_endpoint_picker: saturation_gate.queue_depth_threshold must be greater than zero".into());
    }
    Ok(())
}

/// Validate KV-cache utilization threshold.
fn validate_kv_cache_threshold(cfg: &SaturationGateConfig) -> Result<(), FilterError> {
    if !cfg.kv_cache_util_threshold.is_finite()
        || cfg.kv_cache_util_threshold <= 0.0
        || cfg.kv_cache_util_threshold > 1.0
    {
        return Err(
            "llmd_endpoint_picker: saturation_gate.kv_cache_util_threshold must be finite and in (0.0, 1.0]".into(),
        );
    }
    Ok(())
}

/// Validate pool saturation threshold.
fn validate_pool_saturation(cfg: &SaturationGateConfig) -> Result<(), FilterError> {
    if cfg.pool_saturation_threshold <= 0.0 || !cfg.pool_saturation_threshold.is_finite() {
        return Err(
            "llmd_endpoint_picker: saturation_gate.pool_saturation_threshold must be positive and finite".into(),
        );
    }
    Ok(())
}

/// Validate headroom factor.
fn validate_headroom(cfg: &SaturationGateConfig) -> Result<(), FilterError> {
    if cfg.headroom < 0.0 || !cfg.headroom.is_finite() {
        return Err("llmd_endpoint_picker: saturation_gate.headroom must be non-negative and finite".into());
    }
    Ok(())
}

/// Validate rejection HTTP status code.
fn validate_reject_status(cfg: &SaturationGateConfig) -> Result<(), FilterError> {
    if !(400..=599).contains(&cfg.reject_status) {
        return Err("llmd_endpoint_picker: saturation_gate.reject_status must be 400..=599".into());
    }
    Ok(())
}

/// Validate priority headroom per level.
fn validate_priority_headroom(cfg: &SaturationGateConfig) -> Result<(), FilterError> {
    if cfg.priority_headroom_per_level < 0.0 || !cfg.priority_headroom_per_level.is_finite() {
        return Err(
            "llmd_endpoint_picker: saturation_gate.priority_headroom_per_level must be non-negative and finite".into(),
        );
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Saturation Scoring
// -----------------------------------------------------------------------------

/// Compute saturation for one endpoint.
///
/// Uses `waiting_requests` (not running+waiting) and `kv_cache` as a
/// fraction (0-1). Returns the maximum of the queue ratio and KV ratio.
#[allow(
    clippy::cast_precision_loss,
    reason = "queue depth values are small enough that f64 is lossless"
)]
pub(super) fn endpoint_saturation(
    waiting_requests: u64,
    kv_cache_usage_percent: f64,
    config: &SaturationGateConfig,
) -> f64 {
    let queue_ratio = waiting_requests as f64 / config.queue_depth_threshold as f64;
    let kv_fraction = kv_cache_usage_percent / 100.0;
    let kv_ratio = kv_fraction / config.kv_cache_util_threshold;
    queue_ratio.max(kv_ratio)
}

/// Compute pool saturation as the average of candidate endpoint
/// saturations.
pub(super) fn pool_saturation(candidates: &[&EndpointState], config: &SaturationGateConfig) -> f64 {
    if candidates.is_empty() {
        return 0.0;
    }
    let total: f64 = candidates
        .iter()
        .map(|ep| endpoint_saturation(ep.waiting_requests, ep.kv_cache_usage_percent, config))
        .sum();
    #[allow(
        clippy::cast_precision_loss,
        reason = "candidate count is small enough that f64 is lossless"
    )]
    let avg = total / candidates.len() as f64;
    avg
}

/// Filter overloaded endpoints using headroom-adjusted limits.
///
/// Returns the filtered list, or the original list if filtering would
/// remove all candidates (fail-open behavior).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "max_queue is bounded by config values that are small positive numbers"
)]
pub(super) fn filter_saturated_endpoints<'a>(
    candidates: Vec<&'a EndpointState>,
    config: &SaturationGateConfig,
) -> Vec<&'a EndpointState> {
    let max_queue = compute_max_queue(config);
    let max_kv = compute_max_kv(config);
    let filtered: Vec<_> = candidates
        .iter()
        .filter(|ep| ep.waiting_requests <= max_queue && ep.kv_cache_usage_percent <= max_kv)
        .copied()
        .collect();
    if filtered.is_empty() { candidates } else { filtered }
}

/// Compute the headroom-adjusted maximum queue depth.
///
/// Uses `ceil()` so fractional headroom values round up rather than
/// silently truncating. For example, threshold=3 with headroom=0.2
/// gives ceil(3.6) = 4, admitting up to 4 waiting requests.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "queue threshold is a small positive u64; headroom is a small positive f64"
)]
fn compute_max_queue(config: &SaturationGateConfig) -> u64 {
    (config.queue_depth_threshold as f64 * (1.0 + config.headroom)).ceil() as u64
}

/// Compute the headroom-adjusted maximum KV-cache percentage.
fn compute_max_kv(config: &SaturationGateConfig) -> f64 {
    (config.kv_cache_util_threshold * (1.0 + config.headroom)).min(1.0) * 100.0
}

/// Public accessor for the headroom-adjusted maximum queue depth.
///
/// Used by prefill saturation filtering which does not fail-open
/// at the per-endpoint level.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "queue threshold is a small positive u64; headroom is a small positive f64"
)]
pub(super) fn compute_max_queue_for_config(config: &SaturationGateConfig) -> u64 {
    compute_max_queue(config)
}

/// Public accessor for the headroom-adjusted maximum KV-cache
/// percentage.
///
/// Used by prefill saturation filtering which does not fail-open
/// at the per-endpoint level.
pub(super) fn compute_max_kv_for_config(config: &SaturationGateConfig) -> f64 {
    compute_max_kv(config)
}

/// Compute the effective pool saturation threshold for a given
/// request priority.
///
/// Higher priorities raise the threshold, allowing admission at
/// higher saturation. Negative priorities lower it. The result
/// is clamped to at least 0.0.
pub(super) fn compute_effective_threshold(config: &SaturationGateConfig, priority: i32) -> f64 {
    let headroom = f64::from(priority) * config.priority_headroom_per_level;
    (config.pool_saturation_threshold + headroom).max(0.0)
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
    use std::sync::Arc;

    use super::*;

    // -- Config defaults --

    #[test]
    fn config_defaults_are_valid() {
        let cfg = make_default_config();
        assert!(
            validate_saturation_gate_config(&cfg).is_ok(),
            "default config should be valid"
        );
    }

    #[test]
    fn config_defaults_have_expected_values() {
        let cfg = make_default_config();
        assert_eq!(cfg.queue_depth_threshold, 5, "default queue_depth_threshold");
        assert!(
            (cfg.kv_cache_util_threshold - 0.8).abs() < f64::EPSILON,
            "default kv_cache_util_threshold"
        );
        assert!(
            (cfg.pool_saturation_threshold - 1.0).abs() < f64::EPSILON,
            "default pool_saturation_threshold"
        );
        assert!((cfg.headroom - 0.2).abs() < f64::EPSILON, "default headroom");
        assert_eq!(cfg.reject_status, 429, "default reject_status");
    }

    // -- Config validation rejection cases --

    #[test]
    fn config_rejects_zero_queue_depth() {
        let mut cfg = make_default_config();
        cfg.queue_depth_threshold = 0;
        assert!(
            validate_saturation_gate_config(&cfg).is_err(),
            "zero queue_depth_threshold should be rejected"
        );
    }

    #[test]
    fn config_rejects_zero_kv_cache_threshold() {
        let mut cfg = make_default_config();
        cfg.kv_cache_util_threshold = 0.0;
        assert!(
            validate_saturation_gate_config(&cfg).is_err(),
            "zero kv_cache_util_threshold should be rejected"
        );
    }

    #[test]
    fn config_rejects_kv_cache_threshold_above_one() {
        let mut cfg = make_default_config();
        cfg.kv_cache_util_threshold = 1.1;
        assert!(
            validate_saturation_gate_config(&cfg).is_err(),
            "kv_cache_util_threshold > 1.0 should be rejected"
        );
    }

    #[test]
    fn config_rejects_nan_kv_cache_threshold() {
        let mut cfg = make_default_config();
        cfg.kv_cache_util_threshold = f64::NAN;
        assert!(
            validate_saturation_gate_config(&cfg).is_err(),
            "NaN kv_cache_util_threshold should be rejected"
        );
    }

    #[test]
    fn config_rejects_infinite_kv_cache_threshold() {
        let mut cfg = make_default_config();
        cfg.kv_cache_util_threshold = f64::INFINITY;
        assert!(
            validate_saturation_gate_config(&cfg).is_err(),
            "infinite kv_cache_util_threshold should be rejected"
        );
    }

    #[test]
    fn config_accepts_valid_kv_cache_threshold() {
        let mut cfg = make_default_config();
        cfg.kv_cache_util_threshold = 0.8;
        assert!(
            validate_saturation_gate_config(&cfg).is_ok(),
            "0.8 kv_cache_util_threshold should be accepted"
        );
    }

    #[test]
    fn config_rejects_zero_pool_saturation_threshold() {
        let mut cfg = make_default_config();
        cfg.pool_saturation_threshold = 0.0;
        assert!(
            validate_saturation_gate_config(&cfg).is_err(),
            "zero pool_saturation_threshold should be rejected"
        );
    }

    #[test]
    fn config_rejects_infinite_pool_saturation_threshold() {
        let mut cfg = make_default_config();
        cfg.pool_saturation_threshold = f64::INFINITY;
        assert!(
            validate_saturation_gate_config(&cfg).is_err(),
            "infinite pool_saturation_threshold should be rejected"
        );
    }

    #[test]
    fn config_rejects_negative_headroom() {
        let mut cfg = make_default_config();
        cfg.headroom = -0.1;
        assert!(
            validate_saturation_gate_config(&cfg).is_err(),
            "negative headroom should be rejected"
        );
    }

    #[test]
    fn config_rejects_infinite_headroom() {
        let mut cfg = make_default_config();
        cfg.headroom = f64::INFINITY;
        assert!(
            validate_saturation_gate_config(&cfg).is_err(),
            "infinite headroom should be rejected"
        );
    }

    #[test]
    fn config_rejects_reject_status_below_400() {
        let mut cfg = make_default_config();
        cfg.reject_status = 200;
        assert!(
            validate_saturation_gate_config(&cfg).is_err(),
            "reject_status < 400 should be rejected"
        );
    }

    #[test]
    fn config_rejects_reject_status_above_599() {
        let mut cfg = make_default_config();
        cfg.reject_status = 600;
        assert!(
            validate_saturation_gate_config(&cfg).is_err(),
            "reject_status > 599 should be rejected"
        );
    }

    // -- Endpoint saturation formula --

    #[test]
    fn endpoint_saturation_uses_max_of_queue_and_kv() {
        let cfg = make_default_config();

        let sat_queue_dominant = endpoint_saturation(10, 0.0, &cfg);
        assert!(
            (sat_queue_dominant - 2.0).abs() < f64::EPSILON,
            "10 waiting / 5 threshold = 2.0, got {sat_queue_dominant}"
        );

        let sat_kv_dominant = endpoint_saturation(0, 100.0, &cfg);
        let expected_kv = 1.0 / 0.8;
        assert!(
            (sat_kv_dominant - expected_kv).abs() < f64::EPSILON,
            "100% KV / 80% threshold = {expected_kv}, got {sat_kv_dominant}"
        );

        let sat_equal = endpoint_saturation(5, 80.0, &cfg);
        assert!(
            (sat_equal - 1.0).abs() < f64::EPSILON,
            "both at threshold should give 1.0, got {sat_equal}"
        );
    }

    #[test]
    fn endpoint_saturation_zero_load_returns_zero() {
        let cfg = make_default_config();
        let sat = endpoint_saturation(0, 0.0, &cfg);
        assert!(
            sat.abs() < f64::EPSILON,
            "zero load should give zero saturation, got {sat}"
        );
    }

    // -- Pool saturation --

    #[test]
    fn pool_saturation_empty_candidates_returns_zero() {
        let cfg = make_default_config();
        let sat = pool_saturation(&[], &cfg);
        assert!(sat.abs() < f64::EPSILON, "empty candidates should give zero, got {sat}");
    }

    #[test]
    fn pool_saturation_averages_endpoint_saturations() {
        let cfg = make_default_config();
        let ep1 = make_endpoint("ep1", 10, 0.0);
        let ep2 = make_endpoint("ep2", 0, 0.0);
        let candidates: Vec<&EndpointState> = vec![&ep1, &ep2];
        let sat = pool_saturation(&candidates, &cfg);
        assert!(
            (sat - 1.0).abs() < f64::EPSILON,
            "average of 2.0 and 0.0 should be 1.0, got {sat}"
        );
    }

    // -- Pool-level rejection --

    #[test]
    fn pool_saturation_at_threshold_triggers_reject() {
        let cfg = make_default_config();
        let ep1 = make_endpoint("ep1", 5, 80.0);
        let ep2 = make_endpoint("ep2", 5, 80.0);
        let candidates: Vec<&EndpointState> = vec![&ep1, &ep2];
        let sat = pool_saturation(&candidates, &cfg);
        assert!(
            sat >= cfg.pool_saturation_threshold,
            "pool saturation {sat} should be >= threshold {}",
            cfg.pool_saturation_threshold
        );
    }

    // -- Low saturation admits --

    #[test]
    fn low_saturation_admits_request() {
        let cfg = make_default_config();
        let ep1 = make_endpoint("ep1", 1, 10.0);
        let ep2 = make_endpoint("ep2", 0, 5.0);
        let candidates: Vec<&EndpointState> = vec![&ep1, &ep2];
        let sat = pool_saturation(&candidates, &cfg);
        assert!(
            sat < cfg.pool_saturation_threshold,
            "pool saturation {sat} should be < threshold {}",
            cfg.pool_saturation_threshold
        );
    }

    // -- Endpoint filtering --

    #[test]
    fn filter_removes_overloaded_endpoints() {
        let cfg = make_default_config();
        let healthy = make_endpoint("healthy", 1, 10.0);
        let overloaded = make_endpoint("overloaded", 20, 99.0);
        let candidates = vec![&healthy, &overloaded];
        let filtered = filter_saturated_endpoints(candidates, &cfg);
        assert_eq!(filtered.len(), 1, "overloaded endpoint should be removed");
        assert_eq!(
            filtered[0].name.as_ref(),
            "healthy",
            "only the healthy endpoint should remain"
        );
    }

    // -- Fail-open --

    #[test]
    fn filter_fails_open_when_all_would_be_removed() {
        let cfg = make_default_config();
        let ep1 = make_endpoint("ep1", 20, 99.0);
        let ep2 = make_endpoint("ep2", 20, 99.0);
        let candidates = vec![&ep1, &ep2];
        let filtered = filter_saturated_endpoints(candidates, &cfg);
        assert_eq!(
            filtered.len(),
            2,
            "should fail-open and return all candidates when filtering would remove all"
        );
    }

    // -- Fractional headroom --

    #[test]
    fn fractional_headroom_rounds_up_queue_limit() {
        let mut cfg = make_default_config();
        cfg.queue_depth_threshold = 3;
        cfg.headroom = 0.2;
        let healthy = make_endpoint("healthy", 4, 0.0);
        let candidates = vec![&healthy];
        let filtered = filter_saturated_endpoints(candidates, &cfg);
        assert_eq!(filtered.len(), 1, "waiting=4 should be admitted: ceil(3 * 1.2) = 4");
    }

    // -- Config disabled preserves behavior --

    #[test]
    fn disabled_config_deserializes() {
        let yaml = "
enabled: false
";
        let cfg: SaturationGateConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(!cfg.enabled, "config should be disabled");
        assert_eq!(cfg.queue_depth_threshold, 5, "defaults should apply when disabled");
    }

    // -- Priority headroom config defaults --

    #[test]
    fn config_default_priority_headroom_is_zero() {
        let cfg = make_default_config();
        assert!(
            cfg.priority_headroom_per_level.abs() < f64::EPSILON,
            "default priority_headroom_per_level should be 0.0"
        );
    }

    // -- Priority headroom validation --

    #[test]
    fn config_rejects_negative_priority_headroom() {
        let mut cfg = make_default_config();
        cfg.priority_headroom_per_level = -0.1;
        assert!(
            validate_saturation_gate_config(&cfg).is_err(),
            "negative priority_headroom_per_level should be rejected"
        );
    }

    #[test]
    fn config_accepts_zero_priority_headroom() {
        let mut cfg = make_default_config();
        cfg.priority_headroom_per_level = 0.0;
        assert!(
            validate_saturation_gate_config(&cfg).is_ok(),
            "zero priority_headroom_per_level should be accepted"
        );
    }

    #[test]
    fn config_accepts_positive_priority_headroom() {
        let mut cfg = make_default_config();
        cfg.priority_headroom_per_level = 0.1;
        assert!(
            validate_saturation_gate_config(&cfg).is_ok(),
            "positive priority_headroom_per_level should be accepted"
        );
    }

    #[test]
    fn config_rejects_infinite_priority_headroom() {
        let mut cfg = make_default_config();
        cfg.priority_headroom_per_level = f64::INFINITY;
        assert!(
            validate_saturation_gate_config(&cfg).is_err(),
            "infinite priority_headroom_per_level should be rejected"
        );
    }

    #[test]
    fn config_rejects_nan_priority_headroom() {
        let mut cfg = make_default_config();
        cfg.priority_headroom_per_level = f64::NAN;
        assert!(
            validate_saturation_gate_config(&cfg).is_err(),
            "NaN priority_headroom_per_level should be rejected"
        );
    }

    // -- Effective threshold computation --

    #[test]
    fn effective_threshold_zero_headroom_equals_base() {
        let cfg = make_default_config();
        let threshold = compute_effective_threshold(&cfg, 0);
        assert!(
            (threshold - cfg.pool_saturation_threshold).abs() < f64::EPSILON,
            "zero priority should use base threshold"
        );
    }

    #[test]
    fn effective_threshold_zero_priority_uses_base() {
        let mut cfg = make_default_config();
        cfg.priority_headroom_per_level = 0.1;
        let threshold = compute_effective_threshold(&cfg, 0);
        assert!(
            (threshold - cfg.pool_saturation_threshold).abs() < f64::EPSILON,
            "zero priority should use base threshold even with headroom"
        );
    }

    #[test]
    fn effective_threshold_positive_priority_raises() {
        let mut cfg = make_default_config();
        cfg.priority_headroom_per_level = 0.1;
        let threshold = compute_effective_threshold(&cfg, 10);
        let expected = 1.0 + 10.0 * 0.1; // 2.0
        assert!(
            (threshold - expected).abs() < f64::EPSILON,
            "priority 10 with headroom 0.1 should give {expected}, got {threshold}"
        );
    }

    #[test]
    fn effective_threshold_negative_priority_lowers() {
        let mut cfg = make_default_config();
        cfg.priority_headroom_per_level = 0.1;
        let threshold = compute_effective_threshold(&cfg, -5);
        let expected = 1.0 + (-5.0) * 0.1; // 0.5
        assert!(
            (threshold - expected).abs() < f64::EPSILON,
            "priority -5 with headroom 0.1 should give {expected}, got {threshold}"
        );
    }

    #[test]
    fn effective_threshold_clamped_to_zero() {
        let mut cfg = make_default_config();
        cfg.priority_headroom_per_level = 0.1;
        let threshold = compute_effective_threshold(&cfg, -100);
        assert!(
            threshold.abs() < f64::EPSILON,
            "deeply negative priority should clamp threshold to 0.0, got {threshold}"
        );
    }

    #[test]
    fn effective_threshold_preserves_behavior_when_headroom_zero() {
        let cfg = make_default_config();
        for priority in [-10, -1, 0, 1, 10, 100] {
            let threshold = compute_effective_threshold(&cfg, priority);
            assert!(
                (threshold - cfg.pool_saturation_threshold).abs() < f64::EPSILON,
                "priority {priority} with zero headroom should use base threshold"
            );
        }
    }

    // -- Test Utilities --

    fn make_default_config() -> SaturationGateConfig {
        SaturationGateConfig {
            enabled: true,
            queue_depth_threshold: default_queue_depth_threshold(),
            kv_cache_util_threshold: default_kv_cache_util_threshold(),
            pool_saturation_threshold: default_pool_saturation_threshold(),
            headroom: default_headroom(),
            reject_status: default_reject_status(),
            priority_headroom_per_level: default_priority_headroom_per_level(),
        }
    }

    fn make_endpoint(name: &str, waiting: u64, kv: f64) -> EndpointState {
        EndpointState {
            name: Arc::from(name),
            address: Arc::from("127.0.0.1:8000"),
            models: vec![Arc::from("test-model")],
            running_requests: 0,
            waiting_requests: waiting,
            kv_cache_usage_percent: kv,
            healthy: true,
            metrics_url: None,
            role: crate::builtins::http::ai::inference::llmd_endpoint_picker::disaggregation::default_endpoint_role(),
        }
    }
}
