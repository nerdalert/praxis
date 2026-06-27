// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Praxis Contributors

//! Grid ingress trust filter: validates downstream mTLS peer identity
//! against a configured set of trusted gateway peers.

use async_trait::async_trait;
use serde::Deserialize;

use crate::{
    FilterAction, FilterError, Rejection,
    factory::parse_filter_config,
    filter::{HttpFilter, HttpFilterContext},
};

/// Maximum number of trusted peer entries to prevent unbounded config growth.
const MAX_TRUSTED_PEERS: usize = 256;

// -----------------------------------------------------------------------------
// Config
// -----------------------------------------------------------------------------

/// Deserialized YAML config for the grid ingress trust filter.
///
/// ```yaml
/// filter: grid_ingress_trust
/// trusted_peers:
///   - organization: praxis-grid-e2e
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GridIngressTrustConfig {
    /// Configured trusted peer entries.
    trusted_peers: Vec<TrustedPeerConfig>,
}

/// A single trusted peer entry. Matching uses only the configured
/// fields; omitted fields are not checked.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedPeerConfig {
    /// X.509 subject organization to match.
    organization: Option<String>,

    /// Certificate serial number to match.
    serial_number: Option<String>,
}

// -----------------------------------------------------------------------------
// GridIngressTrustFilter
// -----------------------------------------------------------------------------

/// Validates that the downstream mTLS peer identity matches a
/// configured trusted peer before allowing the request to continue.
///
/// Requests without a verified peer identity are rejected with 403.
/// Requests with a peer identity that does not match any trusted
/// peer are also rejected with 403.
///
/// # YAML configuration
///
/// ```yaml
/// filter: grid_ingress_trust
/// trusted_peers:
///   - organization: praxis-grid-e2e
/// ```
pub struct GridIngressTrustFilter {
    /// Validated trusted peer entries.
    trusted_peers: Vec<TrustedPeer>,
}

/// Validated trusted peer entry for runtime matching.
struct TrustedPeer {
    /// X.509 subject organization to match.
    organization: Option<String>,

    /// Certificate serial number to match.
    serial_number: Option<String>,
}

impl GridIngressTrustFilter {
    /// Create a grid ingress trust filter from parsed YAML config.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if the trusted peer list is empty or
    /// a peer entry has no matching fields.
    pub fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: GridIngressTrustConfig = parse_filter_config("grid_ingress_trust", config)?;

        if cfg.trusted_peers.is_empty() {
            return Err("grid_ingress_trust: trusted_peers list must not be empty".into());
        }

        if cfg.trusted_peers.len() > MAX_TRUSTED_PEERS {
            return Err(
                format!("grid_ingress_trust: trusted_peers list exceeds maximum of {MAX_TRUSTED_PEERS}").into(),
            );
        }

        let mut peers = Vec::with_capacity(cfg.trusted_peers.len());
        for (i, p) in cfg.trusted_peers.into_iter().enumerate() {
            if p.organization.as_deref().is_some_and(|value| value.trim().is_empty()) {
                return Err(format!("grid_ingress_trust: trusted_peers[{i}].organization must not be empty").into());
            }
            if p.serial_number.as_deref().is_some_and(|value| value.trim().is_empty()) {
                return Err(format!("grid_ingress_trust: trusted_peers[{i}].serial_number must not be empty").into());
            }
            if p.organization.is_none() && p.serial_number.is_none() {
                return Err(format!(
                    "grid_ingress_trust: trusted_peers[{i}] must specify at least organization or serial_number"
                )
                .into());
            }
            peers.push(TrustedPeer {
                organization: p.organization,
                serial_number: p.serial_number,
            });
        }

        Ok(Box::new(Self { trusted_peers: peers }))
    }

    /// Check whether the request's peer identity matches any trusted peer.
    fn is_trusted(&self, ctx: &HttpFilterContext<'_>) -> bool {
        let Some(identity) = &ctx.peer_identity else {
            return false;
        };

        self.trusted_peers.iter().any(|peer| {
            if let Some(org) = &peer.organization
                && identity.organization.as_deref() != Some(org.as_str())
            {
                return false;
            }
            if let Some(serial) = &peer.serial_number
                && identity.serial_number.as_deref() != Some(serial.as_str())
            {
                return false;
            }
            true
        })
    }
}

#[async_trait]
impl HttpFilter for GridIngressTrustFilter {
    fn name(&self) -> &'static str {
        "grid_ingress_trust"
    }

    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        if self.is_trusted(ctx) {
            tracing::debug!(
                peer_org = ctx.peer_identity.as_ref().and_then(|p| p.organization.as_deref()),
                "grid ingress trust: peer accepted"
            );
            Ok(FilterAction::Continue)
        } else {
            tracing::warn!(
                has_identity = ctx.peer_identity.is_some(),
                "grid ingress trust: rejecting untrusted or missing peer identity"
            );
            Ok(FilterAction::Reject(Rejection::status(403)))
        }
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test suppressions")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests"
)]
mod tests {
    use http::Method;

    use super::*;
    use crate::context::TlsPeerIdentity;

