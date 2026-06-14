// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Request-phase filter execution.

use std::{borrow::Cow, sync::Arc};

use pingora_core::Result;
use pingora_proxy::Session;
use praxis_core::connectivity::normalize_mapped_ipv4;
use praxis_filter::{BodyMode, FilterAction, FilterError, FilterPipeline, Rejection, Request};
use tracing::warn;

use super::super::{
    context::PingoraRequestCtx,
    convert::{request_header_from_session, send_rejection},
};

/// StreamBuffer pre-read logic and TRACE response construction.
mod stream_buffer;
/// Host header validation and Max-Forwards handling.
mod validation;

use stream_buffer::PreReadError;

// -----------------------------------------------------------------------------
// PipelineResult
// -----------------------------------------------------------------------------

/// Results from running the request-phase filter pipeline.
struct PipelineResult {
    /// Final filter action.
    action: FilterAction,

    /// Extra headers to add to the upstream request.
    extra_headers: Vec<(Cow<'static, str>, String)>,

    /// Headers to remove from the upstream request.
    headers_to_remove: Vec<http::header::HeaderName>,

    /// Headers to set (overwrite) on the upstream request.
    headers_to_set: Vec<(http::header::HeaderName, http::header::HeaderValue)>,
}

// -----------------------------------------------------------------------------
// Request Filters
// -----------------------------------------------------------------------------

/// Run the request-phase pipeline, capture client info, and inject headers.
///
/// Host header validation runs first (before the pipeline) to reject
/// ambiguous requests early.
#[allow(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    reason = "orchestration function"
)]
pub(in crate::http) async fn execute(
    pipeline: &FilterPipeline,
    session: &mut Session,
    ctx: &mut PingoraRequestCtx,
) -> Result<bool> {
    if let Some(rejection) = validation::validate_host_header(session) {
        send_rejection(session, rejection).await;
        return Ok(true);
    }

    if let Some(rejection) = super::normalize::normalize_request_headers(session) {
        send_rejection(session, rejection).await;
        return Ok(true);
    }

    if let Some(rejection) = reject_reserved_internal_headers(session) {
        send_rejection(session, rejection).await;
        return Ok(true);
    }

    if let Some(handled) = validation::handle_max_forwards(session).await {
        return Ok(handled);
    }

    ctx.client_http_version = Some(session.req_header().version);

    let mut request = request_header_from_session(session);
    ctx.client_addr = session
        .client_addr()
        .and_then(|a| a.as_inet())
        .map(std::net::SocketAddr::ip)
        .map(normalize_mapped_ipv4);
    ctx.downstream_tls = session.digest().is_some_and(|d| d.ssl_digest.is_some());
    ctx.request_is_idempotent = matches!(
        session.req_header().method,
        http::Method::GET | http::Method::HEAD | http::Method::OPTIONS
    );

    let caps = pipeline.body_capabilities();
    ctx.request_body_mode = caps.request_body_mode;
    ctx.response_body_mode = caps.response_body_mode;

    if matches!(caps.request_body_mode, BodyMode::StreamBuffer { .. }) {
        tracing::debug!("pre-reading request body for StreamBuffer inspection");
        match stream_buffer::pre_read_body(pipeline, session, ctx, &request).await {
            Ok(mutations) => {
                apply_pre_read_mutations(session, &mut request, &mutations.mutations);
                ctx.pre_read_mutations = mutations.mutations;
            },
            Err(PreReadError::Rejected(rejection)) => {
                send_rejection(session, rejection).await;
                return Ok(true);
            },
            Err(PreReadError::Filter(e)) => {
                warn!(error = %e, "body filter error during pre-read");
                send_rejection(session, Rejection::status(500)).await;
                return Ok(true);
            },
            Err(PreReadError::Io(e)) => return Err(e),
        }
    }

    let result = run_pipeline(pipeline, request, ctx).await;
    ctx.pre_read_mutations.clear();
    match result {
        Ok(PipelineResult {
            action: FilterAction::Continue | FilterAction::Release | FilterAction::BodyDone,
            extra_headers,
            headers_to_remove,
            headers_to_set,
        }) => {
            if let Some(ref mut snapshot) = ctx.request_snapshot {
                for name in &headers_to_remove {
                    snapshot.headers.remove(name);
                }
                for (name, value) in &headers_to_set {
                    snapshot.headers.insert(name.clone(), value.clone());
                }
            }
            let req_headers = session.req_header_mut();
            for name in &headers_to_remove {
                let _remove = req_headers.remove_header(name);
            }
            for (name, value) in &headers_to_set {
                let _insert = req_headers.insert_header(name.clone(), value.clone());
            }
            for (name, value) in extra_headers {
                let _insert = req_headers.insert_header(name.into_owned(), value);
            }
            Ok(false)
        },
        Ok(PipelineResult {
            action: FilterAction::Reject(rejection),
            ..
        }) => {
            send_rejection(session, rejection).await;
            Ok(true)
        },
        Err(e) => {
            warn!(error = %e, "filter pipeline error");
            send_rejection(session, Rejection::status(500)).await;
            Ok(true)
        },
    }
}

