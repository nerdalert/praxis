// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Integration tests for the native llm-d endpoint picker filter.

use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::Mutex,
    time::Duration,
};

use praxis_core::config::Config;
use praxis_test_utils::{
    Backend, free_port, http_send, json_post, parse_body, parse_header, parse_status, start_backend_with_shutdown,
    start_echo_backend_with_shutdown, start_header_echo_backend_with_shutdown, start_proxy,
};

/// Mutex guarding environment variable mutations so that parallel
/// tests cannot interfere with each other.
static ENV_MUTEX: Mutex<()> = Mutex::new(());

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
            endpoint_yaml("loaded", loaded_guard.port(), "fake-model", 8, 4, 90.0),
            endpoint_yaml("better", better_guard.port(), "fake-model", 0, 1, 10.0),
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
            endpoint_yaml("model-a", model_a_guard.port(), "model-a", 0, 0, 0.0),
            endpoint_yaml("model-b", model_b_guard.port(), "model-b", 0, 0, 0.0),
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
fn llmd_endpoint_picker_passes_sse_response_through() {
    let sse_body = "data: {\"token\":\"a\"}\n\ndata: {\"token\":\"b\"}\n\n";
    let sse_guard = Backend::fixed(sse_body)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .start_with_shutdown();
    let proxy_port = free_port();
    let yaml = picker_yaml(
        proxy_port,
        &[endpoint_yaml("sse", sse_guard.port(), "fake-model", 0, 0, 0.0)],
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

#[test]
fn llmd_endpoint_picker_metrics_scrape_changes_routing() {
    let scrape_target_guard = start_backend_with_shutdown("scrape-target");
    let static_guard = start_backend_with_shutdown("static-endpoint");

    // Reserve a port for the metrics server but don't start it yet.
    // The worker will fail to scrape and mark scrape-target unhealthy,
    // so static-endpoint wins initially.
    let metrics_port = free_port();

    let proxy_port = free_port();
    let yaml = picker_yaml_with_metrics(
        proxy_port,
        &[
            endpoint_yaml_with_metrics(
                "scrape-target",
                scrape_target_guard.port(),
                "fake-model",
                0,
                0,
                0.0,
                Some(metrics_port),
            ),
            endpoint_yaml_with_metrics("static-endpoint", static_guard.port(), "fake-model", 2, 1, 20.0, None),
        ],
    );
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let request = json_post(
        "/v1/chat/completions",
        r#"{"model":"fake-model","messages":[{"role":"user","content":"hi"}]}"#,
    );

    // Wait for the worker to fail its first scrape, marking scrape-target
    // unhealthy. The static-endpoint should be the only eligible one.
    std::thread::sleep(Duration::from_millis(150));

    let raw_before = http_send(proxy.addr(), &request);
    assert_eq!(parse_status(&raw_before), 200);
    assert_eq!(
        parse_body(&raw_before),
        "static-endpoint",
        "before metrics server starts, scrape-target should be unhealthy"
    );

    // Now start the metrics server reporting scrape-target as idle.
    let metrics_body = "\
        vllm:num_requests_running{model_name=\"fake-model\"} 0\n\
        vllm:num_requests_waiting{model_name=\"fake-model\"} 0\n\
        vllm:gpu_cache_usage_perc 0.0\n";
    let _metrics_guard = start_metrics_server(metrics_port, metrics_body);

    // Poll until scrape-target becomes healthy and is selected.
    let mut saw_switch = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(50));
        let raw = http_send(proxy.addr(), &request);
        if parse_status(&raw) == 200 && parse_body(&raw) == "scrape-target" {
            saw_switch = true;
            break;
        }
    }

    assert!(
        saw_switch,
        "after metrics server starts, scrape-target should become healthy and be preferred"
    );
}

