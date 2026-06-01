// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! llm-d native endpoint picker filter.

mod config;
mod disaggregation;
mod inference_objective;
mod kubernetes;
mod metrics;
mod model_rewrite;
mod prefix_cache;
mod saturation;
mod state;

use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use config::LlmdEndpointPickerConfig;
use disaggregation::{DisaggregationConfig, PrefillMode};
use inference_objective::{InferenceObjectiveConfig, ObjectiveHandle};
use model_rewrite::{ModelRewriteConfig, ModelRewriteHandle};
use praxis_core::connectivity::{ConnectionOptions, Upstream};
use prefix_cache::{PrefixCacheConfig, PrefixIndex};
use saturation::SaturationGateConfig;
use state::{EndpointSnapshot, EndpointState, EndpointStateHandle, EndpointStateWorker, WorkerParams};
use tracing::debug;

use crate::{
    FilterAction, FilterError, Rejection,
    body::{BodyAccess, BodyMode},
    factory::parse_filter_config,
    filter::{HttpFilter, HttpFilterContext},
};

// -----------------------------------------------------------------------------
// LlmdEndpointPickerFilter
// -----------------------------------------------------------------------------

/// Native llm-d endpoint picker for OpenAI-compatible inference requests.
///
/// Extracts the request `model`, filters endpoints by model and health, scores
/// with queue depth and KV-cache utilization, and writes the selected upstream
/// into [`HttpFilterContext`].
///
/// Optionally scrapes vLLM Prometheus endpoints in the background to keep
/// endpoint state fresh.
pub struct LlmdEndpointPickerFilter {
    /// Request body size limit for `StreamBuffer`.
    max_body_bytes: usize,

    /// Logical pool name used for metadata and cluster accounting.
    pool_name: Arc<str>,

    /// Weight applied to inverse queue-depth scoring.
    queue_weight: f64,

    /// Weight applied to inverse KV-cache pressure scoring.
    kv_cache_weight: f64,

    /// Endpoint state provider.
    state: EndpointStateHandle,

    /// Plain HTTP connection options for selected upstreams.
    connection: Arc<ConnectionOptions>,

    /// Background metrics worker (kept alive while the filter exists).
    _worker: Option<EndpointStateWorker>,

    /// Optional prefix-cache index shared across requests.
    prefix_index: Option<Arc<Mutex<PrefixIndex>>>,

    /// Optional prefix-cache configuration.
    prefix_config: Option<PrefixCacheConfig>,

    /// Optional saturation/admission gate configuration.
    saturation_config: Option<SaturationGateConfig>,

    /// Optional prefill/decode disaggregation configuration.
    disagg_config: Option<DisaggregationConfig>,

    /// Optional model rewrite handle for `InferenceModelRewrite`.
    model_rewrite_handle: Option<ModelRewriteHandle>,

    /// Whether model rewrite is enabled.
    model_rewrite_enabled: bool,

    /// Optional inference objective handle for priority lookup.
    objective_handle: Option<ObjectiveHandle>,

    /// Whether inference objective is enabled.
    objective_enabled: bool,
}

impl LlmdEndpointPickerFilter {
    /// Create from parsed YAML config.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] when endpoint config is invalid.
    pub fn from_config(yaml: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: LlmdEndpointPickerConfig = parse_filter_config("llmd_endpoint_picker", yaml)?;
        config::validate_config(&cfg)?;
        build_filter(cfg)
    }

    /// Select the highest-scored endpoint from a pre-filtered candidate
    /// list, applying queue/KV and optional prefix-cache scoring.
    fn select_from_candidates<'a>(
        &self,
        candidates: &[&'a EndpointState],
        prefix_scores: &HashMap<Arc<str>, f64>,
    ) -> Option<(&'a EndpointState, f64)> {
        let prefix_weight = self.prefix_weight();
        candidates
            .iter()
            .map(|ep| {
                let ps = prefix_scores.get(&ep.name).copied().unwrap_or(0.0);
                (*ep, self.score(ep) + prefix_weight * ps)
            })
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
    }

    /// Score one endpoint using queue depth and KV-cache pressure.
    fn score(&self, endpoint: &EndpointState) -> f64 {
        let queue_depth = endpoint.running_requests.saturating_add(endpoint.waiting_requests);
        let bounded_queue_depth = u32::try_from(queue_depth).unwrap_or(u32::MAX);
        let queue_score = 1.0 / (1.0 + f64::from(bounded_queue_depth));
        let kv_pressure = endpoint.kv_cache_usage_percent.clamp(0.0, 100.0);
        let kv_score = 1.0 - (kv_pressure / 100.0);
        (self.queue_weight * queue_score) + (self.kv_cache_weight * kv_score)
    }

    /// Return the prefix weight, or zero when prefix scoring is off.
    fn prefix_weight(&self) -> f64 {
        self.prefix_config.as_ref().map_or(0.0, |c| c.weight)
    }

    /// Select an endpoint from a buffered request body and apply it to context.
    fn pick_from_body(&self, ctx: &mut HttpFilterContext<'_>, body: &mut Option<Bytes>) -> FilterAction {
        let raw = match body.as_ref() {
            Some(b) => b.as_ref(),
            None => return reject(400, "llmd_endpoint_picker: missing request body"),
        };
        let info = match prefix_cache::extract_request_info(raw) {
            Ok(info) => info,
            Err(rejection) => return rejection,
        };

        let (info, _rewrote) = self.maybe_rewrite_model(ctx, body, info);
        let priority = self.resolve_and_set_objective_metadata(ctx);

        let prefix_data = self.compute_prefix_data(&info);
        let rctx = RoutingContext {
            prefix_data: &prefix_data,
            priority,
        };
        let snapshot = self.state.snapshot();

        if self.disagg_config.is_some() {
            let (action, prefill_addr) = self.pick_disaggregated(ctx, &snapshot, &info, &rctx);
            if let Some(ref addr) = prefill_addr {
                inject_kv_transfer_body(self, ctx, body, addr);
            }
            return action;
        }

        self.pick_standard(ctx, &snapshot, &info, &rctx)
    }

    /// Whether `kv_transfer` body mutation is active.
    fn kv_transfer_enabled(&self) -> bool {
        self.disagg_config
            .as_ref()
            .is_some_and(|dc| dc.inject_kv_transfer_params)
    }

    /// Resolve and set inference objective metadata from headers.
    ///
    /// Returns the resolved priority value for use by the saturation
    /// gate. Returns 0 when no objective is configured or found.
    fn resolve_and_set_objective_metadata(&self, ctx: &mut HttpFilterContext<'_>) -> i32 {
        let Some(ref handle) = self.objective_handle else {
            return 0;
        };
        if !self.objective_enabled {
            ctx.set_metadata("llmd.inference_objective", "none".to_owned());
            ctx.set_metadata("llmd.inference_objective_priority", "0".to_owned());
            return 0;
        }

        let header_value = inference_objective::extract_objective_header(&ctx.request.headers);
        let Some(objective_name) = header_value else {
            ctx.set_metadata("llmd.inference_objective", "none".to_owned());
            ctx.set_metadata("llmd.inference_objective_priority", "0".to_owned());
            return 0;
        };

        let snap = handle.snapshot();
        if let Some(result) = snap.lookup(objective_name) {
            ctx.set_metadata("llmd.inference_objective", objective_name.to_owned());
            ctx.set_metadata("llmd.inference_objective_priority", result.priority.to_string());
            ctx.set_metadata("llmd.inference_objective_source", result.source_name.to_owned());
            result.priority
        } else {
            ctx.set_metadata("llmd.inference_objective", "unknown".to_owned());
            ctx.set_metadata("llmd.inference_objective_priority", "0".to_owned());
            0
        }
    }

    /// Apply model rewrite if configured and a rule matches.
    ///
    /// Returns the (possibly updated) request info and whether a
    /// rewrite was applied.
    fn maybe_rewrite_model(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        info: prefix_cache::RequestInfo,
    ) -> (prefix_cache::RequestInfo, bool) {
        let Some(ref handle) = self.model_rewrite_handle else {
            return (info, false);
        };
        if !self.model_rewrite_enabled {
            ctx.set_metadata("llmd.model_rewrite", "none".to_owned());
            return (info, false);
        }

        let snap = handle.snapshot();
        let Some(result) = snap.lookup(&info.model) else {
            ctx.set_metadata("llmd.model_rewrite", "none".to_owned());
            return (info, false);
        };

        if result.target == info.model {
            ctx.set_metadata("llmd.model_rewrite", "none".to_owned());
            return (info, false);
        }

        let source = result.source_name.map(str::to_owned);
        apply_rewrite_impl(ctx, body, info, result.target, source.as_deref())
    }