// -----------------------------------------------------------------------------
// Header-Phase Pipeline
// -----------------------------------------------------------------------------

/// Run the request-phase filter pipeline and snapshot the request for later phases.
///
/// Returns the final action and any extra headers promoted by filters.
#[allow(clippy::too_many_lines, reason = "writeback destructuring")]
async fn run_pipeline(
    pipeline: &FilterPipeline,
    request: Request,
    ctx: &mut PingoraRequestCtx,
) -> std::result::Result<PipelineResult, FilterError> {
    let baseline_request_body_mode = ctx.request_body_mode;
    let (
        action,
        extra_headers,
        headers_to_remove,
        headers_to_set,
        cluster,
        upstream,
        rewritten_path,
        request_body_mode,
        selected_endpoint_index,
        filter_metadata,
        structured_metadata,
        filter_state,
    ) = {
        let mut filter_ctx = ctx.build_filter_context(pipeline, &request, None);

        let action = pipeline.execute_http_request(&mut filter_ctx).await;
        (
            action,
            filter_ctx.extra_request_headers,
            filter_ctx.request_headers_to_remove,
            filter_ctx.request_headers_to_set,
            filter_ctx.cluster,
            filter_ctx.upstream,
            filter_ctx.rewritten_path,
            filter_ctx.request_body_mode,
            filter_ctx.selected_endpoint_index,
            filter_ctx.filter_metadata,
            filter_ctx.structured_metadata,
            filter_ctx.filter_state,
        )
    };

    ctx.request_snapshot = Some(request);
    ctx.filter_metadata = filter_metadata;
    ctx.structured_metadata = structured_metadata;
    ctx.filter_state = filter_state;
    ctx.metrics_cluster_shared = cluster.as_ref().map(|c| ::metrics::SharedString::from(Arc::clone(c)));
    ctx.metrics_cluster = cluster.clone();

    match action {
        Ok(FilterAction::Continue | FilterAction::Release | FilterAction::BodyDone) => {
            ctx.cluster = cluster;
            ctx.upstream = upstream;
            ctx.rewritten_path = rewritten_path;
            ctx.request_body_mode = super::clamp_body_mode_to_ceiling(request_body_mode, baseline_request_body_mode);
            ctx.selected_endpoint_index = selected_endpoint_index;
            Ok(PipelineResult {
                action: FilterAction::Continue,
                extra_headers,
                headers_to_remove,
                headers_to_set,
            })
        },
        Ok(FilterAction::Reject(rejection)) => Ok(PipelineResult {
            action: FilterAction::Reject(rejection),
            extra_headers: Vec::new(),
            headers_to_remove: Vec::new(),
            headers_to_set: Vec::new(),
        }),
        Err(e) => Err(e),
    }
}

/// Apply trusted pre-read mutations to the session and request in
/// operation order.
fn apply_pre_read_mutations(
    session: &mut Session,
    request: &mut Request,
    mutations: &[praxis_filter::TrustedHeaderMutation],
) {
    apply_pre_read_mutations_to_request(request, mutations);
    apply_pre_read_mutations_to_session(session, mutations);
}