#[test]
fn llmd_endpoint_picker_prefix_cache_changes_routing() {
    let backend_a = start_backend_with_shutdown("backend-a");
    let backend_b = start_backend_with_shutdown("backend-b");

    let metrics_a_port = free_port();
    let metrics_b_port = free_port();

    let idle_metrics = "\
        vllm:num_requests_running 0\n\
        vllm:num_requests_waiting 0\n\
        vllm:kv_cache_usage_perc 0.0\n";
    let _metrics_a = start_metrics_server(metrics_a_port, idle_metrics);
    let _metrics_b = start_metrics_server(metrics_b_port, idle_metrics);

    let proxy_port = free_port();
    let yaml = picker_yaml_with_prefix_cache(
        proxy_port,
        &[
            endpoint_yaml_with_metrics("ep-a", backend_a.port(), "fake-model", 0, 0, 0.0, Some(metrics_a_port)),
            endpoint_yaml_with_metrics("ep-b", backend_b.port(), "fake-model", 0, 0, 0.0, Some(metrics_b_port)),
        ],
    );
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    std::thread::sleep(Duration::from_millis(150));

    let same_prefix = json_post(
        "/v1/chat/completions",
        r#"{"model":"fake-model","messages":[{"role":"user","content":"this is a long enough prefix to generate multiple hash blocks for the test"}]}"#,
    );
    let raw1 = http_send(proxy.addr(), &same_prefix);
    assert_eq!(parse_status(&raw1), 200);
    let first_backend = parse_body(&raw1);

    let other_backend = if first_backend == "backend-a" {
        "backend-b"
    } else {
        "backend-a"
    };

    drop(_metrics_a);
    drop(_metrics_b);

    let loaded_metrics = "\
        vllm:num_requests_running 20\n\
        vllm:num_requests_waiting 10\n\
        vllm:kv_cache_usage_perc 0.9\n";

    let (_new_metrics_a, _new_metrics_b) = if first_backend == "backend-a" {
        (
            start_metrics_server(metrics_a_port, loaded_metrics),
            start_metrics_server(metrics_b_port, idle_metrics),
        )
    } else {
        (
            start_metrics_server(metrics_a_port, idle_metrics),
            start_metrics_server(metrics_b_port, loaded_metrics),
        )
    };

    let mut prefix_hit = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(50));
        let raw = http_send(proxy.addr(), &same_prefix);
        if parse_status(&raw) == 200 && parse_body(&raw) == first_backend {
            prefix_hit = true;
            break;
        }
    }

    assert!(
        prefix_hit,
        "prefix cache should keep routing to {first_backend} despite worse load/KV metrics"
    );

    let diff_prefix = json_post(
        "/v1/chat/completions",
        r#"{"model":"fake-model","messages":[{"role":"user","content":"completely different prompt text that should not match the prefix index at all"}]}"#,
    );
    let raw_diff = http_send(proxy.addr(), &diff_prefix);
    assert_eq!(parse_status(&raw_diff), 200);
    assert_eq!(
        parse_body(&raw_diff),
        other_backend,
        "a different prefix should route to the lower-load endpoint"
    );
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

#[test]
fn llmd_endpoint_picker_saturation_gate_rejects_when_saturated() {
    let backend_a = start_backend_with_shutdown("backend-a");
    let backend_b = start_backend_with_shutdown("backend-b");
    let proxy_port = free_port();
    let yaml = picker_yaml_with_saturation(
        proxy_port,
        &[
            endpoint_yaml("ep-a", backend_a.port(), "fake-model", 0, 20, 99.0),
            endpoint_yaml("ep-b", backend_b.port(), "fake-model", 0, 20, 99.0),
        ],
        2,
        0.5,
        0.5,
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

    assert_eq!(parse_status(&raw), 429, "saturated pool should reject with 429");
}

#[test]
fn llmd_endpoint_picker_saturation_gate_routes_to_healthy() {
    let saturated_guard = start_backend_with_shutdown("saturated-backend");
    let healthy_guard = start_backend_with_shutdown("healthy-backend");
    let proxy_port = free_port();
    let yaml = picker_yaml_with_saturation(
        proxy_port,
        &[
            endpoint_yaml("saturated", saturated_guard.port(), "fake-model", 0, 20, 99.0),
            endpoint_yaml("healthy", healthy_guard.port(), "fake-model", 0, 0, 5.0),
        ],
        5,
        0.8,
        5.0,
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

    assert_eq!(parse_status(&raw), 200, "request should be admitted");
    assert_eq!(
        parse_body(&raw),
        "healthy-backend",
        "should route to the healthy endpoint, not the saturated one"
    );
}

#[test]
#[allow(
    unsafe_code,
    reason = "set_var/remove_var are unsafe in edition 2024; guarded by ENV_MUTEX"
)]
fn llmd_endpoint_picker_saturation_gate_objective_aware_admission() {
    ensure_crypto_provider();
    let _env_guard = ENV_MUTEX.lock().unwrap();
    let temp_dir = tempfile::tempdir().unwrap();
    let tmp = temp_dir.path();

    std::fs::write(tmp.join("token"), "test-token").unwrap();
    std::fs::write(tmp.join("namespace"), "default").unwrap();

    let (ca_pem, server_config) = generate_test_tls_pair(tmp);
    let fake_k8s = start_fake_k8s_api(server_config);
    std::fs::write(tmp.join("ca.pem"), ca_pem).unwrap();

    let saved = save_env_vars();
    set_k8s_env_vars(tmp, fake_k8s.port);

    let backend_a = start_backend_with_shutdown("backend-a");
    let backend_b = start_backend_with_shutdown("backend-b");
    let proxy_port = free_port();

    let yaml = picker_yaml_with_saturation_and_objective(
        proxy_port,
        &[
            endpoint_yaml("ep-a", backend_a.port(), "fake-model", 0, 5, 80.0),
            endpoint_yaml("ep-b", backend_b.port(), "fake-model", 0, 5, 80.0),
        ],
    );
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    wait_for_objective_sync(proxy.addr());

    let no_header_raw = http_send(
        proxy.addr(),
        &json_post(
            "/v1/chat/completions",
            r#"{"model":"fake-model","messages":[{"role":"user","content":"hi"}]}"#,
        ),
    );
    assert_eq!(
        parse_status(&no_header_raw),
        429,
        "request without objective header should be rejected (pool at saturation threshold)"
    );

    let high_pri_request = build_request_with_objective_header("high-priority");
    let high_pri_raw = http_send(proxy.addr(), &high_pri_request);
    assert_eq!(
        parse_status(&high_pri_raw),
        200,
        "high-priority request should be admitted past saturation"
    );

    restore_env_vars(&saved);
    drop(fake_k8s);
}

