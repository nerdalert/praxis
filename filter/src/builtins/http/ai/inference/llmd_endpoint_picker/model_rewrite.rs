// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! `InferenceModelRewrite` support for the llm-d endpoint picker.
//!
//! Watches `InferenceModelRewrite` resources in Kubernetes and builds
//! a snapshot mapping incoming model names to weighted rewrite targets.
//! At request time, the filter looks up the model, selects a target
//! using weighted random selection, and mutates the request body.

use std::{collections::HashMap, sync::Arc};

use arc_swap::ArcSwap;
use serde::Deserialize;
use tracing::debug;

use crate::FilterError;

// -----------------------------------------------------------------------------
// Config
// -----------------------------------------------------------------------------

/// Default API version for `InferenceModelRewrite` resources.
const DEFAULT_REWRITE_API_VERSION: &str = "llm-d.ai/v1alpha2";
/// Default API group for the pool reference.
const DEFAULT_POOL_REF_GROUP: &str = "inference.networking.k8s.io";
/// Default kind for the pool reference.
const DEFAULT_POOL_REF_KIND: &str = "InferencePool";

/// Configuration for `InferenceModelRewrite` support.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelRewriteConfig {
    /// Whether model rewrite is active.
    pub enabled: bool,

    /// Namespace to list `InferenceModelRewrite` resources from.
    #[serde(default)]
    pub namespace: Option<String>,

    /// API version for the `InferenceModelRewrite` CRD.
    #[serde(default = "default_rewrite_api_version")]
    pub api_version: String,

    /// Reference to the `InferencePool` to filter rewrites by.
    pub pool_ref: PoolRefConfig,
}

/// Reference to a target `InferencePool` used to filter rewrite rules.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PoolRefConfig {
    /// Name of the pool resource.
    pub name: String,

    /// API group of the pool resource.
    #[serde(default = "default_pool_ref_group")]
    pub group: String,

    /// Kind of the pool resource.
    #[serde(default = "default_pool_ref_kind")]
    pub kind: String,
}

/// Default API version for model rewrite resources.
fn default_rewrite_api_version() -> String {
    DEFAULT_REWRITE_API_VERSION.to_owned()
}

/// Default API group for pool references.
fn default_pool_ref_group() -> String {
    DEFAULT_POOL_REF_GROUP.to_owned()
}

/// Default kind for pool references.
fn default_pool_ref_kind() -> String {
    DEFAULT_POOL_REF_KIND.to_owned()
}

impl ModelRewriteConfig {
    /// Return the effective namespace, reading the service account
    /// namespace file if not explicitly configured.
    pub fn effective_namespace(&self) -> String {
        if let Some(ref ns) = self.namespace {
            return ns.clone();
        }
        std::fs::read_to_string(super::kubernetes::sa_namespace_path())
            .ok()
            .map_or_else(|| "default".to_owned(), |s| s.trim().to_owned())
    }
}

// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

/// Validate model rewrite configuration.
pub(super) fn validate_model_rewrite_config(cfg: &ModelRewriteConfig) -> Result<(), FilterError> {
    if cfg.pool_ref.name.trim().is_empty() {
        return Err("llmd_endpoint_picker: model_rewrite.pool_ref.name must not be empty".into());
    }
    validate_rewrite_api_version(&cfg.api_version)
}

/// Validate the API version format (must be exactly `group/version`).
fn validate_rewrite_api_version(api_version: &str) -> Result<(), FilterError> {
    match api_version.split_once('/') {
        None => Err("llmd_endpoint_picker: model_rewrite.api_version must contain group/version".into()),
        Some((group, version)) => {
            if group.is_empty() || version.is_empty() || version.contains('/') {
                return Err("llmd_endpoint_picker: model_rewrite.api_version must be exactly group/version".into());
            }
            Ok(())
        },
    }
}

// -----------------------------------------------------------------------------
// K8s Types - InferenceModelRewrite List
// -----------------------------------------------------------------------------

/// Top-level list response for `InferenceModelRewrite` resources.
#[derive(Debug, Deserialize)]
pub(super) struct RewriteListResponse {
    /// Items in the list.
    pub items: Vec<RewriteItem>,
}

/// A single `InferenceModelRewrite` resource.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RewriteItem {
    /// Resource metadata.
    pub metadata: RewriteMetadata,
    /// Resource spec.
    pub spec: RewriteSpec,
}

