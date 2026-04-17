// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Raw TCP/L4 bidirectional forwarding protocol.

use std::{sync::Arc, time::Duration};

use pingora_core::services::listening::Service;
use praxis_core::{ProxyError, config::Config};
use praxis_filter::{FilterPipeline, FilterRegistry};

use crate::{ListenerPipelines, Protocol};

/// Bidirectional TCP proxy application.
pub(crate) mod proxy;
/// TLS configuration and listener grouping utilities.
mod tls_setup;

// -----------------------------------------------------------------------------
// PingoraTcp
// -----------------------------------------------------------------------------

/// Pingora-backed raw TCP/L4 protocol implementation.
///
/// Groups TCP listeners by `(upstream address, idle timeout, max duration)`,
/// creating one bidirectional forwarder per unique combination. Implements [`Protocol`].
///
/// [`Protocol`]: crate::Protocol
pub struct PingoraTcp;

#[allow(clippy::expect_used, reason = "infallible")]
impl Protocol for PingoraTcp {
    fn register(
        self: Box<Self>,
        server: &mut praxis_core::PingoraServerRuntime,
        config: &Config,
        pipelines: &ListenerPipelines,
    ) -> Result<(), ProxyError> {
        let groups = tls_setup::group_tcp_listeners(config);
        let fallback_pipeline = Arc::new(
            FilterPipeline::build(&mut [], &FilterRegistry::with_builtins()).expect("empty pipeline is valid"),
        );

        for ((upstream_opt, timeout_ms, max_dur_secs), listeners) in groups {
            let pipeline = listeners
                .first()
                .and_then(|l| pipelines.get(&l.name))
                .cloned()
                .unwrap_or_else(|| Arc::clone(&fallback_pipeline));

            let idle_timeout = timeout_ms.map(Duration::from_millis);
            let max_duration = max_dur_secs.map(Duration::from_secs);
            let service_name = match upstream_opt.as_deref() {
                Some(addr) => format!("tcp-proxy:{addr}"),
                None => "tcp-proxy:filter-routed".to_owned(),
            };
            let app = proxy::PingoraTcpProxy::new(upstream_opt.clone(), pipeline, idle_timeout, max_duration);
            let mut service = Service::new(service_name, app);

            tls_setup::register_tcp_listeners(&mut service, &listeners, upstream_opt.as_deref())?;
            server.server_mut().add_service(service);
        }

        Ok(())
    }
}