#[test]
#[allow(
    unsafe_code,
    reason = "set_var/remove_var are unsafe in edition 2024; guarded by ENV_MUTEX"
)]
fn llmd_endpoint_picker_saturation_gate_negative_priority_rejected() {
    ensure_crypto_provider();
    let _env_guard = ENV_MUTEX.lock().unwrap();
    let temp_dir = tempfile::tempdir().unwrap();
    let tmp = temp_dir.path();

    std::fs::write(tmp.join("token"), "test-token").unwrap();
    std::fs::write(tmp.join("namespace"), "default").unwrap();

    let (ca_pem, server_config) = generate_test_tls_pair(tmp);
    let fake_k8s = start_fake_k8s_api(server_config);
    std::fs::write(tmp.join("ca.pem"), ca_pem).unwrap();

    let saved = save_env_vars();
    set_k8s_env_vars(tmp, fake_k8s.port);

    let backend_a = start_backend_with_shutdown("backend-a");
    let backend_b = start_backend_with_shutdown("backend-b");
    let proxy_port = free_port();

    let yaml = picker_yaml_with_saturation_and_objective_moderate(
        proxy_port,
        &[
            endpoint_yaml("ep-a", backend_a.port(), "fake-model", 0, 4, 70.0),
            endpoint_yaml("ep-b", backend_b.port(), "fake-model", 0, 4, 70.0),
        ],
    );
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    wait_for_objective_sync(proxy.addr());

    let no_header_raw = http_send(
        proxy.addr(),
        &json_post(
            "/v1/chat/completions",
            r#"{"model":"fake-model","messages":[{"role":"user","content":"hi"}]}"#,
        ),
    );
    assert_eq!(
        parse_status(&no_header_raw),
        200,
        "request without objective header at moderate load should be admitted (priority 0)"
    );

    let low_pri_request = build_request_with_objective_header("low-priority");
    let low_pri_raw = http_send(proxy.addr(), &low_pri_request);
    assert_eq!(
        parse_status(&low_pri_raw),
        429,
        "low-priority (-5) request should be rejected because effective threshold is lowered"
    );

    restore_env_vars(&saved);
    drop(fake_k8s);
}

/// Build proxy YAML with saturation gate and inference objective enabled.
///
/// Endpoints are at saturation threshold (pool_saturation_threshold=1.0),
/// so default priority (0) is rejected but priority 10 adds 1.0 headroom.
fn picker_yaml_with_saturation_and_objective(proxy_port: u16, endpoints: &[String]) -> String {
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
        metrics_refresh_ms: 50
        metrics_timeout_ms: 200
        saturation_gate:
          enabled: true
          queue_depth_threshold: 5
          kv_cache_util_threshold: 0.8
          pool_saturation_threshold: 1.0
          headroom: 0.2
          reject_status: 429
          priority_headroom_per_level: 0.1
        inference_objective:
          enabled: true
          namespace: default
          pool_ref:
            name: sim-pool
        endpoints:
{endpoints_yaml}
"#
    )
}