/// Apply ordered pre-read mutations to the Praxis [`Request`].
///
/// Both [`Set`] and [`Add`] use overwrite (insert) semantics.
/// [`resolve_trusted_header`] treats [`Add`] as append for
/// ambiguity detection, but promoted request headers use
/// overwrite to match Pingora's `insert_header` and the Go
/// EPP's `SetHeaders` convention.
///
/// [`Set`]: praxis_filter::TrustedHeaderMutation::Set
/// [`Add`]: praxis_filter::TrustedHeaderMutation::Add
/// [`resolve_trusted_header`]: praxis_filter::HttpFilterContext::resolve_trusted_header
fn apply_pre_read_mutations_to_request(request: &mut Request, mutations: &[praxis_filter::TrustedHeaderMutation]) {
    for mutation in mutations {
        match mutation {
            praxis_filter::TrustedHeaderMutation::Remove(name) => {
                request.headers.remove(name);
            },
            praxis_filter::TrustedHeaderMutation::Set(name, value) => {
                request.headers.insert(name.clone(), value.clone());
            },
            praxis_filter::TrustedHeaderMutation::Add(name, value) => {
                if let Ok(hval) = http::header::HeaderValue::from_str(value) {
                    request.headers.insert(name.clone(), hval);
                }
            },
        }
    }
}

/// Mirror ordered pre-read mutations to the Pingora session.
///
/// Both [`Set`] and [`Add`] use overwrite semantics to match
/// the request application and Pingora's `insert_header`.
///
/// [`Set`]: praxis_filter::TrustedHeaderMutation::Set
/// [`Add`]: praxis_filter::TrustedHeaderMutation::Add
fn apply_pre_read_mutations_to_session(session: &mut Session, mutations: &[praxis_filter::TrustedHeaderMutation]) {
    let req_headers = session.req_header_mut();
    for mutation in mutations {
        match mutation {
            praxis_filter::TrustedHeaderMutation::Remove(name) => {
                let _remove = req_headers.remove_header(name);
            },
            praxis_filter::TrustedHeaderMutation::Set(name, value) => {
                let _insert = req_headers.insert_header(name.clone(), value.clone());
            },
            praxis_filter::TrustedHeaderMutation::Add(name, value) => {
                if let Ok(hval) = http::header::HeaderValue::from_str(value) {
                    let _insert = req_headers.insert_header(name.clone(), hval);
                } else {
                    tracing::warn!(header = %name, "skipping invalid promoted header");
                }
            },
        }
    }
}

/// Reject client-supplied reserved internal headers before special handling
/// or filter execution can observe them.
fn reject_reserved_internal_headers(session: &Session) -> Option<Rejection> {
    let reserved_count = session
        .req_header()
        .headers
        .keys()
        .filter(|name| super::reserved_headers::is_reserved_internal_header(name))
        .count();

    if reserved_count == 0 {
        return None;
    }

    warn!(
        count = reserved_count,
        "rejecting request with client-supplied reserved internal headers"
    );
    Some(Rejection::status(400))
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::significant_drop_tightening,
    reason = "tests"
)]
mod tests {
    use std::net::IpAddr;

    use http::{HeaderMap, Method, Uri};
    use pingora_core::upstreams::peer::Peer;
    use praxis_core::config::FailureMode;
    use praxis_filter::{BodyMode, FilterAction, FilterPipeline, FilterRegistry, Request};

    use super::*;
    use crate::http::pingora::context::PingoraRequestCtx;

    #[tokio::test]
    async fn empty_pipeline_continues() {
        let result = run_pipeline(&empty_pipeline(), make_request(), &mut make_ctx())
            .await
            .unwrap();

        assert!(
            matches!(result.action, FilterAction::Continue),
            "empty pipeline should continue"
        );
        assert!(
            result.extra_headers.is_empty(),
            "empty pipeline should produce no extra headers"
        );
    }

    #[tokio::test]
    async fn snapshot_always_stored() {
        let mut ctx = make_ctx();

        drop(run_pipeline(&empty_pipeline(), make_request(), &mut ctx).await.unwrap());

        assert!(
            ctx.request_snapshot.is_some(),
            "request snapshot should be stored after pipeline run"
        );
    }

