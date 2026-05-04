// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Integration tests for the `a2a` classifier filter.

use praxis_core::config::Config;
use praxis_test_utils::{
    free_port, http_send, json_post, parse_body, parse_status,
    start_backend_with_shutdown, start_header_echo_backend_with_shutdown, start_proxy,
};

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[test]
fn a2a_send_message_routes_to_agent() {
    let agent_guard = start_backend_with_shutdown("agent-alpha");
    let proxy_port = free_port();

    let yaml = a2a_routing_yaml(proxy_port, agent_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/a2a/",
            r#"{"jsonrpc":"2.0","id":"msg-1","method":"SendMessage","params":{"message":{"content":"hello"}}}"#,
        ),
    );

    assert_eq!(parse_status(&raw), 200);
    assert_eq!(parse_body(&raw), "agent-alpha");
}

#[test]
fn a2a_get_task_routes_by_static_config() {
    let agent_guard = start_backend_with_shutdown("task-agent");
    let proxy_port = free_port();

    let yaml = a2a_routing_yaml(proxy_port, agent_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/a2a/",
            r#"{"jsonrpc":"2.0","id":"req-2","method":"GetTask","params":{"id":"task-123"}}"#,
        ),
    );

    assert_eq!(parse_status(&raw), 200);
    assert_eq!(parse_body(&raw), "task-agent");
}

#[test]
fn a2a_promotes_headers() {
    let backend_guard = start_header_echo_backend_with_shutdown();
    let proxy_port = free_port();

    let yaml = a2a_echo_yaml(proxy_port, backend_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/a2a/",
            r#"{"jsonrpc":"2.0","id":"msg-3","method":"SendMessage","params":{"message":{}}}"#,
        ),
    );

    assert_eq!(parse_status(&raw), 200);
    let body = parse_body(&raw);
    assert!(
        body.to_lowercase().contains("x-praxis-a2a-method: sendmessage"),
        "expected A2A method header, got:\n{body}"
    );
    assert!(
        body.to_lowercase().contains("x-praxis-a2a-family: message"),
        "expected A2A family header, got:\n{body}"
    );
}

#[test]
fn a2a_v03_alias_routes_correctly() {
    let agent_guard = start_backend_with_shutdown("agent-v03");
    let proxy_port = free_port();

    let yaml = a2a_alias_yaml(proxy_port, agent_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/a2a/",
            r#"{"jsonrpc":"2.0","id":"msg-4","method":"message/send","params":{"message":{}}}"#,
        ),
    );

    assert_eq!(parse_status(&raw), 200);
    assert_eq!(parse_body(&raw), "agent-v03");
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

fn a2a_routing_yaml(proxy_port: u16, agent_port: u16) -> String {
    format!(
        r#"
listeners:
  - name: default
    address: "127.0.0.1:{proxy_port}"
    filter_chains: [main]
filter_chains:
  - name: main
    filters:
      - filter: a2a
        max_body_bytes: 65536
        on_invalid: continue
      - filter: router
        routes:
          - path_prefix: "/a2a/"
            cluster: "agent"
          - path_prefix: "/"
            cluster: "agent"
      - filter: load_balancer
        clusters:
          - name: "agent"
            endpoints:
              - "127.0.0.1:{agent_port}"
"#
    )
}

fn a2a_echo_yaml(proxy_port: u16, backend_port: u16) -> String {
    format!(
        r#"
listeners:
  - name: default
    address: "127.0.0.1:{proxy_port}"
    filter_chains: [main]
filter_chains:
  - name: main
    filters:
      - filter: a2a
        max_body_bytes: 65536
        on_invalid: continue
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

fn a2a_alias_yaml(proxy_port: u16, agent_port: u16) -> String {
    format!(
        r#"
listeners:
  - name: default
    address: "127.0.0.1:{proxy_port}"
    filter_chains: [main]
filter_chains:
  - name: main
    filters:
      - filter: a2a
        max_body_bytes: 65536
        on_invalid: continue
        method_aliases:
          "message/send": SendMessage
          "tasks/get": GetTask
      - filter: router
        routes:
          - path_prefix: "/a2a/"
            headers:
              x-praxis-a2a-method: "SendMessage"
            cluster: "agent"
          - path_prefix: "/a2a/"
            headers:
              x-praxis-a2a-method: "GetTask"
            cluster: "agent"
          - path_prefix: "/"
            cluster: "agent"
      - filter: load_balancer
        clusters:
          - name: "agent"
            endpoints:
              - "127.0.0.1:{agent_port}"
"#
    )
}