/// Build proxy YAML with moderate load where default priority (0) is
/// admitted but negative priority (-5) lowers the threshold enough to
/// reject.
///
/// With `pool_saturation_threshold: 1.0` and `priority_headroom_per_level:
/// 0.1`, priority -5 gives effective threshold 0.5. Endpoints with
/// `waiting_requests: 4` and `kv_cache_usage_percent: 70.0` produce pool
/// saturation ~0.875, which exceeds 0.5 but not 1.0.
fn picker_yaml_with_saturation_and_objective_moderate(proxy_port: u16, endpoints: &[String]) -> String {
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
        metrics_refresh_ms: 50
        metrics_timeout_ms: 200
        saturation_gate:
          enabled: true
          queue_depth_threshold: 5
          kv_cache_util_threshold: 0.8
          pool_saturation_threshold: 1.0
          headroom: 0.2
          reject_status: 429
          priority_headroom_per_level: 0.1
        inference_objective:
          enabled: true
          namespace: default
          pool_ref:
            name: sim-pool
        endpoints:
{endpoints_yaml}
"#
    )
}

/// Build an HTTP request with the `x-llm-d-inference-objective` header.
fn build_request_with_objective_header(objective: &str) -> String {
    let body = r#"{"model":"fake-model","messages":[{"role":"user","content":"hi"}]}"#;
    format!(
        "POST /v1/chat/completions HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         x-llm-d-inference-objective: {objective}\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {body}",
        body.len(),
    )
}

/// Poll until the proxy's objective worker has loaded objectives from
/// the fake K8s API.
///
/// Without this synchronization the test may run before the async
/// worker has fetched the `InferenceObjective` list.
fn wait_for_objective_sync(addr: &str) {
    let request = build_request_with_objective_header("high-priority");
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(100));
        let raw = http_send(addr, &request);
        let status = parse_status(&raw);
        if status == 200 || status == 429 {
            return;
        }
    }
}

fn picker_yaml_with_saturation(
    proxy_port: u16,
    endpoints: &[String],
    queue_depth_threshold: u64,
    kv_cache_util_threshold: f64,
    pool_saturation_threshold: f64,
) -> String {
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
        saturation_gate:
          enabled: true
          queue_depth_threshold: {queue_depth_threshold}
          kv_cache_util_threshold: {kv_cache_util_threshold}
          pool_saturation_threshold: {pool_saturation_threshold}
          headroom: 0.2
          reject_status: 429
        endpoints:
{endpoints_yaml}
"#
    )
}

fn picker_yaml_with_prefix_cache(proxy_port: u16, endpoints: &[String]) -> String {
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
        metrics_refresh_ms: 50
        metrics_timeout_ms: 200
        prefix_cache:
          enabled: true
          weight: 10.0
          block_size_tokens: 4
          max_prefix_blocks_to_match: 64
          lru_capacity_per_endpoint: 1000
        endpoints:
{endpoints_yaml}
"#
    )
}

fn start_metrics_server(port: u16, body: &str) -> MetricsServerGuard {
    let listener = TcpListener::bind(format!("127.0.0.1:{port}")).unwrap();
    listener.set_nonblocking(false).unwrap();
    let body = body.to_owned();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_clone = std::sync::Arc::clone(&stop);

    let join = std::thread::spawn(move || {
        listener.set_nonblocking(true).expect("failed to set non-blocking");
        while !stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    let mut buf = [0u8; 1024];
                    drop(stream.read(&mut buf));

                    let response = format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\
                         \r\n\
                         {}",
                        body.len(),
                        body
                    );
                    drop(stream.write_all(response.as_bytes()));
                },
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                },
                Err(_) => break,
            }
        }
    });

    MetricsServerGuard { stop, join: Some(join) }
}

struct MetricsServerGuard {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for MetricsServerGuard {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _result = join.join();
        }
    }
}

fn picker_yaml_with_metrics(proxy_port: u16, endpoints: &[String]) -> String {
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
        metrics_refresh_ms: 50
        metrics_timeout_ms: 200
        endpoints:
{endpoints_yaml}
"#
    )
}

#[allow(clippy::too_many_arguments, reason = "test YAML builder")]
fn endpoint_yaml_with_metrics(
    name: &str,
    port: u16,
    model: &str,
    running: u64,
    waiting: u64,
    kv: f64,
    metrics_port: Option<u16>,
) -> String {
    let metrics_line = match metrics_port {
        Some(mp) => format!("\n            metrics_url: \"http://127.0.0.1:{mp}/metrics\""),
        None => String::new(),
    };
    format!(
        r#"          - name: {name}
            address: "127.0.0.1:{port}"
            models: ["{model}"]
            running_requests: {running}
            waiting_requests: {waiting}
            kv_cache_usage_percent: {kv}{metrics_line}"#
    )
}

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