    /// Standard (non-disaggregated) endpoint selection.
    fn pick_standard(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        snapshot: &EndpointSnapshot,
        info: &prefix_cache::RequestInfo,
        rctx: &RoutingContext<'_>,
    ) -> FilterAction {
        let candidates = build_candidates(snapshot, &info.model);
        if let Some(action) = self.apply_saturation_gate(ctx, &candidates, rctx.priority) {
            return action;
        }

        let scored_candidates = self.filter_by_saturation(candidates);
        let Some((endpoint, score)) = self.select_from_candidates(&scored_candidates, &rctx.prefix_data.scores) else {
            debug!(model = %info.model, "llm-d endpoint picker found no eligible endpoint");
            return reject(503, "llmd_endpoint_picker: no eligible endpoint");
        };

        self.record_prefix_selection(endpoint, &rctx.prefix_data.block_hashes);
        self.apply_selection(ctx, &info.model, endpoint, score);
        FilterAction::Release
    }

    /// Disaggregated endpoint selection: pick a decode target, then optionally a prefill endpoint.
    ///
    /// Returns the filter action and the selected prefill address (if any)
    /// so the caller can perform body mutation outside this method.
    fn pick_disaggregated(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        snapshot: &EndpointSnapshot,
        info: &prefix_cache::RequestInfo,
        rctx: &RoutingContext<'_>,
    ) -> (FilterAction, Option<Arc<str>>) {
        ctx.set_metadata("llmd.disaggregation", "enabled".to_owned());
        let decode_candidates = build_role_candidates(snapshot, &info.model, disaggregation::is_decode_candidate);

        if let Some(action) = self.apply_saturation_gate(ctx, &decode_candidates, rctx.priority) {
            return (action, None);
        }

        let scored = self.filter_by_saturation(decode_candidates);
        let Some((decode_ep, score)) = self.select_from_candidates(&scored, &rctx.prefix_data.scores) else {
            debug!(model = %info.model, "no eligible decode endpoint");
            return (reject(503, "llmd_endpoint_picker: no eligible decode endpoint"), None);
        };

        self.record_prefix_selection(decode_ep, &rctx.prefix_data.block_hashes);
        ctx.set_metadata("llmd.decode_endpoint", decode_ep.name.to_string());
        self.apply_selection(ctx, &info.model, decode_ep, score);
        let prefill_addr = self.maybe_inject_prefill(ctx, snapshot, &info.model, decode_ep);
        (FilterAction::Release, prefill_addr)
    }

    /// If disaggregation is enabled with Always mode, select and inject a prefill header.
    ///
    /// Returns the selected prefill endpoint address when one is found.
    fn maybe_inject_prefill(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        snapshot: &EndpointSnapshot,
        model: &str,
        decode_ep: &EndpointState,
    ) -> Option<Arc<str>> {
        let dc = self.disagg_config.as_ref()?;
        if dc.prefill_mode != PrefillMode::Always {
            return None;
        }
        let prefill = build_role_candidates(snapshot, model, disaggregation::is_prefill_candidate);
        let preferred: Vec<&EndpointState> = prefill
            .iter()
            .filter(|ep| ep.address != decode_ep.address)
            .copied()
            .collect();
        let candidates = if preferred.is_empty() { &prefill } else { &preferred };
        let filtered = self.filter_prefill_by_saturation(candidates);
        let empty = HashMap::new();
        let Some((prefill_ep, _)) = self.select_from_candidates(&filtered, &empty) else {
            ctx.set_metadata("llmd.prefill_endpoint", "none".to_owned());
            return None;
        };

        ctx.set_metadata("llmd.prefill_endpoint", prefill_ep.name.to_string());
        ctx.extra_request_headers
            .push((Cow::Owned(dc.prefill_header.clone()), prefill_ep.address.to_string()));
        Some(Arc::clone(&prefill_ep.address))
    }

    /// Filter overloaded prefill endpoints when saturation gate is
    /// enabled. If all are filtered, returns empty to fail open to
    /// decode-only (no prefill).
    fn filter_prefill_by_saturation<'a>(&self, candidates: &[&'a EndpointState]) -> Vec<&'a EndpointState> {
        let Some(cfg) = self.active_saturation_config() else {
            return candidates.to_vec();
        };
        let max_queue = saturation::compute_max_queue_for_config(cfg);
        let max_kv = saturation::compute_max_kv_for_config(cfg);
        candidates
            .iter()
            .filter(|ep| ep.waiting_requests <= max_queue && ep.kv_cache_usage_percent <= max_kv)
            .copied()
            .collect()
    }

    /// Check pool-level saturation and reject if above the
    /// priority-adjusted threshold.
    ///
    /// Returns `Some(FilterAction)` when the gate rejects, `None` to
    /// continue normal routing.
    fn apply_saturation_gate(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        candidates: &[&EndpointState],
        priority: i32,
    ) -> Option<FilterAction> {
        let cfg = self.active_saturation_config()?;
        let sat = saturation::pool_saturation(candidates, cfg);
        let threshold = saturation::compute_effective_threshold(cfg, priority);
        ctx.set_metadata("llmd.pool_saturation", format!("{sat:.4}"));
        ctx.set_metadata("llmd.saturation_gate", "enabled".to_owned());

        if sat >= threshold {
            debug!(pool_saturation = sat, threshold, "saturation gate rejecting request");
            return Some(reject_dynamic(
                cfg.reject_status,
                "llmd_endpoint_picker: pool saturated",
            ));
        }
        None
    }

    /// Filter individual overloaded endpoints when saturation gate is
    /// enabled. Falls through to unfiltered candidates when disabled.
    fn filter_by_saturation<'a>(&self, candidates: Vec<&'a EndpointState>) -> Vec<&'a EndpointState> {
        let Some(cfg) = self.active_saturation_config() else {
            return candidates;
        };
        saturation::filter_saturated_endpoints(candidates, cfg)
    }

    /// Return the active saturation config if enabled.
    fn active_saturation_config(&self) -> Option<&SaturationGateConfig> {
        self.saturation_config.as_ref().filter(|c| c.enabled)
    }

    /// Compute prefix block hashes and per-endpoint prefix scores.
    fn compute_prefix_data(&self, info: &prefix_cache::RequestInfo) -> PrefixData {
        let Some(ref cfg) = self.prefix_config else {
            return PrefixData::default();
        };
        let Some(ref material) = info.prefix_material else {
            return PrefixData::default();
        };
        let Some(ref index_lock) = self.prefix_index else {
            return PrefixData::default();
        };

        let max_blocks = cfg.effective_max_blocks();
        let hashes = prefix_cache::compute_block_hashes(&info.model, material, cfg.block_size_tokens, max_blocks);
        if hashes.is_empty() {
            return PrefixData {
                block_hashes: hashes,
                scores: HashMap::new(),
            };
        }

        let scores = compute_prefix_scores_locked(index_lock, &hashes, &self.state);
        PrefixData {
            block_hashes: hashes,
            scores,
        }
    }

    /// Record prefix hashes for the selected endpoint.
    fn record_prefix_selection(&self, endpoint: &EndpointState, block_hashes: &[u64]) {
        if block_hashes.is_empty() {
            return;
        }
        let Some(ref index_lock) = self.prefix_index else {
            return;
        };
        if let Ok(mut index) = index_lock.lock() {
            index.record(&endpoint.name, block_hashes);
        }
    }

    /// Store the selected endpoint in the request context.
    fn apply_selection(&self, ctx: &mut HttpFilterContext<'_>, model: &str, endpoint: &EndpointState, score: f64) {
        debug!(
            model = %model,
            pool = %self.pool_name,
            endpoint = %endpoint.name,
            upstream = %endpoint.address,
            score,
            "llm-d endpoint selected"
        );

        ctx.cluster = Some(Arc::clone(&self.pool_name));
        ctx.set_metadata("llmd.model", model.to_owned());
        ctx.set_metadata("llmd.endpoint", endpoint.name.to_string());
        ctx.upstream = Some(Upstream {
            address: Arc::clone(&endpoint.address),
            connection: Arc::clone(&self.connection),
            tls: None,
        });
    }
}

