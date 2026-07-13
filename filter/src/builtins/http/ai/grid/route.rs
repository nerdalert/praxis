// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Grid route filter: selects an upstream cluster for the request
//! based on the inference model name extracted from a configured
//! request header.
//!
//! Candidate selection is deterministic.  Fresh candidates score 0;
//! stale candidates receive −100.  Candidates on `local_site` receive
//! +10.  First configured candidate wins when scores are equal.
//!
//! No request-time metrics or control-plane lookups are performed.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use super::descriptor::{self, CandidateConfig, CapabilityKind, RouteCandidate};
use crate::{
    FilterAction, FilterError, Rejection,
    factory::parse_filter_config,
    filter::{HttpFilter, HttpFilterContext},
};

/// Maximum length for header values read from the request.
const MAX_HEADER_VALUE_LEN: usize = 256;

/// Score penalty for stale candidates.
const STALE_PENALTY: i32 = 100;

/// Score bonus for candidates on the local site.
const LOCAL_PREFERENCE: i32 = 10;

// -----------------------------------------------------------------------------
// Config
// -----------------------------------------------------------------------------

/// Deserialized YAML config for the grid route filter.
///
/// ```yaml
/// filter: grid_route
/// local_site: site-a
/// model_header: x-model
/// candidates:
///   - kind: inference_model
///     name: local-model
///     site: site-a
///     cluster: local-inference
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GridRouteConfig {
    /// Static list of route candidates.
    candidates: Vec<CandidateConfig>,

    /// Name of the local site.
    local_site: String,

    /// Header name that carries the model name (default: `X-Model`).
    #[serde(default = "default_model_header")]
    model_header: String,
}

/// Default model header name.
fn default_model_header() -> String {
    "X-Model".to_owned()
}

// -----------------------------------------------------------------------------
// GridRouteFilter
// -----------------------------------------------------------------------------

/// Selects an upstream cluster from a static site/capability descriptor
/// by matching the inference model name from a configured request header.
///
/// **Behavior:**
/// - If `ctx.cluster` is already set by an earlier filter, the selection is preserved and no metadata is written.
/// - If the model header is absent, the filter returns `Continue` without routing.
/// - If the header is blank, oversized, or non-UTF-8, the filter rejects with 400.
/// - If a matching candidate is found, `ctx.cluster` is set and bounded route-decision metadata is written.
/// - If no matching candidate is found, the filter rejects with 404.
///
/// **Scoring:** candidates are scored deterministically.  Fresh candidates
/// score 0; stale candidates receive −100.  Candidates on `local_site`
/// receive +10.  Freshness is stronger than local preference.  First
/// configured candidate wins on equal scores.
///
/// **Metadata:** on successful selection, bounded in-process filter
/// metadata is written under the `grid.route.` namespace (`kind`, `name`,
/// `site`, `cluster`, `local_site`).  No HTTP forwarding headers are
/// written.  No request-time database, control-plane, or metrics
/// lookups are performed.
///
/// **Scope:** only `inference_model` candidates are matched in this
/// release.  MCP tool routing and A2A agent routing are separate PRs.
pub struct GridRouteFilter {
    /// Validated route candidates.
    candidates: Vec<RouteCandidate>,
    /// Local site identifier for scoring and metadata.
    local_site: Arc<str>,
    /// Header that carries the model name.
    model_header: http::header::HeaderName,
}

impl GridRouteFilter {
    /// Create a grid route filter from parsed YAML config.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if the candidate list is empty,
    /// any name field is blank, or the model header is invalid.
    pub fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: GridRouteConfig = parse_filter_config("grid_route", config)?;

        descriptor::validate_local_site(&cfg.local_site)?;
        let model_header = descriptor::validate_model_header(&cfg.model_header)?;
        let candidates = descriptor::validate_candidates(cfg.candidates)?;