#[allow(clippy::too_many_arguments, reason = "test YAML builder")]
fn endpoint_yaml(name: &str, port: u16, model: &str, running: u64, waiting: u64, kv: f64) -> String {
    format!(
        r#"          - name: {name}
            address: "127.0.0.1:{port}"
            models: ["{model}"]
            running_requests: {running}
            waiting_requests: {waiting}
            kv_cache_usage_percent: {kv}"#
    )
}

#[allow(clippy::too_many_arguments, reason = "test YAML builder")]
fn endpoint_yaml_with_role(
    name: &str,
    port: u16,
    model: &str,
    running: u64,
    waiting: u64,
    kv: f64,
    role: &str,
) -> String {
    format!(
        r#"          - name: {name}
            address: "127.0.0.1:{port}"
            models: ["{model}"]
            running_requests: {running}
            waiting_requests: {waiting}
            kv_cache_usage_percent: {kv}
            role: {role}"#
    )
}

fn picker_yaml_with_disaggregation(proxy_port: u16, endpoints: &[String]) -> String {
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
        disaggregation:
          enabled: true
          prefill_mode: always
        endpoints:
{endpoints_yaml}
"#
    )
}

#[test]
fn llmd_endpoint_picker_disaggregation_routes_to_decode() {
    let decode_guard = start_backend_with_shutdown("decode-backend");
    let prefill_guard = start_backend_with_shutdown("prefill-backend");
    let proxy_port = free_port();
    let yaml = picker_yaml_with_disaggregation(
        proxy_port,
        &[
            endpoint_yaml_with_role("decode-ep", decode_guard.port(), "fake-model", 0, 0, 0.0, "decode"),
            endpoint_yaml_with_role("prefill-ep", prefill_guard.port(), "fake-model", 0, 0, 0.0, "prefill"),
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

    assert_eq!(parse_status(&raw), 200, "should route successfully");
    assert_eq!(
        parse_body(&raw),
        "decode-backend",
        "disaggregation should route to the decode endpoint, not the prefill endpoint"
    );
}

#[test]
fn llmd_endpoint_picker_disaggregation_forwards_prefill_header() {
    let decode_guard = start_header_echo_backend_with_shutdown();
    let prefill_guard = start_backend_with_shutdown("prefill-backend");
    let proxy_port = free_port();
    let yaml = picker_yaml_with_disaggregation(
        proxy_port,
        &[
            endpoint_yaml_with_role("decode-ep", decode_guard.port(), "fake-model", 0, 0, 0.0, "decode"),
            endpoint_yaml_with_role("prefill-ep", prefill_guard.port(), "fake-model", 0, 0, 0.0, "prefill"),
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

    assert_eq!(parse_status(&raw), 200, "should route successfully");
    let body = parse_body(&raw);
    assert!(
        body.contains("x-prefiller-host-port"),
        "response body should contain the prefill header name, got: {body}"
    );
    let prefill_addr = format!("127.0.0.1:{}", prefill_guard.port());
    assert!(
        body.contains(&prefill_addr),
        "response body should contain the prefill backend address {prefill_addr}, got: {body}"
    );
}

#[test]
fn llmd_endpoint_picker_disaggregation_injects_kv_transfer_params() {
    let decode_guard = start_echo_backend_with_shutdown();
    let prefill_guard = start_backend_with_shutdown("prefill-backend");
    let proxy_port = free_port();
    let yaml = picker_yaml_with_disaggregation(
        proxy_port,
        &[
            endpoint_yaml_with_role("decode-ep", decode_guard.port(), "fake-model", 0, 0, 0.0, "decode"),
            endpoint_yaml_with_role("prefill-ep", prefill_guard.port(), "fake-model", 0, 0, 0.0, "prefill"),
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

    assert_eq!(parse_status(&raw), 200, "should route successfully");
    let body = parse_body(&raw);
    let prefill_port = prefill_guard.port();
    let parsed: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("echo body should be valid JSON: {e}\nbody: {body}"));
    let kv = &parsed["kv_transfer_params"];
    assert_eq!(kv["do_remote_decode"], true, "do_remote_decode should be true");
    assert_eq!(kv["do_remote_prefill"], false, "do_remote_prefill should be false");
    assert!(kv["remote_host"].as_str().is_some(), "remote_host should be a string");
    let remote_host = kv["remote_host"].as_str().unwrap();
    assert!(
        remote_host.contains(&format!("{prefill_port}")),
        "remote_host should contain prefill port {prefill_port}, got: {remote_host}"
    );
}

#[test]
#[allow(
    unsafe_code,
    reason = "set_var/remove_var are unsafe in edition 2024; guarded by ENV_MUTEX"
)]
fn llmd_endpoint_picker_model_rewrite_rewrites_upstream_body() {
    ensure_crypto_provider();
    let _env_guard = ENV_MUTEX.lock().unwrap();
    let temp_dir = tempfile::tempdir().unwrap();
    let tmp = temp_dir.path();

    std::fs::write(tmp.join("token"), "test-token").unwrap();
    std::fs::write(tmp.join("namespace"), "default").unwrap();

    let (ca_pem, server_config) = generate_test_tls_pair(tmp);
    let fake_k8s = start_fake_k8s_api(server_config);

    std::fs::write(tmp.join("ca.pem"), ca_pem).unwrap();

    let saved = save_env_vars();
    set_k8s_env_vars(tmp, fake_k8s.port);

    let original_guard = start_backend_with_shutdown("original-backend");
    let rewritten_guard = start_echo_backend_with_shutdown();
    let proxy_port = free_port();

    let yaml = picker_yaml_with_model_rewrite(proxy_port, original_guard.port(), rewritten_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    let request = json_post(
        "/v1/chat/completions",
        r#"{"model":"original-model","messages":[{"role":"user","content":"hi"}]}"#,
    );

    let mut saw_rewrite = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(100));
        let raw = http_send(proxy.addr(), &request);
        if parse_status(&raw) == 200 {
            let body = parse_body(&raw);
            if body.contains("rewritten-model") {
                saw_rewrite = true;
                break;
            }
        }
    }

    restore_env_vars(&saved);
    drop(fake_k8s);

    assert!(saw_rewrite, "upstream should receive rewritten-model in the body");
}

// -----------------------------------------------------------------------------
// Inference Objective Test
// -----------------------------------------------------------------------------

#[test]
#[allow(
    unsafe_code,
    reason = "set_var/remove_var are unsafe in edition 2024; guarded by ENV_MUTEX"
)]
fn llmd_endpoint_picker_inference_objective_does_not_break_routing() {
    ensure_crypto_provider();
    let _env_guard = ENV_MUTEX.lock().unwrap();
    let temp_dir = tempfile::tempdir().unwrap();
    let tmp = temp_dir.path();

    std::fs::write(tmp.join("token"), "test-token").unwrap();
    std::fs::write(tmp.join("namespace"), "default").unwrap();

    let (ca_pem, server_config) = generate_test_tls_pair(tmp);
    let fake_k8s = start_fake_k8s_api(server_config);

    std::fs::write(tmp.join("ca.pem"), ca_pem).unwrap();

    let saved = save_env_vars();
    set_k8s_env_vars(tmp, fake_k8s.port);

    let backend_guard = start_backend_with_shutdown("objective-backend");
    let proxy_port = free_port();

    let yaml = picker_yaml_with_objective(proxy_port, backend_guard.port());
    let config = Config::from_yaml(&yaml).unwrap();
    let proxy = start_proxy(&config);

    // Wait for the worker to load objectives from the fake K8s API.
    // The proof: the worker contacts the fake HTTPS K8s API, parses
    // the InferenceObjective list, and builds a snapshot without
    // breaking routing. Objective metadata is internal and cannot be
    // inspected from an integration test, so HTTP 200 with correct
    // backend selection proves the feature integrates end-to-end.
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         x-llm-d-inference-objective: high-priority\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {}",
        r#"{"model":"fake-model","messages":[{"role":"user","content":"hi"}]}"#.len(),
        r#"{"model":"fake-model","messages":[{"role":"user","content":"hi"}]}"#,
    );

    let mut saw_success = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(100));
        let raw = http_send(proxy.addr(), &request);
        if parse_status(&raw) == 200 && parse_body(&raw) == "objective-backend" {
            saw_success = true;
            break;
        }
    }

    restore_env_vars(&saved);
    drop(fake_k8s);

    assert!(
        saw_success,
        "routing should succeed with inference_objective enabled and the worker loading objectives from K8s"
    );
}

