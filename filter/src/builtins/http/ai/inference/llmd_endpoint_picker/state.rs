// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Endpoint state snapshot, handle, and background refresh worker.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use arc_swap::ArcSwap;
use tracing::{debug, warn};

use super::{
    config::{EndpointConfig, GatewayApiConfig, InferencePoolConfig},
    disaggregation::EndpointRole,
    inference_objective::{InferenceObjectiveConfig, ObjectiveHandle},
    kubernetes::KubeClient,
    metrics::{parse_vllm_metrics, scrape_http_metrics},
    model_rewrite::{ModelRewriteConfig, ModelRewriteHandle},
};
use crate::FilterError;

// -----------------------------------------------------------------------------
// Endpoint State
// -----------------------------------------------------------------------------

/// Runtime state for one inference endpoint.
#[derive(Debug, Clone)]
pub(super) struct EndpointState {
    /// Stable endpoint name.
    pub name: Arc<str>,

    /// Upstream address in `host:port` form.
    pub address: Arc<str>,

    /// Models served by the endpoint.
    pub models: Vec<Arc<str>>,

    /// Current running request count.
    pub running_requests: u64,

    /// Current waiting request count.
    pub waiting_requests: u64,

    /// Current KV-cache pressure percentage (0-100).
    pub kv_cache_usage_percent: f64,

    /// Whether this endpoint should receive new traffic.
    pub healthy: bool,

    /// Optional metrics scrape URL for dynamic refresh.
    pub metrics_url: Option<Arc<str>>,

    /// Role in a disaggregated serving topology.
    pub role: EndpointRole,
}

impl TryFrom<EndpointConfig> for EndpointState {
    type Error = FilterError;

    fn try_from(cfg: EndpointConfig) -> Result<Self, Self::Error> {
        super::config::validate_endpoint_config(&cfg)?;

        Ok(Self {
            name: Arc::from(cfg.name),
            address: Arc::from(cfg.address),
            models: cfg.models.into_iter().map(Arc::from).collect(),
            running_requests: cfg.running_requests,
            waiting_requests: cfg.waiting_requests,
            kv_cache_usage_percent: cfg.kv_cache_usage_percent,
            healthy: cfg.healthy,
            metrics_url: cfg.metrics_url.map(Arc::from),
            role: cfg.role,
        })
    }
}

// -----------------------------------------------------------------------------
// Endpoint Snapshot
// -----------------------------------------------------------------------------

/// Immutable endpoint state snapshot read by request filters.
#[derive(Debug)]
pub(super) struct EndpointSnapshot {
    /// Endpoint states visible to request filters.
    pub endpoints: Vec<EndpointState>,
}

// -----------------------------------------------------------------------------
// Endpoint State Handle
// -----------------------------------------------------------------------------

/// Cloneable handle that provides cheap reads of the latest snapshot.
#[derive(Debug, Clone)]
pub(super) struct EndpointStateHandle {
    /// Atomically swappable snapshot pointer.
    current: Arc<ArcSwap<EndpointSnapshot>>,
}

impl EndpointStateHandle {
    /// Build a handle from an initial snapshot.
    pub fn new(snapshot: EndpointSnapshot) -> Self {
        Self {
            current: Arc::new(ArcSwap::from_pointee(snapshot)),
        }
    }

    /// Return the latest endpoint snapshot.
    pub fn snapshot(&self) -> Arc<EndpointSnapshot> {
        self.current.load_full()
    }

    /// Publish a new endpoint snapshot.
    pub fn update(&self, snapshot: EndpointSnapshot) {
        self.current.store(Arc::new(snapshot));
    }
}

// -----------------------------------------------------------------------------
// Background Worker
// -----------------------------------------------------------------------------

/// Parameters for spawning the background metrics worker.
pub(super) struct WorkerParams {
    /// Optional inference pool discovery config.
    pub pool_config: Option<InferencePoolConfig>,
    /// Optional Gateway API discovery config.
    pub gateway_config: Option<GatewayApiConfig>,
    /// Optional model rewrite config and handle.
    pub model_rewrite: Option<(ModelRewriteConfig, ModelRewriteHandle)>,
    /// Optional inference objective config and handle.
    pub inference_objective: Option<(InferenceObjectiveConfig, ObjectiveHandle)>,
    /// Worker refresh interval.
    pub refresh_interval: Duration,
    /// Per-endpoint scrape timeout.
    pub scrape_timeout: Duration,
}

