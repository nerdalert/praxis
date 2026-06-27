// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Praxis Contributors

//! Grid route filter: selects a local or remote gateway cluster
//! based on a static site/capability descriptor, the promoted
//! model name, and MCP tool metadata.

use std::{collections::HashSet, sync::Arc};

use async_trait::async_trait;
use serde::Deserialize;

use crate::{
    FilterAction, FilterError, Rejection,
    factory::parse_filter_config,
    filter::{HttpFilter, HttpFilterContext},
};

/// Maximum number of route candidates to prevent unbounded config growth.
const MAX_CANDIDATES: usize = 1024;

/// Maximum length for name strings in config.
const MAX_NAME_LEN: usize = 256;

/// Header prefixes reserved for internal gateway/protocol metadata.
const RESERVED_HEADER_PREFIXES: &[&str] = &["x-praxis-", "x-mcp-", "x-a2a-"];

/// Score penalty applied to stale candidates.
const STALE_PENALTY: i32 = 100;

/// Score bonus applied to candidates on the local site.
const LOCAL_PREFERENCE: i32 = 10;

// -----------------------------------------------------------------------------
// Config
// -----------------------------------------------------------------------------

/// Deserialized YAML config for the grid route filter.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GridRouteConfig {
    /// Static list of route candidates.
    candidates: Vec<CandidateConfig>,

    /// Name of the local site for scoring and metadata.
    local_site: String,

    /// Header name that carries the model name (default: `X-Model`).
    #[serde(default = "default_model_header")]
    model_header: String,
}

/// A single route candidate in config.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateConfig {
    /// Cluster name to select when this candidate is chosen.
    cluster: String,

    /// Whether this candidate is fresh (default: `true`).
    #[serde(default = "default_fresh")]
    fresh: bool,

    /// Capability kind.
    kind: CapabilityKind,

    /// Capability name (model name, tool name, or agent name).
    name: String,

    /// Site that owns this capability.
    site: String,
}

/// Capability kind for route matching.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapabilityKind {
    /// OpenAI-compatible inference model (matched via `X-Model` header).
    InferenceModel,

    /// MCP tool (matched via `mcp.name` filter metadata).
    McpTool,
}

/// Default model header name.
fn default_model_header() -> String {
    "X-Model".to_owned()
}

/// Default freshness state.
fn default_fresh() -> bool {
    true
}

// -----------------------------------------------------------------------------
// GridRouteFilter
// -----------------------------------------------------------------------------

/// Selects a local or remote gateway cluster from a static
/// site/capability descriptor with deterministic scoring.
///
/// Scoring rules (higher wins):
/// 1. Fresh candidates score 0; stale candidates score -100.
/// 2. Candidates on the local site get +10.
/// 3. Ties broken by candidate config order (first wins).
pub struct GridRouteFilter {
    /// Resolved candidate list.
    candidates: Vec<RouteCandidate>,

    /// Name of the local site for scoring.
    local_site: Arc<str>,

    /// Header name that carries the model name.
    model_header: http::header::HeaderName,
}

/// A resolved route candidate ready for runtime matching.
struct RouteCandidate {
    /// Cluster name to select.
    cluster: Arc<str>,

    /// Whether this candidate is fresh.
    fresh: bool,

    /// Capability kind.
    kind: CapabilityKind,

    /// Capability name.
    name: Arc<str>,

    /// Site that owns this capability.
    site: Arc<str>,
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

        if cfg.candidates.is_empty() {
            return Err("grid_route: candidates list must not be empty".into());
        }
        if cfg.candidates.len() > MAX_CANDIDATES {
            return Err(format!("grid_route: candidates exceeds maximum of {MAX_CANDIDATES}").into());
        }
        validate_name("local_site", &cfg.local_site)?;

        let model_header = parse_model_header(&cfg.model_header)?;
        let candidates = validate_candidates(cfg.candidates)?;

