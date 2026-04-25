// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Token bucket rate limiter with global, per-IP, and descriptor modes.

mod config;
mod limiter;

#[cfg(test)]
mod tests;

use std::{borrow::Cow, net::IpAddr, time::Instant};

use async_trait::async_trait;
use dashmap::DashMap;

use self::config::{DescriptorConfig, DescriptorSource, MissingBehavior, RateLimitConfig};
use super::token_bucket::TokenBucket;
use crate::{
    FilterAction, FilterError, Rejection,
    factory::parse_filter_config,
    filter::{HttpFilter, HttpFilterContext},
};

// -----------------------------------------------------------------------------
// Rate-Limiter Constants
// -----------------------------------------------------------------------------

/// Maximum number of per-IP entries before eviction is triggered.
const MAX_PER_IP_ENTRIES: usize = 100_000;

/// Maximum number of descriptor entries before eviction is triggered.
const MAX_DESCRIPTOR_ENTRIES: usize = 100_000;

/// Maximum entries to scan during a single eviction pass.
const EVICTION_SCAN_LIMIT: usize = 128;

/// Rate limit header: maximum bucket capacity.
const HEADER_RATELIMIT_LIMIT: &str = "X-RateLimit-Limit";

/// Rate limit header: remaining tokens.
const HEADER_RATELIMIT_REMAINING: &str = "X-RateLimit-Remaining";

/// Rate limit header: Unix timestamp when the bucket fully refills.
const HEADER_RATELIMIT_RESET: &str = "X-RateLimit-Reset";

// -----------------------------------------------------------------------------
// RateLimitState
// -----------------------------------------------------------------------------

/// Per-filter state: global bucket, per-IP buckets, or descriptor-keyed buckets.
enum RateLimitState {
    /// One shared bucket for all clients.
    Global(TokenBucket),

    /// Independent bucket per source IP address.
    PerIp(DashMap<IpAddr, TokenBucket>),

    /// Independent bucket per descriptor key string.
    Descriptor(DashMap<String, TokenBucket>),
}

// -----------------------------------------------------------------------------
// DescriptorPolicy
// -----------------------------------------------------------------------------

/// Resolved descriptor policy from config, used at runtime.
struct DescriptorPolicy {
    /// Policy name for logging and metrics.
    name: String,

    /// Ordered sources for building the composite key.
    sources: Vec<DescriptorSource>,

    /// Behavior when a source value is missing.
    missing: MissingBehavior,
}

// -----------------------------------------------------------------------------
// RateLimitFilter
// -----------------------------------------------------------------------------

/// Token bucket rate limiter that rejects excess traffic with 429.
///
/// Supports `global` (one shared bucket), `per_ip` (one bucket per
/// source IP), and `descriptor` (one bucket per composite key built
/// from context metadata or trusted headers) modes.
///
/// # YAML configuration
///
/// ```yaml
/// filter: rate_limit
/// mode: per_ip        # "per_ip", "global", or "descriptor"
/// rate: 100           # tokens per second
/// burst: 200          # max bucket capacity
/// ```
///
/// Descriptor mode:
///
/// ```yaml
/// filter: rate_limit
/// mode: descriptor
/// rate: 10
/// burst: 20
/// descriptor:
///   name: maas-subscription-model
///   sources:
///     - context: subscription
///     - context: model
///   missing: reject
/// ```
///
/// # Example
///
/// ```ignore
/// use praxis_filter::RateLimitFilter;
///
/// let yaml: serde_yaml::Value = serde_yaml::from_str(
///     r#"
/// mode: global
/// rate: 50
/// burst: 100
/// "#,
/// )
/// .unwrap();
/// let filter = RateLimitFilter::from_config(&yaml).unwrap();
/// assert_eq!(filter.name(), "rate_limit");
/// ```
///
/// [`DashMap`]: dashmap::DashMap
pub struct RateLimitFilter {
    /// Bucket state (global, per-IP, or descriptor).
    pub(self) state: RateLimitState,

    /// Tokens replenished per second.
    pub(self) rate: f64,

    /// Maximum bucket capacity.
    pub(self) burst: f64,

    /// Monotonic clock reference; all timestamps are offsets from this.
    pub(self) epoch: Instant,

    /// Descriptor policy (only for descriptor mode).
    pub(self) descriptor_policy: Option<DescriptorPolicy>,
}

