// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Process-global Prometheus metrics recorder and `/metrics` rendering.

use std::sync::{Mutex, OnceLock};

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use praxis_core::ProxyError;

// -----------------------------------------------------------------------------
// Recorder Installation
// -----------------------------------------------------------------------------

/// Global Prometheus handle, installed once per process.
static PROMETHEUS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Serializes recorder installation to prevent concurrent callers
/// from racing on the global `metrics` recorder.
static INSTALL_GUARD: Mutex<()> = Mutex::new(());

/// Install the Prometheus metrics recorder.
///
/// Idempotent and race-safe: concurrent callers serialize through
/// a mutex and only the first installs the recorder.
///
/// # Errors
///
/// Returns [`ProxyError`] if the recorder cannot be installed.
pub(crate) fn install_recorder() -> Result<&'static PrometheusHandle, ProxyError> {
    if let Some(handle) = PROMETHEUS_HANDLE.get() {
        return Ok(handle);
    }

    let _guard = INSTALL_GUARD
        .lock()
        .map_err(|e| ProxyError::Config(format!("metrics install lock poisoned: {e}")))?;

    if let Some(handle) = PROMETHEUS_HANDLE.get() {
        return Ok(handle);
    }

    let handle = PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| ProxyError::Config(format!("failed to install prometheus recorder: {e}")))?;

    drop(PROMETHEUS_HANDLE.set(handle));
    Ok(PROMETHEUS_HANDLE.get().expect("prometheus handle installed"))
}

/// Render all collected metrics in Prometheus text exposition format.
///
/// Returns `None` if the recorder has not been installed.
pub(crate) fn render() -> Option<String> {
    PROMETHEUS_HANDLE.get().map(PrometheusHandle::render)
}