/// Background thread that periodically scrapes endpoint metrics and
/// optionally discovers endpoints from Kubernetes.
pub(super) struct EndpointStateWorker {
    /// Signal to stop the worker loop.
    stop: Arc<AtomicBool>,
    /// Worker thread join handle.
    join: Option<std::thread::JoinHandle<()>>,
}

impl EndpointStateWorker {
    /// Spawn the background worker if needed.
    ///
    /// The worker starts when either:
    /// - any static endpoint has `metrics_url`, or
    /// - `inference_pool` or `gateway_api` discovery is configured, or
    /// - `model_rewrite` is enabled, or
    /// - `inference_objective` is enabled.
    ///
    /// Returns `None` if no background work is needed.
    pub fn maybe_spawn(
        handle: &EndpointStateHandle,
        static_endpoints: Vec<EndpointState>,
        params: WorkerParams,
    ) -> Option<Self> {
        let has_scrape = static_endpoints.iter().any(|ep| ep.metrics_url.is_some());
        let has_discovery = params.pool_config.is_some() || params.gateway_config.is_some();
        let has_rewrite = params.model_rewrite.is_some();
        let has_objective = params.inference_objective.is_some();
        if !has_scrape && !has_discovery && !has_rewrite && !has_objective {
            return None;
        }

        let ctx = WorkerContext {
            static_endpoints,
            pool_config: params.pool_config,
            gateway_config: params.gateway_config,
            model_rewrite: params.model_rewrite,
            inference_objective: params.inference_objective,
            refresh_interval: params.refresh_interval,
            scrape_timeout: params.scrape_timeout,
        };
        spawn_worker(handle, ctx, has_discovery || has_rewrite || has_objective)
    }
}

impl Drop for EndpointStateWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            drop(join.join());
        }
    }
}

/// Spawn the worker thread with the given context.
fn spawn_worker(handle: &EndpointStateHandle, ctx: WorkerContext, has_discovery: bool) -> Option<EndpointStateWorker> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop);
    let handle_clone = handle.clone();
    let interval_ms = ctx.refresh_interval.as_millis();

    let join = match std::thread::Builder::new()
        .name("llmd-metrics-worker".to_owned())
        .spawn(move || {
            worker_loop(&handle_clone, &stop_clone, ctx);
        }) {
        Ok(h) => h,
        Err(err) => {
            warn!(
                error = %err,
                "failed to start llm-d metrics worker; using static endpoint state"
            );
            return None;
        },
    };

    debug!(interval_ms, discovery = has_discovery, "llm-d metrics worker started");
    Some(EndpointStateWorker { stop, join: Some(join) })
}

/// Immutable context for the worker loop.
struct WorkerContext {
    /// Static endpoints from config.
    static_endpoints: Vec<EndpointState>,
    /// Optional inference pool discovery config.
    pool_config: Option<InferencePoolConfig>,
    /// Optional Gateway API discovery config.
    gateway_config: Option<GatewayApiConfig>,
    /// Optional model rewrite config and handle.
    model_rewrite: Option<(ModelRewriteConfig, ModelRewriteHandle)>,
    /// Optional inference objective config and handle.
    inference_objective: Option<(InferenceObjectiveConfig, ObjectiveHandle)>,
    /// Worker refresh interval.
    refresh_interval: Duration,
    /// Per-endpoint scrape timeout.
    scrape_timeout: Duration,
}