impl RateLimitFilter {
    /// Create a rate limit filter from parsed YAML config.
    ///
    /// # Errors
    ///
    /// Returns an error if any field is missing, `rate` is not
    /// positive, `burst` is zero, `burst < rate`, or `mode` is
    /// unrecognised.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use praxis_filter::RateLimitFilter;
    ///
    /// let yaml: serde_yaml::Value = serde_yaml::from_str(
    ///     r#"
    /// mode: per_ip
    /// rate: 100
    /// burst: 200
    /// "#,
    /// )
    /// .unwrap();
    /// let filter = RateLimitFilter::from_config(&yaml).unwrap();
    /// assert_eq!(filter.name(), "rate_limit");
    ///
    /// // Invalid: rate is zero.
    /// let bad: serde_yaml::Value = serde_yaml::from_str("mode: global\nrate: 0\nburst: 10").unwrap();
    /// assert!(RateLimitFilter::from_config(&bad).is_err());
    /// ```
    pub fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: RateLimitConfig = parse_filter_config("rate_limit", config)?;

        if cfg.rate <= 0.0 {
            return Err("rate_limit: rate must be greater than 0".into());
        }
        if cfg.burst == 0 {
            return Err("rate_limit: burst must be at least 1".into());
        }
        if f64::from(cfg.burst) < cfg.rate {
            return Err("rate_limit: burst must be >= rate".into());
        }

        let burst = f64::from(cfg.burst);
        let (state, descriptor_policy) = match cfg.mode.as_str() {
            "global" => (RateLimitState::Global(TokenBucket::new(burst)), None),
            "per_ip" => (RateLimitState::PerIp(DashMap::new()), None),
            "descriptor" => {
                let desc = cfg.descriptor.ok_or_else(|| {
                    FilterError::from("rate_limit: descriptor mode requires a 'descriptor' config block")
                })?;
                validate_descriptor_config(&desc)?;
                let policy = DescriptorPolicy {
                    name: desc.name,
                    sources: desc.sources,
                    missing: desc.missing,
                };
                (RateLimitState::Descriptor(DashMap::new()), Some(policy))
            },
            other => return Err(format!("rate_limit: unknown mode '{other}'").into()),
        };

        Ok(Box::new(Self {
            state,
            rate: cfg.rate,
            burst,
            epoch: Instant::now(),
            descriptor_policy,
        }))
    }
}

/// Validate descriptor config at construction time.
fn validate_descriptor_config(desc: &DescriptorConfig) -> Result<(), FilterError> {
    if desc.sources.is_empty() {
        return Err("rate_limit: descriptor mode requires at least one source".into());
    }
    for source in &desc.sources {
        if let DescriptorSource::Header { .. } = source {
            if !desc.trusted_headers {
                return Err(
                    "rate_limit: header sources require 'trusted_headers: true' to prevent client spoofing".into(),
                );
            }
        }
    }
    Ok(())
}

/// Build a collision-safe composite descriptor key from resolved values.
///
/// Format: `source_name=value;source_name=value` with length-prefixed
/// values to prevent collision across different value distributions.
fn build_descriptor_key(parts: &[(&str, &str)]) -> String {
    let mut key = String::new();
    for (i, (name, value)) in parts.iter().enumerate() {
        if i > 0 {
            key.push(';');
        }
        key.push_str(&name.len().to_string());
        key.push(':');
        key.push_str(name);
        key.push('=');
        key.push_str(&value.len().to_string());
        key.push(':');
        key.push_str(value);
    }
    key
}

/// Resolve descriptor sources from context metadata and request headers.
///
/// Returns `None` if any source is missing and the caller should apply
/// the configured missing behavior.
fn resolve_descriptor<'a>(
    policy: &'a DescriptorPolicy,
    ctx: &'a HttpFilterContext<'_>,
) -> Option<Vec<(&'a str, Cow<'a, str>)>> {
    let mut parts = Vec::with_capacity(policy.sources.len());
    for source in &policy.sources {
        match source {
            DescriptorSource::Context { context } => {
                let value = ctx.metadata(context)?;
                parts.push((context.as_str(), Cow::Borrowed(value)));
            },
            DescriptorSource::Header { header } => {
                let value = ctx.request.headers.get(header.as_str())?.to_str().ok()?;
                parts.push((header.as_str(), Cow::Borrowed(value)));
            },
        }
    }
    Some(parts)
}