    #[tokio::test]
    async fn cluster_and_upstream_propagated_on_continue() {
        let mut ctx = make_ctx();

        drop(run_pipeline(&empty_pipeline(), make_request(), &mut ctx).await.unwrap());

        assert!(ctx.cluster.is_none(), "empty pipeline should leave cluster unset");
        assert!(ctx.upstream.is_none(), "empty pipeline should leave upstream unset");
    }

    #[tokio::test]
    async fn rejection_propagated_from_pipeline() {
        let pipeline = rejecting_pipeline(403);
        let mut ctx = make_ctx();

        let result = run_pipeline(&pipeline, make_request(), &mut ctx).await.unwrap();

        assert!(matches!(result.action, FilterAction::Reject(r) if r.status == 403));
    }

    #[tokio::test]
    async fn rejection_does_not_set_cluster() {
        let pipeline = rejecting_pipeline(429);
        let mut ctx = make_ctx();

        drop(run_pipeline(&pipeline, make_request(), &mut ctx).await.unwrap());

        assert!(ctx.cluster.is_none(), "rejection should not set cluster");
        assert!(ctx.upstream.is_none(), "rejection should not set upstream");
    }

    #[tokio::test]
    async fn extra_headers_returned_from_pipeline() {
        let pipeline = empty_pipeline();
        let mut ctx = make_ctx();

        let result = run_pipeline(&pipeline, make_request(), &mut ctx).await.unwrap();

        assert!(
            result.extra_headers.is_empty(),
            "empty pipeline should produce no extra headers"
        );
    }