/// Build proxy YAML with inference objective enabled.
fn picker_yaml_with_objective(proxy_port: u16, backend_port: u16) -> String {
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
        metrics_refresh_ms: 50
        metrics_timeout_ms: 200
        inference_objective:
          enabled: true
          namespace: default
          pool_ref:
            name: sim-pool
        endpoints:
          - name: objective-ep
            address: "127.0.0.1:{backend_port}"
            models: ["fake-model"]
"#
    )
}

// -----------------------------------------------------------------------------
// Model Rewrite Test Utilities
// -----------------------------------------------------------------------------

/// Names of environment variables mutated by the model rewrite test.
const K8S_ENV_VARS: &[&str] = &[
    "KUBERNETES_SERVICE_HOST",
    "KUBERNETES_SERVICE_PORT",
    "PRAXIS_K8S_SERVICEACCOUNT_TOKEN_PATH",
    "PRAXIS_K8S_SERVICEACCOUNT_CA_PATH",
    "PRAXIS_K8S_SERVICEACCOUNT_NAMESPACE_PATH",
];

/// Saved environment variable state for later restoration.
struct SavedEnv(Vec<(&'static str, Option<String>)>);

/// Capture the current values of all K8s env vars.
fn save_env_vars() -> SavedEnv {
    SavedEnv(
        K8S_ENV_VARS
            .iter()
            .map(|&name| (name, std::env::var(name).ok()))
            .collect(),
    )
}

/// Restore environment variables to their previously saved state.
///
/// Must be called while the `ENV_MUTEX` lock is held so that no
/// other thread reads the process environment concurrently.
#[allow(unsafe_code, reason = "set_var/remove_var are unsafe in edition 2024")]
fn restore_env_vars(saved: &SavedEnv) {
    for &(name, ref val) in &saved.0 {
        match val {
            // SAFETY: guarded by ENV_MUTEX; no concurrent readers.
            Some(v) => unsafe { std::env::set_var(name, v) },
            // SAFETY: guarded by ENV_MUTEX; no concurrent readers.
            None => unsafe { std::env::remove_var(name) },
        }
    }
}

/// Set environment variables to point at the fake K8s API.
///
/// Must be called while the `ENV_MUTEX` lock is held so that no
/// other thread reads the process environment concurrently.
#[allow(unsafe_code, reason = "set_var/remove_var are unsafe in edition 2024")]
fn set_k8s_env_vars(tmp: &std::path::Path, port: u16) {
    // SAFETY: guarded by ENV_MUTEX; no concurrent readers.
    unsafe {
        std::env::set_var("KUBERNETES_SERVICE_HOST", "127.0.0.1");
        std::env::set_var("KUBERNETES_SERVICE_PORT", port.to_string());
        std::env::set_var(
            "PRAXIS_K8S_SERVICEACCOUNT_TOKEN_PATH",
            tmp.join("token").to_str().unwrap(),
        );
        std::env::set_var(
            "PRAXIS_K8S_SERVICEACCOUNT_CA_PATH",
            tmp.join("ca.pem").to_str().unwrap(),
        );
        std::env::set_var(
            "PRAXIS_K8S_SERVICEACCOUNT_NAMESPACE_PATH",
            tmp.join("namespace").to_str().unwrap(),
        );
    }
}

/// Generate a CA + server TLS certificate pair for the fake K8s
/// API server, returning `(ca_pem, server_config)`.
fn generate_test_tls_pair(_tmp: &std::path::Path) -> (String, std::sync::Arc<rustls::ServerConfig>) {
    use rcgen::{CertificateParams, DnType, IsCa, Issuer, KeyPair, SanType};

    let ca_key = KeyPair::generate().expect("CA key generation");
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca_params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(DnType::CommonName, "Fake K8s CA");
    let ca_cert = ca_params.self_signed(&ca_key).expect("CA self-sign");
    let issuer = Issuer::from_params(&ca_params, &ca_key);

    let server_key = KeyPair::generate().expect("server key generation");
    let mut server_params = CertificateParams::new(vec!["localhost".to_owned(), "kubernetes.default.svc".to_owned()])
        .expect("server params");
    server_params.distinguished_name.push(DnType::CommonName, "localhost");
    server_params
        .subject_alt_names
        .push(SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)));
    let server_cert = server_params.signed_by(&server_key, &issuer).expect("sign");

    let ca_pem = ca_cert.pem();
    let cert_der = server_cert.der().to_vec();
    let key_der = server_key.serialize_der();

    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(cert_der)],
            rustls::pki_types::PrivateKeyDer::try_from(key_der).expect("key DER"),
        )
        .expect("server config");

    (ca_pem, std::sync::Arc::new(server_config))
}

