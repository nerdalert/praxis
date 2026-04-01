// SPDX-License-Identifier: LGPL-3.0-only
// Copyright (c) 2024 Shane Utt

//! Proxy startup and configuration test utilities for integration tests.

use std::{collections::HashMap, sync::Arc};

use praxis_core::{
    config::{Config, Listener},
    server::RuntimeOptions,
};
use praxis_filter::{FilterFactory, FilterPipeline, FilterRegistry, HttpFilter};
use praxis_protocol::http::load_http_handler;

// -----------------------------------------------------------------------------
// Pipeline Building
// -----------------------------------------------------------------------------

/// Resolve a listener's filter chains into a [`FilterPipeline`].
///
/// Collects all [`FilterEntry`] items from the named chains
/// referenced by the listener, then builds the pipeline via
/// the provided registry.
///
/// [`FilterPipeline`]: praxis_filter::FilterPipeline
/// [`FilterEntry`]: praxis_core::config::FilterEntry
fn resolve_listener_pipeline(config: &Config, listener: &Listener, registry: &FilterRegistry) -> Arc<FilterPipeline> {
    let chains: HashMap<&str, &Vec<_>> = config
        .filter_chains
        .iter()
        .map(|c| (c.name.as_str(), &c.filters))
        .collect();

    let mut entries = Vec::new();
    for chain_name in &listener.filter_chains {
        if let Some(filters) = chains.get(chain_name.as_str()) {
            entries.extend_from_slice(filters);
        }
    }

    let mut pipeline = FilterPipeline::build(&mut entries, registry).unwrap();
    pipeline
        .apply_body_limits(
            config.body_limits.max_request_bytes,
            config.body_limits.max_response_bytes,
            config.insecure_options.allow_unbounded_body,
        )
        .unwrap();
    Arc::new(pipeline)
}

/// Build the filter pipeline from the config using the
/// builtin registry (uses first listener).
///
/// # Panics
///
/// Panics if `config.listeners` is empty.
pub fn build_pipeline(config: &Config) -> FilterPipeline {
    let registry = FilterRegistry::with_builtins();
    let listener = config
        .listeners
        .first()
        .expect("config must have at least one listener");

    Arc::try_unwrap(resolve_listener_pipeline(config, listener, &registry))
        .unwrap_or_else(|_| panic!("pipeline Arc should have single owner"))
}

// -----------------------------------------------------------------------------
// Proxy Startup
// -----------------------------------------------------------------------------

/// Start the proxy server in a background thread.
///
/// Returns the address string (e.g. `"127.0.0.1:12345"`).
///
/// # Panics
///
/// Panics if `config.listeners` is empty.
pub fn start_proxy(config: &Config) -> String {
    start_proxy_with_registry(config, &FilterRegistry::with_builtins())
}

/// Start the proxy with a custom filter registry.
///
/// # Panics
///
/// Panics if `config.listeners` is empty.
pub fn start_proxy_with_registry(config: &Config, registry: &FilterRegistry) -> String {
    let addr = config
        .listeners
        .first()
        .expect("config must have at least one listener")
        .address
        .clone();
    let mut server = praxis_core::server::build_http_server(config.shutdown_timeout_secs, &RuntimeOptions::default());

    for listener in &config.listeners {
        let pipeline = resolve_listener_pipeline(config, listener, registry);
        load_http_handler(&mut server, listener, pipeline).unwrap();
    }

    if let Some(admin_addr) = &config.admin.address {
        praxis_protocol::http::pingora::health::add_health_endpoint_to_pingora_server(
            &mut server,
            admin_addr,
            None,
            config.admin.verbose,
        );
    }

    std::thread::spawn(move || {
        server.run_forever();
    });

    crate::net::wait::wait_for_http(&addr);
    addr
}

/// Start a full proxy server (HTTP + TCP protocols) in a background thread.
pub fn start_full_proxy(config: Config) {
    std::thread::spawn(move || {
        praxis::run_server(config);
    });
}