    #[tokio::test]
    async fn idempotent_methods_detected_in_request() {
        for method in [Method::GET, Method::HEAD, Method::OPTIONS] {
            let req = Request {
                method,
                uri: Uri::from_static("/"),
                headers: HeaderMap::new(),
            };
            let is_idempotent = matches!(req.method, Method::GET | Method::HEAD | Method::OPTIONS);
            assert!(is_idempotent, "{} should be idempotent", req.method);
        }

        for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            let req = Request {
                method,
                uri: Uri::from_static("/"),
                headers: HeaderMap::new(),
            };
            let is_idempotent = matches!(req.method, Method::GET | Method::HEAD | Method::OPTIONS);
            assert!(!is_idempotent, "{} should not be idempotent", req.method);
        }
    }

    #[test]
    fn normalize_mapped_ipv4_converts_mapped_to_v4() {
        let mapped: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        let expected: IpAddr = "10.0.0.1".parse().unwrap();
        assert_eq!(
            normalize_mapped_ipv4(mapped),
            expected,
            "::ffff:10.0.0.1 should normalize to 10.0.0.1"
        );
    }

    #[test]
    fn normalize_mapped_ipv4_preserves_native_v4() {
        let native: IpAddr = "192.168.1.1".parse().unwrap();
        assert_eq!(normalize_mapped_ipv4(native), native, "native IPv4 should be unchanged");
    }

    #[test]
    fn normalize_mapped_ipv4_preserves_native_v6() {
        let native: IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(normalize_mapped_ipv4(native), native, "native IPv6 should be unchanged");
    }

    #[test]
    fn normalize_mapped_ipv4_preserves_loopback_v6() {
        let loopback: IpAddr = "::1".parse().unwrap();
        assert_eq!(
            normalize_mapped_ipv4(loopback),
            loopback,
            "IPv6 loopback should be unchanged"
        );
    }

    #[test]
    fn normalize_mapped_ipv4_converts_mapped_loopback() {
        let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        let expected: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(
            normalize_mapped_ipv4(mapped),
            expected,
            "::ffff:127.0.0.1 should normalize to 127.0.0.1"
        );
    }

    #[test]
    fn clamp_body_mode_to_ceiling_caps_stream_buffer_limit() {
        let clamped = super::super::clamp_body_mode_to_ceiling(
            BodyMode::StreamBuffer { max_bytes: Some(4096) },
            BodyMode::StreamBuffer { max_bytes: Some(1024) },
        );
        assert_eq!(
            clamped,
            BodyMode::StreamBuffer { max_bytes: Some(1024) },
            "runtime StreamBuffer widening should be clamped to baseline ceiling"
        );
    }

    #[test]
    fn clamp_body_mode_to_ceiling_caps_unbounded_stream_buffer() {
        let clamped = super::super::clamp_body_mode_to_ceiling(
            BodyMode::StreamBuffer { max_bytes: None },
            BodyMode::SizeLimit { max_bytes: 512 },
        );
        assert_eq!(
            clamped,
            BodyMode::StreamBuffer { max_bytes: Some(512) },
            "runtime unbounded StreamBuffer should be clamped to baseline ceiling"
        );
    }

    #[test]
    fn clamp_body_mode_to_ceiling_stream_passes_through_with_ceiling() {
        let clamped = super::super::clamp_body_mode_to_ceiling(
            BodyMode::Stream,
            BodyMode::StreamBuffer { max_bytes: Some(1024) },
        );
        assert_eq!(
            clamped,
            BodyMode::Stream,
            "Stream has no buffer to clamp and should pass through unchanged"
        );
    }

    #[test]
    fn clamp_body_mode_to_ceiling_stream_passes_through_without_ceiling() {
        let clamped = super::super::clamp_body_mode_to_ceiling(BodyMode::Stream, BodyMode::Stream);
        assert_eq!(
            clamped,
            BodyMode::Stream,
            "Stream baseline imposes no ceiling; Stream mode passes through"
        );
    }

    #[test]
    fn clamp_body_mode_to_ceiling_size_limit_clamped_to_baseline() {
        let clamped = super::super::clamp_body_mode_to_ceiling(
            BodyMode::SizeLimit { max_bytes: 8192 },
            BodyMode::SizeLimit { max_bytes: 2048 },
        );
        assert_eq!(
            clamped,
            BodyMode::SizeLimit { max_bytes: 2048 },
            "runtime SizeLimit should be clamped to baseline ceiling"
        );
    }

    #[test]
    fn clamp_body_mode_to_ceiling_no_ceiling_passes_through() {
        let clamped = super::super::clamp_body_mode_to_ceiling(
            BodyMode::StreamBuffer { max_bytes: Some(4096) },
            BodyMode::StreamBuffer { max_bytes: None },
        );
        assert_eq!(
            clamped,
            BodyMode::StreamBuffer { max_bytes: Some(4096) },
            "unbounded baseline imposes no ceiling; runtime mode passes through"
        );
    }

    #[test]
    fn clamp_body_mode_to_ceiling_within_limit_unchanged() {
        let clamped = super::super::clamp_body_mode_to_ceiling(
            BodyMode::StreamBuffer { max_bytes: Some(512) },
            BodyMode::StreamBuffer { max_bytes: Some(1024) },
        );
        assert_eq!(
            clamped,
            BodyMode::StreamBuffer { max_bytes: Some(512) },
            "runtime limit within baseline ceiling should be unchanged"
        );
    }

    // -------------------------------------------------------------------------
    // Pre-Read Mutation Propagation
    // -------------------------------------------------------------------------

    #[test]
    fn pre_read_remove_deletes_from_request() {
        let mut req = make_request_with_header("x-remove-me", "gone");
        let mutations = make_mutations_remove("x-remove-me");

        apply_pre_read_mutations_to_request(&mut req, &mutations);

        assert!(
            req.headers.get("x-remove-me").is_none(),
            "removed header should be absent from request"
        );
    }

    #[test]
    fn pre_read_set_overwrites_in_request() {
        let mut req = make_request_with_header("x-overwrite", "old");
        let mutations = make_mutations_set("x-overwrite", "new");

        apply_pre_read_mutations_to_request(&mut req, &mutations);

        assert_eq!(
            req.headers.get("x-overwrite").unwrap(),
            "new",
            "set mutation should overwrite existing value"
        );
    }

    #[test]
    fn pre_read_add_inserts_into_request() {
        let mut req = make_request();
        let mutations = make_mutations_add("x-added", "value");

        apply_pre_read_mutations_to_request(&mut req, &mutations);

        assert_eq!(
            req.headers.get("x-added").unwrap(),
            "value",
            "add mutation should add header"
        );
    }

    #[test]
    fn pre_read_add_overwrites_existing_in_request() {
        let mut req = make_request_with_header("x-existing", "old");
        let mutations = make_mutations_add("x-existing", "new");

        apply_pre_read_mutations_to_request(&mut req, &mutations);

        assert_eq!(
            req.headers.get("x-existing").unwrap(),
            "new",
            "add mutation should overwrite existing header"
        );
    }

    #[test]
    fn pre_read_ordered_remove_then_set_then_add() {
        let mut req = make_request_with_header("x-target", "original");
        let mutations = vec![
            praxis_filter::TrustedHeaderMutation::Remove("x-target".parse().unwrap()),
            praxis_filter::TrustedHeaderMutation::Set("x-target".parse().unwrap(), "from-set".parse().unwrap()),
            praxis_filter::TrustedHeaderMutation::Add("x-target".parse().unwrap(), "from-add".to_owned()),
        ];

        apply_pre_read_mutations_to_request(&mut req, &mutations);

        assert_eq!(
            req.headers.get("x-target").unwrap(),
            "from-add",
            "add (last writer) should win after remove+set+add sequence"
        );
    }

    #[test]
    fn pre_read_empty_mutations_is_noop() {
        let mut req = make_request_with_header("x-keep", "kept");
        let mutations: Vec<praxis_filter::TrustedHeaderMutation> = Vec::new();

        apply_pre_read_mutations_to_request(&mut req, &mutations);

        assert_eq!(
            req.headers.get("x-keep").unwrap(),
            "kept",
            "empty mutations should not change request"
        );
    }

    // -------------------------------------------------------------------------
    // Production Lifecycle Tests
    //
    // Stop condition: `request_filter::execute` and
    // `stream_buffer::pre_read_body` require a live `pingora_proxy::Session`
    // created by Pingora's internal networking stack. `Session` is an opaque
    // type that cannot be constructed outside a real HTTP connection
    // lifecycle (see `convert.rs` doc comments). These tests therefore
    // exercise the nearest testable production helpers — `run_pipeline`
    // for request-phase execution, plus `execute()` snapshot stripping
    // logic and `upstream_peer::execute` for peer selection — without
    // manually reproducing their internal logic.
    //
    // The remaining Session-dependent boundary (pre-read body I/O,
    // session header promotion, session-level stripping) is closed by
    // the unchanged-Go-EPP local smoke test.
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn lifecycle_provenance_through_selector_strip_and_peer() {
        let registry = FilterRegistry::with_builtins();
        let config: serde_yaml::Value = serde_yaml::from_str(
            "source_header: x-gateway-dest\nrequired: true\nstrip_header: true\nstatus_on_required_failure: 503",
        )
        .unwrap();
        let mut entries = vec![praxis_filter::FilterEntry {
            branch_chains: None,
            filter_type: "endpoint_selector".into(),
            config,
            conditions: vec![],
            name: None,
            response_conditions: vec![],
            failure_mode: FailureMode::default(),
        }];
        let pipeline = FilterPipeline::build(&mut entries, &registry).unwrap();

        let mut req = make_request();
        req.headers
            .insert("x-gateway-dest", "evil.client:6666".parse().unwrap());

        let mut ctx = make_ctx();
        ctx.pre_read_mutations = vec![
            praxis_filter::TrustedHeaderMutation::Remove("x-gateway-dest".parse().unwrap()),
            praxis_filter::TrustedHeaderMutation::Add("x-gateway-dest".parse().unwrap(), "127.0.0.1:8080".to_owned()),
        ];

        let result = run_pipeline(&pipeline, req, &mut ctx).await.unwrap();
        ctx.pre_read_mutations.clear();

        assert!(
            matches!(result.action, FilterAction::Continue),
            "pipeline should continue after selecting endpoint"
        );

        assert_eq!(
            ctx.upstream.as_ref().map(|u| &*u.address),
            Some("127.0.0.1:8080"),
            "upstream should be the trusted EPP destination, not the client header"
        );

        assert!(
            ctx.pre_read_mutations.is_empty(),
            "provenance must be empty after normal pipeline"
        );

        if let Some(ref mut snapshot) = ctx.request_snapshot {
            for name in &result.headers_to_remove {
                snapshot.headers.remove(name);
            }
        }

        let snapshot = ctx.request_snapshot.as_ref().expect("snapshot should be stored");
        assert!(
            snapshot.headers.get("x-gateway-dest").is_none(),
            "routing header must be absent from snapshot after stripping"
        );

        let peer = super::super::upstream_peer::execute(&mut ctx).await.unwrap();
        assert_eq!(
            peer.address().to_string(),
            "127.0.0.1:8080",
            "peer selection must consume the endpoint_selector upstream"
        );

        assert!(
            ctx.upstream.is_none(),
            "upstream should be moved to upstream_for_retry after peer selection"
        );
        assert!(
            ctx.upstream_for_retry.is_some(),
            "upstream_for_retry should hold the selected upstream"
        );
    }

    #[tokio::test]
    async fn lifecycle_required_503_when_epp_omits_destination() {
        let registry = FilterRegistry::with_builtins();
        let config: serde_yaml::Value =
            serde_yaml::from_str("source_header: x-gateway-dest\nrequired: true\nstatus_on_required_failure: 503")
                .unwrap();
        let mut entries = vec![praxis_filter::FilterEntry {
            branch_chains: None,
            filter_type: "endpoint_selector".into(),
            config,
            conditions: vec![],
            name: None,
            response_conditions: vec![],
            failure_mode: FailureMode::default(),
        }];
        let pipeline = FilterPipeline::build(&mut entries, &registry).unwrap();

        let mut req = make_request();
        req.headers
            .insert("x-gateway-dest", "evil.client:9999".parse().unwrap());

        let mut ctx = make_ctx();

        let result = run_pipeline(&pipeline, req, &mut ctx).await.unwrap();

        assert!(
            matches!(result.action, FilterAction::Reject(ref r) if r.status == 503),
            "required mode with no trusted destination must reject with 503"
        );
        assert!(ctx.upstream.is_none(), "no upstream should be set on routing failure");
    }

    // -------------------------------------------------------------------------
    // Test Utilities
    // -------------------------------------------------------------------------

    /// Create a minimal GET request for tests.
    fn make_request() -> Request {
        Request {
            method: Method::GET,
            uri: Uri::from_static("/"),
            headers: HeaderMap::new(),
        }
    }

    /// Create a default request context for tests.
    fn make_ctx() -> PingoraRequestCtx {
        PingoraRequestCtx::default()
    }

    /// Build an empty filter pipeline for tests.
    fn empty_pipeline() -> FilterPipeline {
        let registry = FilterRegistry::with_builtins();
        FilterPipeline::build(&mut [], &registry).unwrap()
    }

    /// Create a request with a single header.
    fn make_request_with_header(name: &str, value: &str) -> Request {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            http::header::HeaderValue::from_str(value).unwrap(),
        );
        Request {
            method: Method::GET,
            uri: Uri::from_static("/"),
            headers,
        }
    }

    /// Build mutations with a single remove entry.
    fn make_mutations_remove(name: &str) -> Vec<praxis_filter::TrustedHeaderMutation> {
        vec![praxis_filter::TrustedHeaderMutation::Remove(name.parse().unwrap())]
    }

    /// Build mutations with a single set/overwrite entry.
    fn make_mutations_set(name: &str, value: &str) -> Vec<praxis_filter::TrustedHeaderMutation> {
        vec![praxis_filter::TrustedHeaderMutation::Set(
            name.parse().unwrap(),
            value.parse().unwrap(),
        )]
    }

    /// Build mutations with a single add/append entry.
    fn make_mutations_add(name: &str, value: &str) -> Vec<praxis_filter::TrustedHeaderMutation> {
        vec![praxis_filter::TrustedHeaderMutation::Add(
            name.parse().unwrap(),
            value.to_owned(),
        )]
    }

    /// Build a pipeline with a single `static_response` filter that rejects.
    fn rejecting_pipeline(status: u16) -> FilterPipeline {
        let registry = FilterRegistry::with_builtins();
        let yaml = format!("status: {status}");
        let config: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let mut entries = vec![praxis_filter::FilterEntry {
            branch_chains: None,
            filter_type: "static_response".into(),
            config,
            conditions: vec![],
            name: None,
            response_conditions: vec![],
            failure_mode: FailureMode::default(),
        }];
        FilterPipeline::build(&mut entries, &registry).unwrap()
    }
}
