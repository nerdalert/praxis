// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! `InferenceObjective` priority metadata for the llm-d endpoint picker.
//!
//! Watches `InferenceObjective` resources in Kubernetes and builds a
//! snapshot mapping objective names to priority values. At request time,
//! the filter reads the objective header, looks up its priority, and
//! sets metadata fields for downstream observability.

use std::{collections::HashMap, sync::Arc};

use arc_swap::ArcSwap;
use serde::Deserialize;
use tracing::debug;

use super::model_rewrite::PoolRefConfig;
use crate::FilterError;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Default API version for `InferenceObjective` resources.
const DEFAULT_OBJECTIVE_API_VERSION: &str = "llm-d.ai/v1alpha2";

/// Current header name for the inference objective.
const OBJECTIVE_HEADER: &str = "x-llm-d-inference-objective";

/// Deprecated header name for the inference objective.
const OBJECTIVE_HEADER_DEPRECATED: &str = "x-gateway-inference-objective";

// -----------------------------------------------------------------------------
// Config
// -----------------------------------------------------------------------------

/// Configuration for `InferenceObjective` priority support.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InferenceObjectiveConfig {
    /// Whether objective priority lookup is active.
    pub enabled: bool,

    /// Namespace to list `InferenceObjective` resources from.
    #[serde(default)]
    pub namespace: Option<String>,

    /// API version for the `InferenceObjective` CRD.
    #[serde(default = "default_objective_api_version")]
    pub api_version: String,

    /// Reference to the `InferencePool` to filter objectives by.
    pub pool_ref: PoolRefConfig,
}

/// Default API version for objective resources.
fn default_objective_api_version() -> String {
    DEFAULT_OBJECTIVE_API_VERSION.to_owned()
}

impl InferenceObjectiveConfig {
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

/// Validate inference objective configuration.
pub(super) fn validate_objective_config(cfg: &InferenceObjectiveConfig) -> Result<(), FilterError> {
    if cfg.pool_ref.name.trim().is_empty() {
        return Err("llmd_endpoint_picker: inference_objective.pool_ref.name must not be empty".into());
    }
    validate_objective_api_version(&cfg.api_version)
}

/// Validate the API version format (must be exactly `group/version`).
fn validate_objective_api_version(api_version: &str) -> Result<(), FilterError> {
    match api_version.split_once('/') {
        None => Err("llmd_endpoint_picker: inference_objective.api_version must contain group/version".into()),
        Some((group, version)) => {
            if group.is_empty() || version.is_empty() || version.contains('/') {
                return Err(
                    "llmd_endpoint_picker: inference_objective.api_version must be exactly group/version".into(),
                );
            }
            Ok(())
        },
    }
}

// -----------------------------------------------------------------------------
// K8s Types - InferenceObjective List
// -----------------------------------------------------------------------------

/// Top-level list response for `InferenceObjective` resources.
#[derive(Debug, Deserialize)]
pub(super) struct ObjectiveListResponse {
    /// Items in the list.
    pub items: Vec<ObjectiveItem>,
}

/// A single `InferenceObjective` resource.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ObjectiveItem {
    /// Resource metadata.
    pub metadata: ObjectiveMetadata,
    /// Resource spec.
    pub spec: ObjectiveSpec,
}

/// Metadata for an `InferenceObjective` resource.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ObjectiveMetadata {
    /// Resource name used to trace actions back to their source.
    #[serde(default)]
    pub name: Option<String>,
    /// Creation timestamp for precedence ordering.
    #[serde(default)]
    pub creation_timestamp: Option<String>,
}

/// Spec for an `InferenceObjective` resource.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ObjectiveSpec {
    /// Reference to the target `InferencePool`.
    #[serde(default)]
    pub pool_ref: Option<ObjectivePoolRef>,
    /// Priority value for this objective.
    #[serde(default)]
    pub priority: Option<i32>,
}