/// Metadata for an `InferenceModelRewrite` resource.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RewriteMetadata {
    /// Resource name used to trace rewrite actions back to their source.
    #[serde(default)]
    pub name: Option<String>,
    /// Creation timestamp for precedence ordering.
    #[serde(default)]
    pub creation_timestamp: Option<String>,
}

/// Spec for an `InferenceModelRewrite` resource.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RewriteSpec {
    /// Reference to the target `InferencePool`.
    #[serde(default)]
    pub pool_ref: Option<RewritePoolRef>,
    /// Rewrite rules.
    #[serde(default)]
    pub rules: Vec<RewriteRule>,
}

/// Pool reference within a rewrite resource.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RewritePoolRef {
    /// API group of the pool resource.
    #[serde(default)]
    pub group: Option<String>,
    /// Kind of the pool resource.
    #[serde(default)]
    pub kind: Option<String>,
    /// Name of the pool resource.
    pub name: String,
}

/// A single rewrite rule containing matches and targets.
#[derive(Debug, Deserialize)]
pub(super) struct RewriteRule {
    /// Match conditions for this rule.
    #[serde(default)]
    pub matches: Vec<RuleMatch>,
    /// Target models for rewrites.
    #[serde(default)]
    pub targets: Vec<TargetModel>,
}

/// A match condition within a rewrite rule.
#[derive(Debug, Deserialize)]
pub(super) struct RuleMatch {
    /// Model match specification.
    #[serde(default)]
    pub model: Option<ModelMatch>,
}

/// Model match specification.
#[derive(Debug, Deserialize)]
pub(super) struct ModelMatch {
    /// Match type (e.g. `"Exact"`). Only `"Exact"` (or absent) is
    /// supported; other values cause the match to be skipped.
    #[serde(default)]
    pub r#type: Option<String>,
    /// Model name to match.
    pub value: String,
}

/// A target model with an optional weight.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TargetModel {
    /// Selection weight (0 means equal distribution).
    #[serde(default)]
    pub weight: i32,
    /// Model name to rewrite to.
    pub model_rewrite: String,
}

// -----------------------------------------------------------------------------
// Rewrite Snapshot
// -----------------------------------------------------------------------------

/// Immutable snapshot mapping incoming model names to rewrite actions.
#[derive(Debug)]
pub(super) struct ModelRewriteSnapshot {
    /// Exact model name to rewrite action mapping.
    rules: HashMap<String, RewriteAction>,
    /// Fallback action for unmatched models.
    generic: Option<RewriteAction>,
}

/// A rewrite action containing weighted targets.
#[derive(Debug, Clone)]
struct RewriteAction {
    /// Weighted target models.
    targets: Vec<WeightedTarget>,
    /// Name of the `InferenceModelRewrite` resource that produced this action.
    source_name: Option<String>,
}

/// A single weighted target model.
#[derive(Debug, Clone)]
struct WeightedTarget {
    /// Target model name.
    model: String,
    /// Selection weight.
    weight: u32,
}

/// Result of a model rewrite lookup.
#[derive(Debug)]
pub(super) struct LookupResult<'a> {
    /// The target model name to rewrite to.
    pub target: &'a str,
    /// The name of the `InferenceModelRewrite` resource that matched.
    pub source_name: Option<&'a str>,
}

impl ModelRewriteSnapshot {
    /// Look up a rewrite for the given model name.
    ///
    /// Uses a random roll for weighted target selection so that
    /// requests for the same model distribute across targets.
    pub fn lookup(&self, model: &str) -> Option<LookupResult<'_>> {
        let action = self.rules.get(model).or(self.generic.as_ref())?;
        let roll = rand::random::<u64>();
        Some(LookupResult {
            target: select_target_with_roll(action, roll),
            source_name: action.source_name.as_deref(),
        })
    }

    /// Return `true` if there are no rewrite rules.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.generic.is_none()
    }
}

// -----------------------------------------------------------------------------
// Snapshot Building
// -----------------------------------------------------------------------------