        Ok(Box::new(Self {
            candidates,
            local_site: Arc::from(cfg.local_site.as_str()),
            model_header,
        }))
    }
}

#[async_trait]
impl HttpFilter for GridRouteFilter {
    fn name(&self) -> &'static str {
        "grid_route"
    }

    fn selects_cluster(&self) -> bool {
        true
    }

    fn selected_clusters(&self) -> Vec<String> {
        self.candidates.iter().map(|c| c.cluster.to_string()).collect()
    }

    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        if ctx.cluster.is_some() {
            tracing::debug!("grid_route: cluster already set; preserving");
            return Ok(FilterAction::Continue);
        }

        let lookup = extract_model_lookup(ctx, &self.model_header);

        let (kind, name) = match lookup {
            Lookup::Route { kind, name } => (kind, name),
            Lookup::Skip => return Ok(FilterAction::Continue),
            Lookup::Invalid => return Ok(FilterAction::Reject(Rejection::status(400))),
        };

        if let Some(c) = select_candidate(&self.candidates, kind, &name, &self.local_site) {
            tracing::debug!(
                kind = kind.as_str(),
                name = %name,
                site = &*c.site,
                cluster = &*c.cluster,
                fresh = c.fresh,
                score = score_candidate(c, &self.local_site),
                "grid_route: selected"
            );
            ctx.cluster = Some(Arc::clone(&c.cluster));
            record_route_decision(ctx, &self.local_site, c);
            Ok(FilterAction::Continue)
        } else {
            tracing::debug!(kind = kind.as_str(), name = %name, "grid_route: no candidate");
            Ok(FilterAction::Reject(Rejection::status(404)))
        }
    }
}

// -----------------------------------------------------------------------------
// Lookup Extraction
// -----------------------------------------------------------------------------

/// Result of extracting a routable model from the request.
enum Lookup {
    /// A routable model was found.
    Route {
        /// Capability kind.
        kind: CapabilityKind,
        /// Capability name.
        name: String,
    },
    /// Model header absent; continue without routing.
    Skip,
    /// Header present but invalid; fail closed.
    Invalid,
}

/// Extract an inference model lookup from the promoted model header.
fn extract_model_lookup(ctx: &HttpFilterContext<'_>, model_header: &http::header::HeaderName) -> Lookup {
    let Some(value) = ctx.request.headers.get(model_header) else {
        tracing::debug!("grid_route: no model header; skipping");
        return Lookup::Skip;
    };
    let Ok(model) = value.to_str() else {
        tracing::debug!("grid_route: model header is not valid UTF-8; rejecting");
        return Lookup::Invalid;
    };
    if model.trim().is_empty() || model.len() > MAX_HEADER_VALUE_LEN {
        tracing::debug!("grid_route: model header blank or oversized; rejecting");
        return Lookup::Invalid;
    }
    Lookup::Route {
        kind: CapabilityKind::InferenceModel,
        name: model.to_owned(),
    }
}

// -----------------------------------------------------------------------------
// Candidate Selection
// -----------------------------------------------------------------------------

/// Select the best candidate by deterministic scoring.
///
/// Returns `None` when no candidate of the given `kind` matches
/// `name`. When multiple candidates match, the highest-scored wins;
/// ties are broken by config order (first configured wins).
fn select_candidate<'a>(
    candidates: &'a [RouteCandidate],
    kind: CapabilityKind,
    name: &str,
    local_site: &str,
) -> Option<&'a RouteCandidate> {
    let mut best: Option<(i32, &RouteCandidate)> = None;
    for c in candidates {
        if c.kind != kind || &*c.name != name {
            continue;
        }
        let s = score_candidate(c, local_site);
        match best {
            Some((best_score, _)) if s <= best_score => {},
            _ => best = Some((s, c)),
        }
    }
    best.map(|(_, c)| c)
}

