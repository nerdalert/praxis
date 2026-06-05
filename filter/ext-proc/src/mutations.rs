// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Proto-to-Praxis conversions for `ext_proc` header mutations.
//!
//! Translates between the Envoy `ext_proc` protobuf types and
//! Praxis filter context operations: building [`HttpHeaders`]
//! from request/response state and applying [`HeaderMutation`]
//! and [`ImmediateResponse`] results back to the context.
//!
//! [`HttpHeaders`]: praxis_proto::envoy::service::ext_proc::v3::HttpHeaders
//! [`HeaderMutation`]: praxis_proto::envoy::service::ext_proc::v3::HeaderMutation
//! [`ImmediateResponse`]: praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse

use std::borrow::Cow;

use bytes::Bytes;
use praxis_filter::{FilterAction, HttpFilterContext, Rejection};
use praxis_proto::envoy::service::{
    common::v3::{HeaderValue, HeaderValueOption},
    ext_proc::v3::{HeaderMutation, HeadersResponse, HttpHeaders, ImmediateResponse},
};

use crate::Phase;

// -----------------------------------------------------------------------------
// Request → Proto
// -----------------------------------------------------------------------------

/// Build [`HttpHeaders`] from the current request context.
///
/// Includes `:method` and `:path` pseudo-headers followed by all
/// request headers, matching the Envoy `ext_proc` convention that
/// external processors expect.
pub(crate) fn request_to_proto_headers(ctx: &HttpFilterContext<'_>) -> HttpHeaders {
    let mut headers = Vec::new();

    headers.push(HeaderValue {
        key: ":method".to_owned(),
        value: ctx.request.method.as_str().to_owned(),
        raw_value: Vec::new(),
    });
    headers.push(HeaderValue {
        key: ":path".to_owned(),
        value: ctx
            .request
            .uri
            .path_and_query()
            .map_or(ctx.request.uri.path(), http::uri::PathAndQuery::as_str)
            .to_owned(),
        raw_value: Vec::new(),
    });

    for (name, value) in &ctx.request.headers {
        headers.push(HeaderValue {
            key: name.as_str().to_owned(),
            value: value.to_str().unwrap_or_default().to_owned(),
            raw_value: Vec::new(),
        });
    }

    HttpHeaders {
        headers: Some(praxis_proto::envoy::service::ext_proc::v3::HeaderMap { headers }),
        end_of_stream: false,
    }
}

/// Build [`HttpHeaders`] from the upstream response context.
///
/// Includes a `:status` pseudo-header followed by all response
/// headers. Returns empty headers when `ctx.response_header` is
/// `None` (should not happen during the response phase).
pub(crate) fn response_to_proto_headers(ctx: &HttpFilterContext<'_>) -> HttpHeaders {
    let mut headers = Vec::new();

    if let Some(resp) = ctx.response_header.as_ref() {
        headers.push(HeaderValue {
            key: ":status".to_owned(),
            value: resp.status.as_u16().to_string(),
            raw_value: Vec::new(),
        });

        for (name, value) in &resp.headers {
            headers.push(HeaderValue {
                key: name.as_str().to_owned(),
                value: value.to_str().unwrap_or_default().to_owned(),
                raw_value: Vec::new(),
            });
        }
    }

    HttpHeaders {
        headers: Some(praxis_proto::envoy::service::ext_proc::v3::HeaderMap { headers }),
        end_of_stream: false,
    }
}

// -----------------------------------------------------------------------------
// Proto → Praxis mutations
// -----------------------------------------------------------------------------

/// Apply a [`HeadersResponse`] to the filter context.
///
/// Delegates to request or response mutation based on the
/// current processing [`Phase`].
pub(crate) fn apply_headers_response(hr: &HeadersResponse, ctx: &mut HttpFilterContext<'_>, phase: Phase) {
    let Some(common) = &hr.response else {
        return;
    };
    let Some(mutation) = &common.header_mutation else {
        return;
    };

    match phase {
        Phase::Request => apply_request_header_mutation(mutation, ctx),
        Phase::Response => apply_response_header_mutation(mutation, ctx),
    }
}

/// Apply header mutations to the upstream request.
///
/// Adds headers via [`HttpFilterContext::extra_request_headers`].
/// Pseudo-headers (`:` prefix) are skipped because Praxis sets
/// method and path through dedicated context fields. Removals
/// are logged and skipped: request headers are immutable by
/// filter time in the Praxis pipeline.
pub(crate) fn apply_request_header_mutation(mutation: &HeaderMutation, ctx: &mut HttpFilterContext<'_>) {
    for hvo in &mutation.set_headers {
        if let Some(hv) = &hvo.header {
            if is_pseudo_header(&hv.key) {
                continue;
            }
            let value = header_value_string(hv);
            ctx.extra_request_headers.push((Cow::Owned(hv.key.clone()), value));
        }
    }

    for name in &mutation.remove_headers {
        if is_pseudo_header(name) {
            continue;
        }
        tracing::debug!(
            header = %name,
            "ext_proc: skipping request header removal (request headers are immutable)"
        );
    }
}

