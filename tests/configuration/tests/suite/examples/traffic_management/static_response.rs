// SPDX-License-Identifier: LGPL-3.0-only
// Copyright (c) 2024 Shane Utt

//! Static response example tests.

use std::collections::HashMap;

use praxis_test_utils::{free_port, http_get, start_proxy};

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[test]
fn static_response() {
    let proxy_port = free_port();
    let config = crate::example_utils::load_example_config(
        "traffic-management/static-response.yaml",
        proxy_port,
        HashMap::new(),
    );
    let addr = start_proxy(&config);
    let (status, body) = http_get(&addr, "/", None);
    assert_eq!(status, 200, "static response should return 200");
    assert!(
        body.contains("Welcome to Praxis"),
        "static response should contain welcome message"
    );
}