/// Main worker loop: discover, merge, scrape, publish, sleep.
#[allow(clippy::needless_pass_by_value, reason = "owned by the worker thread")]
fn worker_loop(handle: &EndpointStateHandle, stop: &AtomicBool, ctx: WorkerContext) {
    let needs_kube = ctx.pool_config.is_some()
        || ctx.gateway_config.is_some()
        || ctx.model_rewrite.is_some()
        || ctx.inference_objective.is_some();
    let kube_client = if needs_kube {
        KubeClient::from_in_cluster(ctx.scrape_timeout).or_else(|| {
            warn!("K8s credentials unavailable; discovery will not run");
            None
        })
    } else {
        None
    };

    let mut last_discovered: Vec<EndpointState> = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        let previous = handle.snapshot();

        if let Some(client) = &kube_client {
            run_discovery(client, &ctx, &previous, &mut last_discovered);
            run_model_rewrite_discovery(client, &ctx);
            run_objective_discovery(client, &ctx);
        }

        let mut all_endpoints = ctx.static_endpoints.clone();
        all_endpoints.extend(last_discovered.clone());
        dedup_by_address(&mut all_endpoints);

        let refreshed = refresh_all_endpoints(&all_endpoints, ctx.scrape_timeout);
        handle.update(EndpointSnapshot { endpoints: refreshed });

        interruptible_sleep(ctx.refresh_interval, stop);
    }
}

/// Run one discovery cycle using either direct pool or gateway mode.
fn run_discovery(
    client: &KubeClient,
    ctx: &WorkerContext,
    previous: &EndpointSnapshot,
    last_discovered: &mut Vec<EndpointState>,
) {
    let result = if let Some(ref gw) = ctx.gateway_config {
        client.discover_via_gateway(gw)
    } else if let Some(ref cfg) = ctx.pool_config {
        client.discover(cfg)
    } else {
        return;
    };

    match result {
        Some(new_discovered) => {
            debug!(count = new_discovered.len(), "K8s discovery found endpoints");
            *last_discovered = merge_prior_metrics(&new_discovered, &previous.endpoints);
        },
        None => {
            warn!("K8s discovery failed; preserving last known endpoints");
        },
    }
}

/// Run one model rewrite discovery cycle if configured.
fn run_model_rewrite_discovery(client: &KubeClient, ctx: &WorkerContext) {
    if let Some((ref cfg, ref handle)) = ctx.model_rewrite {
        super::model_rewrite::refresh_rewrites(client, cfg, handle);
    }
}

/// Run one inference objective discovery cycle if configured.
fn run_objective_discovery(client: &KubeClient, ctx: &WorkerContext) {
    if let Some((ref cfg, ref handle)) = ctx.inference_objective {
        super::inference_objective::refresh_objectives(client, cfg, handle);
    }
}

/// Merge prior numeric metrics and health into newly discovered
/// endpoints by matching on address.
fn merge_prior_metrics(newly_discovered: &[EndpointState], previous: &[EndpointState]) -> Vec<EndpointState> {
    let prior_by_addr: HashMap<&str, &EndpointState> = previous.iter().map(|ep| (ep.address.as_ref(), ep)).collect();

    newly_discovered
        .iter()
        .map(|ep| {
            if let Some(prior) = prior_by_addr.get(ep.address.as_ref()) {
                let mut merged = ep.clone();
                merged.running_requests = prior.running_requests;
                merged.waiting_requests = prior.waiting_requests;
                merged.kv_cache_usage_percent = prior.kv_cache_usage_percent;
                merged.healthy = prior.healthy;
                merged
            } else {
                ep.clone()
            }
        })
        .collect()
}

/// Scrape metrics for all endpoints that have a `metrics_url`.
fn refresh_all_endpoints(endpoints: &[EndpointState], timeout: Duration) -> Vec<EndpointState> {
    endpoints.iter().map(|ep| refresh_endpoint(ep, timeout)).collect()
}

/// Refresh one endpoint by scraping its metrics URL if configured.
fn refresh_endpoint(ep: &EndpointState, timeout: Duration) -> EndpointState {
    let Some(ref url) = ep.metrics_url else {
        return ep.clone();
    };

    if let Some(body) = scrape_http_metrics(url, timeout) {
        let parsed = parse_vllm_metrics(&body);
        let mut refreshed = ep.clone();
        if let Some(v) = parsed.running_requests {
            refreshed.running_requests = v;
        }
        if let Some(v) = parsed.waiting_requests {
            refreshed.waiting_requests = v;
        }
        if let Some(v) = parsed.kv_cache_usage_percent {
            refreshed.kv_cache_usage_percent = v;
        }
        refreshed.healthy = true;
        refreshed
    } else {
        warn!(
            endpoint = %ep.name,
            url = %url,
            "metrics scrape failed, marking endpoint unhealthy"
        );
        let mut failed = ep.clone();
        failed.healthy = false;
        failed
    }
}