/// Pool reference within an objective resource.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ObjectivePoolRef {
    /// API group of the pool resource.
    #[serde(default)]
    pub group: Option<String>,
    /// Kind of the pool resource.
    #[serde(default)]
    pub kind: Option<String>,
    /// Name of the pool resource.
    pub name: String,
}

// -----------------------------------------------------------------------------
// Objective Snapshot
// -----------------------------------------------------------------------------

/// Default pool reference API group.
const DEFAULT_POOL_REF_GROUP: &str = "inference.networking.k8s.io";
/// Default pool reference kind.
const DEFAULT_POOL_REF_KIND: &str = "InferencePool";

/// Immutable snapshot mapping objective names to priority entries.
#[derive(Debug)]
pub(super) struct ObjectiveSnapshot {
    /// Objective name to priority entry mapping.
    objectives: HashMap<String, ObjectiveEntry>,
}

/// A resolved objective entry with priority and source.
#[derive(Debug, Clone)]
struct ObjectiveEntry {
    /// Priority value for this objective.
    priority: i32,
    /// Name of the K8s resource that defined this entry.
    source_name: String,
}

/// Result of an objective priority lookup.
#[derive(Debug)]
pub(super) struct ObjectiveLookup<'a> {
    /// Resolved priority value.
    pub priority: i32,
    /// Name of the source K8s resource.
    pub source_name: &'a str,
}

impl ObjectiveSnapshot {
    /// Look up an objective by name and return its priority.
    pub fn lookup(&self, name: &str) -> Option<ObjectiveLookup<'_>> {
        self.objectives.get(name).map(|entry| ObjectiveLookup {
            priority: entry.priority,
            source_name: &entry.source_name,
        })
    }

    /// Return `true` if there are no objectives.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.objectives.is_empty()
    }
}

// -----------------------------------------------------------------------------
// Snapshot Building
// -----------------------------------------------------------------------------

/// Build an [`ObjectiveSnapshot`] from a parsed list response.
///
/// Filters items by pool reference, sorts by creation timestamp
/// (oldest first), and applies first-writer-wins semantics.
pub(super) fn build_snapshot(items: &[ObjectiveItem], cfg: &InferenceObjectiveConfig) -> ObjectiveSnapshot {
    let mut sorted: Vec<&ObjectiveItem> = items.iter().filter(|item| pool_ref_matches(item, cfg)).collect();
    sorted.sort_by(|a, b| cmp_creation_timestamp(a, b));

    let mut objectives: HashMap<String, ObjectiveEntry> = HashMap::new();
    for item in sorted {
        insert_objective(item, &mut objectives);
    }

    ObjectiveSnapshot { objectives }
}

/// Insert an objective entry, applying first-writer-wins semantics.
fn insert_objective(item: &ObjectiveItem, objectives: &mut HashMap<String, ObjectiveEntry>) {
    let Some(ref name) = item.metadata.name else {
        return;
    };
    if name.is_empty() {
        return;
    }
    objectives.entry(name.clone()).or_insert_with(|| ObjectiveEntry {
        priority: item.spec.priority.unwrap_or(0),
        source_name: name.clone(),
    });
}