/// Apply header mutations to the upstream response.
///
/// Modifies [`HttpFilterContext::response_header`] directly and
/// sets [`HttpFilterContext::response_headers_modified`] when
/// any mutation is applied. Pseudo-headers are skipped.
pub(crate) fn apply_response_header_mutation(mutation: &HeaderMutation, ctx: &mut HttpFilterContext<'_>) {
    let Some(resp) = ctx.response_header.as_mut() else {
        return;
    };

    let sets = set_response_headers(&mutation.set_headers, resp);
    let removes = remove_response_headers(&mutation.remove_headers, resp);

    if sets || removes {
        ctx.response_headers_modified = true;
    }
}

/// Apply set-header mutations to a response, returning whether any were applied.
fn set_response_headers(headers: &[HeaderValueOption], resp: &mut praxis_filter::Response) -> bool {
    let mut modified = false;
    for hvo in headers {
        if let Some(hv) = &hvo.header {
            if is_pseudo_header(&hv.key) {
                continue;
            }
            let value = header_value_string(hv);
            if let (Ok(name), Ok(val)) = (http::HeaderName::try_from(&hv.key), http::HeaderValue::try_from(&value)) {
                if should_append(hvo) {
                    resp.headers.append(name, val);
                } else {
                    resp.headers.insert(name, val);
                }
                modified = true;
            }
        }
    }
    modified
}

/// Apply remove-header mutations to a response, returning whether any were applied.
fn remove_response_headers(names: &[String], resp: &mut praxis_filter::Response) -> bool {
    let mut modified = false;
    for name in names {
        if is_pseudo_header(name) {
            continue;
        }
        if let Ok(header_name) = http::HeaderName::try_from(name.as_str()) {
            resp.headers.remove(&header_name);
            modified = true;
        }
    }
    modified
}

/// Convert an [`ImmediateResponse`] to a [`FilterAction::Reject`].
///
/// Maps the proto status code (defaulting to 200 when absent),
/// body, and response headers to a [`Rejection`].
pub(crate) fn immediate_to_rejection(imm: &ImmediateResponse) -> FilterAction {
    let status = imm.status.as_ref().map_or(200, |s| {
        let code = s.code;
        u16::try_from(code).unwrap_or(500)
    });

    let status = if (100..=599).contains(&status) { status } else { 500 };

    let mut rejection = Rejection::status(status);

    if !imm.body.is_empty() {
        rejection = rejection.with_body(Bytes::copy_from_slice(imm.body.as_bytes()));
    }

    if let Some(hm) = &imm.headers {
        for hvo in &hm.set_headers {
            if let Some(hv) = &hvo.header {
                let value = header_value_string(hv);
                rejection = rejection.with_header(hv.key.clone(), value);
            }
        }
    }

    FilterAction::Reject(rejection)
}

// -----------------------------------------------------------------------------
// Utilities
// -----------------------------------------------------------------------------

/// Extract the string value from a [`HeaderValue`].
///
/// Prefers `raw_value` (as UTF-8) over `value` when non-empty,
/// matching the Envoy convention where `raw_value` carries the
/// original bytes.
pub(crate) fn header_value_string(hv: &HeaderValue) -> String {
    if hv.raw_value.is_empty() {
        hv.value.clone()
    } else {
        String::from_utf8_lossy(&hv.raw_value).into_owned()
    }
}

/// Returns `true` if the header name is an HTTP/2 pseudo-header.
pub(crate) fn is_pseudo_header(name: &str) -> bool {
    name.starts_with(':')
}

/// Whether the [`HeaderValueOption`] indicates an append operation.
///
/// Uses `append_action` when set (non-zero), falling back to the
/// deprecated `append` field. Default behaviour (both unset) is
/// append, matching the proto3 default of `APPEND_IF_EXISTS_OR_ADD`
/// (enum value 0).
fn should_append(hvo: &HeaderValueOption) -> bool {
    use praxis_proto::envoy::service::common::v3::header_value_option::HeaderAppendAction;

    if hvo.append_action != 0 {
        return hvo.append_action == HeaderAppendAction::AppendIfExistsOrAdd as i32;
    }

    // proto3 default for append_action is 0 (APPEND_IF_EXISTS_OR_ADD).
    // Fall back to deprecated `append`; default to true (append)
    // when neither field is explicitly set.
    hvo.append.unwrap_or(true)
}
