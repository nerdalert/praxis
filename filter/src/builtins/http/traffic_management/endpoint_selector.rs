// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Endpoint selector filter: selects an upstream endpoint from a request header.

use std::sync::Arc;

use async_trait::async_trait;
use praxis_core::connectivity::{ConnectionOptions, Upstream};
use serde::Deserialize;

use crate::{
    FilterAction, FilterError, Rejection,
    factory::parse_filter_config,
    filter::{HttpFilter, HttpFilterContext},
};

// -----------------------------------------------------------------------------
// Config
// -----------------------------------------------------------------------------

/// Configuration for the endpoint selector filter.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointSelectorConfig {
    /// The request header to read the upstream endpoint address from.
    source_header: String,

    /// Whether the destination header is required (fail-closed).
    ///
    /// When `true`, requests without a trusted destination header
    /// are rejected. Use for compositions where an external
    /// processor is expected to always supply a destination.
    #[serde(default)]
    required: bool,

    /// HTTP status code for required-mode routing failures.
    ///
    /// Only used when `required: true`. Defaults to 500.
    /// Track B compositions typically set 503.
    #[serde(default = "default_status_on_required_failure")]
    status_on_required_failure: u16,

    /// Whether to remove the source header after reading it.
    #[serde(default = "default_strip_header")]
    strip_header: bool,
}

/// Default value for `status_on_required_failure`.
fn default_status_on_required_failure() -> u16 {
    500
}

/// Default value for `strip_header`.
fn default_strip_header() -> bool {
    true
}

// -----------------------------------------------------------------------------
// EndpointSelectorFilter
// -----------------------------------------------------------------------------

/// Selects an upstream endpoint by reading a configured request header.
///
/// The header value must be a single `host:port` address. If the header
/// is absent or empty, the filter does nothing and returns
/// [`FilterAction::Continue`].
///
/// # YAML configuration
///
/// ```yaml
/// filter: endpoint_selector
/// source_header: x-gateway-destination-endpoint
/// strip_header: true  # default true
/// ```
///
/// # Example
///
/// ```ignore
/// use praxis_filter::EndpointSelectorFilter;
///
/// let yaml: serde_yaml::Value = serde_yaml::from_str(
///     "source_header: x-destination\nstrip_header: false"
/// ).unwrap();
/// let filter = EndpointSelectorFilter::from_config(&yaml).unwrap();
/// assert_eq!(filter.name(), "endpoint_selector");
/// ```
pub struct EndpointSelectorFilter {
    /// Connection options for constructed upstreams.
    connection: Arc<ConnectionOptions>,

    /// Whether the destination header is required (fail-closed).
    required: bool,

    /// The request header to read.
    source_header: http::HeaderName,

    /// HTTP status code for required-mode routing failures.
    status_on_required_failure: u16,

    /// Whether to strip the source header after reading.
    strip_header: bool,
}

impl EndpointSelectorFilter {
    /// Create an endpoint selector filter from parsed YAML config.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if `source_header` is missing or not a
    /// valid HTTP header name.
    ///
    /// [`FilterError`]: crate::FilterError
    pub fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: EndpointSelectorConfig = parse_filter_config("endpoint_selector", config)?;

        let source_header: http::HeaderName = cfg
            .source_header
            .parse()
            .map_err(|e| format!("endpoint_selector: invalid source_header: {e}"))?;

        if !(100..=599).contains(&cfg.status_on_required_failure) {
            let code = cfg.status_on_required_failure;
            return Err(format!(
                "endpoint_selector: status_on_required_failure {code} is not a valid HTTP status code (must be 100..=599)"
            )
            .into());
        }

        Ok(Box::new(Self {
            connection: Arc::new(ConnectionOptions::default()),
            required: cfg.required,
            source_header,
            status_on_required_failure: cfg.status_on_required_failure,
            strip_header: cfg.strip_header,
        }))
    }

    /// Return a routing failure as either a rejection or an error.
    ///
    /// Required-mode failures return [`Reject`] so they cannot be
    /// bypassed by `failure_mode: open`. Optional-mode failures
    /// return [`FilterError`] for conventional failure-mode handling.
    ///
    /// [`Reject`]: FilterAction::Reject
    fn routing_failure(&self, reason: String) -> Result<FilterAction, FilterError> {
        if self.required {
            tracing::warn!(%reason, "required endpoint_selector rejecting request");
            Ok(FilterAction::Reject(Rejection::status(self.status_on_required_failure)))
        } else {
            Err(reason.into())
        }
    }
}