        Ok(Box::new(Self {
            candidates,
            local_site: Arc::from(cfg.local_site.as_str()),
            model_header,
        }))
    }

    /// Score and select the best candidate for the given kind and name.
    fn select(&self, kind: CapabilityKind, name: &str) -> Option<&RouteCandidate> {
        let mut best = None;
        for candidate in self.candidates.iter().filter(|c| c.kind == kind && &*c.name == name) {
            match best {
                Some(current) if self.score(candidate) <= self.score(current) => {},
                _ => best = Some(candidate),
            }
        }
        best
    }

    /// Deterministic score for a candidate. Higher is better.
    fn score(&self, c: &RouteCandidate) -> i32 {
        let mut s: i32 = 0;
        if !c.fresh {
            s -= STALE_PENALTY;
        }
        if *c.site == *self.local_site {
            s += LOCAL_PREFERENCE;
        }
        s
    }
}

#[async_trait]
impl HttpFilter for GridRouteFilter {
    fn name(&self) -> &'static str {
        "grid_route"
    }

    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        let (kind, name) = match extract_lookup(ctx, &self.model_header) {
            Lookup::Found { kind, name } => (kind, name),
            Lookup::Missing => {
                tracing::debug!("grid_route: no routable capability; skipping");
                return Ok(FilterAction::Continue);
            },
            Lookup::Invalid { result } => {
                tracing::debug!(result, "grid_route: invalid route input; rejecting");
                ctx.set_metadata("grid_route.result", result);
                return Ok(FilterAction::Reject(Rejection::status(400)));
            },
        };

        if let Some(candidate) = self.select(kind, &name) {
            tracing::debug!(
                kind = ?kind, name = %name,
                site = &*candidate.site, cluster = &*candidate.cluster,
                fresh = candidate.fresh, score = self.score(candidate),
                "grid_route: selected"
            );
            record_decision(ctx, &name, &self.local_site, kind, Some(candidate));
            ctx.cluster = Some(Arc::clone(&candidate.cluster));
            Ok(FilterAction::Continue)
        } else {
            tracing::debug!(kind = ?kind, name = %name, "grid_route: no candidate");
            record_decision(ctx, &name, &self.local_site, kind, None);
            Ok(FilterAction::Reject(Rejection::status(404)))
        }
    }
}

// -----------------------------------------------------------------------------
// Private Helpers
// -----------------------------------------------------------------------------

/// Route lookup extracted from request metadata.
enum Lookup {
    /// A routeable capability was found.
    Found {
        /// Capability kind.
        kind: CapabilityKind,

        /// Capability name.
        name: String,
    },

    /// No routeable capability is available.
    Missing,

    /// Route input is present but invalid and should fail closed.
    Invalid {
        /// Safe bounded result string for route metadata.
        result: &'static str,
    },
}

/// Validate and resolve the candidate list from config.
fn validate_candidates(raw: Vec<CandidateConfig>) -> Result<Vec<RouteCandidate>, FilterError> {
    let mut candidates = Vec::with_capacity(raw.len());
    let mut seen: HashSet<(CapabilityKind, String, String)> = HashSet::with_capacity(raw.len());
    for (i, c) in raw.into_iter().enumerate() {
        validate_name(&format!("candidates[{i}].name"), &c.name)?;
        validate_name(&format!("candidates[{i}].site"), &c.site)?;
        validate_name(&format!("candidates[{i}].cluster"), &c.cluster)?;
        if !seen.insert((c.kind, c.name.clone(), c.site.clone())) {
            return Err(format!(
                "grid_route: duplicate candidate '{}/{}/{}'",
                c.kind_str(),
                c.name,
                c.site
            )
            .into());
        }
        candidates.push(RouteCandidate {
            cluster: Arc::from(c.cluster.as_str()),
            fresh: c.fresh,
            kind: c.kind,
            name: Arc::from(c.name.as_str()),
            site: Arc::from(c.site.as_str()),
        });
    }
    Ok(candidates)
}