/// Start an HTTP proxy with a TLS listener, waiting for HTTPS readiness before returning.
///
/// Uses the same server construction as [`start_proxy`] but
/// waits for TLS readiness instead of plain HTTP readiness.
///
/// Returns the address string.
///
/// # Panics
///
/// Panics if `config.listeners` is empty.
pub fn start_tls_proxy(config: &Config, client_config: &Arc<rustls::ClientConfig>) -> String {
    let registry = FilterRegistry::with_builtins();
    let addr = config
        .listeners
        .first()
        .expect("config must have at least one listener")
        .address
        .clone();
    let mut server = praxis_core::server::build_http_server(config.shutdown_timeout_secs, &RuntimeOptions::default());

    for listener in &config.listeners {
        let pipeline = resolve_listener_pipeline(config, listener, &registry);
        load_http_handler(&mut server, listener, pipeline).unwrap();
    }

    std::thread::spawn(move || {
        server.run_forever();
    });

    crate::net::tls::wait_for_https(&addr, client_config);
    addr
}

/// Start an HTTP proxy with a TLS listener without waiting for readiness.
///
/// Returns the address string. The caller must wait for the proxy to
/// become ready using an appropriate readiness check.
///
/// # Panics
///
/// Panics if `config.listeners` is empty.
pub fn start_tls_proxy_no_wait(config: &Config) -> String {
    let registry = FilterRegistry::with_builtins();
    let addr = config
        .listeners
        .first()
        .expect("config must have at least one listener")
        .address
        .clone();
    let mut server = praxis_core::server::build_http_server(config.shutdown_timeout_secs, &RuntimeOptions::default());

    for listener in &config.listeners {
        let pipeline = resolve_listener_pipeline(config, listener, &registry);
        load_http_handler(&mut server, listener, pipeline).unwrap();
    }

    std::thread::spawn(move || {
        server.run_forever();
    });

    addr
}

// -----------------------------------------------------------------------------
// YAML Config Test Utilities
// -----------------------------------------------------------------------------

/// Filter chain YAML: one listener, catch-all route, one backend.
pub fn simple_proxy_yaml(proxy_port: u16, backend_port: u16) -> String {
    format!(
        r#"
listeners:
  - name: default
    address: "127.0.0.1:{proxy_port}"
    filter_chains: [main]
filter_chains:
  - name: main
    filters:
      - filter: router
        routes:
          - path_prefix: "/"
            cluster: "backend"
      - filter: load_balancer
        clusters:
          - name: "backend"
            endpoints:
              - "127.0.0.1:{backend_port}"
"#
    )
}

/// Filter chain YAML: one listener, a custom filter first,
/// then router + `load_balancer`.
pub fn custom_filter_yaml(proxy_port: u16, backend_port: u16, filter_name: &str) -> String {
    format!(
        r#"
listeners:
  - name: default
    address: "127.0.0.1:{proxy_port}"
    filter_chains: [main]
filter_chains:
  - name: main
    filters:
      - filter: {filter_name}
      - filter: router
        routes:
          - path_prefix: "/"
            cluster: "backend"
      - filter: load_balancer
        clusters:
          - name: "backend"
            endpoints:
              - "127.0.0.1:{backend_port}"
"#
    )
}

// -----------------------------------------------------------------------------
// Registry Test Utilities
// -----------------------------------------------------------------------------

/// Build a [`FilterRegistry`] with builtins plus one custom
/// test filter.
///
/// # Panics
///
/// Panics if the filter name conflicts with a builtin.
///
/// [`FilterRegistry`]: praxis_filter::FilterRegistry
pub fn registry_with(name: &str, make: fn() -> Box<dyn HttpFilter>) -> FilterRegistry {
    let mut registry = FilterRegistry::with_builtins();
    registry
        .register(name, FilterFactory::Http(Arc::new(move |_| Ok(make()))))
        .expect("duplicate filter name in test registry");
    registry
}
