// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Integration tests for the native llm-d endpoint picker filter.

use praxis_core::config::Config;
use praxis_test_utils::{
    Backend, free_port, http_send, json_post, parse_body, parse_header, parse_status, start_backend_with_shutdown,
    start_proxy,
};

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[test]
fn llmd_endpoint_picker_selects_lowest_pressure_endpoint() {
    let loaded_guard = start_backend_with_shutdown("loaded-backend");
    let better_guard = start_backend_with_shutdown("better-backend");
    let proxy_port = free_port();
    let yaml = picker_yaml(
        proxy_port,
        &[
            endpoint_yaml(
                "loaded",
                loaded_guard.port(),
                "fake-model",
                EndpointLoad {
                    running: 8,
                    waiting: 4,
                    kv: 90.0,
                },
            ),
            endpoint_yaml(
                "better",
                better_guard.port(),
                "fake-model",
                EndpointLoad {
                    running: 0,
                    waiting: 1,
                    kv: 10.0,
                },
            ),
        ],
    );
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/v1/chat/completions",
            r#"{"model":"fake-model","messages":[{"role":"user","content":"hi"}]}"#,
        ),
    );

    assert_eq!(parse_status(&raw), 200);
    assert_eq!(
        parse_body(&raw),
        "better-backend",
        "endpoint picker should route to the endpoint with better queue/KV score"
    );
}

#[test]
fn llmd_endpoint_picker_filters_by_model() {
    let model_a_guard = start_backend_with_shutdown("model-a-backend");
    let model_b_guard = start_backend_with_shutdown("model-b-backend");
    let proxy_port = free_port();
    let yaml = picker_yaml(
        proxy_port,
        &[
            endpoint_yaml("model-a", model_a_guard.port(), "model-a", IDLE),
            endpoint_yaml("model-b", model_b_guard.port(), "model-b", IDLE),
        ],
    );
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post("/v1/completions", r#"{"model":"model-b","prompt":"hi","max_tokens":8}"#),
    );

    assert_eq!(parse_status(&raw), 200);
    assert_eq!(
        parse_body(&raw),
        "model-b-backend",
        "endpoint picker should only select endpoints serving the requested model"
    );
}

#[test]
fn llmd_endpoint_picker_rejects_missing_model() {
    let backend_guard = start_backend_with_shutdown("backend");
    let proxy_port = free_port();
    let yaml = picker_yaml(
        proxy_port,
        &[endpoint_yaml("ep", backend_guard.port(), "fake-model", IDLE)],
    );
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(proxy.addr(), &json_post("/v1/chat/completions", r#"{"prompt":"hi"}"#));

    assert_eq!(
        parse_status(&raw),
        400,
        "request without model field should be rejected with 400"
    );
}

#[test]
fn llmd_endpoint_picker_rejects_no_eligible_endpoint() {
    let backend_guard = start_backend_with_shutdown("backend");
    let proxy_port = free_port();
    let yaml = picker_yaml(
        proxy_port,
        &[endpoint_yaml("ep", backend_guard.port(), "served-model", IDLE)],
    );
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post("/v1/chat/completions", r#"{"model":"unknown-model","messages":[]}"#),
    );

    assert_eq!(
        parse_status(&raw),
        503,
        "request for unserved model should be rejected with 503"
    );
}

#[test]
fn llmd_endpoint_picker_passes_sse_response_through() {
    let sse_body = "data: {\"token\":\"a\"}\n\ndata: {\"token\":\"b\"}\n\n";
    let sse_guard = Backend::fixed(sse_body)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .start_with_shutdown();
    let proxy_port = free_port();
    let yaml = picker_yaml(
        proxy_port,
        &[endpoint_yaml("sse", sse_guard.port(), "fake-model", IDLE)],
    );
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/v1/chat/completions",
            r#"{"model":"fake-model","messages":[],"stream":true}"#,
        ),
    );

    assert_eq!(parse_status(&raw), 200);
    assert_eq!(
        parse_header(&raw, "content-type").as_deref(),
        Some("text/event-stream"),
        "SSE content-type should pass through"
    );
    assert_eq!(parse_body(&raw), sse_body, "SSE response body should be unchanged");
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

/// Build proxy YAML with the llm-d endpoint picker filter.
fn picker_yaml(proxy_port: u16, endpoints: &[String]) -> String {
    let endpoints_yaml = endpoints.join("\n");
    format!(
        r#"
listeners:
  - name: default
    address: "127.0.0.1:{proxy_port}"
    filter_chains: [main]
filter_chains:
  - name: main
    filters:
      - filter: llmd_endpoint_picker
        endpoints:
{endpoints_yaml}
"#
    )
}

/// Static endpoint load for test YAML generation.
#[derive(Clone, Copy)]
struct EndpointLoad {
    running: u64,
    waiting: u64,
    kv: f64,
}

/// Idle endpoint with no queue pressure or cache usage.
const IDLE: EndpointLoad = EndpointLoad {
    running: 0,
    waiting: 0,
    kv: 0.0,
};

/// Build one endpoint entry for the picker YAML.
fn endpoint_yaml(name: &str, port: u16, model: &str, EndpointLoad { running, waiting, kv }: EndpointLoad) -> String {
    format!(
        r#"          - name: {name}
            address: "127.0.0.1:{port}"
            models: ["{model}"]
            running_requests: {running}
            waiting_requests: {waiting}
            kv_cache_usage_percent: {kv}"#
    )
}