#[async_trait]
impl HttpFilter for LlmdEndpointPickerFilter {
    fn name(&self) -> &'static str {
        "llmd_endpoint_picker"
    }

    fn request_body_access(&self) -> BodyAccess {
        if self.model_rewrite_enabled || self.kv_transfer_enabled() {
            BodyAccess::ReadWrite
        } else {
            BodyAccess::ReadOnly
        }
    }

    fn request_body_mode(&self) -> BodyMode {
        BodyMode::StreamBuffer {
            max_bytes: Some(self.max_body_bytes),
        }
    }

    async fn on_request(&self, _ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        Ok(FilterAction::Continue)
    }

    async fn on_request_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        if !end_of_stream {
            return Ok(FilterAction::Continue);
        }

        Ok(self.pick_from_body(ctx, body))
    }
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// Inject `kv_transfer_params` into the request body when enabled.
fn inject_kv_transfer_body(
    filter: &LlmdEndpointPickerFilter,
    ctx: &mut HttpFilterContext<'_>,
    body: &mut Option<Bytes>,
    prefill_address: &str,
) {
    let Some(ref dc) = filter.disagg_config else {
        return;
    };
    if !dc.inject_kv_transfer_params {
        return;
    }
    let raw = match body.as_ref() {
        Some(b) => b.as_ref(),
        None => return,
    };
    let Some(mutated) = disaggregation::inject_kv_transfer_params(raw, prefill_address) else {
        return;
    };
    let len = mutated.len();
    *body = Some(Bytes::from(mutated));
    ctx.extra_request_headers
        .push((Cow::Borrowed("content-length"), len.to_string()));
}

/// Perform the actual body mutation and request info re-extraction.
fn apply_rewrite_impl(
    ctx: &mut HttpFilterContext<'_>,
    body: &mut Option<Bytes>,
    info: prefix_cache::RequestInfo,
    target: &str,
    source_name: Option<&str>,
) -> (prefix_cache::RequestInfo, bool) {
    let raw = match body.as_ref() {
        Some(b) => b.as_ref(),
        None => return (info, false),
    };

    let Some(mutated) = model_rewrite::mutate_model_in_body(raw, target) else {
        ctx.set_metadata("llmd.model_rewrite", "none".to_owned());
        return (info, false);
    };

    ctx.set_metadata("llmd.original_model", info.model.clone());
    ctx.set_metadata("llmd.model_rewrite", target.to_owned());
    if let Some(name) = source_name {
        ctx.set_metadata("llmd.model_rewrite_source", name.to_owned());
    }
    let len = mutated.len();
    let new_bytes = Bytes::from(mutated);
    let Ok(new_info) = prefix_cache::extract_request_info(&new_bytes) else {
        return (info, false);
    };
    *body = Some(new_bytes);
    ctx.extra_request_headers
        .push((Cow::Borrowed("content-length"), len.to_string()));
    (new_info, true)
}

/// Bundle of prefix-cache block hashes and per-endpoint scores.
#[derive(Default)]
struct PrefixData {
    /// Computed block hashes for the current request.
    block_hashes: Vec<u64>,
    /// Per-endpoint prefix match scores.
    scores: HashMap<Arc<str>, f64>,
}

/// Per-request routing context passed through the pick pipeline.
struct RoutingContext<'a> {
    /// Prefix-cache data for the current request.
    prefix_data: &'a PrefixData,
    /// Resolved objective priority for saturation gating.
    priority: i32,
}

/// Build a filter instance from validated config.
fn build_filter(cfg: LlmdEndpointPickerConfig) -> Result<Box<dyn HttpFilter>, FilterError> {
    let (prefix_index, prefix_config) = build_prefix_state(cfg.prefix_cache);
    let saturation_config = build_saturation_config(cfg.saturation_gate);
    let disagg_config = build_disagg_config(cfg.disaggregation);
    let (mr, obj) = build_feature_state(cfg.model_rewrite, cfg.inference_objective);
    let params = WorkerParams {
        pool_config: cfg.inference_pool,
        gateway_config: cfg.gateway_api,
        model_rewrite: mr.worker_param,
        inference_objective: obj.worker_param,
        refresh_interval: Duration::from_millis(cfg.metrics_refresh_ms),
        scrape_timeout: Duration::from_millis(cfg.metrics_timeout_ms),
    };
    let (state, worker) = build_endpoint_state(cfg.endpoints, params)?;
    Ok(Box::new(LlmdEndpointPickerFilter {
        max_body_bytes: cfg.max_body_bytes,
        pool_name: Arc::from(cfg.pool_name),
        queue_weight: cfg.queue_weight,
        kv_cache_weight: cfg.kv_cache_weight,
        state,
        connection: Arc::new(ConnectionOptions::default()),
        _worker: worker,
        prefix_index,
        prefix_config,
        saturation_config,
        disagg_config,
        model_rewrite_handle: mr.handle,
        model_rewrite_enabled: mr.enabled,
        objective_handle: obj.handle,
        objective_enabled: obj.enabled,
    }))
}

/// Build endpoint state handle and optional background worker.
fn build_endpoint_state(
    endpoints: Vec<config::EndpointConfig>,
    params: WorkerParams,
) -> Result<(EndpointStateHandle, Option<EndpointStateWorker>), FilterError> {
    let static_endpoints: Vec<EndpointState> = endpoints
        .into_iter()
        .map(EndpointState::try_from)
        .collect::<Result<_, _>>()?;
    let state = EndpointStateHandle::new(EndpointSnapshot {
        endpoints: static_endpoints.clone(),
    });
    let worker = EndpointStateWorker::maybe_spawn(&state, static_endpoints, params);
    Ok((state, worker))
}

/// Build saturation gate config, retaining only when enabled.
fn build_saturation_config(saturation_gate: Option<SaturationGateConfig>) -> Option<SaturationGateConfig> {
    saturation_gate.filter(|sg| sg.enabled)
}

/// Build disaggregation config, retaining only when enabled.
fn build_disagg_config(disaggregation: Option<DisaggregationConfig>) -> Option<DisaggregationConfig> {
    disaggregation.filter(|dc| dc.enabled)
}

/// Resolved model rewrite state for filter construction.
struct ModelRewriteState {
    /// Handle for reading the latest rewrite snapshot.
    handle: Option<ModelRewriteHandle>,
    /// Whether model rewrite is enabled.
    enabled: bool,
    /// Config and handle pair for the background worker.
    worker_param: Option<(ModelRewriteConfig, ModelRewriteHandle)>,
}

/// Build model rewrite state from config, if enabled.
fn build_model_rewrite_state(model_rewrite: Option<ModelRewriteConfig>) -> ModelRewriteState {
    match model_rewrite {
        Some(mr) if mr.enabled => {
            let handle = ModelRewriteHandle::new(model_rewrite::empty_snapshot());
            let param = Some((mr, handle.clone()));
            ModelRewriteState {
                handle: Some(handle),
                enabled: true,
                worker_param: param,
            }
        },
        _ => ModelRewriteState {
            handle: None,
            enabled: false,
            worker_param: None,
        },
    }
}

/// Resolved inference objective state for filter construction.
struct ObjectiveState {
    /// Handle for reading the latest objective snapshot.
    handle: Option<ObjectiveHandle>,
    /// Whether inference objective is enabled.
    enabled: bool,
    /// Config and handle pair for the background worker.
    worker_param: Option<(InferenceObjectiveConfig, ObjectiveHandle)>,
}

/// Build inference objective state from config, if enabled.
fn build_objective_state(objective: Option<InferenceObjectiveConfig>) -> ObjectiveState {
    match objective {
        Some(cfg) if cfg.enabled => {
            let handle = ObjectiveHandle::new(inference_objective::empty_snapshot());
            let param = Some((cfg, handle.clone()));
            ObjectiveState {
                handle: Some(handle),
                enabled: true,
                worker_param: param,
            }
        },
        _ => ObjectiveState {
            handle: None,
            enabled: false,
            worker_param: None,
        },
    }
}

/// Build model rewrite and objective feature state together.
fn build_feature_state(
    model_rewrite: Option<ModelRewriteConfig>,
    objective: Option<InferenceObjectiveConfig>,
) -> (ModelRewriteState, ObjectiveState) {
    (
        build_model_rewrite_state(model_rewrite),
        build_objective_state(objective),
    )
}

/// Build prefix-cache state from config, if enabled.
fn build_prefix_state(
    prefix_cache: Option<PrefixCacheConfig>,
) -> (Option<Arc<Mutex<PrefixIndex>>>, Option<PrefixCacheConfig>) {
    match prefix_cache {
        Some(pc) if pc.enabled => {
            let index = PrefixIndex::new(pc.lru_capacity_per_endpoint);
            (Some(Arc::new(Mutex::new(index))), Some(pc))
        },
        _ => (None, None),
    }
}