/// Parse and validate the promoted model header name.
fn parse_model_header(raw: &str) -> Result<http::header::HeaderName, FilterError> {
    if raw.trim().is_empty() {
        return Err("grid_route: model_header must not be empty".into());
    }
    let header: http::header::HeaderName = raw
        .parse()
        .map_err(|e| -> FilterError { format!("grid_route: invalid model_header: {e}").into() })?;
    if RESERVED_HEADER_PREFIXES.iter().any(|p| header.as_str().starts_with(p)) {
        return Err("grid_route: model_header must not use a reserved internal header prefix".into());
    }
    Ok(header)
}

/// Validate bounded, non-blank names from config.
fn validate_name(field: &str, value: &str) -> Result<(), FilterError> {
    if value.trim().is_empty() || value.len() > MAX_NAME_LEN {
        return Err(format!("grid_route: {field} must be 1-256 non-blank characters").into());
    }
    Ok(())
}

/// Extract the routable capability from request context.
fn extract_lookup(ctx: &HttpFilterContext<'_>, model_header: &http::header::HeaderName) -> Lookup {
    if let Some(lookup) = extract_mcp_lookup(ctx) {
        return lookup;
    }
    extract_model_lookup(ctx, model_header)
}

/// Try to extract an MCP tool lookup from filter metadata.
fn extract_mcp_lookup(ctx: &HttpFilterContext<'_>) -> Option<Lookup> {
    let tool_name = ctx.get_metadata("mcp.name")?;
    let method = ctx.get_metadata("mcp.method").unwrap_or("");
    if method != "tools/call" {
        return None;
    }
    if tool_name.trim().is_empty() || tool_name.len() > MAX_NAME_LEN {
        return Some(Lookup::Invalid {
            result: "invalid_mcp_tool",
        });
    }
    Some(Lookup::Found {
        kind: CapabilityKind::McpTool,
        name: tool_name.to_owned(),
    })
}

/// Try to extract a model lookup from the promoted header.
fn extract_model_lookup(ctx: &HttpFilterContext<'_>, model_header: &http::header::HeaderName) -> Lookup {
    let Some(value) = ctx.request.headers.get(model_header) else {
        return Lookup::Missing;
    };
    let Ok(model) = value.to_str() else {
        return Lookup::Invalid {
            result: "invalid_model_header",
        };
    };
    if model.trim().is_empty() || model.len() > MAX_NAME_LEN {
        return Lookup::Invalid {
            result: "invalid_model",
        };
    }
    Lookup::Found {
        kind: CapabilityKind::InferenceModel,
        name: model.to_owned(),
    }
}

/// Write safe, bounded route decision metadata.
fn record_decision(
    ctx: &mut HttpFilterContext<'_>,
    name: &str,
    local_site: &str,
    kind: CapabilityKind,
    candidate: Option<&RouteCandidate>,
) {
    ctx.set_metadata("grid_route.kind", kind.as_str());
    ctx.set_metadata("grid_route.name", name);
    ctx.set_metadata("grid_route.local_site", local_site);
    if let Some(c) = candidate {
        ctx.set_metadata("grid_route.site", &*c.site);
        ctx.set_metadata("grid_route.cluster", &*c.cluster);
        ctx.set_metadata("grid_route.fresh", if c.fresh { "true" } else { "false" });
    } else {
        ctx.set_metadata("grid_route.result", "no_candidate");
    }
}

impl CapabilityKind {
    /// Static string representation for metadata.
    fn as_str(self) -> &'static str {
        match self {
            Self::InferenceModel => "inference_model",
            Self::McpTool => "mcp_tool",
        }
    }
}