/// Build a [`ModelRewriteSnapshot`] from a parsed list response.
///
/// Filters items by pool reference, sorts by creation timestamp
/// (oldest first), and applies first-writer-wins semantics.
pub(super) fn build_snapshot(items: &[RewriteItem], cfg: &ModelRewriteConfig) -> ModelRewriteSnapshot {
    let mut sorted: Vec<&RewriteItem> = items.iter().filter(|item| pool_ref_matches(item, cfg)).collect();
    sorted.sort_by(|a, b| cmp_creation_timestamp(a, b));

    let mut rules: HashMap<String, RewriteAction> = HashMap::new();
    let mut generic: Option<RewriteAction> = None;

    for item in sorted {
        apply_item_rules(item, &mut rules, &mut generic);
    }

    ModelRewriteSnapshot { rules, generic }
}

/// Check whether an item's pool reference matches the config.
fn pool_ref_matches(item: &RewriteItem, cfg: &ModelRewriteConfig) -> bool {
    let Some(ref pr) = item.spec.pool_ref else {
        return false;
    };
    if pr.name != cfg.pool_ref.name {
        return false;
    }
    let group = pr.group.as_deref().unwrap_or(DEFAULT_POOL_REF_GROUP);
    let kind = pr.kind.as_deref().unwrap_or(DEFAULT_POOL_REF_KIND);
    group == cfg.pool_ref.group && kind == cfg.pool_ref.kind
}

/// Compare two items by creation timestamp (oldest first).
fn cmp_creation_timestamp(a: &RewriteItem, b: &RewriteItem) -> std::cmp::Ordering {
    let ts_a = a.metadata.creation_timestamp.as_deref().unwrap_or("");
    let ts_b = b.metadata.creation_timestamp.as_deref().unwrap_or("");
    ts_a.cmp(ts_b)
}

/// Apply rules from one item into the snapshot maps.
fn apply_item_rules(
    item: &RewriteItem,
    rules: &mut HashMap<String, RewriteAction>,
    generic: &mut Option<RewriteAction>,
) {
    let source_name = item.metadata.name.clone();
    for rule in &item.spec.rules {
        if has_negative_weight(&rule.targets) {
            continue;
        }
        let action = build_action(&rule.targets, source_name.clone());
        if action.targets.is_empty() {
            continue;
        }
        let supported = collect_supported_matches(&rule.matches);
        if rule.matches.is_empty() {
            if generic.is_none() {
                *generic = Some(action);
            }
            continue;
        }
        if supported.is_empty() {
            continue;
        }
        insert_match_rules(&supported, &action, rules);
    }
}

/// Return `true` if any target in the list has a negative weight.
fn has_negative_weight(targets: &[TargetModel]) -> bool {
    targets.iter().any(|t| t.weight < 0)
}

/// Collect only matches with supported type (Exact or missing).
///
/// Matches with an explicit type that is not `"Exact"` are skipped.
fn collect_supported_matches(matches: &[RuleMatch]) -> Vec<&RuleMatch> {
    matches.iter().filter(|m| is_supported_match(m)).collect()
}

/// Return `true` if the match type is supported (Exact or absent).
fn is_supported_match(m: &RuleMatch) -> bool {
    let Some(ref model) = m.model else {
        return true;
    };
    match model.r#type.as_deref() {
        None | Some("Exact") => true,
        Some(_) => false,
    }
}

/// Insert exact-match rules into the map (first writer wins).
fn insert_match_rules(matches: &[&RuleMatch], action: &RewriteAction, rules: &mut HashMap<String, RewriteAction>) {
    for m in matches {
        let Some(ref model) = m.model else {
            continue;
        };
        if model.value.is_empty() {
            continue;
        }
        rules.entry(model.value.clone()).or_insert_with(|| action.clone());
    }
}

/// Build a [`RewriteAction`] from target models.
///
/// Targets with an empty `modelRewrite` are dropped.
fn build_action(targets: &[TargetModel], source_name: Option<String>) -> RewriteAction {
    let weighted: Vec<WeightedTarget> = targets
        .iter()
        .filter(|t| !t.model_rewrite.is_empty())
        .map(|t| WeightedTarget {
            model: t.model_rewrite.clone(),
            weight: t.weight.max(0).unsigned_abs(),
        })
        .collect();
    RewriteAction {
        targets: weighted,
        source_name,
    }
}

// -----------------------------------------------------------------------------
// Weighted Selection
// -----------------------------------------------------------------------------

