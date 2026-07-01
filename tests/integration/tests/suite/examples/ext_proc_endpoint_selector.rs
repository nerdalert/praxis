// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Praxis Contributors

//! Functional integration test for the ext-proc-endpoint-selector example config.

use std::collections::HashMap;

use praxis_test_utils::free_port;

#[test]
fn ext_proc_endpoint_selector_example_routes() {
    let backend = praxis_test_utils::start_echo_backend();
    let proc_guard =
        super::super::ext_proc::start_mock_processor(super::super::ext_proc::MockBehavior::RouteOnRequestEos {
            destination: format!("127.0.0.1:{}", backend.port()),
        });
    let proxy_port = free_port();
    let config = super::load_example_config(
        "traffic-management/ext-proc-endpoint-selector.yaml",
        proxy_port,
        HashMap::from([("127.0.0.1:9002", proc_guard.addr.port())]),
    );
    let registry = praxis::build_full_registry();
    let proxy = praxis_test_utils::start_proxy_with_registry(&config, &registry);

    let (status, body) = praxis_test_utils::http_post(proxy.addr(), "/example-test", "example body");
    assert_eq!(status, 200, "example config should route successfully");
    assert_eq!(body, "example body", "backend should echo the original body");
}
