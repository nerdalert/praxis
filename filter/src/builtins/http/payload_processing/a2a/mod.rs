// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! A2A protocol classifier filter for body-aware routing.

pub(crate) mod config;
pub(crate) mod envelope;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::err_expect,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::needless_raw_strings,
    clippy::needless_raw_string_hashes,
    reason = "tests"
)]
mod tests;

use std::borrow::Cow;

use async_trait::async_trait;
use bytes::Bytes;
use tracing::trace;

use self::{
    config::{A2aConfig, build_config},
    envelope::{A2aEnvelope, extract_a2a_envelope},
};
use super::json_rpc::{
    config::JsonRpcConfig,
    contains_control_chars,
    envelope::parse_json_rpc_envelope,
};
use crate::{
    FilterAction, FilterError, Rejection,
    body::{BodyAccess, BodyMode},
    factory::parse_filter_config,
    filter::{HttpFilter, HttpFilterContext},
};

// -----------------------------------------------------------------------------
// A2aFilter
// -----------------------------------------------------------------------------

/// Extracts A2A protocol metadata from JSON-RPC request bodies and promotes
/// method, family, task presence, and streaming mode to request headers and
/// filter results for routing.
///
/// # Basic YAML
///
/// ```yaml
/// filter: a2a
/// ```
///
/// # Full YAML
///
/// ```yaml
/// filter: a2a
/// max_body_bytes: 65536
/// on_invalid: continue
/// method_aliases:
///   "message/send": "SendMessage"
///   "tasks/get": "GetTask"
/// headers:
///   method: x-praxis-a2a-method
///   family: x-praxis-a2a-family
///   task_present: x-praxis-a2a-task-present
///   streaming: x-praxis-a2a-streaming
/// ```
///
/// # Example
///
/// ```ignore
/// use praxis_filter::A2aFilter;
///
/// let yaml: serde_yaml::Value = serde_yaml::from_str(
///     r#"
/// max_body_bytes: 65536
/// on_invalid: reject
/// "#,
/// )
/// .unwrap();
/// let filter = A2aFilter::from_config(&yaml).unwrap();
/// assert_eq!(filter.name(), "a2a");
/// ```
pub struct A2aFilter {
    /// Parsed filter configuration.
    config: A2aConfig,
    /// Dummy JSON-RPC config for the shared parser.
    json_rpc_config: JsonRpcConfig,
    /// Maximum body bytes for `StreamBuffer`.
    max_body_bytes: usize,
}

impl A2aFilter {
    /// Create a filter from parsed YAML config.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if the YAML config is invalid.
    ///
    /// [`FilterError`]: crate::FilterError
    pub fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: A2aConfig = parse_filter_config("a2a", config)?;
        let (max_body_bytes, validated_config) = build_config(cfg)?;

        let json_rpc_config = JsonRpcConfig {
            max_body_bytes,
            batch_policy: super::json_rpc::config::BatchPolicy::Reject,
            on_invalid: super::json_rpc::config::InvalidJsonRpcBehavior::Continue,
            headers: super::json_rpc::config::JsonRpcHeaders {
                method: None,
                id: None,
                kind: None,
            },
        };

        Ok(Box::new(Self {
            config: validated_config,
            json_rpc_config,
            max_body_bytes,
        }))
    }
}

#[async_trait]
impl HttpFilter for A2aFilter {
    fn name(&self) -> &'static str {
        "a2a"
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
        _end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        let Some(chunk) = body.as_ref() else {
            return Ok(FilterAction::Continue);
        };

        let Ok(Some(envelope)) = parse_json_rpc_envelope(chunk, &self.json_rpc_config) else {
            return handle_non_a2a(&self.config);
        };

        let Some(method_str) = &envelope.method else {
            return handle_non_a2a(&self.config);
        };

        let a2a_envelope = extract_a2a_envelope(chunk, method_str, &self.config.method_aliases);

        write_metadata(ctx, &a2a_envelope, &envelope);
        promote_a2a_headers(&a2a_envelope, &self.config, &mut ctx.extra_request_headers);
        promote_a2a_filter_results(&a2a_envelope, ctx)?;

        trace!(
            a2a_method = a2a_envelope.method.as_str(),
            a2a_family = a2a_envelope.method.family().as_str(),
            task_present = a2a_envelope.task_id.is_some(),
            streaming = a2a_envelope.streaming,
            "extracted A2A envelope metadata"
        );

        Ok(FilterAction::Release)
    }
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// Handle non-A2A input based on the configured behavior.
#[allow(clippy::unnecessary_wraps, reason = "caller returns Result<FilterAction, FilterError> from trait method")]
fn handle_non_a2a(config: &A2aConfig) -> Result<FilterAction, FilterError> {
    match config.on_invalid {
        config::InvalidA2aBehavior::Continue => Ok(FilterAction::Continue),
        config::InvalidA2aBehavior::Reject => Ok(FilterAction::Reject(Rejection::status(400))),
    }
}

/// Write durable metadata that persists across all Pingora phases.
fn write_metadata(
    ctx: &mut HttpFilterContext<'_>,
    a2a: &A2aEnvelope,
    json_rpc: &super::json_rpc::envelope::JsonRpcEnvelope,
) {
    if let Some(method_str) = &json_rpc.method {
        ctx.set_metadata("json_rpc.method", method_str.clone());
    }
    ctx.set_metadata("a2a.method", a2a.method.as_str());
    ctx.set_metadata("a2a.family", a2a.method.family().as_str());
    if let Some(task_id) = &a2a.task_id {
        ctx.set_metadata("a2a.task_id", task_id.clone());
    }
    if let Some(context_id) = &a2a.context_id {
        ctx.set_metadata("a2a.context_id", context_id.clone());
    }
    ctx.set_metadata("a2a.streaming", a2a.streaming.to_string());
    ctx.set_metadata("json_rpc.kind", json_rpc.kind.as_str());
}

/// Promote A2A metadata to internal request headers.
fn promote_a2a_headers(
    a2a: &A2aEnvelope,
    config: &A2aConfig,
    headers: &mut Vec<(Cow<'static, str>, String)>,
) {
    if let Some(header_name) = &config.headers.method {
        let method_str = a2a.method.as_str();
        if !contains_control_chars(method_str) {
            headers.push((Cow::Owned(header_name.clone()), method_str.to_owned()));
        }
    }

    if let Some(header_name) = &config.headers.family {
        headers.push((
            Cow::Owned(header_name.clone()),
            a2a.method.family().as_str().to_owned(),
        ));
    }

    if let Some(header_name) = &config.headers.task_present {
        let present = if a2a.task_id.is_some() { "true" } else { "false" };
        headers.push((Cow::Owned(header_name.clone()), present.to_owned()));
    }

    if let Some(header_name) = &config.headers.streaming {
        let streaming = if a2a.streaming { "true" } else { "false" };
        headers.push((Cow::Owned(header_name.clone()), streaming.to_owned()));
    }
}

/// Promote A2A metadata to filter results for branch evaluation.
fn promote_a2a_filter_results(a2a: &A2aEnvelope, ctx: &mut HttpFilterContext<'_>) -> Result<(), FilterError> {
    let method = a2a.method.as_str().to_owned();
    let family = a2a.method.family().as_str().to_owned();
    let task_present = if a2a.task_id.is_some() { "true" } else { "false" };
    let streaming = if a2a.streaming { "true" } else { "false" };

    let results = ctx.filter_results.entry("a2a").or_default();
    results.set("method", method)?;
    results.set("family", family)?;
    results.set("task_present", task_present)?;
    results.set("streaming", streaming)?;
    Ok(())
}