/// RAII guard for the fake K8s API TLS server.
struct FakeK8sApi {
    /// Port the server is listening on.
    port: u16,
    /// Stop signal for the server thread.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Server thread join handle.
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for FakeK8sApi {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.join.take() {
            drop(h.join());
        }
    }
}

/// JSON response for `InferenceModelRewrite` list.
const REWRITE_LIST_JSON: &str = r#"{"items":[{
  "metadata":{"name":"rewrite-rule","creationTimestamp":"2026-01-01T00:00:00Z"},
  "spec":{
    "poolRef":{"group":"inference.networking.k8s.io","kind":"InferencePool","name":"sim-pool"},
    "rules":[{
      "matches":[{"model":{"type":"Exact","value":"original-model"}}],
      "targets":[{"modelRewrite":"rewritten-model","weight":1}]
    }]
  }
}]}"#;

/// JSON response for `InferenceObjective` list.
const OBJECTIVE_LIST_JSON: &str = r#"{"items":[
  {
    "metadata":{"name":"high-priority","creationTimestamp":"2026-01-01T00:00:00Z"},
    "spec":{
      "poolRef":{"group":"inference.networking.k8s.io","kind":"InferencePool","name":"sim-pool"},
      "priority":10
    }
  },
  {
    "metadata":{"name":"low-priority","creationTimestamp":"2026-01-01T00:00:00Z"},
    "spec":{
      "poolRef":{"group":"inference.networking.k8s.io","kind":"InferencePool","name":"sim-pool"},
      "priority":-5
    }
  }
]}"#;

