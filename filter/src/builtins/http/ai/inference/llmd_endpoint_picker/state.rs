// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Endpoint state types for the llm-d endpoint picker.

use std::sync::Arc;

use super::config::EndpointConfig;
use crate::FilterError;

// -----------------------------------------------------------------------------
// Endpoint State
// -----------------------------------------------------------------------------

/// Runtime state for one inference endpoint.
#[derive(Debug, Clone)]
pub(super) struct EndpointState {
    /// Stable endpoint name.
    pub name: Arc<str>,

    /// Upstream address in `host:port` form.
    pub address: Arc<str>,

    /// Whether this endpoint should receive new traffic.
    pub healthy: bool,

    /// Current KV-cache pressure percentage (0-100).
    pub kv_cache_usage_percent: f64,

    /// Models served by the endpoint.
    pub models: Vec<Arc<str>>,

    /// Current running request count.
    pub running_requests: u64,

    /// Current waiting request count.
    pub waiting_requests: u64,
}

impl TryFrom<EndpointConfig> for EndpointState {
    type Error = FilterError;

    fn try_from(cfg: EndpointConfig) -> Result<Self, Self::Error> {
        super::config::validate_endpoint_config(&cfg)?;

        Ok(Self {
            name: Arc::from(cfg.name),
            address: Arc::from(cfg.address),
            healthy: cfg.healthy,
            kv_cache_usage_percent: cfg.kv_cache_usage_percent,
            models: cfg.models.into_iter().map(Arc::from).collect(),
            running_requests: cfg.running_requests,
            waiting_requests: cfg.waiting_requests,
        })
    }
}

// -----------------------------------------------------------------------------
// Endpoint Snapshot
// -----------------------------------------------------------------------------

/// Immutable endpoint state snapshot read by request filters.
#[derive(Debug)]
pub(super) struct EndpointSnapshot {
    /// Endpoint states visible to request filters.
    pub endpoints: Vec<EndpointState>,
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
    fn try_from_config_creates_valid_state() {
        let cfg = EndpointConfig {
            name: "ep".to_owned(),
            address: "127.0.0.1:8000".to_owned(),
            models: vec!["model-a".to_owned()],
            running_requests: 3,
            waiting_requests: 1,
            kv_cache_usage_percent: 42.5,
            healthy: true,
        };

        let state = EndpointState::try_from(cfg).unwrap();

        assert_eq!(state.name.as_ref(), "ep", "name");
        assert_eq!(state.address.as_ref(), "127.0.0.1:8000", "address");
        assert_eq!(state.models.len(), 1, "model count");
        assert_eq!(state.running_requests, 3, "running requests");
        assert_eq!(state.kv_cache_usage_percent, 42.5, "kv cache");
    }

    #[test]
    fn try_from_config_rejects_empty_name() {
        let cfg = EndpointConfig {
            name: String::new(),
            address: "127.0.0.1:8000".to_owned(),
            models: vec!["model".to_owned()],
            running_requests: 0,
            waiting_requests: 0,
            kv_cache_usage_percent: 0.0,
            healthy: true,
        };

        assert!(EndpointState::try_from(cfg).is_err(), "empty name should be rejected");
    }
}