/// Remove duplicate endpoints by address, keeping the first occurrence.
///
/// Because static endpoints are prepended before discovered ones, this
/// ensures static config wins when both lists contain the same address.
fn dedup_by_address(endpoints: &mut Vec<EndpointState>) {
    let mut seen = std::collections::HashSet::new();
    endpoints.retain(|ep| seen.insert(ep.address.to_string()));
}

/// Sleep in small increments so the worker can stop promptly.
fn interruptible_sleep(duration: Duration, stop: &AtomicBool) {
    let chunk = Duration::from_millis(50);
    let mut remaining = duration;
    while remaining > Duration::ZERO && !stop.load(Ordering::Relaxed) {
        let sleep_time = remaining.min(chunk);
        std::thread::sleep(sleep_time);
        remaining = remaining.saturating_sub(sleep_time);
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests"
)]
mod tests {
    use super::*;
    use crate::builtins::http::ai::inference::llmd_endpoint_picker::disaggregation::default_endpoint_role;

    #[test]
    fn handle_publishes_updated_state() {
        let handle = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_endpoint("a", 0, 0, 0.0), make_endpoint("b", 10, 5, 80.0)],
        });

        let snap1 = handle.snapshot();
        assert_eq!(snap1.endpoints[0].running_requests, 0, "initial: endpoint a is idle");

        handle.update(EndpointSnapshot {
            endpoints: vec![make_endpoint("a", 20, 10, 95.0), make_endpoint("b", 0, 0, 5.0)],
        });

        let snap2 = handle.snapshot();
        assert_eq!(
            snap2.endpoints[0].running_requests, 20,
            "updated: endpoint a is now heavily loaded"
        );
        assert_eq!(
            snap2.endpoints[1].running_requests, 0,
            "updated: endpoint b is now idle"
        );
    }

    #[test]
    fn refresh_preserves_values_when_no_metrics_url() {
        let ep = EndpointState {
            name: Arc::from("static"),
            address: Arc::from("127.0.0.1:8000"),
            models: vec![Arc::from("model")],
            running_requests: 5,
            waiting_requests: 3,
            kv_cache_usage_percent: 42.0,
            healthy: true,
            metrics_url: None,
            role: default_endpoint_role(),
        };

        let refreshed = refresh_endpoint(&ep, Duration::from_millis(100));

        assert_eq!(
            refreshed.running_requests, 5,
            "static endpoint should preserve running_requests"
        );
        assert_eq!(
            refreshed.kv_cache_usage_percent, 42.0,
            "static endpoint should preserve kv_cache"
        );
        assert!(refreshed.healthy, "static endpoint should stay healthy");
    }

    #[test]
    fn refresh_marks_unhealthy_on_scrape_failure() {
        let ep = EndpointState {
            name: Arc::from("bad"),
            address: Arc::from("127.0.0.1:1"),
            models: vec![Arc::from("model")],
            running_requests: 5,
            waiting_requests: 3,
            kv_cache_usage_percent: 42.0,
            healthy: true,
            metrics_url: Some(Arc::from("http://127.0.0.1:1/metrics")),
            role: default_endpoint_role(),
        };

        let refreshed = refresh_endpoint(&ep, Duration::from_millis(100));

        assert!(!refreshed.healthy, "failed scrape should mark unhealthy");
        assert_eq!(
            refreshed.running_requests, 5,
            "failed scrape should preserve previous values"
        );
    }

    #[test]
    fn worker_not_spawned_without_metrics_urls_or_discovery() {
        let handle = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_endpoint("a", 0, 0, 0.0)],
        });
        let static_eps = vec![make_endpoint("a", 0, 0, 0.0)];

        let params = WorkerParams {
            pool_config: None,
            gateway_config: None,
            model_rewrite: None,
            inference_objective: None,
            refresh_interval: Duration::from_millis(100),
            scrape_timeout: Duration::from_millis(50),
        };
        let worker = EndpointStateWorker::maybe_spawn(&handle, static_eps, params);

        assert!(
            worker.is_none(),
            "worker should not spawn when no metrics_url or discovery is configured"
        );
    }

    #[test]
    fn merge_prior_metrics_preserves_existing_values() {
        let newly_discovered = vec![EndpointState {
            name: Arc::from("pod-a:8000"),
            address: Arc::from("10.0.0.1:8000"),
            models: vec![Arc::from("model")],
            running_requests: 0,
            waiting_requests: 0,
            kv_cache_usage_percent: 0.0,
            healthy: true,
            metrics_url: Some(Arc::from("http://10.0.0.1:8000/metrics")),
            role: default_endpoint_role(),
        }];
        let previous = vec![EndpointState {
            name: Arc::from("pod-a:8000"),
            address: Arc::from("10.0.0.1:8000"),
            models: vec![Arc::from("model")],
            running_requests: 7,
            waiting_requests: 3,
            kv_cache_usage_percent: 45.0,
            healthy: false,
            metrics_url: Some(Arc::from("http://10.0.0.1:8000/metrics")),
            role: default_endpoint_role(),
        }];

        let merged = merge_prior_metrics(&newly_discovered, &previous);

        assert_eq!(merged.len(), 1, "one merged endpoint");
        assert_eq!(merged[0].running_requests, 7, "should preserve prior running_requests");
        assert_eq!(merged[0].kv_cache_usage_percent, 45.0, "should preserve prior kv_cache");
        assert!(!merged[0].healthy, "should preserve prior healthy status");
    }

    #[test]
    fn merge_prior_metrics_uses_defaults_for_new_pods() {
        let newly_discovered = vec![EndpointState {
            name: Arc::from("new-pod:8000"),
            address: Arc::from("10.0.0.99:8000"),
            models: vec![Arc::from("model")],
            running_requests: 0,
            waiting_requests: 0,
            kv_cache_usage_percent: 0.0,
            healthy: true,
            metrics_url: Some(Arc::from("http://10.0.0.99:8000/metrics")),
            role: default_endpoint_role(),
        }];
        let previous: Vec<EndpointState> = vec![];

        let merged = merge_prior_metrics(&newly_discovered, &previous);

        assert_eq!(merged.len(), 1, "one new endpoint");
        assert_eq!(
            merged[0].running_requests, 0,
            "new pod should have zero running_requests"
        );
        assert!(merged[0].healthy, "new pod should be healthy by default");
    }

    #[test]
    fn dedup_by_address_keeps_first_occurrence() {
        let static_ep = make_named_endpoint("static-ep", "10.0.0.1:8000");
        let discovered_ep = make_named_endpoint("discovered-ep", "10.0.0.1:8000");
        let unique_ep = make_named_endpoint("unique-ep", "10.0.0.2:8000");

        let mut endpoints = vec![static_ep, discovered_ep, unique_ep];
        dedup_by_address(&mut endpoints);

        assert_eq!(endpoints.len(), 2, "duplicate address should be removed");
        assert_eq!(
            endpoints[0].name.as_ref(),
            "static-ep",
            "static endpoint should win over discovered"
        );
        assert_eq!(endpoints[1].name.as_ref(), "unique-ep", "unique endpoint preserved");
    }

    // -- Test Utilities --

    fn make_named_endpoint(name: &str, address: &str) -> EndpointState {
        EndpointState {
            name: Arc::from(name),
            address: Arc::from(address),
            models: vec![Arc::from("model")],
            running_requests: 0,
            waiting_requests: 0,
            kv_cache_usage_percent: 0.0,
            healthy: true,
            metrics_url: None,
            role: default_endpoint_role(),
        }
    }

    fn make_endpoint(name: &str, running: u64, waiting: u64, kv: f64) -> EndpointState {
        EndpointState {
            name: Arc::from(name),
            address: Arc::from(format!("127.0.0.1:{}", 8000 + running)),
            models: vec![Arc::from("fake-model")],
            running_requests: running,
            waiting_requests: waiting,
            kv_cache_usage_percent: kv,
            healthy: true,
            metrics_url: None,
            role: default_endpoint_role(),
        }
    }
}
