// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Tests for the llm-d endpoint picker example config.

use std::collections::HashMap;

use praxis_test_utils::{
    free_port, http_send, json_post, parse_body, parse_status, start_backend_with_shutdown, start_proxy,
};

use super::load_example_config;

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[test]
fn llmd_endpoint_picker_example_routes_to_least_loaded() {
    let vllm_a_guard = start_backend_with_shutdown("vllm-a-response");
    let vllm_b_guard = start_backend_with_shutdown("vllm-b-response");
    let vllm_c_guard = start_backend_with_shutdown("vllm-c-response");
    let proxy_port = free_port();

    let port_map = HashMap::from([
        ("10.0.1.1:8000", vllm_a_guard.port()),
        ("10.0.1.2:8000", vllm_b_guard.port()),
        ("10.0.2.1:8000", vllm_c_guard.port()),
    ]);
    let config = load_example_config("ai/llmd-endpoint-picker.yaml", proxy_port, port_map);
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/v1/chat/completions",
            r#"{"model":"meta-llama/Llama-3.2-3B-Instruct","messages":[{"role":"user","content":"hi"}]}"#,
        ),
    );

    assert_eq!(parse_status(&raw), 200);
    assert_eq!(
        parse_body(&raw),
        "vllm-a-response",
        "should route to vllm-a (lowest load for Llama-3.2-3B-Instruct)"
    );
}

#[test]
fn llmd_endpoint_picker_example_routes_by_model() {
    let vllm_a_guard = start_backend_with_shutdown("vllm-a-response");
    let vllm_b_guard = start_backend_with_shutdown("vllm-b-response");
    let vllm_c_guard = start_backend_with_shutdown("vllm-c-response");
    let proxy_port = free_port();

    let port_map = HashMap::from([
        ("10.0.1.1:8000", vllm_a_guard.port()),
        ("10.0.1.2:8000", vllm_b_guard.port()),
        ("10.0.2.1:8000", vllm_c_guard.port()),
    ]);
    let config = load_example_config("ai/llmd-endpoint-picker.yaml", proxy_port, port_map);
    let proxy = start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post(
            "/v1/chat/completions",
            r#"{"model":"Qwen/Qwen3-0.6B","messages":[{"role":"user","content":"hi"}]}"#,
        ),
    );

    assert_eq!(parse_status(&raw), 200);
    assert_eq!(
        parse_body(&raw),
        "vllm-c-response",
        "should route to vllm-c (lowest load for Qwen3-0.6B)"
    );
}