#[async_trait]
impl HttpFilter for EndpointSelectorFilter {
    fn name(&self) -> &'static str {
        "endpoint_selector"
    }

    #[allow(clippy::too_many_lines, reason = "header resolution, validation, and stripping")]
    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        let pending = match ctx.pending_header_value(&self.source_header) {
            Ok(v) => v,
            Err(e) => return self.routing_failure(format!("endpoint_selector: {e}")),
        };

        let value = if let Some(v) = pending {
            v
        } else {
            match ctx.resolve_trusted_header(&self.source_header) {
                Ok(Some(v)) => v,
                Ok(None) => {
                    if self.required {
                        return self.routing_failure(format!(
                            "endpoint_selector: required destination header '{header}' absent",
                            header = self.source_header
                        ));
                    }
                    return Ok(FilterAction::Continue);
                },
                Err(e) => return self.routing_failure(format!("endpoint_selector: {e}")),
            }
        };

        if value.is_empty() {
            return self.routing_failure(format!(
                "endpoint_selector: header '{header}' has an empty trusted value",
                header = self.source_header
            ));
        }

        if value.contains(',') {
            return self.routing_failure(format!(
                "endpoint_selector: header '{header}' contains multiple values (commas not allowed)",
                header = self.source_header
            ));
        }

        if let Err(e) = validate_host_port(&value) {
            return self.routing_failure(format!("{e}"));
        }

        let upstream = Upstream {
            address: Arc::from(value.as_str()),
            connection: Arc::clone(&self.connection),
            tls: None,
        };

        ctx.upstream = Some(upstream);

        if self.strip_header {
            ctx.request_headers_to_remove.push(self.source_header.clone());
            let name_str = self.source_header.as_str();
            ctx.extra_request_headers
                .retain(|(n, _)| !n.eq_ignore_ascii_case(name_str));
            ctx.request_headers_to_set.retain(|(n, _)| *n != self.source_header);
            ctx.pre_read_mutations
                .retain(|m| !m.matches_header(&self.source_header));
        }

        Ok(FilterAction::Continue)
    }
}

// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

/// Validate that `addr` is a well-formed `host:port` address.
///
/// Accepts DNS names, IPv4 addresses, and bracketed IPv6 addresses
/// (e.g. `[::1]:8080`). The port must be a valid `u16`.
fn validate_host_port(addr: &str) -> Result<(), FilterError> {
    // Handle bracketed IPv6: [host]:port
    if let Some(rest) = addr.strip_prefix('[') {
        let (_, port_str) = rest
            .split_once("]:")
            .ok_or_else(|| format!("endpoint_selector: invalid IPv6 address format: '{addr}'"))?;
        parse_port(port_str, addr)?;
        return Ok(());
    }

    // For non-IPv6, split on the last colon.
    let (host, port_str) = addr
        .rsplit_once(':')
        .ok_or_else(|| format!("endpoint_selector: missing port in address: '{addr}'"))?;

    if host.is_empty() {
        return Err(format!("endpoint_selector: empty host in address: '{addr}'").into());
    }

    parse_port(port_str, addr)?;

    Ok(())
}

