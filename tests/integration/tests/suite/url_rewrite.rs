// SPDX-License-Identifier: LGPL-3.0-only
// Copyright (c) 2024 Shane Utt

//! Integration tests for the url_rewrite filter.

use std::collections::HashMap;

use praxis_core::config::Config;
use praxis_test_utils::{free_port, http_get, start_backend, start_proxy};

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[test]
fn url_rewrite_example_config() {
    let backend_port = start_backend("ok");
    let proxy_port = free_port();
    let config = super::examples::load_example_config(
        "transformation/url-rewriting.yaml",
        proxy_port,
        HashMap::from([("127.0.0.1:3000", backend_port)]),
    );
    let addr = start_proxy(&config);

    let (status, body) = http_get(&addr, "/v1/users?debug=true", None);
    assert_eq!(status, 200, "url_rewrite config should proxy successfully");
    assert_eq!(body, "ok", "response body should pass through");
}

#[test]
fn url_rewrite_regex_changes_path_end_to_end() {
    let backend_port = start_backend("rewritten");
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
      - filter: url_rewrite
        operations:
          - regex_replace:
              pattern: "^/old/(.*)"
              replacement: "/new/$1"
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
    );

    let config = Config::from_yaml(&yaml).unwrap();
    let addr = start_proxy(&config);

    let (status, _body) = http_get(&addr, "/old/resource", None);
    assert_eq!(status, 200, "rewritten request should reach backend");
}

#[test]
fn url_rewrite_chained_with_router() {
    let api_port = start_backend("api-v2");
    let fallback_port = start_backend("fallback");
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
      - filter: url_rewrite
        operations:
          - regex_replace:
              pattern: "^/legacy/(.*)"
              replacement: "/api/$1"
      - filter: router
        routes:
          - path_prefix: "/api/"
            cluster: "api"
          - path_prefix: "/"
            cluster: "fallback"
      - filter: load_balancer
        clusters:
          - name: "api"
            endpoints:
              - "127.0.0.1:{api_port}"
          - name: "fallback"
            endpoints:
              - "127.0.0.1:{fallback_port}"
"#
    );

    let config = Config::from_yaml(&yaml).unwrap();
    let addr = start_proxy(&config);

    let (status, body) = http_get(&addr, "/api/users", None);
    assert_eq!(status, 200, "direct /api/ should route to api backend");
    assert_eq!(body, "api-v2", "direct /api/ should reach api backend");

    let (status, body) = http_get(&addr, "/other", None);
    assert_eq!(status, 200, "non-matching path should route to fallback");
    assert_eq!(body, "fallback", "non-matching path should reach fallback backend");
}

#[test]
fn url_rewrite_with_query_param_manipulation() {
    let backend_port = start_backend("stripped");
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
      - filter: url_rewrite
        operations:
          - strip_query_params:
              - debug
          - add_query_params:
              source: proxy
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
    );

    let config = Config::from_yaml(&yaml).unwrap();
    let addr = start_proxy(&config);

    let (status, body) = http_get(&addr, "/data?debug=1&keep=yes", None);
    assert_eq!(status, 200, "query param manipulation should work end-to-end");
    assert_eq!(body, "stripped", "response body should pass through");
}