/// Check whether an item's pool reference matches the config.
fn pool_ref_matches(item: &ObjectiveItem, cfg: &InferenceObjectiveConfig) -> bool {
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
fn cmp_creation_timestamp(a: &ObjectiveItem, b: &ObjectiveItem) -> std::cmp::Ordering {
    let ts_a = a.metadata.creation_timestamp.as_deref().unwrap_or("");
    let ts_b = b.metadata.creation_timestamp.as_deref().unwrap_or("");
    ts_a.cmp(ts_b)
}

// -----------------------------------------------------------------------------
// Objective Handle
// -----------------------------------------------------------------------------

/// Cloneable handle for reading the latest objective snapshot.
#[derive(Debug, Clone)]
pub(super) struct ObjectiveHandle {
    /// Atomically swappable snapshot pointer.
    current: Arc<ArcSwap<ObjectiveSnapshot>>,
}

impl ObjectiveHandle {
    /// Build a handle from an initial snapshot.
    pub fn new(snapshot: ObjectiveSnapshot) -> Self {
        Self {
            current: Arc::new(ArcSwap::from_pointee(snapshot)),
        }
    }

    /// Return the latest objective snapshot.
    pub fn snapshot(&self) -> Arc<ObjectiveSnapshot> {
        self.current.load_full()
    }

    /// Publish a new objective snapshot.
    pub fn update(&self, snapshot: ObjectiveSnapshot) {
        self.current.store(Arc::new(snapshot));
    }
}

/// Build an empty snapshot with no objectives.
pub(super) fn empty_snapshot() -> ObjectiveSnapshot {
    ObjectiveSnapshot {
        objectives: HashMap::new(),
    }
}

// -----------------------------------------------------------------------------
// K8s API Helpers
// -----------------------------------------------------------------------------

/// Parse a raw JSON string into an [`ObjectiveListResponse`].
pub(super) fn parse_objective_list(json: &str) -> Option<ObjectiveListResponse> {
    serde_json::from_str(json).ok()
}

/// Build the Kubernetes API path for listing `InferenceObjective`
/// resources.
pub(super) fn objective_list_path(cfg: &InferenceObjectiveConfig) -> Option<String> {
    let (group, version) = cfg.api_version.split_once('/')?;
    let namespace = cfg.effective_namespace();
    Some(format!(
        "/apis/{group}/{version}/namespaces/{namespace}/inferenceobjectives"
    ))
}

/// Run one objective discovery cycle.
pub(super) fn refresh_objectives(
    client: &super::kubernetes::KubeClient,
    cfg: &InferenceObjectiveConfig,
    handle: &ObjectiveHandle,
) {
    let Some(path) = objective_list_path(cfg) else {
        return;
    };
    let Some(json) = client.get(&path) else {
        tracing::warn!("failed to list InferenceObjective resources");
        return;
    };
    let Some(list) = parse_objective_list(&json) else {
        tracing::warn!("failed to parse InferenceObjective list response");
        return;
    };
    let snapshot = build_snapshot(&list.items, cfg);
    debug!(count = snapshot.objectives.len(), "objective snapshot updated");
    handle.update(snapshot);
}

// -----------------------------------------------------------------------------
// Header Extraction
// -----------------------------------------------------------------------------

/// Extract the objective name from request headers.
///
/// Checks the current header first, then falls back to the deprecated
/// header. Returns `None` if neither is present.
pub(super) fn extract_objective_header(headers: &http::HeaderMap) -> Option<&str> {
    if let Some(val) = headers.get(OBJECTIVE_HEADER) {
        return val.to_str().ok();
    }
    if let Some(val) = headers.get(OBJECTIVE_HEADER_DEPRECATED) {
        return val.to_str().ok();
    }
    None
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
            validate_objective_config(&cfg).is_ok(),
            "valid config should be accepted"
        );
    }

    #[test]
    fn validate_rejects_empty_pool_ref_name() {
        let mut cfg = make_config();
        cfg.pool_ref.name = String::new();
        assert!(
            validate_objective_config(&cfg).is_err(),
            "empty pool_ref.name should be rejected"
        );
    }

    #[test]
    fn validate_rejects_api_version_without_slash() {
        let mut cfg = make_config();
        cfg.api_version = "v1alpha2".to_owned();
        assert!(
            validate_objective_config(&cfg).is_err(),
            "api_version without slash should be rejected"
        );
    }

    #[test]
    fn validate_rejects_api_version_empty_group() {
        let mut cfg = make_config();
        cfg.api_version = "/v1alpha2".to_owned();
        assert!(
            validate_objective_config(&cfg).is_err(),
            "api_version with empty group should be rejected"
        );
    }

    #[test]
    fn validate_rejects_api_version_extra_slash() {
        let mut cfg = make_config();
        cfg.api_version = "llm-d.ai/v1/extra".to_owned();
        assert!(
            validate_objective_config(&cfg).is_err(),
            "api_version with extra slash should be rejected"
        );
    }

    // -- Parse objective list JSON --

    #[test]
    fn parse_objective_list_valid_json() {
        let json = r#"{
            "items": [{
                "metadata": { "name": "obj-1", "creationTimestamp": "2024-01-01T00:00:00Z" },
                "spec": {
                    "poolRef": { "name": "my-pool" },
                    "priority": 10
                }
            }]
        }"#;

        let list = parse_objective_list(json).unwrap();

        assert_eq!(list.items.len(), 1, "one item");
        assert_eq!(list.items[0].spec.priority, Some(10), "priority should be 10");
    }

    #[test]
    fn parse_objective_list_invalid_json() {
        assert!(
            parse_objective_list("not json").is_none(),
            "invalid JSON should return None"
        );
    }

    // -- Snapshot building --

    #[test]
    fn build_snapshot_single_objective() {
        let items = vec![make_item("obj-a", "2024-01-01T00:00:00Z", "my-pool", Some(5))];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        let result = snapshot.lookup("obj-a").unwrap();
        assert_eq!(result.priority, 5, "priority should be 5");
        assert_eq!(result.source_name, "obj-a", "source name");
    }

    #[test]
    fn build_snapshot_filters_by_pool_ref() {
        let items = vec![make_item("obj-other", "2024-01-01T00:00:00Z", "other-pool", Some(5))];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        assert!(snapshot.is_empty(), "items for other pools should be filtered out");
    }

    #[test]
    fn build_snapshot_priority_positive() {
        let items = vec![make_item("obj-pos", "2024-01-01T00:00:00Z", "my-pool", Some(100))];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        assert_eq!(
            snapshot.lookup("obj-pos").unwrap().priority,
            100,
            "positive priority preserved"
        );
    }

    #[test]
    fn build_snapshot_priority_zero() {
        let items = vec![make_item("obj-zero", "2024-01-01T00:00:00Z", "my-pool", Some(0))];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        assert_eq!(
            snapshot.lookup("obj-zero").unwrap().priority,
            0,
            "zero priority preserved"
        );
    }

    #[test]
    fn build_snapshot_priority_negative() {
        let items = vec![make_item("obj-neg", "2024-01-01T00:00:00Z", "my-pool", Some(-5))];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        assert_eq!(
            snapshot.lookup("obj-neg").unwrap().priority,
            -5,
            "negative priority preserved"
        );
    }

    #[test]
    fn build_snapshot_missing_priority_defaults_to_zero() {
        let items = vec![make_item("obj-none", "2024-01-01T00:00:00Z", "my-pool", None)];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        assert_eq!(
            snapshot.lookup("obj-none").unwrap().priority,
            0,
            "missing priority should default to 0"
        );
    }

    #[test]
    fn build_snapshot_duplicate_names_oldest_wins() {
        let items = vec![
            make_item("obj-dup", "2024-01-01T00:00:00Z", "my-pool", Some(10)),
            make_item("obj-dup", "2024-06-01T00:00:00Z", "my-pool", Some(99)),
        ];
        let cfg = make_config();

        let snapshot = build_snapshot(&items, &cfg);

        assert_eq!(
            snapshot.lookup("obj-dup").unwrap().priority,
            10,
            "oldest item should win for duplicate names"
        );
    }

    #[test]
    fn build_snapshot_empty_items() {
        let snapshot = build_snapshot(&[], &make_config());

        assert!(snapshot.is_empty(), "empty items should produce empty snapshot");
        assert!(snapshot.lookup("any").is_none(), "no objectives available");
    }

    // -- Lookup --

    #[test]
    fn lookup_found_returns_priority_and_source() {
        let items = vec![make_item("my-obj", "2024-01-01T00:00:00Z", "my-pool", Some(42))];
        let cfg = make_config();
        let snapshot = build_snapshot(&items, &cfg);

        let result = snapshot.lookup("my-obj").unwrap();

        assert_eq!(result.priority, 42, "priority should be 42");
        assert_eq!(result.source_name, "my-obj", "source name");
    }

    #[test]
    fn lookup_not_found_returns_none() {
        let items = vec![make_item("my-obj", "2024-01-01T00:00:00Z", "my-pool", Some(5))];
        let cfg = make_config();
        let snapshot = build_snapshot(&items, &cfg);

        assert!(
            snapshot.lookup("unknown-obj").is_none(),
            "unknown objective should return None"
        );
    }

    // -- Handle --

    #[test]
    fn handle_publishes_and_reads_snapshot() {
        let handle = ObjectiveHandle::new(empty_snapshot());

        assert!(handle.snapshot().is_empty(), "initial snapshot should be empty");

        let items = vec![make_item("obj-1", "2024-01-01T00:00:00Z", "my-pool", Some(7))];
        let new_snapshot = build_snapshot(&items, &make_config());
        handle.update(new_snapshot);

        let snap = handle.snapshot();
        assert!(!snap.is_empty(), "updated snapshot should not be empty");
        assert_eq!(
            snap.lookup("obj-1").unwrap().priority,
            7,
            "should find objective after update"
        );
    }

    // -- Objective list path --

    #[test]
    fn objective_list_path_generates_correct_url() {
        let mut cfg = make_config();
        cfg.namespace = Some("test-ns".to_owned());

        let path = objective_list_path(&cfg).unwrap();

        assert_eq!(
            path, "/apis/llm-d.ai/v1alpha2/namespaces/test-ns/inferenceobjectives",
            "should generate correct K8s API path"
        );
    }

    // -- Config deserialization --

    #[test]
    fn config_deserializes_with_defaults() {
        let cfg: InferenceObjectiveConfig = serde_yaml::from_str("enabled: true\npool_ref:\n  name: my-pool").unwrap();

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
        let result =
            serde_yaml::from_str::<InferenceObjectiveConfig>("enabled: true\npool_ref:\n  name: p\nunknown: 42");
        assert!(result.is_err(), "unknown fields should be rejected");
    }

    // -- Header extraction --

    #[test]
    fn extract_current_header() {
        let mut headers = http::HeaderMap::new();
        headers.insert(OBJECTIVE_HEADER, "my-objective".parse().unwrap());

        assert_eq!(
            extract_objective_header(&headers),
            Some("my-objective"),
            "current header should be extracted"
        );
    }

    #[test]
    fn extract_deprecated_header_fallback() {
        let mut headers = http::HeaderMap::new();
        headers.insert(OBJECTIVE_HEADER_DEPRECATED, "old-objective".parse().unwrap());

        assert_eq!(
            extract_objective_header(&headers),
            Some("old-objective"),
            "deprecated header should be used as fallback"
        );
    }

    #[test]
    fn current_header_wins_over_deprecated() {
        let mut headers = http::HeaderMap::new();
        headers.insert(OBJECTIVE_HEADER, "current".parse().unwrap());
        headers.insert(OBJECTIVE_HEADER_DEPRECATED, "deprecated".parse().unwrap());

        assert_eq!(
            extract_objective_header(&headers),
            Some("current"),
            "current header should take priority over deprecated"
        );
    }

    #[test]
    fn missing_headers_returns_none() {
        let headers = http::HeaderMap::new();

        assert!(
            extract_objective_header(&headers).is_none(),
            "no headers should return None"
        );
    }

    // -- Test Utilities --

    fn make_config() -> InferenceObjectiveConfig {
        InferenceObjectiveConfig {
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

    fn make_item(name: &str, timestamp: &str, pool_name: &str, priority: Option<i32>) -> ObjectiveItem {
        ObjectiveItem {
            metadata: ObjectiveMetadata {
                name: Some(name.to_owned()),
                creation_timestamp: Some(timestamp.to_owned()),
            },
            spec: ObjectiveSpec {
                pool_ref: Some(ObjectivePoolRef {
                    group: Some("inference.networking.k8s.io".to_owned()),
                    kind: Some("InferencePool".to_owned()),
                    name: pool_name.to_owned(),
                }),
                priority,
            },
        }
    }
}