    #[test]
    fn empty_trusted_peers_rejected_at_config_time() {
        let yaml: serde_yaml::Value = serde_yaml::from_str("trusted_peers: []").unwrap();
        let err = GridIngressTrustFilter::from_config(&yaml)
            .err()
            .expect("empty trusted_peers should fail");
        assert!(
            err.to_string().contains("must not be empty"),
            "empty trusted_peers should be rejected: {err}"
        );
    }

    #[test]
    fn peer_with_no_fields_rejected_at_config_time() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            "
trusted_peers:
  - {}
",
        )
        .unwrap();
        let err = GridIngressTrustFilter::from_config(&yaml)
            .err()
            .expect("empty peer entry should fail");
        assert!(
            err.to_string().contains("must specify at least"),
            "empty peer entry should be rejected: {err}"
        );
    }

    #[test]
    fn peer_with_empty_field_rejected_at_config_time() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
trusted_peers:
  - organization: ""
"#,
        )
        .unwrap();
        let err = GridIngressTrustFilter::from_config(&yaml)
            .err()
            .expect("empty organization should fail");
        assert!(
            err.to_string().contains("organization must not be empty"),
            "empty organization should be rejected: {err}"
        );
    }

    #[test]
    fn valid_config_parses() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            "
trusted_peers:
  - organization: praxis-grid-e2e
",
        )
        .unwrap();
        assert!(
            GridIngressTrustFilter::from_config(&yaml).is_ok(),
            "valid config should parse"
        );
    }

    #[tokio::test]
    async fn trusted_serial_number_accepts() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            "
trusted_peers:
  - serial_number: abc123
",
        )
        .unwrap();
        let filter = GridIngressTrustFilter::from_config(&yaml).unwrap();
        let req = crate::test_utils::make_request(Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.peer_identity = Some(TlsPeerIdentity {
            cert_digest: vec![1, 2, 3],
            organization: Some("different-org".to_owned()),
            serial_number: Some("abc123".to_owned()),
        });

        let action = filter.on_request(&mut ctx).await.unwrap();
        assert!(
            matches!(action, FilterAction::Continue),
            "trusted serial number should continue"
        );
    }

    #[tokio::test]
    async fn wrong_serial_number_rejects() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            "
trusted_peers:
  - organization: praxis-grid-e2e
    serial_number: abc123
",
        )
        .unwrap();
        let filter = GridIngressTrustFilter::from_config(&yaml).unwrap();
        let req = crate::test_utils::make_request(Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.peer_identity = Some(TlsPeerIdentity {
            cert_digest: vec![1, 2, 3],
            organization: Some("praxis-grid-e2e".to_owned()),
            serial_number: Some("wrong".to_owned()),
        });

        let action = filter.on_request(&mut ctx).await.unwrap();
        assert!(
            matches!(action, FilterAction::Reject(r) if r.status == 403),
            "wrong serial number should reject with 403 even when organization matches"
        );
    }

    #[tokio::test]
    async fn no_peer_identity_rejects() {
        let filter = make_filter("praxis-grid-e2e");
        let req = crate::test_utils::make_request(Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let action = filter.on_request(&mut ctx).await.unwrap();
        assert!(
            matches!(action, FilterAction::Reject(r) if r.status == 403),
            "missing peer identity should reject with 403"
        );
    }

    #[tokio::test]
    async fn trusted_organization_accepts() {
        let filter = make_filter("praxis-grid-e2e");
        let req = crate::test_utils::make_request(Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.peer_identity = Some(TlsPeerIdentity {
            cert_digest: vec![1, 2, 3],
            organization: Some("praxis-grid-e2e".to_owned()),
            serial_number: Some("123".to_owned()),
        });

        let action = filter.on_request(&mut ctx).await.unwrap();
        assert!(
            matches!(action, FilterAction::Continue),
            "trusted organization should continue"
        );
    }

    #[tokio::test]
    async fn wrong_organization_rejects() {
        let filter = make_filter("praxis-grid-e2e");
        let req = crate::test_utils::make_request(Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.peer_identity = Some(TlsPeerIdentity {
            cert_digest: vec![1, 2, 3],
            organization: Some("wrong-org".to_owned()),
            serial_number: Some("123".to_owned()),
        });

        let action = filter.on_request(&mut ctx).await.unwrap();
        assert!(
            matches!(action, FilterAction::Reject(r) if r.status == 403),
            "wrong organization should reject with 403"
        );
    }

    #[tokio::test]
    async fn missing_organization_in_identity_rejects() {
        let filter = make_filter("praxis-grid-e2e");
        let req = crate::test_utils::make_request(Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.peer_identity = Some(TlsPeerIdentity {
            cert_digest: vec![1, 2, 3],
            organization: None,
            serial_number: Some("123".to_owned()),
        });

        let action = filter.on_request(&mut ctx).await.unwrap();
        assert!(
            matches!(action, FilterAction::Reject(r) if r.status == 403),
            "identity with no organization should reject when org is required"
        );
    }

    // -------------------------------------------------------------------------
    // Test Utilities
    // -------------------------------------------------------------------------

    fn make_filter(org: &str) -> Box<dyn HttpFilter> {
        let yaml: serde_yaml::Value =
            serde_yaml::from_str(&format!("\ntrusted_peers:\n  - organization: {org}\n")).unwrap();
        GridIngressTrustFilter::from_config(&yaml).unwrap()
    }
}
