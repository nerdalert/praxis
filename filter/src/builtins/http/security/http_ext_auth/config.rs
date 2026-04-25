// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Configuration types for the HTTP external auth filter.

use std::collections::HashMap;

use serde::Deserialize;

// -----------------------------------------------------------------------------
// HttpExtAuthConfig
// -----------------------------------------------------------------------------

/// Deserialized YAML config for the HTTP external auth filter.
#[derive(Debug, Deserialize)]
pub(super) struct HttpExtAuthConfig {
    /// URL of the auth service endpoint.
    pub endpoint: String,

    /// Request timeout in milliseconds.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Skip TLS certificate verification for the auth endpoint.
    /// Only for development/testing — do not use in production.
    #[serde(default)]
    pub tls_skip_verify: bool,

    /// Response handling configuration.
    #[serde(default)]
    pub response: ResponseConfig,

    /// Headers to strip from the upstream request after auth.
    #[serde(default)]
    pub strip: StripConfig,
}

/// Response parsing configuration.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ResponseConfig {
    /// Map from metadata key to JSON response field name.
    #[serde(default)]
    pub metadata: HashMap<String, String>,

    /// Map from upstream header name to JSON response field name.
    #[serde(default)]
    pub upstream_headers: HashMap<String, String>,
}

/// Headers to strip before upstream.
#[derive(Debug, Default, Deserialize)]
pub(super) struct StripConfig {
    /// Request header names to remove.
    #[serde(default)]
    pub request_headers: Vec<String>,
}

// -----------------------------------------------------------------------------
// Defaults
// -----------------------------------------------------------------------------

/// Default timeout: 500ms.
fn default_timeout_ms() -> u64 {
    500
}