/// Compute prefix scores under the index lock and clean up stale
/// entries for endpoints that are no longer in the active snapshot.
fn compute_prefix_scores_locked(
    index_lock: &Arc<Mutex<PrefixIndex>>,
    hashes: &[u64],
    state: &EndpointStateHandle,
) -> HashMap<Arc<str>, f64> {
    let Ok(mut index) = index_lock.lock() else {
        return HashMap::new();
    };
    let snapshot = state.snapshot();
    let names: Vec<Arc<str>> = snapshot.endpoints.iter().map(|ep| Arc::clone(&ep.name)).collect();
    let active: std::collections::HashSet<Arc<str>> = names.iter().cloned().collect();
    index.cleanup_stale(&active);
    prefix_cache::compute_prefix_scores(&index, hashes, hashes.len(), &names)
}

/// Build the list of healthy, model-compatible candidate endpoints.
fn build_candidates<'a>(snapshot: &'a EndpointSnapshot, model: &str) -> Vec<&'a EndpointState> {
    snapshot
        .endpoints
        .iter()
        .filter(|ep| ep.healthy && ep.models.iter().any(|m| m.as_ref() == model))
        .collect()
}

/// Build the list of healthy, model-compatible, role-matching candidate endpoints.
fn build_role_candidates<'a>(
    snapshot: &'a EndpointSnapshot,
    model: &str,
    role_filter: fn(disaggregation::EndpointRole) -> bool,
) -> Vec<&'a EndpointState> {
    snapshot
        .endpoints
        .iter()
        .filter(|ep| ep.healthy && ep.models.iter().any(|m| m.as_ref() == model) && role_filter(ep.role))
        .collect()
}

/// Build a plain-text rejection response.
fn reject(status: u16, message: &'static str) -> FilterAction {
    FilterAction::Reject(
        Rejection::status(status)
            .with_header("content-type", "text/plain; charset=utf-8")
            .with_body(Bytes::from_static(message.as_bytes())),
    )
}