/// Start a fake HTTPS K8s API server that responds to
/// `InferenceModelRewrite` list requests.
fn start_fake_k8s_api(server_config: std::sync::Arc<rustls::ServerConfig>) -> FakeK8sApi {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake K8s API");
    let port = listener.local_addr().unwrap().port();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_clone = std::sync::Arc::clone(&stop);

    let join = std::thread::spawn(move || {
        listener.set_nonblocking(true).expect("set non-blocking");
        run_fake_k8s_loop(&listener, &server_config, &stop_clone);
    });

    FakeK8sApi {
        port,
        stop,
        join: Some(join),
    }
}

/// Main accept loop for the fake K8s API server.
fn run_fake_k8s_loop(
    listener: &TcpListener,
    server_config: &std::sync::Arc<rustls::ServerConfig>,
    stop: &std::sync::atomic::AtomicBool,
) {
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                handle_fake_k8s_conn(stream, server_config);
            },
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            },
            Err(_) => break,
        }
    }
}

/// Handle one TLS connection on the fake K8s API server.
fn handle_fake_k8s_conn(stream: std::net::TcpStream, server_config: &std::sync::Arc<rustls::ServerConfig>) {
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
    let tls_conn = match rustls::ServerConnection::new(std::sync::Arc::clone(server_config)) {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut tls = rustls::StreamOwned::new(tls_conn, stream);

    let mut buf = [0u8; 4096];
    let mut data = Vec::new();
    loop {
        match tls.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                let raw = String::from_utf8_lossy(&data);
                if raw.contains("\r\n\r\n") {
                    break;
                }
            },
        }
    }

    let raw = String::from_utf8_lossy(&data);
    let response = build_fake_k8s_response(&raw);
    drop(tls.write_all(response.as_bytes()));
    drop(tls.flush());
    tls.conn.send_close_notify();
    drop(tls.flush());
}

/// Build the HTTP response for a fake K8s API request.
fn build_fake_k8s_response(raw: &str) -> String {
    let body = if raw.contains("inferencemodelrewrites") {
        Some(REWRITE_LIST_JSON)
    } else if raw.contains("inferenceobjectives") {
        Some(OBJECTIVE_LIST_JSON)
    } else {
        None
    };

    match body {
        Some(json) => format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Length: {}\r\n\
             Content-Type: application/json\r\n\
             Connection: close\r\n\r\n\
             {}",
            json.len(),
            json
        ),
        None => "HTTP/1.1 404 Not Found\r\n\
                 Content-Length: 0\r\n\
                 Connection: close\r\n\r\n"
            .to_owned(),
    }
}

/// Install a process-wide default rustls crypto provider.
///
/// Idempotent: if a provider is already installed the call is a no-op.
fn ensure_crypto_provider() {
    drop(rustls::crypto::aws_lc_rs::default_provider().install_default());
}

/// Build proxy YAML configuration with model rewrite enabled.
fn picker_yaml_with_model_rewrite(proxy_port: u16, original_port: u16, rewritten_port: u16) -> String {
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
        metrics_refresh_ms: 50
        metrics_timeout_ms: 200
        model_rewrite:
          enabled: true
          namespace: default
          pool_ref:
            name: sim-pool
        endpoints:
          - name: original-ep
            address: "127.0.0.1:{original_port}"
            models: ["original-model"]
          - name: rewritten-ep
            address: "127.0.0.1:{rewritten_port}"
            models: ["rewritten-model"]
"#
    )
}
