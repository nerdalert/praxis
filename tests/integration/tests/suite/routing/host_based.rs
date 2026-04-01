// SPDX-License-Identifier: LGPL-3.0-only
// Copyright (c) 2024 Shane Utt

//! Host-based routing tests.

use praxis_core::config::Config;
use praxis_test_utils::{free_port, http_get, start_backend, start_proxy};

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[test]
fn host_based_routing() {
    let api_port = start_backend("api host");
    let default_port = start_backend("default host");
    let proxy_port = free_port();

    let yaml = format!(
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
            host: "api.example.com"
            cluster: "api"
          - path_prefix: "/"
            cluster: "default"
      - filter: load_balancer
        clusters:
          - name: "api"
            endpoints:
              - "127.0.0.1:{api_port}"
          - name: "default"
            endpoints:
              - "127.0.0.1:{default_port}"
"#
    );

    let config = Config::from_yaml(&yaml).unwrap();
    let addr = start_proxy(&config);

    let (status, body) = http_get(&addr, "/", Some("api.example.com"));
    assert_eq!(status, 200, "api.example.com host should return 200");
    assert_eq!(body, "api host", "api.example.com should route to api backend");

    let (status, body) = http_get(&addr, "/", Some("other.com"));
    assert_eq!(status, 200, "other.com host should return 200");
    assert_eq!(
        body, "default host",
        "unrecognized host should route to default backend"
    );
}
