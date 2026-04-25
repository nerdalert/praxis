// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Deserialized YAML configuration types for the rate limit filter.

use serde::Deserialize;

// -----------------------------------------------------------------------------
// RateLimitConfig
// -----------------------------------------------------------------------------

/// Deserialized YAML config for the rate limit filter.
#[derive(Debug, Deserialize)]
pub(super) struct RateLimitConfig {
    /// `"per_ip"`, `"global"`, or `"descriptor"`.
    pub mode: String,

    /// Tokens replenished per second.
    pub rate: f64,

    /// Maximum bucket capacity.
    pub burst: u32,

    /// Descriptor configuration (required when `mode` is `"descriptor"`).
    #[serde(default)]
    pub descriptor: Option<DescriptorConfig>,
}

// -----------------------------------------------------------------------------
// DescriptorConfig
// -----------------------------------------------------------------------------

/// Configuration for descriptor-based rate limiting.
///
/// Descriptors are composite keys built from filter context metadata
/// or trusted request headers. Each unique descriptor value gets its
/// own independent token bucket.
#[derive(Debug, Deserialize)]
pub(super) struct DescriptorConfig {
    /// Human-readable policy name for logging and metrics.
    #[serde(default = "default_descriptor_name")]
    pub name: String,

    /// Ordered list of descriptor sources.
    pub sources: Vec<DescriptorSource>,

    /// Behavior when a descriptor value cannot be resolved.
    #[serde(default)]
    pub missing: MissingBehavior,

    /// Whether header sources should be trusted.
    ///
    /// When `false` (default), header-based sources are rejected at
    /// config time unless explicitly acknowledged. This prevents
    /// accidental use of client-supplied headers for rate-limit keys.
    #[serde(default)]
    pub trusted_headers: bool,
}

/// A single source for building a descriptor key component.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum DescriptorSource {
    /// Read from filter context metadata (set by auth, model extraction, etc.).
    Context {
        /// Metadata key name (e.g. `"subscription"`, `"model"`).
        context: String,
    },

    /// Read from a request header (requires `trusted_headers: true`).
    Header {
        /// Header name (e.g. `"X-MaaS-Subscription"`).
        header: String,
    },
}

/// Behavior when a descriptor source value is missing.
#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum MissingBehavior {
    /// Reject the request with 429 (default).
    #[default]
    Reject,

    /// Skip rate limiting for this request.
    Skip,
}

// -----------------------------------------------------------------------------
// Defaults
// -----------------------------------------------------------------------------

/// Default descriptor policy name.
fn default_descriptor_name() -> String {
    "default".to_owned()
}