/// Deterministic score for a candidate. Higher is better.
fn score_candidate(candidate: &RouteCandidate, local_site: &str) -> i32 {
    let mut s: i32 = 0;
    if !candidate.fresh {
        s -= STALE_PENALTY;
    }
    if *candidate.site == *local_site {
        s += LOCAL_PREFERENCE;
    }
    s
}

// -----------------------------------------------------------------------------
// Route Decision Metadata
// -----------------------------------------------------------------------------

/// Write bounded route-decision metadata on successful selection.
///
/// Keys use `grid.route.` namespace. All values are bounded by the
/// existing `set_metadata` limits.  No HTTP forwarding headers are
/// written by this function.
fn record_route_decision(ctx: &mut HttpFilterContext<'_>, local_site: &Arc<str>, candidate: &RouteCandidate) {
    ctx.set_metadata("grid.route.kind", candidate.kind.as_str());
    ctx.set_metadata("grid.route.name", &*candidate.name);
    ctx.set_metadata("grid.route.site", &*candidate.site);
    ctx.set_metadata("grid.route.cluster", &*candidate.cluster);
    ctx.set_metadata("grid.route.local_site", &**local_site);
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test suppressions")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests"
)]
mod tests {
    use http::Method;

    use super::*;

    // ---- Config validation ----

    #[test]
    fn valid_minimal_config() {
        let yaml = "local_site: site-a\ncandidates:\n  - kind: inference_model\n    name: llama\n    site: site-a\n    cluster: inf\n    fresh: true\n";
        assert!(parse(yaml).is_ok(), "minimal valid config should parse");
    }