/// Parse and validate a port string as a `u16`.
fn parse_port(port_str: &str, addr: &str) -> Result<u16, FilterError> {
    port_str
        .parse::<u16>()
        .map_err(|_parse_err| format!("endpoint_selector: invalid port in address: '{addr}'").into())
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
    use crate::context::TrustedHeaderMutation;

    #[test]
    fn parse_valid_config() {
        let config: serde_yaml::Value =
            serde_yaml::from_str("source_header: x-destination\nstrip_header: false").unwrap();
        let filter = EndpointSelectorFilter::from_config(&config).unwrap();
        assert_eq!(filter.name(), "endpoint_selector", "filter name should match");
    }

    #[test]
    fn parse_missing_source_header_errors() {
        let config: serde_yaml::Value = serde_yaml::from_str("strip_header: true").unwrap();
        assert!(
            EndpointSelectorFilter::from_config(&config).is_err(),
            "missing source_header should error"
        );
    }

    #[tokio::test]
    async fn selects_upstream_from_header() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "backend.local:8080".to_owned(),
        ));

        let action = filter.on_request(&mut ctx).await.unwrap();

        assert!(matches!(action, FilterAction::Continue), "should return Continue");
        assert_eq!(
            ctx.upstream_addr(),
            Some("backend.local:8080"),
            "upstream should be set from header"
        );
    }

    #[tokio::test]
    async fn strips_header_by_default() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "backend.local:8080".to_owned(),
        ));

        let _action = filter.on_request(&mut ctx).await.unwrap();

        let header_name: http::HeaderName = "x-dest".parse().unwrap();
        assert!(
            ctx.request_headers_to_remove.contains(&header_name),
            "source header should be in remove list when strip_header is true"
        );
    }

    #[tokio::test]
    async fn preserve_header_when_strip_false() {
        let filter = make_filter("x-dest", false);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "backend.local:8080".to_owned(),
        ));

        let _action = filter.on_request(&mut ctx).await.unwrap();

        assert!(
            ctx.request_headers_to_remove.is_empty(),
            "source header should NOT be in remove list when strip_header is false"
        );
    }

    #[tokio::test]
    async fn ignores_absent_header() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let action = filter.on_request(&mut ctx).await.unwrap();

        assert!(
            matches!(action, FilterAction::Continue),
            "should return Continue when header absent"
        );
        assert!(ctx.upstream.is_none(), "upstream should remain None when header absent");
    }

    #[tokio::test]
    async fn validates_ipv4_endpoint() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "10.0.0.1:8080".to_owned(),
        ));

        let _action = filter.on_request(&mut ctx).await.unwrap();

        assert_eq!(
            ctx.upstream_addr(),
            Some("10.0.0.1:8080"),
            "IPv4 address with port should be accepted"
        );
    }

    #[tokio::test]
    async fn validates_ipv6_endpoint() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "[::1]:8080".to_owned(),
        ));

        let _action = filter.on_request(&mut ctx).await.unwrap();

        assert_eq!(
            ctx.upstream_addr(),
            Some("[::1]:8080"),
            "bracketed IPv6 address with port should be accepted"
        );
    }

    #[tokio::test]
    async fn rejects_comma_separated() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "host1:80,host2:80".to_owned(),
        ));

        let result = filter.on_request(&mut ctx).await;

        assert!(result.is_err(), "comma-separated values should be rejected");
    }

    #[tokio::test]
    async fn rejects_empty_value() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations
            .push(TrustedHeaderMutation::Add("x-dest".parse().unwrap(), String::new()));

        let result = filter.on_request(&mut ctx).await;

        assert!(result.is_err(), "empty trusted destination must be rejected");
        assert!(ctx.upstream.is_none(), "upstream should remain None");
    }

    #[tokio::test]
    async fn rejects_no_port() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "hostname".to_owned(),
        ));

        let result = filter.on_request(&mut ctx).await;

        assert!(result.is_err(), "address without port should be rejected");
    }

    #[tokio::test]
    async fn rejects_invalid_port() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "host:99999".to_owned(),
        ));

        let result = filter.on_request(&mut ctx).await;

        assert!(result.is_err(), "invalid port (>65535) should be rejected");
    }

    #[tokio::test]
    async fn reads_effective_header_from_pending_set() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);

        ctx.request_headers_to_set
            .push(("x-dest".parse().unwrap(), "set-by-prior:9090".parse().unwrap()));

        let _action = filter.on_request(&mut ctx).await.unwrap();

        assert_eq!(
            ctx.upstream_addr(),
            Some("set-by-prior:9090"),
            "should read header from pending set (prior filter)"
        );
    }

    #[tokio::test]
    async fn reads_effective_header_from_trusted_mutations() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);

        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "extra-host:7070".to_owned(),
        ));

        let _action = filter.on_request(&mut ctx).await.unwrap();

        assert_eq!(
            ctx.upstream_addr(),
            Some("extra-host:7070"),
            "should read header from trusted pre-read mutations"
        );
    }

    #[tokio::test]
    async fn removed_header_not_selected() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);

        ctx.request_headers_to_remove.push("x-dest".parse().unwrap());

        let action = filter.on_request(&mut ctx).await.unwrap();

        assert!(
            matches!(action, FilterAction::Continue),
            "should return Continue when header is removed"
        );
        assert!(
            ctx.upstream.is_none(),
            "upstream should remain None when header is in remove list"
        );
    }

    #[tokio::test]
    async fn client_supplied_header_not_selected() {
        let filter = make_filter("x-dest", true);
        let mut req = crate::test_utils::make_request(http::Method::GET, "/");
        req.headers.insert("x-dest", "evil.attacker:9999".parse().unwrap());
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let action = filter.on_request(&mut ctx).await.unwrap();

        assert!(
            matches!(action, FilterAction::Continue),
            "should continue without selecting upstream"
        );
        assert!(
            ctx.upstream.is_none(),
            "client-supplied destination must not select upstream"
        );
    }

    // -------------------------------------------------------------------------
    // Required / Fail-Closed Tests
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn required_mode_rejects_when_absent() {
        let filter = make_required_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let action = filter.on_request(&mut ctx).await.unwrap();

        assert!(
            matches!(action, FilterAction::Reject(ref r) if r.status == 503),
            "required mode should reject with configured 503 when header absent"
        );
        assert!(ctx.upstream.is_none(), "upstream must remain None on rejection");
    }

    #[tokio::test]
    async fn optional_mode_continues_when_absent() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let action = filter.on_request(&mut ctx).await.unwrap();

        assert!(
            matches!(action, FilterAction::Continue),
            "optional mode should continue when header absent"
        );
        assert!(ctx.upstream.is_none(), "upstream should remain None");
    }

    #[tokio::test]
    async fn required_config_parses() {
        let config: serde_yaml::Value = serde_yaml::from_str("source_header: x-dest\nrequired: true").unwrap();
        let filter = EndpointSelectorFilter::from_config(&config).unwrap();
        assert_eq!(filter.name(), "endpoint_selector", "filter name should match");
    }

    #[test]
    fn custom_failure_status_parses() {
        let config: serde_yaml::Value =
            serde_yaml::from_str("source_header: x-dest\nrequired: true\nstatus_on_required_failure: 503").unwrap();
        assert!(
            EndpointSelectorFilter::from_config(&config).is_ok(),
            "503 should be a valid failure status"
        );
    }

    #[test]
    fn invalid_failure_status_rejected() {
        let config: serde_yaml::Value =
            serde_yaml::from_str("source_header: x-dest\nstatus_on_required_failure: 0").unwrap();
        assert!(
            EndpointSelectorFilter::from_config(&config).is_err(),
            "status 0 should be rejected"
        );
    }

    #[test]
    fn out_of_range_failure_status_rejected() {
        let config: serde_yaml::Value =
            serde_yaml::from_str("source_header: x-dest\nstatus_on_required_failure: 600").unwrap();
        assert!(
            EndpointSelectorFilter::from_config(&config).is_err(),
            "status 600 should be rejected"
        );
    }

    // -------------------------------------------------------------------------
    // Trusted Mutation Semantics Tests
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn trusted_set_destination_selected() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Set(
            "x-dest".parse().unwrap(),
            "set-host:9090".parse().unwrap(),
        ));

        let _action = filter.on_request(&mut ctx).await.unwrap();

        assert_eq!(
            ctx.upstream_addr(),
            Some("set-host:9090"),
            "set mutation should select endpoint"
        );
    }

    #[tokio::test]
    async fn two_distinct_trusted_destinations_rejected() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "host-a:8080".to_owned(),
        ));
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "host-b:9090".to_owned(),
        ));

        let result = filter.on_request(&mut ctx).await;

        assert!(
            result.is_err(),
            "distinct duplicate trusted destinations must be rejected"
        );
        assert!(ctx.upstream.is_none(), "upstream must remain None");
    }

    #[tokio::test]
    async fn identical_trusted_duplicates_accepted() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "same:8080".to_owned(),
        ));
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "same:8080".to_owned(),
        ));

        let _action = filter.on_request(&mut ctx).await.unwrap();

        assert_eq!(
            ctx.upstream_addr(),
            Some("same:8080"),
            "identical duplicate trusted destinations should be accepted"
        );
    }

    #[tokio::test]
    async fn add_then_remove_produces_no_destination() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "first:8080".to_owned(),
        ));
        ctx.pre_read_mutations
            .push(TrustedHeaderMutation::Remove("x-dest".parse().unwrap()));

        let action = filter.on_request(&mut ctx).await.unwrap();

        assert!(
            matches!(action, FilterAction::Continue),
            "add then remove should leave no destination"
        );
        assert!(ctx.upstream.is_none(), "upstream should remain None after add+remove");
    }

    #[tokio::test]
    async fn remove_then_add_selects_later_destination() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations
            .push(TrustedHeaderMutation::Remove("x-dest".parse().unwrap()));
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "later:7070".to_owned(),
        ));

        let _action = filter.on_request(&mut ctx).await.unwrap();

        assert_eq!(
            ctx.upstream_addr(),
            Some("later:7070"),
            "remove then add should select the later destination"
        );
    }

    #[tokio::test]
    async fn malicious_client_plus_valid_epp_selects_epp() {
        let filter = make_required_filter("x-dest", true);
        let mut req = crate::test_utils::make_request(http::Method::GET, "/");
        req.headers.insert("x-dest", "evil.attacker:9999".parse().unwrap());
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "trusted-epp:8080".to_owned(),
        ));

        let _action = filter.on_request(&mut ctx).await.unwrap();

        assert_eq!(
            ctx.upstream_addr(),
            Some("trusted-epp:8080"),
            "trusted EPP destination must be selected, not client header"
        );
    }

    #[tokio::test]
    async fn malicious_client_plus_epp_omission_rejects_503() {
        let filter = make_required_filter("x-dest", true);
        let mut req = crate::test_utils::make_request(http::Method::GET, "/");
        req.headers.insert("x-dest", "evil.attacker:9999".parse().unwrap());
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let action = filter.on_request(&mut ctx).await.unwrap();

        assert!(
            matches!(action, FilterAction::Reject(ref r) if r.status == 503),
            "required mode with no trusted destination must reject with 503"
        );
        assert!(ctx.upstream.is_none(), "upstream must remain None on rejection");
    }

    #[tokio::test]
    async fn strip_clears_pre_read_mutations() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "host:8080".to_owned(),
        ));

        let _action = filter.on_request(&mut ctx).await.unwrap();

        assert!(
            ctx.pre_read_mutations.is_empty(),
            "strip should clear trusted mutations for the routing header"
        );
    }

    #[tokio::test]
    async fn strip_preserves_unrelated_mutations() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "host:8080".to_owned(),
        ));
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Set(
            "x-other".parse().unwrap(),
            "keep-me".parse().unwrap(),
        ));

        let _action = filter.on_request(&mut ctx).await.unwrap();

        assert_eq!(
            ctx.pre_read_mutations.len(),
            1,
            "unrelated mutations should be preserved"
        );
        assert!(
            ctx.pre_read_mutations[0].matches_header(&"x-other".parse().unwrap()),
            "remaining mutation should be for x-other"
        );
    }

    #[tokio::test]
    async fn set_overrides_earlier_add() {
        let filter = make_filter("x-dest", true);
        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Add(
            "x-dest".parse().unwrap(),
            "first:8080".to_owned(),
        ));
        ctx.pre_read_mutations.push(TrustedHeaderMutation::Set(
            "x-dest".parse().unwrap(),
            "override:9090".parse().unwrap(),
        ));

        let _action = filter.on_request(&mut ctx).await.unwrap();

        assert_eq!(
            ctx.upstream_addr(),
            Some("override:9090"),
            "set should override earlier add"
        );
    }

    // -------------------------------------------------------------------------
    // Fail-Open Regression Tests
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn required_503_survives_fail_open_pipeline() {
        use praxis_core::config::FailureMode;

        let registry = crate::registry::FilterRegistry::with_builtins();
        let config: serde_yaml::Value =
            serde_yaml::from_str("source_header: x-dest\nrequired: true\nstatus_on_required_failure: 503").unwrap();
        let mut entries = vec![crate::FilterEntry {
            branch_chains: None,
            filter_type: "endpoint_selector".into(),
            config,
            conditions: vec![],
            name: None,
            response_conditions: vec![],
            failure_mode: FailureMode::Open,
        }];
        let pipeline = crate::FilterPipeline::build(&mut entries, &registry).unwrap();

        let req = crate::test_utils::make_request(http::Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let action = pipeline.execute_http_request(&mut ctx).await.unwrap();

        assert!(
            matches!(action, FilterAction::Reject(ref r) if r.status == 503),
            "required 503 rejection must survive failure_mode: open"
        );
        assert!(ctx.upstream.is_none(), "upstream must remain None");
    }

    // -------------------------------------------------------------------------
    // Test Utilities
    // -------------------------------------------------------------------------

    /// Build an [`EndpointSelectorFilter`] with the given header name and strip flag.
    fn make_filter(header: &str, strip: bool) -> EndpointSelectorFilter {
        EndpointSelectorFilter {
            connection: Arc::new(ConnectionOptions::default()),
            required: false,
            source_header: header.parse().expect("valid header name"),
            status_on_required_failure: 500,
            strip_header: strip,
        }
    }

    /// Build a required [`EndpointSelectorFilter`] (fail-closed, 503).
    fn make_required_filter(header: &str, strip: bool) -> EndpointSelectorFilter {
        EndpointSelectorFilter {
            connection: Arc::new(ConnectionOptions::default()),
            required: true,
            source_header: header.parse().expect("valid header name"),
            status_on_required_failure: 503,
            strip_header: strip,
        }
    }
}
