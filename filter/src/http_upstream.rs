// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! HTTP-specific policy associated with a selected upstream transport.


use http::header::HeaderValue;
use praxis_core::connectivity::Upstream;

/// Request policy applied when sending an HTTP request upstream.
#[derive(Clone, Debug, Default)]
pub struct HttpUpstreamRequestPolicy {
    /// Optional authority sent in the upstream HTTP request.
    authority: Option<HeaderValue>,
}

impl HttpUpstreamRequestPolicy {
    /// Build a policy from an authority parsed during configuration.
    #[must_use]
    pub fn new(authority: Option<HeaderValue>) -> Self {
        Self { authority }
    }

    /// Return the configured HTTP authority override, if any.
    #[must_use]
    pub fn authority(&self) -> Option<&HeaderValue> {
        self.authority.as_ref()
    }
}

/// A selected HTTP upstream: transport state plus request policy.
#[derive(Clone, Debug)]
pub struct HttpUpstream {
    /// Generic transport endpoint and connection settings.
    transport: Upstream,
    /// HTTP request policy captured with the selected endpoint.
    request_policy: HttpUpstreamRequestPolicy,
}

impl HttpUpstream {
    /// Combine a transport endpoint with its HTTP request policy.
    #[must_use]
    pub fn new(transport: Upstream, request_policy: HttpUpstreamRequestPolicy) -> Self {
        Self {
            transport,
            request_policy,
        }
    }

    /// Borrow the transport state used to establish the upstream connection.
    #[must_use]
    pub fn transport(&self) -> &Upstream {
        &self.transport
    }

    /// Mutably borrow transport state for per-attempt connection changes.
    pub fn transport_mut(&mut self) -> &mut Upstream {
        &mut self.transport
    }

    /// Borrow the request policy captured with this selected upstream.
    #[must_use]
    pub fn request_policy(&self) -> &HttpUpstreamRequestPolicy {
        &self.request_policy
    }

    /// Return the selected transport address.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.transport.address
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use praxis_core::connectivity::ConnectionOptions;

    use super::*;

    #[test]
    fn policy_defaults_without_authority() {
        assert!(HttpUpstreamRequestPolicy::default().authority().is_none());
    }

    #[test]
    fn selected_upstream_keeps_transport_and_policy_independent() {
        let transport = Upstream {
            address: Arc::from("10.0.0.1:443"),
            connection: Arc::new(ConnectionOptions::default()),
            tls: None,
        };
        let policy = HttpUpstreamRequestPolicy::new(Some(HeaderValue::from_static("api.example.com")));
        let upstream = HttpUpstream::new(transport, policy);

        assert_eq!(upstream.address(), "10.0.0.1:443");
        assert_eq!(
            upstream
                .request_policy()
                .authority()
                .and_then(|value| value.to_str().ok()),
            Some("api.example.com")
        );
    }
}