    #[tokio::test]
    async fn default_model_header_is_x_model() {
        let f = make_filter(&[("inference_model", "llama", "site-a", "inf")]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers.insert("X-Model", http::HeaderValue::from_static("llama"));
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(
            matches!(action, FilterAction::Continue),
            "default model header X-Model should route"
        );
        assert_eq!(ctx.cluster.as_deref(), Some("inf"), "cluster should be set");
    }

    #[test]
    fn blank_local_site_rejected() {
        let err = parse_err(
            "local_site: \"\"\ncandidates:\n  - kind: inference_model\n    name: m\n    site: s\n    cluster: c\n    fresh: true\n",
        );
        assert!(
            err.to_string().contains("blank"),
            "blank local_site should be rejected: {err}"
        );
    }

    #[test]
    fn missing_candidates_rejected() {
        let err = parse_err("local_site: site-a\ncandidates: []\n");
        assert!(
            err.to_string().contains("empty"),
            "empty candidates should be rejected: {err}"
        );
    }

    #[test]
    fn blank_model_header_rejected() {
        let err = parse_err(
            "local_site: site-a\nmodel_header: \"\"\ncandidates:\n  - kind: inference_model\n    name: m\n    site: s\n    cluster: c\n    fresh: true\n",
        );
        assert!(
            err.to_string().contains("blank") || err.to_string().contains("empty"),
            "blank model_header should be rejected: {err}"
        );
    }

    #[test]
    fn reserved_model_header_rejected() {
        let err = parse_err(
            "local_site: site-a\nmodel_header: x-praxis-foo\ncandidates:\n  - kind: inference_model\n    name: m\n    site: s\n    cluster: c\n    fresh: true\n",
        );
        assert!(
            err.to_string().contains("reserved"),
            "reserved model_header should be rejected: {err}"
        );
    }

    #[test]
    fn invalid_candidate_rejected() {
        let err = parse_err(
            "local_site: site-a\ncandidates:\n  - kind: inference_model\n    name: \"\"\n    site: s\n    cluster: c\n    fresh: true\n",
        );
        assert!(
            err.to_string().contains("blank"),
            "blank candidate name should be rejected: {err}"
        );
    }

    // ---- Model header extraction ----

    #[tokio::test]
    async fn absent_model_header_continues() {
        let f = make_filter(&[("inference_model", "llama", "site-a", "inf")]);
        let req = crate::test_utils::make_request(Method::POST, "/chat");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(
            matches!(action, FilterAction::Continue),
            "absent model header should continue without routing"
        );
        assert!(ctx.cluster.is_none(), "no cluster should be set");
        assert_no_route_metadata(&ctx);
    }

    #[tokio::test]
    async fn blank_model_header_rejects() {
        let f = make_filter(&[("inference_model", "llama", "site-a", "inf")]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers.insert("X-Model", http::HeaderValue::from_static(""));
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(
            matches!(action, FilterAction::Reject(r) if r.status == 400),
            "blank model header should reject 400"
        );
        assert_no_route_metadata(&ctx);
    }

    #[tokio::test]
    async fn oversized_model_header_rejects_no_metadata() {
        let f = make_filter(&[("inference_model", "llama", "site-a", "inf")]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        let big = "a".repeat(MAX_HEADER_VALUE_LEN + 1);
        req.headers
            .insert("X-Model", http::HeaderValue::from_str(&big).unwrap());
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(
            matches!(action, FilterAction::Reject(r) if r.status == 400),
            "oversized model header should reject 400"
        );
        assert_no_route_metadata(&ctx);
    }

    #[tokio::test]
    async fn invalid_utf8_model_header_rejects_no_metadata() {
        let f = make_filter(&[("inference_model", "llama", "site-a", "inf")]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers
            .insert("X-Model", http::HeaderValue::from_bytes(b"\xff\xfe").unwrap());
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(
            matches!(action, FilterAction::Reject(r) if r.status == 400),
            "non-UTF-8 model header should reject 400"
        );
        assert_no_route_metadata(&ctx);
    }

    // ---- Candidate selection ----

    #[tokio::test]
    async fn unknown_model_rejects_404() {
        let f = make_filter(&[("inference_model", "llama", "site-a", "inf")]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers
            .insert("X-Model", http::HeaderValue::from_static("unknown-model"));
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(
            matches!(action, FilterAction::Reject(r) if r.status == 404),
            "unknown model should reject 404"
        );
        assert_no_route_metadata(&ctx);
    }

    #[tokio::test]
    async fn local_inference_sets_cluster() {
        let f = make_filter(&[("inference_model", "llama", "site-a", "local-inf")]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers.insert("X-Model", http::HeaderValue::from_static("llama"));
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(matches!(action, FilterAction::Continue));
        assert_eq!(ctx.cluster.as_deref(), Some("local-inf"), "cluster should be set");
    }

    #[tokio::test]
    async fn remote_inference_sets_gateway_cluster() {
        let f = make_filter(&[("inference_model", "llama", "site-b", "remote-gw")]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers.insert("X-Model", http::HeaderValue::from_static("llama"));
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(matches!(action, FilterAction::Continue));
        assert_eq!(ctx.cluster.as_deref(), Some("remote-gw"));
    }

    // ---- Cluster preservation ----

    #[tokio::test]
    async fn preserves_existing_cluster() {
        let f = make_filter(&[("inference_model", "llama", "site-a", "inf")]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers.insert("X-Model", http::HeaderValue::from_static("llama"));
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.cluster = Some(Arc::from("pre-set-cluster"));

        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(matches!(action, FilterAction::Continue));
        assert_eq!(
            ctx.cluster.as_deref(),
            Some("pre-set-cluster"),
            "pre-set cluster should be preserved"
        );
        assert_no_route_metadata(&ctx);
    }

    // ---- Scoring ----

    #[tokio::test]
    async fn local_fresh_beats_remote_fresh() {
        let f = make_filter(&[
            ("inference_model", "llama", "site-b", "remote-inf"),
            ("inference_model", "llama", "site-a", "local-inf"),
        ]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers.insert("X-Model", http::HeaderValue::from_static("llama"));
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let _unused = f.on_request(&mut ctx).await.unwrap();
        // cluster checked below
        assert_eq!(
            ctx.cluster.as_deref(),
            Some("local-inf"),
            "local candidate should win over remote"
        );
    }

    #[tokio::test]
    async fn config_order_breaks_equal_score_ties() {
        let f = make_filter(&[
            ("inference_model", "llama", "site-b", "first-remote"),
            ("inference_model", "llama", "site-c", "second-remote"),
        ]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers.insert("X-Model", http::HeaderValue::from_static("llama"));
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let _unused = f.on_request(&mut ctx).await.unwrap();
        // cluster checked below
        assert_eq!(
            ctx.cluster.as_deref(),
            Some("first-remote"),
            "first configured candidate wins on equal score"
        );
    }

    #[tokio::test]
    async fn fresh_remote_beats_stale_remote() {
        let f = make_scored_filter(&[
            ("inference_model", "llama", "site-b", "stale-remote", false),
            ("inference_model", "llama", "site-c", "fresh-remote", true),
        ]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers.insert("X-Model", http::HeaderValue::from_static("llama"));
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let _unused = f.on_request(&mut ctx).await.unwrap();
        // cluster checked below
        assert_eq!(
            ctx.cluster.as_deref(),
            Some("fresh-remote"),
            "fresh candidate should beat stale"
        );
    }

    #[tokio::test]
    async fn fresh_remote_beats_stale_local() {
        let f = make_scored_filter(&[
            ("inference_model", "llama", "site-a", "stale-local", false),
            ("inference_model", "llama", "site-b", "fresh-remote", true),
        ]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers.insert("X-Model", http::HeaderValue::from_static("llama"));
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let _unused = f.on_request(&mut ctx).await.unwrap();
        // cluster checked below
        assert_eq!(
            ctx.cluster.as_deref(),
            Some("fresh-remote"),
            "fresh remote beats stale local"
        );
    }

    #[tokio::test]
    async fn stale_local_beats_stale_remote() {
        let f = make_scored_filter(&[
            ("inference_model", "llama", "site-b", "stale-remote", false),
            ("inference_model", "llama", "site-a", "stale-local", false),
        ]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers.insert("X-Model", http::HeaderValue::from_static("llama"));
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let _unused = f.on_request(&mut ctx).await.unwrap();
        // cluster checked below
        assert_eq!(
            ctx.cluster.as_deref(),
            Some("stale-local"),
            "stale local beats stale remote"
        );
    }

    // ---- Route metadata ----

    #[tokio::test]
    async fn scored_route_metadata_reflects_winner() {
        let f = make_filter(&[
            ("inference_model", "llama", "site-b", "remote-inf"),
            ("inference_model", "llama", "site-a", "local-inf"),
        ]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers.insert("X-Model", http::HeaderValue::from_static("llama"));
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let _unused = f.on_request(&mut ctx).await.unwrap();
        assert_eq!(ctx.get_metadata("grid.route.cluster"), Some("local-inf"));
        assert_eq!(ctx.get_metadata("grid.route.kind"), Some("inference_model"));
        assert_eq!(ctx.get_metadata("grid.route.name"), Some("llama"));
        assert_eq!(ctx.get_metadata("grid.route.site"), Some("site-a"));
        assert_eq!(ctx.get_metadata("grid.route.local_site"), Some("site-a"));
    }

    #[tokio::test]
    async fn local_route_writes_metadata() {
        let f = make_filter(&[("inference_model", "llama", "site-a", "local-inf")]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers.insert("X-Model", http::HeaderValue::from_static("llama"));
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let _unused = f.on_request(&mut ctx).await.unwrap();
        assert_eq!(ctx.get_metadata("grid.route.cluster"), Some("local-inf"));
        assert_eq!(ctx.get_metadata("grid.route.local_site"), Some("site-a"));
    }

    #[tokio::test]
    async fn remote_route_writes_metadata() {
        let f = make_filter(&[("inference_model", "llama", "site-b", "remote-gw")]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers.insert("X-Model", http::HeaderValue::from_static("llama"));
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let _unused = f.on_request(&mut ctx).await.unwrap();
        assert_eq!(ctx.get_metadata("grid.route.cluster"), Some("remote-gw"));
        assert_eq!(ctx.get_metadata("grid.route.site"), Some("site-b"));
    }

    #[tokio::test]
    async fn unknown_model_writes_no_metadata() {
        let f = make_filter(&[("inference_model", "llama", "site-a", "inf")]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers.insert("X-Model", http::HeaderValue::from_static("unknown"));
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let _unused = f.on_request(&mut ctx).await.unwrap();
        assert_no_route_metadata(&ctx);
    }

    #[tokio::test]
    async fn blank_model_writes_no_metadata() {
        let f = make_filter(&[("inference_model", "llama", "site-a", "inf")]);
        let mut req = crate::test_utils::make_request(Method::POST, "/chat");
        req.headers.insert("X-Model", http::HeaderValue::from_static(""));
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let _unused = f.on_request(&mut ctx).await.unwrap();
        assert_no_route_metadata(&ctx);
    }

    #[tokio::test]
    async fn missing_header_writes_no_metadata() {
        let f = make_filter(&[("inference_model", "llama", "site-a", "inf")]);
        let req = crate::test_utils::make_request(Method::POST, "/chat");
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let _unused = f.on_request(&mut ctx).await.unwrap();
        assert_no_route_metadata(&ctx);
    }

    #[tokio::test]
    async fn preserved_cluster_writes_no_metadata() {
        let f = make_filter(&[("inference_model", "llama", "site-a", "inf")]);
        let req = crate::test_utils::make_request(Method::POST, "/chat");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.cluster = Some(Arc::from("pre-set"));

        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(matches!(action, FilterAction::Continue));
        assert!(
            ctx.get_metadata("grid.route.kind").is_none(),
            "preserved cluster path should not write route metadata"
        );
    }

    // ---- Test utilities ----

    fn assert_no_route_metadata(ctx: &HttpFilterContext<'_>) {
        assert!(
            ctx.get_metadata("grid.route.kind").is_none(),
            "grid.route.kind should be absent"
        );
        assert!(
            ctx.get_metadata("grid.route.name").is_none(),
            "grid.route.name should be absent"
        );
        assert!(
            ctx.get_metadata("grid.route.site").is_none(),
            "grid.route.site should be absent"
        );
        assert!(
            ctx.get_metadata("grid.route.cluster").is_none(),
            "grid.route.cluster should be absent"
        );
        assert!(
            ctx.get_metadata("grid.route.local_site").is_none(),
            "grid.route.local_site should be absent"
        );
    }

    fn parse(yaml: &str) -> Result<Box<dyn HttpFilter>, FilterError> {
        let val: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        GridRouteFilter::from_config(&val)
    }

    fn parse_err(yaml: &str) -> FilterError {
        parse(yaml).err().expect("config should have been rejected")
    }

    fn make_filter(candidates: &[(&str, &str, &str, &str)]) -> Box<dyn HttpFilter> {
        let scored: Vec<(&str, &str, &str, &str, bool)> =
            candidates.iter().map(|(k, n, s, c)| (*k, *n, *s, *c, true)).collect();
        make_scored_filter(&scored)
    }

    fn make_scored_filter(candidates: &[(&str, &str, &str, &str, bool)]) -> Box<dyn HttpFilter> {
        use std::fmt::Write as _;

        let mut yaml = String::from("local_site: site-a\ncandidates:\n");
        for (kind, name, site, cluster, fresh) in candidates {
            writeln!(
                yaml,
                "  - kind: {kind}\n    name: {name}\n    site: {site}\n    cluster: {cluster}\n    fresh: {fresh}"
            )
            .expect("String write is infallible");
        }
        parse(&yaml).unwrap()
    }
}