/// Select a target model from a rewrite action using a roll value.
///
/// If all weights are zero, distributes evenly. Otherwise uses
/// cumulative weight selection with `roll % total_weight`.
fn select_target_with_roll(action: &RewriteAction, roll: u64) -> &str {
    if action.targets.is_empty() {
        return "";
    }
    if action.targets.len() == 1 {
        return action.targets.first().map_or("", |t| &t.model);
    }
    let total: u64 = action.targets.iter().map(|t| u64::from(t.weight)).sum();
    if total == 0 {
        return select_even(action, roll);
    }
    select_weighted(action, roll, total)
}

/// Select evenly when all weights are zero.
fn select_even(action: &RewriteAction, roll: u64) -> &str {
    let len = u64::try_from(action.targets.len()).unwrap_or(1);
    let idx = usize::try_from(roll % len).unwrap_or(0);
    action.targets.get(idx).map_or("", |t| &t.model)
}

/// Select using cumulative weight distribution.
fn select_weighted(action: &RewriteAction, roll: u64, total: u64) -> &str {
    let pick = roll % total;
    let mut cumulative: u64 = 0;
    for target in &action.targets {
        cumulative += u64::from(target.weight);
        if pick < cumulative {
            return &target.model;
        }
    }
    action.targets.last().map_or("", |t| &t.model)
}

// -----------------------------------------------------------------------------
// Body Mutation
// -----------------------------------------------------------------------------

/// Mutate the `"model"` field in a JSON request body.
///
/// Returns `None` if the body is not valid JSON or not an object.
pub(super) fn mutate_model_in_body(body: &[u8], new_model: &str) -> Option<Vec<u8>> {
    let mut value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value
        .as_object_mut()?
        .insert("model".to_owned(), serde_json::Value::String(new_model.to_owned()));
    serde_json::to_vec(&value).ok()
}

// -----------------------------------------------------------------------------
// Rewrite Handle
// -----------------------------------------------------------------------------

/// Cloneable handle for reading the latest model rewrite snapshot.
#[derive(Debug, Clone)]
pub(super) struct ModelRewriteHandle {
    /// Atomically swappable snapshot pointer.
    current: Arc<ArcSwap<ModelRewriteSnapshot>>,
}

impl ModelRewriteHandle {
    /// Build a handle from an initial snapshot.
    pub fn new(snapshot: ModelRewriteSnapshot) -> Self {
        Self {
            current: Arc::new(ArcSwap::from_pointee(snapshot)),
        }
    }

    /// Return the latest model rewrite snapshot.
    pub fn snapshot(&self) -> Arc<ModelRewriteSnapshot> {
        self.current.load_full()
    }

    /// Publish a new model rewrite snapshot.
    pub fn update(&self, snapshot: ModelRewriteSnapshot) {
        self.current.store(Arc::new(snapshot));
    }
}

/// Build an empty snapshot with no rewrite rules.
pub(super) fn empty_snapshot() -> ModelRewriteSnapshot {
    ModelRewriteSnapshot {
        rules: HashMap::new(),
        generic: None,
    }
}

/// Parse a raw JSON string into a [`RewriteListResponse`].
pub(super) fn parse_rewrite_list(json: &str) -> Option<RewriteListResponse> {
    serde_json::from_str(json).ok()
}

/// Build the Kubernetes API path for listing `InferenceModelRewrite`
/// resources.
pub(super) fn rewrite_list_path(cfg: &ModelRewriteConfig) -> Option<String> {
    let (group, version) = cfg.api_version.split_once('/')?;
    let namespace = cfg.effective_namespace();
    Some(format!(
        "/apis/{group}/{version}/namespaces/{namespace}/inferencemodelrewrites"
    ))
}

