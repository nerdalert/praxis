// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Configuration types for the A2A filter.

use std::collections::HashMap;

use serde::Deserialize;

use crate::FilterError;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Default maximum request body size for `StreamBuffer` mode (64 `KiB`).
pub(crate) const DEFAULT_MAX_BODY_BYTES: usize = 65_536;

// -----------------------------------------------------------------------------
// InvalidA2aBehavior
// -----------------------------------------------------------------------------

/// How to handle non-A2A (non-JSON-RPC) input.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InvalidA2aBehavior {
    /// Reject with HTTP 400.
    #[default]
    Reject,
    /// Continue processing (pass through).
    Continue,
}

// -----------------------------------------------------------------------------
// A2aHeaders
// -----------------------------------------------------------------------------

/// Header configuration for A2A metadata promotion.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct A2aHeaders {
    /// Header name for the A2A method (e.g., `x-praxis-a2a-method`).
    #[serde(default = "default_method_header")]
    pub method: Option<String>,

    /// Header name for the A2A method family (e.g., `x-praxis-a2a-family`).
    #[serde(default = "default_family_header")]
    pub family: Option<String>,

    /// Header name for whether a task ID is present (e.g., `x-praxis-a2a-task-present`).
    #[serde(default = "default_task_present_header")]
    pub task_present: Option<String>,

    /// Header name for whether the method is streaming (e.g., `x-praxis-a2a-streaming`).
    #[serde(default = "default_streaming_header")]
    pub streaming: Option<String>,
}

impl Default for A2aHeaders {
    fn default() -> Self {
        Self {
            method: default_method_header(),
            family: default_family_header(),
            task_present: default_task_present_header(),
            streaming: default_streaming_header(),
        }
    }
}

/// Default method header name.
#[allow(clippy::unnecessary_wraps, reason = "serde default functions require Option return type")]
fn default_method_header() -> Option<String> {
    Some("x-praxis-a2a-method".to_owned())
}

/// Default family header name.
#[allow(clippy::unnecessary_wraps, reason = "serde default functions require Option return type")]
fn default_family_header() -> Option<String> {
    Some("x-praxis-a2a-family".to_owned())
}

/// Default task-present header name.
#[allow(clippy::unnecessary_wraps, reason = "serde default functions require Option return type")]
fn default_task_present_header() -> Option<String> {
    Some("x-praxis-a2a-task-present".to_owned())
}

/// Default streaming header name.
#[allow(clippy::unnecessary_wraps, reason = "serde default functions require Option return type")]
fn default_streaming_header() -> Option<String> {
    Some("x-praxis-a2a-streaming".to_owned())
}

// -----------------------------------------------------------------------------
// A2aConfig
// -----------------------------------------------------------------------------

/// YAML configuration for [`A2aFilter`].
///
/// [`A2aFilter`]: super::A2aFilter
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct A2aConfig {
    /// Maximum body size in bytes for `StreamBuffer`.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,

    /// How to handle non-A2A input.
    #[serde(default)]
    pub on_invalid: InvalidA2aBehavior,

    /// Maps alternate method names to canonical A2A method names.
    #[serde(default)]
    pub method_aliases: HashMap<String, String>,

    /// Header names for metadata promotion.
    #[serde(default)]
    pub headers: A2aHeaders,
}

/// Default max body bytes.
fn default_max_body_bytes() -> usize {
    DEFAULT_MAX_BODY_BYTES
}

// -----------------------------------------------------------------------------
// Config Validation
// -----------------------------------------------------------------------------

/// Validate and build the final configuration.
pub(crate) fn build_config(cfg: A2aConfig) -> Result<(usize, A2aConfig), FilterError> {
    if cfg.max_body_bytes == 0 {
        return Err("a2a: 'max_body_bytes' must be greater than 0".into());
    }

    validate_header_name("method", cfg.headers.method.as_deref())?;
    validate_header_name("family", cfg.headers.family.as_deref())?;
    validate_header_name("task_present", cfg.headers.task_present.as_deref())?;
    validate_header_name("streaming", cfg.headers.streaming.as_deref())?;

    Ok((cfg.max_body_bytes, cfg))
}

/// Validate configured header names using the HTTP header-name parser.
fn validate_header_name(field: &str, header_name: Option<&str>) -> Result<(), FilterError> {
    let Some(header_name) = header_name else {
        return Ok(());
    };

    if header_name.is_empty() {
        return Err(format!("a2a: {field} header name must not be empty").into());
    }

    if http::HeaderName::from_bytes(header_name.as_bytes()).is_err() {
        return Err(format!("a2a: {field} header name is not a valid HTTP header name").into());
    }

    Ok(())
}
