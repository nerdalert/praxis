// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Grid route inference example tests.

use std::collections::HashMap;

use praxis_test_utils::{free_port, http_send, json_post, parse_body, parse_status, start_backend};

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[test]
fn grid_route_inference_routes_by_model() {
    let local_port = start_backend("local-model");
    let remote_port = start_backend("remote-model");
    let proxy_port = free_port();
    let config = crate::example_utils::load_example_config(
        "ai/grid-route-inference.yaml",
        proxy_port,
        HashMap::from([("127.0.0.1:8001", local_port), ("127.0.0.1:8002", remote_port)]),
    );
    let proxy = praxis_test_utils::start_proxy(&config);

    let raw = http_send(
        proxy.addr(),
        &json_post("/v1/chat/completions", r#"{"model":"llama-3.2-8b"}"#),
    );

    assert_eq!(parse_status(&raw), 200, "known model should route");
    assert_eq!(
        parse_body(&raw),
        "remote-model",
        "grid_route should select the candidate cluster for the requested model"
    );
}