/// Run one model rewrite discovery cycle.
pub(super) fn refresh_rewrites(
    client: &super::kubernetes::KubeClient,
    cfg: &ModelRewriteConfig,
    handle: &ModelRewriteHandle,
) {
    let Some(path) = rewrite_list_path(cfg) else {
        return;
    };
    let Some(json) = client.get(&path) else {
        tracing::warn!("failed to list InferenceModelRewrite resources");
        return;
    };
    let Some(list) = parse_rewrite_list(&json) else {
        tracing::warn!("failed to parse InferenceModelRewrite list response");
        return;
    };
    let snapshot = build_snapshot(&list.items, cfg);
    debug!(
        exact_rules = snapshot.rules.len(),
        has_generic = snapshot.generic.is_some(),
        "model rewrite snapshot updated"
    );
    handle.update(snapshot);
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

    // -- Config validation --

    #[test]
    fn validate_accepts_valid_config() {
        let cfg = make_config();
        assert!(
            validate_model_rewrite_config(&cfg).is_ok(),
            "valid config should be accepted"
        );
    }

    #[test]
    fn validate_rejects_empty_pool_ref_name() {
        let mut cfg = make_config();
        cfg.pool_ref.name = String::new();
        assert!(
            validate_model_rewrite_config(&cfg).is_err(),
            "empty pool_ref.name should be rejected"
        );
    }

    #[test]
    fn validate_rejects_api_version_without_slash() {
        let mut cfg = make_config();
        cfg.api_version = "v1alpha2".to_owned();
        assert!(
            validate_model_rewrite_config(&cfg).is_err(),
            "api_version without slash should be rejected"
        );
    }

    #[test]
    fn validate_rejects_api_version_empty_group() {
        let mut cfg = make_config();
        cfg.api_version = "/v1alpha2".to_owned();
        assert!(
            validate_model_rewrite_config(&cfg).is_err(),
            "api_version with empty group should be rejected"
        );
    }

    #[test]
    fn validate_rejects_api_version_extra_slash() {
        let mut cfg = make_config();
        cfg.api_version = "llm-d.ai/v1/extra".to_owned();
        assert!(
            validate_model_rewrite_config(&cfg).is_err(),
            "api_version with extra slash should be rejected"
        );
    }

    // -- Snapshot building --

    #[test]
    fn build_snapshot_from_single_exact_match() {
        let items = vec![make_item(
            "rewrite-1",
            "2024-01-01T00:00:00Z",
            "my-pool",
            vec![make_rule(
                vec![make_exact_match("gpt-4")],
                vec![make_target("gpt-4-turbo", 1)],
            )],
        )];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        assert_eq!(snapshot.rules.len(), 1, "one exact rule");
        assert!(snapshot.generic.is_none(), "no generic rule");
        let result = snapshot.lookup("gpt-4").unwrap();
        assert_eq!(result.target, "gpt-4-turbo", "should rewrite gpt-4 to gpt-4-turbo");
    }

    #[test]
    fn build_snapshot_generic_fallback() {
        let items = vec![make_item(
            "rewrite-1",
            "2024-01-01T00:00:00Z",
            "my-pool",
            vec![make_rule(vec![], vec![make_target("default-model", 1)])],
        )];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        assert!(snapshot.rules.is_empty(), "no exact rules");
        assert!(snapshot.generic.is_some(), "should have generic fallback");
        let result = snapshot.lookup("any-model").unwrap();
        assert_eq!(result.target, "default-model", "generic should rewrite any model");
    }

    #[test]
    fn build_snapshot_first_writer_wins() {
        let items = vec![
            make_item(
                "rewrite-old",
                "2024-01-01T00:00:00Z",
                "my-pool",
                vec![make_rule(
                    vec![make_exact_match("gpt-4")],
                    vec![make_target("old-target", 1)],
                )],
            ),
            make_item(
                "rewrite-new",
                "2024-06-01T00:00:00Z",
                "my-pool",
                vec![make_rule(
                    vec![make_exact_match("gpt-4")],
                    vec![make_target("new-target", 1)],
                )],
            ),
        ];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        let result = snapshot.lookup("gpt-4").unwrap();
        assert_eq!(result.target, "old-target", "oldest item should win");
    }

    #[test]
    fn build_snapshot_filters_by_pool_ref() {
        let items = vec![make_item(
            "rewrite-1",
            "2024-01-01T00:00:00Z",
            "other-pool",
            vec![make_rule(
                vec![make_exact_match("gpt-4")],
                vec![make_target("other-target", 1)],
            )],
        )];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        assert!(snapshot.is_empty(), "items for other pools should be filtered out");
    }

    #[test]
    fn build_snapshot_empty_items() {
        let snapshot = build_snapshot(&[], &make_config());

        assert!(snapshot.is_empty(), "empty items should produce empty snapshot");
        assert!(snapshot.lookup("any").is_none(), "no rewrites available");
    }

    // -- Weighted selection --

    #[test]
    fn select_target_single_target() {
        let action = make_weighted_action(&[("only-target", 1)]);

        assert_eq!(select_target_with_roll(&action, 0), "only-target", "single target");
        assert_eq!(select_target_with_roll(&action, 999), "only-target", "any roll");
    }

    #[test]
    fn select_target_weighted_80_20_boundary() {
        let action = make_weighted_action(&[("a", 80), ("b", 20)]);

        assert_eq!(select_target_with_roll(&action, 0), "a", "roll 0 selects first");
        assert_eq!(select_target_with_roll(&action, 79), "a", "roll 79 selects first");
        assert_eq!(select_target_with_roll(&action, 80), "b", "roll 80 selects second");
        assert_eq!(select_target_with_roll(&action, 99), "b", "roll 99 selects second");
    }

    #[test]
    fn select_target_all_zero_weights() {
        let action = make_weighted_action(&[("a", 0), ("b", 0)]);

        assert_eq!(select_target_with_roll(&action, 0), "a", "even distribution: roll 0");
        assert_eq!(select_target_with_roll(&action, 1), "b", "even distribution: roll 1");
    }

    #[test]
    fn select_target_empty_targets() {
        let action = make_weighted_action(&[]);

        assert_eq!(select_target_with_roll(&action, 0), "", "empty targets returns empty");
    }

    // -- Body mutation --

    #[test]
    fn mutate_model_replaces_existing() {
        let body = br#"{"model":"old-model","messages":[]}"#;

        let result = mutate_model_in_body(body, "new-model").unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&result).unwrap();

        assert_eq!(
            parsed["model"].as_str(),
            Some("new-model"),
            "model field should be replaced"
        );
        assert!(parsed["messages"].is_array(), "other fields should be preserved");
    }

    #[test]
    fn mutate_model_adds_if_missing() {
        let body = br#"{"messages":[]}"#;

        let result = mutate_model_in_body(body, "new-model").unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&result).unwrap();

        assert_eq!(
            parsed["model"].as_str(),
            Some("new-model"),
            "model field should be added"
        );
    }

    #[test]
    fn mutate_model_invalid_json_returns_none() {
        assert!(
            mutate_model_in_body(b"not json", "model").is_none(),
            "invalid JSON should return None"
        );
    }

    #[test]
    fn mutate_model_non_object_returns_none() {
        assert!(
            mutate_model_in_body(b"[1,2,3]", "model").is_none(),
            "JSON array should return None"
        );
    }

    // -- Handle --

    #[test]
    fn handle_publishes_and_reads_snapshot() {
        let handle = ModelRewriteHandle::new(empty_snapshot());

        assert!(handle.snapshot().is_empty(), "initial snapshot should be empty");

        let items = vec![make_item(
            "r1",
            "2024-01-01T00:00:00Z",
            "my-pool",
            vec![make_rule(
                vec![make_exact_match("gpt-4")],
                vec![make_target("gpt-4-turbo", 1)],
            )],
        )];
        let new_snapshot = build_snapshot(&items, &make_config());
        handle.update(new_snapshot);

        let snap = handle.snapshot();
        assert!(!snap.is_empty(), "updated snapshot should not be empty");
        let result = snap.lookup("gpt-4").unwrap();
        assert_eq!(result.target, "gpt-4-turbo", "should find rewrite after update");
    }

    // -- Rewrite list path --

    #[test]
    fn rewrite_list_path_generates_correct_url() {
        let mut cfg = make_config();
        cfg.namespace = Some("test-ns".to_owned());

        let path = rewrite_list_path(&cfg).unwrap();

        assert_eq!(
            path, "/apis/llm-d.ai/v1alpha2/namespaces/test-ns/inferencemodelrewrites",
            "should generate correct K8s API path"
        );
    }

    // -- Parse rewrite list --

    #[test]
    fn parse_rewrite_list_valid_json() {
        let json = r#"{
            "items": [{
                "metadata": { "name": "r1", "creationTimestamp": "2024-01-01T00:00:00Z" },
                "spec": {
                    "poolRef": { "name": "my-pool" },
                    "rules": [{
                        "matches": [{ "model": { "value": "gpt-4" } }],
                        "targets": [{ "weight": 1, "modelRewrite": "gpt-4-turbo" }]
                    }]
                }
            }]
        }"#;

        let list = parse_rewrite_list(json).unwrap();

        assert_eq!(list.items.len(), 1, "one item");
        assert_eq!(list.items[0].spec.rules.len(), 1, "one rule");
    }

    #[test]
    fn parse_rewrite_list_invalid_json() {
        assert!(
            parse_rewrite_list("not json").is_none(),
            "invalid JSON should return None"
        );
    }

    // -- Config deserialization --

    #[test]
    fn config_deserializes_with_defaults() {
        let cfg: ModelRewriteConfig = serde_yaml::from_str("enabled: true\npool_ref:\n  name: my-pool").unwrap();

        assert!(cfg.enabled, "enabled should be true");
        assert_eq!(cfg.api_version, "llm-d.ai/v1alpha2", "should use default api_version");
        assert_eq!(
            cfg.pool_ref.group, "inference.networking.k8s.io",
            "should use default group"
        );
        assert_eq!(cfg.pool_ref.kind, "InferencePool", "should use default kind");
    }

    #[test]
    fn config_rejects_unknown_fields() {
        let result = serde_yaml::from_str::<ModelRewriteConfig>("enabled: true\npool_ref:\n  name: p\nunknown: 42");
        assert!(result.is_err(), "unknown fields should be rejected");
    }

    // -- Weighted selection: distribution across requests --

    #[test]
    fn different_rolls_select_different_targets_equal_weights() {
        let action = make_weighted_action(&[("a", 1), ("b", 1)]);

        let result_0 = select_target_with_roll(&action, 0);
        let result_1 = select_target_with_roll(&action, 1);
        assert_ne!(result_0, result_1, "different rolls should select different targets");
    }

    #[test]
    fn zero_weight_targets_excluded_when_positive_exist() {
        let action = make_weighted_action(&[("skip", 0), ("pick", 1)]);

        for roll in 0..10 {
            assert_eq!(
                select_target_with_roll(&action, roll),
                "pick",
                "zero-weight target should never be selected"
            );
        }
    }

    // -- Unsupported match types --

    #[test]
    fn unsupported_match_type_regex_is_ignored() {
        let items = vec![make_item(
            "rewrite-regex",
            "2024-01-01T00:00:00Z",
            "my-pool",
            vec![make_rule(
                vec![RuleMatch {
                    model: Some(ModelMatch {
                        r#type: Some("Regex".to_owned()),
                        value: "gpt-.*".to_owned(),
                    }),
                }],
                vec![make_target("target-model", 1)],
            )],
        )];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        assert!(snapshot.is_empty(), "regex match should be ignored");
        assert!(snapshot.lookup("gpt-4").is_none(), "regex should not match");
    }

    #[test]
    fn missing_match_type_treated_as_exact() {
        let items = vec![make_item(
            "rewrite-no-type",
            "2024-01-01T00:00:00Z",
            "my-pool",
            vec![make_rule(
                vec![RuleMatch {
                    model: Some(ModelMatch {
                        r#type: None,
                        value: "gpt-4".to_owned(),
                    }),
                }],
                vec![make_target("target-model", 1)],
            )],
        )];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        let result = snapshot.lookup("gpt-4").unwrap();
        assert_eq!(result.target, "target-model", "absent type should be Exact");
    }

    #[test]
    fn all_unsupported_matches_skips_rule() {
        let items = vec![make_item(
            "rewrite-only-regex",
            "2024-01-01T00:00:00Z",
            "my-pool",
            vec![make_rule(
                vec![
                    RuleMatch {
                        model: Some(ModelMatch {
                            r#type: Some("Regex".to_owned()),
                            value: "gpt-.*".to_owned(),
                        }),
                    },
                    RuleMatch {
                        model: Some(ModelMatch {
                            r#type: Some("Prefix".to_owned()),
                            value: "gpt-".to_owned(),
                        }),
                    },
                ],
                vec![make_target("target", 1)],
            )],
        )];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        assert!(snapshot.is_empty(), "all-unsupported matches should skip rule");
    }

    // -- Invalid target weights --

    #[test]
    fn negative_weight_drops_entire_rule() {
        let items = vec![make_item(
            "rewrite-neg",
            "2024-01-01T00:00:00Z",
            "my-pool",
            vec![make_rule(
                vec![make_exact_match("gpt-4")],
                vec![make_target("good-target", 1), make_target("bad-target", -1)],
            )],
        )];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        assert!(snapshot.is_empty(), "rule with negative weight should be dropped");
    }

    #[test]
    fn empty_model_rewrite_target_dropped() {
        let items = vec![make_item(
            "rewrite-empty-mr",
            "2024-01-01T00:00:00Z",
            "my-pool",
            vec![make_rule(
                vec![make_exact_match("gpt-4")],
                vec![
                    TargetModel {
                        weight: 1,
                        model_rewrite: String::new(),
                    },
                    make_target("valid-target", 1),
                ],
            )],
        )];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        let result = snapshot.lookup("gpt-4").unwrap();
        assert_eq!(result.target, "valid-target", "empty modelRewrite should be dropped");
    }

    #[test]
    fn all_empty_model_rewrite_drops_rule() {
        let items = vec![make_item(
            "rewrite-all-empty",
            "2024-01-01T00:00:00Z",
            "my-pool",
            vec![make_rule(
                vec![make_exact_match("gpt-4")],
                vec![TargetModel {
                    weight: 1,
                    model_rewrite: String::new(),
                }],
            )],
        )];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        assert!(snapshot.is_empty(), "all-empty modelRewrite should drop rule");
    }

    #[test]
    fn all_zero_weights_still_works() {
        let items = vec![make_item(
            "rewrite-zeros",
            "2024-01-01T00:00:00Z",
            "my-pool",
            vec![make_rule(
                vec![make_exact_match("gpt-4")],
                vec![make_target("a", 0), make_target("b", 0)],
            )],
        )];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        assert!(
            snapshot.lookup("gpt-4").is_some(),
            "all-zero weights should still match"
        );
    }

    // -- Rewrite source metadata --

    #[test]
    fn lookup_carries_source_name() {
        let items = vec![make_item(
            "my-rewrite-rule",
            "2024-01-01T00:00:00Z",
            "my-pool",
            vec![make_rule(
                vec![make_exact_match("gpt-4")],
                vec![make_target("gpt-4-turbo", 1)],
            )],
        )];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        let result = snapshot.lookup("gpt-4").unwrap();
        assert_eq!(
            result.source_name,
            Some("my-rewrite-rule"),
            "source name should be carried"
        );
    }

    #[test]
    fn no_match_returns_none() {
        let items = vec![make_item(
            "rewrite-1",
            "2024-01-01T00:00:00Z",
            "my-pool",
            vec![make_rule(
                vec![make_exact_match("gpt-4")],
                vec![make_target("gpt-4-turbo", 1)],
            )],
        )];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        assert!(snapshot.lookup("gpt-3").is_none(), "unmatched model returns None");
    }

    // -- Test Utilities --

    fn make_config() -> ModelRewriteConfig {
        ModelRewriteConfig {
            enabled: true,
            namespace: Some("default".to_owned()),
            api_version: "llm-d.ai/v1alpha2".to_owned(),
            pool_ref: PoolRefConfig {
                name: "my-pool".to_owned(),
                group: "inference.networking.k8s.io".to_owned(),
                kind: "InferencePool".to_owned(),
            },
        }
    }

    fn make_item(name: &str, timestamp: &str, pool_name: &str, rules: Vec<RewriteRule>) -> RewriteItem {
        RewriteItem {
            metadata: RewriteMetadata {
                name: Some(name.to_owned()),
                creation_timestamp: Some(timestamp.to_owned()),
            },
            spec: RewriteSpec {
                pool_ref: Some(RewritePoolRef {
                    group: Some("inference.networking.k8s.io".to_owned()),
                    kind: Some("InferencePool".to_owned()),
                    name: pool_name.to_owned(),
                }),
                rules,
            },
        }
    }

    fn make_rule(matches: Vec<RuleMatch>, targets: Vec<TargetModel>) -> RewriteRule {
        RewriteRule { matches, targets }
    }

    fn make_exact_match(value: &str) -> RuleMatch {
        RuleMatch {
            model: Some(ModelMatch {
                r#type: Some("Exact".to_owned()),
                value: value.to_owned(),
            }),
        }
    }

    fn make_target(model: &str, weight: i32) -> TargetModel {
        TargetModel {
            weight,
            model_rewrite: model.to_owned(),
        }
    }

    fn make_weighted_action(entries: &[(&str, u32)]) -> RewriteAction {
        RewriteAction {
            targets: entries
                .iter()
                .map(|(m, w)| WeightedTarget {
                    model: (*m).to_owned(),
                    weight: *w,
                })
                .collect(),
            source_name: None,
        }
    }
}