/// Build a plain-text rejection response with a dynamic message.
fn reject_dynamic(status: u16, message: &str) -> FilterAction {
    FilterAction::Reject(
        Rejection::status(status)
            .with_header("content-type", "text/plain; charset=utf-8")
            .with_body(Bytes::from(message.to_owned())),
    )
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

    #[test]
    fn from_config_rejects_empty_endpoints() {
        let yaml: serde_yaml::Value = serde_yaml::from_str("endpoints: []").unwrap();
        let Err(err) = LlmdEndpointPickerFilter::from_config(&yaml) else {
            panic!("empty endpoints should fail");
        };

        assert!(
            err.to_string()
                .contains("either endpoints, inference_pool, or gateway_api must be configured"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn body_access_uses_stream_buffer() {
        let filter = make_filter();

        assert_eq!(filter.request_body_access(), BodyAccess::ReadOnly);
        assert_eq!(
            filter.request_body_mode(),
            BodyMode::StreamBuffer {
                max_bytes: Some(crate::body::DEFAULT_JSON_BODY_MAX_BYTES)
            }
        );
    }

    #[tokio::test]
    async fn selects_lowest_pressure_matching_endpoint() {
        let filter = make_filter();
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"fake-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release));
        assert_eq!(ctx.upstream_addr(), Some("127.0.0.1:18082"));
        assert_eq!(ctx.get_metadata("llmd.model"), Some("fake-model"));
        assert_eq!(ctx.get_metadata("llmd.endpoint"), Some("less-loaded"));
    }

    #[tokio::test]
    async fn filters_by_requested_model() {
        let filter = make_filter();
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"other-model","prompt":"hi"}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release));
        assert_eq!(ctx.upstream_addr(), Some("127.0.0.1:18083"));
        assert_eq!(ctx.get_metadata("llmd.endpoint"), Some("other-model"));
    }

    #[tokio::test]
    async fn rejects_when_model_is_missing() {
        let filter = make_filter();
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"prompt":"hi"}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Reject(r) if r.status == 400));
        assert!(ctx.upstream.is_none(), "missing model must not select an upstream");
    }

    #[tokio::test]
    async fn rejects_when_no_endpoint_serves_model() {
        let filter = make_filter();
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"missing","prompt":"hi"}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Reject(r) if r.status == 503));
        assert!(
            ctx.upstream.is_none(),
            "no eligible endpoint must not select an upstream"
        );
    }

    #[tokio::test]
    async fn snapshot_update_changes_selection() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![
                make_test_endpoint("ep-a", "127.0.0.1:9001", 0, 0, 0.0),
                make_test_endpoint("ep-b", "127.0.0.1:9002", 10, 5, 90.0),
            ],
        });
        let filter = make_filter_with_state(state.clone());

        let addr1 = pick_endpoint(&filter, "test-model").await;
        assert_eq!(
            addr1.as_deref(),
            Some("127.0.0.1:9001"),
            "initially ep-a should be selected (lower load)"
        );

        state.update(EndpointSnapshot {
            endpoints: vec![
                make_test_endpoint("ep-a", "127.0.0.1:9001", 20, 10, 95.0),
                make_test_endpoint("ep-b", "127.0.0.1:9002", 0, 0, 5.0),
            ],
        });

        let addr2 = pick_endpoint(&filter, "test-model").await;
        assert_eq!(
            addr2.as_deref(),
            Some("127.0.0.1:9002"),
            "after update ep-b should be selected (lower load)"
        );
    }

    #[tokio::test]
    async fn discovery_only_routes_to_discovered_endpoint() {
        let state = EndpointStateHandle::new(EndpointSnapshot { endpoints: Vec::new() });
        let filter = make_filter_with_state(state.clone());

        let addr_before = pick_endpoint(&filter, "test-model").await;
        assert!(
            addr_before.is_none(),
            "no endpoints should be available before discovery"
        );

        state.update(EndpointSnapshot {
            endpoints: vec![
                make_test_endpoint("discovered-a:8000", "10.0.0.1:8000", 2, 1, 30.0),
                make_test_endpoint("discovered-b:8000", "10.0.0.2:8000", 0, 0, 5.0),
            ],
        });

        let addr_after = pick_endpoint(&filter, "test-model").await;
        assert_eq!(
            addr_after.as_deref(),
            Some("10.0.0.2:8000"),
            "should route to the lower-pressure discovered endpoint"
        );
    }

    // -- Test Utilities --

    fn make_test_endpoint(name: &str, address: &str, running: u64, waiting: u64, kv: f64) -> EndpointState {
        EndpointState {
            name: Arc::from(name),
            address: Arc::from(address),
            models: vec![Arc::from("test-model")],
            running_requests: running,
            waiting_requests: waiting,
            kv_cache_usage_percent: kv,
            healthy: true,
            metrics_url: None,
            role: disaggregation::default_endpoint_role(),
        }
    }

    fn make_filter_with_state(state: EndpointStateHandle) -> LlmdEndpointPickerFilter {
        LlmdEndpointPickerFilter {
            max_body_bytes: 1_048_576, // 1 MiB
            pool_name: Arc::from("test"),
            queue_weight: 2.0,
            kv_cache_weight: 2.0,
            state,
            connection: Arc::new(ConnectionOptions::default()),
            _worker: None,
            prefix_index: None,
            prefix_config: None,
            saturation_config: None,
            disagg_config: None,
            model_rewrite_handle: None,
            model_rewrite_enabled: false,
            objective_handle: None,
            objective_enabled: false,
        }
    }

    async fn pick_endpoint(filter: &LlmdEndpointPickerFilter, model: &str) -> Option<String> {
        let body_json = format!(r#"{{"model":"{model}","messages":[]}}"#);
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from(body_json));
        let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();
        ctx.upstream_addr().map(str::to_owned)
    }

    fn make_filter() -> Box<dyn HttpFilter> {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
endpoints:
  - name: loaded
    address: "127.0.0.1:18081"
    models: ["fake-model"]
    running_requests: 8
    waiting_requests: 4
    kv_cache_usage_percent: 92.0
  - name: less-loaded
    address: "127.0.0.1:18082"
    models: ["fake-model"]
    running_requests: 1
    waiting_requests: 0
    kv_cache_usage_percent: 10.0
  - name: other-model
    address: "127.0.0.1:18083"
    models: ["other-model"]
    running_requests: 0
    waiting_requests: 0
    kv_cache_usage_percent: 0.0
"#,
        )
        .unwrap();
        LlmdEndpointPickerFilter::from_config(&yaml).unwrap()
    }

    fn make_disagg_endpoint(name: &str, address: &str, role: disaggregation::EndpointRole) -> EndpointState {
        let mut ep = make_test_endpoint(name, address, 0, 0, 0.0);
        ep.role = role;
        ep
    }

    fn make_disagg_filter(state: EndpointStateHandle) -> LlmdEndpointPickerFilter {
        LlmdEndpointPickerFilter {
            max_body_bytes: 1_048_576, // 1 MiB
            pool_name: Arc::from("test"),
            queue_weight: 2.0,
            kv_cache_weight: 2.0,
            state,
            connection: Arc::new(ConnectionOptions::default()),
            _worker: None,
            prefix_index: None,
            prefix_config: None,
            saturation_config: None,
            disagg_config: Some(DisaggregationConfig {
                enabled: true,
                prefill_header: "x-prefiller-host-port".to_owned(),
                prefill_mode: PrefillMode::Always,
                inject_kv_transfer_params: true,
            }),
            model_rewrite_handle: None,
            model_rewrite_enabled: false,
            objective_handle: None,
            objective_enabled: false,
        }
    }

    fn make_disagg_filter_with_saturation() -> LlmdEndpointPickerFilter {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![
                make_loaded_prefill("healthy-prefill", "10.0.0.1:8000", 0, 5.0),
                make_loaded_prefill("saturated-prefill", "10.0.0.3:8000", 20, 99.0),
                make_disagg_endpoint("decode-ep", "10.0.0.2:8000", disaggregation::EndpointRole::Decode),
            ],
        });
        let mut filter = make_disagg_filter(state);
        filter.saturation_config = Some(SaturationGateConfig {
            enabled: true,
            queue_depth_threshold: 5,
            kv_cache_util_threshold: 0.8,
            pool_saturation_threshold: 5.0,
            headroom: 0.2,
            reject_status: 429,
            priority_headroom_per_level: 0.0,
        });
        filter
    }

    fn make_loaded_prefill(name: &str, address: &str, waiting: u64, kv: f64) -> EndpointState {
        let mut ep = make_disagg_endpoint(name, address, disaggregation::EndpointRole::Prefill);
        ep.waiting_requests = waiting;
        ep.kv_cache_usage_percent = kv;
        ep
    }

    fn make_rewrite_disagg_filter(from: &str, to: &str) -> LlmdEndpointPickerFilter {
        let decode =
            make_disagg_endpoint_with_model("decode-ep", "10.0.0.2:8000", to, disaggregation::EndpointRole::Decode);
        let prefill =
            make_disagg_endpoint_with_model("prefill-ep", "10.0.0.1:8000", to, disaggregation::EndpointRole::Prefill);
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![decode, prefill],
        });
        let snapshot = make_rewrite_snapshot(vec![(from, to)]);
        let handle = ModelRewriteHandle::new(model_rewrite::empty_snapshot());
        handle.update(snapshot);
        let mut filter = make_disagg_filter(state);
        filter.model_rewrite_handle = Some(handle);
        filter.model_rewrite_enabled = true;
        filter
    }

    fn make_disagg_endpoint_with_model(
        name: &str,
        address: &str,
        model: &str,
        role: disaggregation::EndpointRole,
    ) -> EndpointState {
        let mut ep = make_disagg_endpoint(name, address, role);
        ep.models = vec![Arc::from(model)];
        ep
    }

    #[tokio::test]
    async fn disaggregation_routes_to_decode_endpoint() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![
                make_disagg_endpoint("prefill-ep", "10.0.0.1:8000", disaggregation::EndpointRole::Prefill),
                make_disagg_endpoint("decode-ep", "10.0.0.2:8000", disaggregation::EndpointRole::Decode),
            ],
        });
        let filter = make_disagg_filter(state);
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release), "should release");
        assert_eq!(
            ctx.upstream_addr(),
            Some("10.0.0.2:8000"),
            "should route to decode endpoint"
        );
        assert_eq!(
            ctx.get_metadata("llmd.disaggregation"),
            Some("enabled"),
            "disaggregation metadata"
        );
        assert_eq!(
            ctx.get_metadata("llmd.decode_endpoint"),
            Some("decode-ep"),
            "decode metadata"
        );
    }

    #[tokio::test]
    async fn disaggregation_injects_prefill_header() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![
                make_disagg_endpoint("prefill-ep", "10.0.0.1:8000", disaggregation::EndpointRole::Prefill),
                make_disagg_endpoint("decode-ep", "10.0.0.2:8000", disaggregation::EndpointRole::Decode),
            ],
        });
        let filter = make_disagg_filter(state);
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release), "should release");
        let prefill_header = ctx
            .extra_request_headers
            .iter()
            .find(|(name, _)| name == "x-prefiller-host-port");
        assert!(prefill_header.is_some(), "should inject prefill header");
        assert_eq!(
            prefill_header.unwrap().1,
            "10.0.0.1:8000",
            "header value should be prefill address"
        );
        assert_eq!(
            ctx.get_metadata("llmd.prefill_endpoint"),
            Some("prefill-ep"),
            "prefill metadata"
        );
    }

    #[tokio::test]
    async fn disaggregation_no_prefill_when_mode_never() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![
                make_disagg_endpoint("prefill-ep", "10.0.0.1:8000", disaggregation::EndpointRole::Prefill),
                make_disagg_endpoint("decode-ep", "10.0.0.2:8000", disaggregation::EndpointRole::Decode),
            ],
        });
        let mut filter = make_disagg_filter(state);
        filter.disagg_config.as_mut().unwrap().prefill_mode = PrefillMode::Never;

        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release), "should release");
        assert!(
            ctx.extra_request_headers.is_empty(),
            "no prefill header when mode is Never"
        );
        assert_eq!(ctx.upstream_addr(), Some("10.0.0.2:8000"), "should route to decode");
    }

    #[tokio::test]
    async fn disaggregation_prefill_decode_role_works_as_both() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_disagg_endpoint(
                "both-ep",
                "10.0.0.1:8000",
                disaggregation::EndpointRole::PrefillDecode,
            )],
        });
        let filter = make_disagg_filter(state);
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release), "should release");
        assert_eq!(
            ctx.upstream_addr(),
            Some("10.0.0.1:8000"),
            "should route to the only endpoint"
        );
    }

    #[tokio::test]
    async fn disaggregation_fails_when_no_decode_candidates() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_disagg_endpoint(
                "prefill-only",
                "10.0.0.1:8000",
                disaggregation::EndpointRole::Prefill,
            )],
        });
        let filter = make_disagg_filter(state);
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(
            matches!(action, FilterAction::Reject(ref r) if r.status == 503),
            "should reject when no decode candidates"
        );
    }

    #[tokio::test]
    async fn disaggregation_saturation_filters_prefill_candidates() {
        let filter = make_disagg_filter_with_saturation();
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release), "should release");
        assert_eq!(
            ctx.get_metadata("llmd.prefill_endpoint"),
            Some("healthy-prefill"),
            "saturated prefill should be filtered; healthy one selected"
        );
    }

    #[tokio::test]
    async fn disaggregation_no_prefill_sets_none_metadata() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_disagg_endpoint(
                "decode-only",
                "10.0.0.2:8000",
                disaggregation::EndpointRole::Decode,
            )],
        });
        let filter = make_disagg_filter(state);
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release), "should release");
        assert_eq!(ctx.upstream_addr(), Some("10.0.0.2:8000"), "should route to decode");
        assert_eq!(
            ctx.get_metadata("llmd.prefill_endpoint"),
            Some("none"),
            "should indicate no prefill available"
        );
        assert!(ctx.extra_request_headers.is_empty(), "no prefill header");
    }

    // -- KV Transfer Params Tests --

    #[tokio::test]
    async fn disaggregation_injects_kv_transfer_params() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![
                make_disagg_endpoint("prefill-ep", "10.0.0.1:8000", disaggregation::EndpointRole::Prefill),
                make_disagg_endpoint("decode-ep", "10.0.0.2:8000", disaggregation::EndpointRole::Decode),
            ],
        });
        let filter = make_disagg_filter(state);
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release), "should release");
        let body_str = String::from_utf8_lossy(body.as_ref().unwrap());
        let parsed: serde_json::Value = serde_json::from_str(&body_str).unwrap();
        let kv = &parsed["kv_transfer_params"];
        assert_eq!(kv["do_remote_decode"], true, "do_remote_decode");
        assert_eq!(kv["do_remote_prefill"], false, "do_remote_prefill");
        assert_eq!(
            kv["remote_host"].as_str(),
            Some("10.0.0.1:8000"),
            "remote_host should be prefill address"
        );
    }

    #[tokio::test]
    async fn disaggregation_kv_transfer_preserves_existing_fields() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![
                make_disagg_endpoint("prefill-ep", "10.0.0.1:8000", disaggregation::EndpointRole::Prefill),
                make_disagg_endpoint("decode-ep", "10.0.0.2:8000", disaggregation::EndpointRole::Decode),
            ],
        });
        let filter = make_disagg_filter(state);
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from(
            br#"{"model":"test-model","messages":[{"role":"user","content":"hi"}],"temperature":0.7}"#.to_vec(),
        ));

        let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        let body_str = String::from_utf8_lossy(body.as_ref().unwrap());
        let parsed: serde_json::Value = serde_json::from_str(&body_str).unwrap();
        assert_eq!(parsed["model"].as_str(), Some("test-model"), "model preserved");
        assert!(parsed["messages"].is_array(), "messages preserved");
        assert_eq!(parsed["temperature"], 0.7, "temperature preserved");
    }

    #[tokio::test]
    async fn disaggregation_no_kv_transfer_when_mode_never() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![
                make_disagg_endpoint("prefill-ep", "10.0.0.1:8000", disaggregation::EndpointRole::Prefill),
                make_disagg_endpoint("decode-ep", "10.0.0.2:8000", disaggregation::EndpointRole::Decode),
            ],
        });
        let mut filter = make_disagg_filter(state);
        filter.disagg_config.as_mut().unwrap().prefill_mode = PrefillMode::Never;

        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        let body_str = String::from_utf8_lossy(body.as_ref().unwrap());
        assert!(
            !body_str.contains("kv_transfer_params"),
            "kv_transfer_params should not be injected when prefill_mode is Never"
        );
    }

    #[tokio::test]
    async fn disaggregation_no_kv_transfer_when_disabled() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![
                make_disagg_endpoint("prefill-ep", "10.0.0.1:8000", disaggregation::EndpointRole::Prefill),
                make_disagg_endpoint("decode-ep", "10.0.0.2:8000", disaggregation::EndpointRole::Decode),
            ],
        });
        let mut filter = make_disagg_filter(state);
        filter.disagg_config.as_mut().unwrap().inject_kv_transfer_params = false;

        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        let body_str = String::from_utf8_lossy(body.as_ref().unwrap());
        assert!(
            !body_str.contains("kv_transfer_params"),
            "kv_transfer_params should not be injected when inject_kv_transfer_params is false"
        );
    }

    #[tokio::test]
    async fn disaggregation_kv_transfer_body_access_is_readwrite() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_disagg_endpoint(
                "decode-ep",
                "10.0.0.2:8000",
                disaggregation::EndpointRole::Decode,
            )],
        });
        let filter = make_disagg_filter(state);

        assert_eq!(
            filter.request_body_access(),
            BodyAccess::ReadWrite,
            "body access should be ReadWrite when kv_transfer is enabled"
        );
    }

    #[tokio::test]
    async fn disaggregation_kv_transfer_disabled_body_access_readonly() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_disagg_endpoint(
                "decode-ep",
                "10.0.0.2:8000",
                disaggregation::EndpointRole::Decode,
            )],
        });
        let mut filter = make_disagg_filter(state);
        filter.disagg_config.as_mut().unwrap().inject_kv_transfer_params = false;

        assert_eq!(
            filter.request_body_access(),
            BodyAccess::ReadOnly,
            "body access should be ReadOnly when kv_transfer is disabled"
        );
    }

    #[tokio::test]
    async fn disaggregation_kv_transfer_updates_content_length() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![
                make_disagg_endpoint("prefill-ep", "10.0.0.1:8000", disaggregation::EndpointRole::Prefill),
                make_disagg_endpoint("decode-ep", "10.0.0.2:8000", disaggregation::EndpointRole::Decode),
            ],
        });
        let filter = make_disagg_filter(state);
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        let cl_header = ctx
            .extra_request_headers
            .iter()
            .find(|(name, _)| name == "content-length");
        assert!(
            cl_header.is_some(),
            "should set content-length header after kv_transfer body mutation"
        );
        let cl_value: usize = cl_header.unwrap().1.parse().unwrap();
        assert_eq!(
            cl_value,
            body.as_ref().unwrap().len(),
            "content-length should match actual body length"
        );
    }

    // -- Model Rewrite Tests --

    fn make_rewrite_filter(state: EndpointStateHandle, rewrite_handle: ModelRewriteHandle) -> LlmdEndpointPickerFilter {
        LlmdEndpointPickerFilter {
            max_body_bytes: 1_048_576, // 1 MiB
            pool_name: Arc::from("test"),
            queue_weight: 2.0,
            kv_cache_weight: 2.0,
            state,
            connection: Arc::new(ConnectionOptions::default()),
            _worker: None,
            prefix_index: None,
            prefix_config: None,
            saturation_config: None,
            disagg_config: None,
            model_rewrite_handle: Some(rewrite_handle),
            model_rewrite_enabled: true,
            objective_handle: None,
            objective_enabled: false,
        }
    }

    fn make_rewrite_snapshot(exact: Vec<(&str, &str)>) -> model_rewrite::ModelRewriteSnapshot {
        let cfg = make_rewrite_config();
        let items: Vec<model_rewrite::RewriteItem> = exact
            .into_iter()
            .map(|(from, to)| make_rewrite_item(from, to))
            .collect();
        model_rewrite::build_snapshot(&items, &cfg)
    }

    fn make_rewrite_config() -> ModelRewriteConfig {
        ModelRewriteConfig {
            enabled: true,
            namespace: Some("default".to_owned()),
            api_version: "llm-d.ai/v1alpha2".to_owned(),
            pool_ref: model_rewrite::PoolRefConfig {
                name: "my-pool".to_owned(),
                group: "inference.networking.k8s.io".to_owned(),
                kind: "InferencePool".to_owned(),
            },
        }
    }

    fn make_rewrite_item(from: &str, to: &str) -> model_rewrite::RewriteItem {
        model_rewrite::RewriteItem {
            metadata: model_rewrite::RewriteMetadata {
                name: Some(format!("rewrite-{from}")),
                creation_timestamp: Some("2024-01-01T00:00:00Z".to_owned()),
            },
            spec: model_rewrite::RewriteSpec {
                pool_ref: Some(model_rewrite::RewritePoolRef {
                    group: Some("inference.networking.k8s.io".to_owned()),
                    kind: Some("InferencePool".to_owned()),
                    name: "my-pool".to_owned(),
                }),
                rules: vec![make_rewrite_rule(from, to)],
            },
        }
    }

    fn make_rewrite_rule(from: &str, to: &str) -> model_rewrite::RewriteRule {
        model_rewrite::RewriteRule {
            matches: vec![model_rewrite::RuleMatch {
                model: Some(model_rewrite::ModelMatch {
                    r#type: Some("Exact".to_owned()),
                    value: from.to_owned(),
                }),
            }],
            targets: vec![model_rewrite::TargetModel {
                weight: 1,
                model_rewrite: to.to_owned(),
            }],
        }
    }

    fn make_rewrite_filter_with_rule(from: &str, to: &str) -> (LlmdEndpointPickerFilter, EndpointStateHandle) {
        let mut ep = make_test_endpoint("ep-rewritten", "127.0.0.1:9002", 0, 0, 0.0);
        ep.models = vec![Arc::from(to)];
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_test_endpoint("ep-original", "127.0.0.1:9001", 0, 0, 0.0), ep],
        });
        let snapshot = make_rewrite_snapshot(vec![(from, to)]);
        let handle = ModelRewriteHandle::new(model_rewrite::empty_snapshot());
        handle.update(snapshot);
        (make_rewrite_filter(state.clone(), handle), state)
    }

    #[tokio::test]
    async fn model_rewrite_routes_to_rewritten_endpoint() {
        let (filter, _state) = make_rewrite_filter_with_rule("test-model", "rewritten-model");
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release), "should release");
        assert_eq!(
            ctx.upstream_addr(),
            Some("127.0.0.1:9002"),
            "should route to rewritten endpoint"
        );
        assert_eq!(
            ctx.get_metadata("llmd.model"),
            Some("rewritten-model"),
            "model metadata"
        );
    }

    #[tokio::test]
    async fn model_rewrite_sets_metadata() {
        let (filter, _state) = make_rewrite_filter_with_rule("test-model", "rewritten-model");
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert_eq!(
            ctx.get_metadata("llmd.original_model"),
            Some("test-model"),
            "original model"
        );
        assert_eq!(
            ctx.get_metadata("llmd.model_rewrite"),
            Some("rewritten-model"),
            "rewrite target"
        );
        assert_eq!(
            ctx.get_metadata("llmd.model_rewrite_source"),
            Some("rewrite-test-model"),
            "rewrite source name"
        );
    }

    #[tokio::test]
    async fn model_rewrite_no_match_has_no_source_metadata() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_test_endpoint("ep-a", "127.0.0.1:9001", 0, 0, 0.0)],
        });
        let snapshot = make_rewrite_snapshot(vec![("other-model", "rewritten")]);
        let handle = ModelRewriteHandle::new(model_rewrite::empty_snapshot());
        handle.update(snapshot);
        let filter = make_rewrite_filter(state, handle);

        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert_eq!(
            ctx.get_metadata("llmd.model_rewrite"),
            Some("none"),
            "no match should set model_rewrite to none"
        );
        assert!(
            ctx.get_metadata("llmd.model_rewrite_source").is_none(),
            "no source metadata when no match"
        );
    }

    #[tokio::test]
    async fn model_rewrite_mutates_body() {
        let (filter, _state) = make_rewrite_filter_with_rule("test-model", "rewritten-model");
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        let body_str = String::from_utf8_lossy(body.as_ref().unwrap());
        assert!(
            body_str.contains("rewritten-model"),
            "body should contain rewritten model"
        );
    }

    #[tokio::test]
    async fn model_rewrite_no_match_routes_normally() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_test_endpoint("ep-a", "127.0.0.1:9001", 0, 0, 0.0)],
        });
        let snapshot = make_rewrite_snapshot(vec![("other-model", "rewritten")]);
        let handle = ModelRewriteHandle::new(model_rewrite::empty_snapshot());
        handle.update(snapshot);
        let filter = make_rewrite_filter(state, handle);

        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release), "should release");
        assert_eq!(ctx.upstream_addr(), Some("127.0.0.1:9001"), "should route normally");
        assert_eq!(
            ctx.get_metadata("llmd.model_rewrite"),
            Some("none"),
            "should indicate no rewrite"
        );
        assert!(
            ctx.get_metadata("llmd.original_model").is_none(),
            "no original_model when no rewrite"
        );
    }

    #[tokio::test]
    async fn model_rewrite_disabled_routes_normally() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_test_endpoint("ep-a", "127.0.0.1:9001", 0, 0, 0.0)],
        });
        let handle = ModelRewriteHandle::new(model_rewrite::empty_snapshot());
        let mut filter = make_rewrite_filter(state, handle);
        filter.model_rewrite_enabled = false;

        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release), "should release");
        assert_eq!(ctx.upstream_addr(), Some("127.0.0.1:9001"), "should route normally");
    }

    #[tokio::test]
    async fn model_rewrite_body_access_is_readwrite_when_enabled() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_test_endpoint("ep-a", "127.0.0.1:9001", 0, 0, 0.0)],
        });
        let handle = ModelRewriteHandle::new(model_rewrite::empty_snapshot());
        let filter = make_rewrite_filter(state, handle);

        assert_eq!(
            filter.request_body_access(),
            BodyAccess::ReadWrite,
            "body access should be ReadWrite when model_rewrite is enabled"
        );
    }

    #[tokio::test]
    async fn model_rewrite_updates_content_length() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![{
                let mut ep = make_test_endpoint("ep-rewritten", "127.0.0.1:9002", 0, 0, 0.0);
                ep.models = vec![Arc::from("much-longer-rewritten-model-name")];
                ep
            }],
        });
        let snapshot = make_rewrite_snapshot(vec![("test-model", "much-longer-rewritten-model-name")]);
        let handle = ModelRewriteHandle::new(model_rewrite::empty_snapshot());
        handle.update(snapshot);
        let filter = make_rewrite_filter(state, handle);

        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release), "should release");
        let cl_header = ctx
            .extra_request_headers
            .iter()
            .find(|(name, _)| name == "content-length");
        assert!(
            cl_header.is_some(),
            "should set content-length header after body mutation"
        );
        let cl_value: usize = cl_header.unwrap().1.parse().unwrap();
        assert_eq!(
            cl_value,
            body.as_ref().unwrap().len(),
            "content-length should match actual body length"
        );
    }

    // -- Inference Objective Tests --

    fn make_objective_filter(
        state: EndpointStateHandle,
        objective_handle: ObjectiveHandle,
    ) -> LlmdEndpointPickerFilter {
        LlmdEndpointPickerFilter {
            max_body_bytes: 1_048_576, // 1 MiB
            pool_name: Arc::from("test"),
            queue_weight: 2.0,
            kv_cache_weight: 2.0,
            state,
            connection: Arc::new(ConnectionOptions::default()),
            _worker: None,
            prefix_index: None,
            prefix_config: None,
            saturation_config: None,
            disagg_config: None,
            model_rewrite_handle: None,
            model_rewrite_enabled: false,
            objective_handle: Some(objective_handle),
            objective_enabled: true,
        }
    }

    fn make_objective_snapshot(entries: Vec<(&str, i32)>) -> inference_objective::ObjectiveSnapshot {
        let cfg = make_objective_config();
        let items: Vec<inference_objective::ObjectiveItem> = entries
            .into_iter()
            .map(|(name, priority)| make_objective_item(name, priority))
            .collect();
        inference_objective::build_snapshot(&items, &cfg)
    }

    fn make_objective_config() -> InferenceObjectiveConfig {
        InferenceObjectiveConfig {
            enabled: true,
            namespace: Some("default".to_owned()),
            api_version: "llm-d.ai/v1alpha2".to_owned(),
            pool_ref: model_rewrite::PoolRefConfig {
                name: "my-pool".to_owned(),
                group: "inference.networking.k8s.io".to_owned(),
                kind: "InferencePool".to_owned(),
            },
        }
    }

    fn make_objective_item(name: &str, priority: i32) -> inference_objective::ObjectiveItem {
        inference_objective::ObjectiveItem {
            metadata: inference_objective::ObjectiveMetadata {
                name: Some(name.to_owned()),
                creation_timestamp: Some("2024-01-01T00:00:00Z".to_owned()),
            },
            spec: inference_objective::ObjectiveSpec {
                pool_ref: Some(inference_objective::ObjectivePoolRef {
                    group: Some("inference.networking.k8s.io".to_owned()),
                    kind: Some("InferencePool".to_owned()),
                    name: "my-pool".to_owned(),
                }),
                priority: Some(priority),
            },
        }
    }

    #[tokio::test]
    async fn objective_current_header_selects_objective() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_test_endpoint("ep-a", "127.0.0.1:9001", 0, 0, 0.0)],
        });
        let handle = ObjectiveHandle::new(inference_objective::empty_snapshot());
        handle.update(make_objective_snapshot(vec![("my-objective", 10)]));
        let filter = make_objective_filter(state, handle);

        let mut req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        req.headers
            .insert("x-llm-d-inference-objective", "my-objective".parse().unwrap());
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert_eq!(
            ctx.get_metadata("llmd.inference_objective"),
            Some("my-objective"),
            "objective name from current header"
        );
        assert_eq!(
            ctx.get_metadata("llmd.inference_objective_priority"),
            Some("10"),
            "priority from snapshot"
        );
        assert_eq!(
            ctx.get_metadata("llmd.inference_objective_source"),
            Some("my-objective"),
            "source name from snapshot"
        );
    }

    #[tokio::test]
    async fn objective_deprecated_header_fallback() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_test_endpoint("ep-a", "127.0.0.1:9001", 0, 0, 0.0)],
        });
        let handle = ObjectiveHandle::new(inference_objective::empty_snapshot());
        handle.update(make_objective_snapshot(vec![("old-obj", 5)]));
        let filter = make_objective_filter(state, handle);

        let mut req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        req.headers
            .insert("x-gateway-inference-objective", "old-obj".parse().unwrap());
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert_eq!(
            ctx.get_metadata("llmd.inference_objective"),
            Some("old-obj"),
            "should use deprecated header as fallback"
        );
        assert_eq!(
            ctx.get_metadata("llmd.inference_objective_priority"),
            Some("5"),
            "priority from deprecated header objective"
        );
    }

    #[tokio::test]
    async fn objective_current_header_wins_over_deprecated() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_test_endpoint("ep-a", "127.0.0.1:9001", 0, 0, 0.0)],
        });
        let handle = ObjectiveHandle::new(inference_objective::empty_snapshot());
        handle.update(make_objective_snapshot(vec![("current", 10), ("deprecated", 99)]));
        let filter = make_objective_filter(state, handle);

        let mut req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        req.headers
            .insert("x-llm-d-inference-objective", "current".parse().unwrap());
        req.headers
            .insert("x-gateway-inference-objective", "deprecated".parse().unwrap());
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert_eq!(
            ctx.get_metadata("llmd.inference_objective"),
            Some("current"),
            "current header should win over deprecated"
        );
        assert_eq!(
            ctx.get_metadata("llmd.inference_objective_priority"),
            Some("10"),
            "priority from current header objective"
        );
    }

    #[tokio::test]
    async fn objective_missing_header_sets_none() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_test_endpoint("ep-a", "127.0.0.1:9001", 0, 0, 0.0)],
        });
        let handle = ObjectiveHandle::new(inference_objective::empty_snapshot());
        handle.update(make_objective_snapshot(vec![("some-obj", 10)]));
        let filter = make_objective_filter(state, handle);

        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert_eq!(
            ctx.get_metadata("llmd.inference_objective"),
            Some("none"),
            "no header should set objective to none"
        );
        assert_eq!(
            ctx.get_metadata("llmd.inference_objective_priority"),
            Some("0"),
            "no header should set priority to 0"
        );
    }

    #[tokio::test]
    async fn objective_unknown_header_sets_unknown() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_test_endpoint("ep-a", "127.0.0.1:9001", 0, 0, 0.0)],
        });
        let handle = ObjectiveHandle::new(inference_objective::empty_snapshot());
        handle.update(make_objective_snapshot(vec![("known-obj", 10)]));
        let filter = make_objective_filter(state, handle);

        let mut req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        req.headers
            .insert("x-llm-d-inference-objective", "unrecognized".parse().unwrap());
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert_eq!(
            ctx.get_metadata("llmd.inference_objective"),
            Some("unknown"),
            "unknown objective should set metadata to unknown"
        );
        assert_eq!(
            ctx.get_metadata("llmd.inference_objective_priority"),
            Some("0"),
            "unknown objective should set priority to 0"
        );
        assert!(
            ctx.get_metadata("llmd.inference_objective_source").is_none(),
            "no source when objective is unknown"
        );
    }

    #[tokio::test]
    async fn objective_negative_priority_preserved() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_test_endpoint("ep-a", "127.0.0.1:9001", 0, 0, 0.0)],
        });
        let handle = ObjectiveHandle::new(inference_objective::empty_snapshot());
        handle.update(make_objective_snapshot(vec![("neg-obj", -42)]));
        let filter = make_objective_filter(state, handle);

        let mut req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        req.headers
            .insert("x-llm-d-inference-objective", "neg-obj".parse().unwrap());
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert_eq!(
            ctx.get_metadata("llmd.inference_objective"),
            Some("neg-obj"),
            "negative priority objective should be found"
        );
        assert_eq!(
            ctx.get_metadata("llmd.inference_objective_priority"),
            Some("-42"),
            "negative priority should be preserved"
        );
    }

    // -- Priority-Aware Saturation Gate Tests --

    fn make_saturation_filter_with_priority(
        state: EndpointStateHandle,
        objective_handle: ObjectiveHandle,
        priority_headroom_per_level: f64,
    ) -> LlmdEndpointPickerFilter {
        LlmdEndpointPickerFilter {
            max_body_bytes: 1_048_576,
            pool_name: Arc::from("test"),
            queue_weight: 2.0,
            kv_cache_weight: 2.0,
            state,
            connection: Arc::new(ConnectionOptions::default()),
            _worker: None,
            prefix_index: None,
            prefix_config: None,
            saturation_config: Some(SaturationGateConfig {
                enabled: true,
                queue_depth_threshold: 5,
                kv_cache_util_threshold: 0.8,
                pool_saturation_threshold: 1.0,
                headroom: 0.2,
                reject_status: 429,
                priority_headroom_per_level,
            }),
            disagg_config: None,
            model_rewrite_handle: None,
            model_rewrite_enabled: false,
            objective_handle: Some(objective_handle),
            objective_enabled: true,
        }
    }

    #[tokio::test]
    async fn high_priority_admitted_when_default_rejected() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![
                make_test_endpoint("ep-a", "127.0.0.1:9001", 0, 5, 80.0),
                make_test_endpoint("ep-b", "127.0.0.1:9002", 0, 5, 80.0),
            ],
        });
        let handle = ObjectiveHandle::new(inference_objective::empty_snapshot());
        handle.update(make_objective_snapshot(vec![("high-pri", 10)]));
        let filter = make_saturation_filter_with_priority(state, handle, 0.1);

        let mut req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        req.headers
            .insert("x-llm-d-inference-objective", "high-pri".parse().unwrap());
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(
            matches!(action, FilterAction::Release),
            "high-priority request should be admitted past saturation"
        );
    }

    #[tokio::test]
    async fn low_priority_rejected_when_default_admitted() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![
                make_test_endpoint("ep-a", "127.0.0.1:9001", 0, 4, 70.0),
                make_test_endpoint("ep-b", "127.0.0.1:9002", 0, 4, 70.0),
            ],
        });
        let handle = ObjectiveHandle::new(inference_objective::empty_snapshot());
        handle.update(make_objective_snapshot(vec![("low-pri", -5)]));
        let filter = make_saturation_filter_with_priority(state, handle, 0.1);

        let mut req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        req.headers
            .insert("x-llm-d-inference-objective", "low-pri".parse().unwrap());
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(
            matches!(action, FilterAction::Reject(ref r) if r.status == 429),
            "low-priority request should be rejected at moderate saturation"
        );
    }

    #[tokio::test]
    async fn priority_routing_unchanged_when_admitted() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![
                make_test_endpoint("ep-a", "127.0.0.1:9001", 0, 5, 80.0),
                make_test_endpoint("ep-b", "127.0.0.1:9002", 0, 0, 0.0),
            ],
        });
        let handle = ObjectiveHandle::new(inference_objective::empty_snapshot());
        handle.update(make_objective_snapshot(vec![("hi", 10)]));
        let filter = make_saturation_filter_with_priority(state, handle, 0.1);

        let mut req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        req.headers.insert("x-llm-d-inference-objective", "hi".parse().unwrap());
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"test-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release), "should be admitted");
        assert_eq!(
            ctx.upstream_addr(),
            Some("127.0.0.1:9002"),
            "should route to less-loaded endpoint"
        );
        assert_eq!(ctx.get_metadata("llmd.endpoint"), Some("ep-b"), "endpoint metadata");
    }

    #[tokio::test]
    async fn model_rewrite_and_disaggregation_compose_correctly() {
        let filter = make_rewrite_disagg_filter("original-model", "rewritten-model");
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from(
            br#"{"model":"original-model","messages":[{"role":"user","content":"hi"}]}"#.to_vec(),
        ));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(matches!(action, FilterAction::Release), "should release");
        let parsed: serde_json::Value = serde_json::from_slice(body.as_ref().unwrap()).unwrap();
        assert_eq!(
            parsed["model"].as_str(),
            Some("rewritten-model"),
            "model should be rewritten"
        );
        assert!(parsed["messages"].is_array(), "messages should be preserved");
        let kv = &parsed["kv_transfer_params"];
        assert_eq!(kv["do_remote_decode"], true, "do_remote_decode from P/D");
        assert_eq!(kv["do_remote_prefill"], false, "do_remote_prefill from P/D");
        assert!(
            kv["remote_host"].as_str().unwrap().contains("10.0.0.1"),
            "remote_host should contain prefill address"
        );
    }

    #[tokio::test]
    async fn rewrite_and_kv_transfer_content_length_correct() {
        let filter = make_rewrite_disagg_filter("original-model", "rewritten-model");
        let req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from(
            br#"{"model":"original-model","messages":[{"role":"user","content":"hi"}]}"#.to_vec(),
        ));

        let _action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        let cl_headers: Vec<_> = ctx
            .extra_request_headers
            .iter()
            .filter(|(name, _)| name == "content-length")
            .collect();
        assert!(
            !cl_headers.is_empty(),
            "content-length should be set after body mutations"
        );
        let last_cl: usize = cl_headers.last().unwrap().1.parse().unwrap();
        assert_eq!(
            last_cl,
            body.as_ref().unwrap().len(),
            "final content-length should match actual body length after both rewrite and kv_transfer"
        );
    }

    #[tokio::test]
    async fn no_candidates_returns_503_not_429_with_negative_priority() {
        let state = EndpointStateHandle::new(EndpointSnapshot {
            endpoints: vec![make_test_endpoint("ep-a", "127.0.0.1:9001", 0, 5, 80.0)],
        });
        let handle = ObjectiveHandle::new(inference_objective::empty_snapshot());
        handle.update(make_objective_snapshot(vec![("neg-obj", -5)]));
        let filter = make_saturation_filter_with_priority(state, handle, 0.1);

        let mut req = crate::test_utils::make_request(http::Method::POST, "/v1/chat/completions");
        req.headers
            .insert("x-llm-d-inference-objective", "neg-obj".parse().unwrap());
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let mut body = Some(Bytes::from_static(br#"{"model":"unsupported-model","messages":[]}"#));

        let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();

        assert!(
            matches!(action, FilterAction::Reject(ref r) if r.status == 503),
            "unsupported model with negative priority should get 503, not 429"
        );
    }
}