#[async_trait]
impl HttpFilter for RateLimitFilter {
    fn name(&self) -> &'static str {
        "rate_limit"
    }

    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        match &self.state {
            RateLimitState::Global(_) | RateLimitState::PerIp(_) => match self.try_acquire_for(ctx.client_addr) {
                Ok(_remaining) => Ok(FilterAction::Continue),
                Err(remaining) => {
                    tracing::info!(client = ?ctx.client_addr, "rate_limit: rejecting request (429)");
                    let (headers, retry_secs) = self.rate_limit_headers(remaining);
                    let mut rejection = Rejection::status(429).with_header("Retry-After", format!("{retry_secs}"));
                    for (name, value) in headers {
                        rejection = rejection.with_header(name, value);
                    }
                    Ok(FilterAction::Reject(rejection))
                },
            },
            RateLimitState::Descriptor(map) => {
                let policy = self.descriptor_policy.as_ref().expect("descriptor policy set");
                let resolved = resolve_descriptor(policy, ctx);

                let Some(parts) = resolved else {
                    tracing::debug!(policy = %policy.name, "rate_limit: descriptor missing");

                    let decision = match policy.missing {
                        MissingBehavior::Reject => "deny",
                        MissingBehavior::Skip => "skip",
                    };

                    metrics::counter!(
                        "praxis_rate_limit_decisions_total",
                        "mode" => "descriptor",
                        "policy" => policy.name.clone(),
                        "decision" => decision,
                        "reason" => "missing_descriptor",
                    )
                    .increment(1);

                    ctx.set_metadata("rate_limit.policy", &policy.name);
                    ctx.set_metadata("rate_limit.decision", decision);

                    return match policy.missing {
                        MissingBehavior::Reject => Ok(FilterAction::Reject(Rejection::status(429))),
                        MissingBehavior::Skip => Ok(FilterAction::Continue),
                    };
                };

                let key_parts: Vec<(&str, &str)> = parts.iter().map(|(n, v)| (*n, v.as_ref())).collect();
                let key = build_descriptor_key(&key_parts);

                ctx.set_metadata("rate_limit.descriptor_key", &key);

                let now = self.now_nanos();
                self.maybe_evict_descriptors(map, now);

                let bucket = map.entry(key).or_insert_with(|| TokenBucket::new(self.burst));
                match bucket.try_acquire(self.rate, self.burst, now) {
                    Some(remaining) => {
                        metrics::counter!(
                            "praxis_rate_limit_decisions_total",
                            "mode" => "descriptor",
                            "policy" => policy.name.clone(),
                            "decision" => "allow",
                            "reason" => "ok",
                        )
                        .increment(1);

                        ctx.set_metadata("rate_limit.policy", &policy.name);
                        ctx.set_metadata("rate_limit.decision", "allow");

                        #[allow(clippy::cast_possible_truncation, reason = "remaining fits u64")]
                        let remaining_str = (remaining.max(0.0) as u64).to_string();
                        ctx.set_metadata("rate_limit.remaining", &remaining_str);

                        Ok(FilterAction::Continue)
                    },
                    None => {
                        let remaining = bucket.current_tokens(self.rate, self.burst, now);
                        tracing::info!(policy = %policy.name, "rate_limit: descriptor rejected (429)");

                        metrics::counter!(
                            "praxis_rate_limit_decisions_total",
                            "mode" => "descriptor",
                            "policy" => policy.name.clone(),
                            "decision" => "deny",
                            "reason" => "bucket_exhausted",
                        )
                        .increment(1);

                        ctx.set_metadata("rate_limit.policy", &policy.name);
                        ctx.set_metadata("rate_limit.decision", "deny");

                        let (headers, retry_secs) = self.rate_limit_headers(remaining);
                        let mut rejection = Rejection::status(429).with_header("Retry-After", format!("{retry_secs}"));
                        for (name, value) in headers {
                            rejection = rejection.with_header(name, value);
                        }
                        Ok(FilterAction::Reject(rejection))
                    },
                }
            },
        }
    }

    async fn on_response(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        let remaining = match &self.state {
            RateLimitState::Global(_) | RateLimitState::PerIp(_) => self.current_remaining(ctx.client_addr),
            RateLimitState::Descriptor(map) => ctx
                .metadata("rate_limit.descriptor_key")
                .and_then(|key| map.get(key))
                .map_or(self.burst, |b| {
                    b.current_tokens(self.rate, self.burst, self.now_nanos())
                }),
        };
        let (headers, _retry_secs) = self.rate_limit_headers(remaining);

        if let Some(ref mut resp) = ctx.response_header {
            for (name, value) in &headers {
                if let Ok(hv) = value.parse()
                    && let Ok(hn) = http::header::HeaderName::from_bytes(name.as_bytes())
                {
                    resp.headers.insert(hn, hv);
                    ctx.response_headers_modified = true;
                }
            }
        }

        Ok(FilterAction::Continue)
    }
}
