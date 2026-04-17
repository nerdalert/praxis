// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Adds TCP or TLS listeners to a Pingora HTTP proxy service.

use pingora_core::services::listening::Service;
use pingora_proxy::HttpProxy;
use praxis_core::ProxyError;
use praxis_tls::ListenerTls;
use tracing::info;

// -----------------------------------------------------------------------------
// Listener Handlers
// -----------------------------------------------------------------------------

/// Add a single HTTP listener to an HTTP proxy service.
pub(crate) fn add_listener<H>(
    service: &mut Service<HttpProxy<H>>,
    listener: &praxis_core::config::Listener,
) -> Result<(), ProxyError> {
    let tls_enabled = listener.tls.is_some();

    if let Some(tls) = &listener.tls {
        let tls_settings = build_tls_settings(tls, &listener.address)?;
        service.add_tls_with_settings(&listener.address, None, tls_settings);
    } else {
        service.add_tcp(&listener.address);
    }

    info!(
        name = %listener.name,
        address = %listener.address,
        tls = tls_enabled,
        "HTTP listener registered"
    );

    Ok(())
}

/// Build [`TlsSettings`] for a listener.
///
/// When `hot_reload` is enabled, uses a [`ReloadableCertResolver`]
/// and spawns a [`CertWatcher`] background task. Otherwise builds
/// a static [`ServerConfig`] via [`build_server_config`].
///
/// [`TlsSettings`]: pingora_core::listeners::tls::TlsSettings
/// [`ServerConfig`]: rustls::ServerConfig
/// [`build_server_config`]: praxis_tls::setup::build_server_config
/// [`ReloadableCertResolver`]: praxis_tls::reload::ReloadableCertResolver
/// [`CertWatcher`]: praxis_tls::watcher::CertWatcher
fn build_tls_settings(
    tls: &ListenerTls,
    address: &str,
) -> Result<pingora_core::listeners::tls::TlsSettings, ProxyError> {
    if tls.is_hot_reload() {
        tracing::debug!(address, "building TLS ServerConfig with hot-reload");
        let (server_config, swap_handle) = praxis_tls::setup::build_reloadable_server_config(tls)
            .map_err(|e| ProxyError::Config(format!("TLS hot-reload for {address}: {e}")))?;

        let pair =
            tls.certificates.first().cloned().ok_or_else(|| {
                ProxyError::Config(format!("TLS hot-reload for {address}: no certificate configured"))
            })?;
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        praxis_tls::watcher::CertWatcher::spawn(swap_handle, pair, shutdown_rx);

        return pingora_core::listeners::tls::TlsSettings::with_server_config(server_config)
            .map_err(|e| ProxyError::Config(format!("TLS for {address}: {e}")));
    }

    tracing::debug!(address, "building TLS ServerConfig");
    let server_config = praxis_tls::setup::build_server_config(tls)
        .map_err(|e| ProxyError::Config(format!("TLS for {address}: {e}")))?;
    pingora_core::listeners::tls::TlsSettings::with_server_config(server_config)
        .map_err(|e| ProxyError::Config(format!("TLS for {address}: {e}")))
}