impl CandidateConfig {
    /// Kind string for error messages.
    fn kind_str(&self) -> &'static str {
        self.kind.as_str()
    }
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
    use http::{HeaderValue, Method};

    use super::*;

    // ---- Config validation ----

    #[test]
    fn empty_candidates_rejected() {
        let err = parse_config("local_site: a\ncandidates: []")
            .err()
            .expect("empty candidates should fail");
        assert!(err.to_string().contains("must not be empty"), "{err}");
    }

    #[test]
    fn empty_name_rejected() {
        let err = parse_config(
            "
local_site: a
candidates:
  - kind: inference_model
    name: ''
    site: a
    cluster: c
",
        )
        .err()
        .expect("empty name should fail");
        assert!(err.to_string().contains("name must be"), "{err}");
    }

    #[test]
    fn duplicate_candidate_same_site_rejected() {
        let err = parse_config(
            "
local_site: a
candidates:
  - kind: inference_model
    name: llama
    site: a
    cluster: c1
  - kind: inference_model
    name: llama
    site: a
    cluster: c2
",
        )
        .err()
        .expect("duplicate kind/name/site should fail");
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn same_name_different_site_allowed_for_scoring() {
        assert!(
            parse_config(
                "
local_site: a
candidates:
  - kind: inference_model
    name: shared
    site: a
    cluster: c1
  - kind: inference_model
    name: shared
    site: b
    cluster: c2
"
            )
            .is_ok(),
            "same kind/name on different sites should be allowed for scoring"
        );
    }

    #[test]
    fn same_name_different_kind_allowed() {
        assert!(
            parse_config(
                "
local_site: a
candidates:
  - kind: inference_model
    name: llama
    site: a
    cluster: c1
  - kind: mcp_tool
    name: llama
    site: b
    cluster: c2
"
            )
            .is_ok(),
            "same name with different kind should be allowed"
        );
    }

    #[test]
    fn reserved_model_header_rejected() {
        for h in ["x-praxis-foo", "x-mcp-bar", "x-a2a-baz"] {
            let err = parse_config(&format!(
                "
local_site: a
model_header: {h}
candidates:
  - kind: inference_model
    name: llama
    site: a
    cluster: c
"
            ))
            .err()
            .expect("reserved model_header should fail");
            assert!(err.to_string().contains("reserved"), "{h}: {err}");
        }
    }

    #[test]
    fn valid_config_parses() {
        assert!(parse_config(STANDARD_CONFIG).is_ok(), "valid config should parse");
    }

    // ---- Inference routing ----

    #[tokio::test]
    async fn local_model_selects_local_cluster() {
        let f = make_filter();
        let mut req = post_chat();
        req.headers.insert("x-model", hv("local-model"));
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(matches!(action, FilterAction::Continue));
        assert_eq!(ctx.cluster.as_deref(), Some("local-inference"));
        assert_eq!(ctx.get_metadata("grid_route.kind"), Some("inference_model"));
    }

    #[tokio::test]
    async fn remote_model_selects_remote_cluster() {
        let f = make_filter();
        let mut req = post_chat();
        req.headers.insert("x-model", hv("site-b-model"));
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(matches!(action, FilterAction::Continue));
        assert_eq!(ctx.cluster.as_deref(), Some("grid-site-b"));
    }

    #[tokio::test]
    async fn unknown_model_rejects_404() {
        let f = make_filter();
        let mut req = post_chat();
        req.headers.insert("x-model", hv("nonexistent"));
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(matches!(action, FilterAction::Reject(r) if r.status == 404));
        assert_eq!(ctx.get_metadata("grid_route.result"), Some("no_candidate"));
    }

    #[tokio::test]
    async fn invalid_model_header_rejects_without_unbounded_metadata() {
        let f = make_filter();
        let mut req = post_chat();
        let long_model = "a".repeat(MAX_NAME_LEN + 1);
        req.headers
            .insert("x-model", HeaderValue::from_str(&long_model).unwrap());
        let mut ctx = crate::test_utils::make_filter_context(&req);

        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(
            matches!(action, FilterAction::Reject(r) if r.status == 400),
            "oversized model header should reject with 400"
        );
        assert_eq!(
            ctx.get_metadata("grid_route.result"),
            Some("invalid_model"),
            "metadata should record invalid_model without storing the oversized value"
        );
        assert!(
            ctx.get_metadata("grid_route.name").is_none(),
            "oversized model value must not be copied into metadata"
        );
        assert_metadata_bounded(&ctx);
    }

    #[tokio::test]
    async fn no_model_header_skips() {
        let f = make_filter();
        let req = crate::test_utils::make_request(Method::GET, "/");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(matches!(action, FilterAction::Continue));
        assert!(ctx.cluster.is_none());
    }

    // ---- Scoring ----

    #[tokio::test]
    async fn fresh_beats_stale() {
        let f = parse_config(
            "
local_site: site-x
candidates:
  - kind: inference_model
    name: shared-model
    site: site-stale
    cluster: stale-cluster
    fresh: false
  - kind: inference_model
    name: shared-model
    site: site-fresh
    cluster: fresh-cluster
    fresh: true
",
        )
        .unwrap();
        let mut req = post_chat();
        req.headers.insert("x-model", hv("shared-model"));
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(matches!(action, FilterAction::Continue));
        assert_eq!(
            ctx.cluster.as_deref(),
            Some("fresh-cluster"),
            "fresh candidate should win over stale"
        );
        assert_eq!(ctx.get_metadata("grid_route.fresh"), Some("true"));
    }

    #[tokio::test]
    async fn local_preference_wins_when_equal() {
        let f = parse_config(
            "
local_site: site-a
candidates:
  - kind: inference_model
    name: shared-model
    site: site-b
    cluster: remote-cluster
  - kind: inference_model
    name: shared-model
    site: site-a
    cluster: local-cluster
",
        )
        .unwrap();
        let mut req = post_chat();
        req.headers.insert("x-model", hv("shared-model"));
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(matches!(action, FilterAction::Continue));
        assert_eq!(
            ctx.cluster.as_deref(),
            Some("local-cluster"),
            "local site should win when candidates are otherwise equal"
        );
    }

    #[tokio::test]
    async fn deterministic_tiebreak_first_highest() {
        let f = parse_config(
            "
local_site: site-x
candidates:
  - kind: inference_model
    name: shared-model
    site: site-a
    cluster: cluster-a
  - kind: inference_model
    name: shared-model
    site: site-b
    cluster: cluster-b
",
        )
        .unwrap();
        let mut req = post_chat();
        req.headers.insert("x-model", hv("shared-model"));
        let mut ctx = crate::test_utils::make_filter_context(&req);
        drop(f.on_request(&mut ctx).await.unwrap());
        assert_eq!(
            ctx.cluster.as_deref(),
            Some("cluster-a"),
            "equally scored candidates should select the first configured candidate"
        );
    }

    // ---- MCP tool routing ----

    #[tokio::test]
    async fn mcp_tool_selects_remote_cluster() {
        let f = make_filter();
        let req = crate::test_utils::make_request(Method::POST, "/mcp");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.set_metadata("mcp.method", "tools/call");
        ctx.set_metadata("mcp.name", "weather-lookup");
        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(matches!(action, FilterAction::Continue));
        assert_eq!(ctx.cluster.as_deref(), Some("grid-site-c"));
        assert_eq!(ctx.get_metadata("grid_route.kind"), Some("mcp_tool"));
    }

    #[tokio::test]
    async fn unknown_mcp_tool_rejects_404() {
        let f = make_filter();
        let req = crate::test_utils::make_request(Method::POST, "/mcp");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.set_metadata("mcp.method", "tools/call");
        ctx.set_metadata("mcp.name", "nonexistent-tool");
        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(matches!(action, FilterAction::Reject(r) if r.status == 404));
    }

    #[tokio::test]
    async fn mcp_non_tools_call_skips() {
        let f = make_filter();
        let req = crate::test_utils::make_request(Method::POST, "/mcp");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.set_metadata("mcp.method", "tools/list");
        ctx.set_metadata("mcp.name", "weather-lookup");
        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(matches!(action, FilterAction::Continue));
        assert!(ctx.cluster.is_none(), "tools/list should not trigger routing");
    }

    #[tokio::test]
    async fn invalid_mcp_tool_rejects_without_unbounded_metadata() {
        let f = make_filter();
        let req = crate::test_utils::make_request(Method::POST, "/mcp");
        let mut ctx = crate::test_utils::make_filter_context(&req);
        ctx.set_metadata("mcp.method", "tools/call");
        ctx.set_metadata("mcp.name", "");

        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(
            matches!(action, FilterAction::Reject(r) if r.status == 400),
            "blank MCP tool name should reject with 400"
        );
        assert_eq!(
            ctx.get_metadata("grid_route.result"),
            Some("invalid_mcp_tool"),
            "metadata should record the safe invalid MCP result"
        );
        assert!(
            ctx.get_metadata("grid_route.name").is_none(),
            "invalid MCP tool name must not be copied into route metadata"
        );
        assert_metadata_bounded(&ctx);
    }

    // ---- Security ----

    #[tokio::test]
    async fn reserved_headers_do_not_influence_route() {
        let f = make_filter();
        let mut req = post_chat();
        req.headers.insert("x-model", hv("local-model"));
        req.headers.insert("x-praxis-grid-site", hv("attacker"));
        let mut ctx = crate::test_utils::make_filter_context(&req);
        let action = f.on_request(&mut ctx).await.unwrap();
        assert!(matches!(action, FilterAction::Continue));
        assert_eq!(ctx.cluster.as_deref(), Some("local-inference"));
        assert_eq!(ctx.get_metadata("grid_route.site"), Some("site-a"));
    }

    // ---- Metadata ----

    #[tokio::test]
    async fn route_metadata_is_bounded() {
        let f = make_filter();
        let mut req = post_chat();
        req.headers.insert("x-model", hv("local-model"));
        let mut ctx = crate::test_utils::make_filter_context(&req);
        drop(f.on_request(&mut ctx).await.unwrap());
        assert_metadata_bounded(&ctx);
    }

    // -------------------------------------------------------------------------
    // Test Utilities
    // -------------------------------------------------------------------------

    const STANDARD_CONFIG: &str = "
local_site: site-a
candidates:
  - kind: inference_model
    name: local-model
    site: site-a
    cluster: local-inference
  - kind: inference_model
    name: site-b-model
    site: site-b
    cluster: grid-site-b
  - kind: inference_model
    name: site-c-model
    site: site-c
    cluster: grid-site-c
  - kind: mcp_tool
    name: weather-lookup
    site: site-c
    cluster: grid-site-c
";

    fn parse_config(yaml: &str) -> Result<Box<dyn HttpFilter>, FilterError> {
        let val: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        GridRouteFilter::from_config(&val)
    }

    fn make_filter() -> Box<dyn HttpFilter> {
        parse_config(STANDARD_CONFIG).unwrap()
    }

    fn post_chat() -> crate::Request {
        crate::test_utils::make_request(Method::POST, "/v1/chat/completions")
    }

    fn hv(s: &'static str) -> HeaderValue {
        HeaderValue::from_static(s)
    }

    fn assert_metadata_bounded(ctx: &HttpFilterContext<'_>) {
        for (key, value) in &ctx.filter_metadata {
            assert!(key.len() <= 64, "metadata key too long: {key}");
            assert!(value.len() <= MAX_NAME_LEN, "metadata value too long for key {key}");
            assert!(
                !value.contains('\n') && !value.contains('\r'),
                "metadata value contains control chars for key {key}"
            );
        }
    }
}
