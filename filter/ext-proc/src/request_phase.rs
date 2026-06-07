// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Request-phase `ext_proc` callout for Track B (llm-d EPP integration).
//!
//! Sends request headers and the complete buffered request body on a
//! single [`ExternalProcessor::Process`] stream, then returns
//! structured request-phase results for the later `llmd_external_epp`
//! filter.
//!
//! Unlike the single-message callouts in the `callout` module, this
//! helper sends two [`ProcessingRequest`] messages (headers then body)
//! and reads one or more [`ProcessingResponse`] messages before
//! returning.
//!
//! The Go EPP returns request body mutations as
//! [`StreamedResponse`] chunks (up to 62 KB each), possibly across
//! multiple `ProcessingResponse::RequestBody` messages, with
//! `end_of_stream = true` on the final chunk. This module reassembles
//! those chunks into a single contiguous body in
//! [`RequestPhaseResult::mutated_body`].
//!
//! Streamed body sequences are fail-closed: if the stream ends before
//! a chunk with `end_of_stream = true`, or a non-streamed mutation
//! type appears mid-sequence, the exchange returns an error rather
//! than forwarding a partial or ambiguous body.
//!
//! [`ExternalProcessor::Process`]: praxis_proto::envoy::service::ext_proc::v3::external_processor_client::ExternalProcessorClient::process
//! [`ProcessingRequest`]: praxis_proto::envoy::service::ext_proc::v3::ProcessingRequest
//! [`ProcessingResponse`]: praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse
//! [`StreamedResponse`]: praxis_proto::envoy::service::ext_proc::v3::StreamedBodyResponse

use std::time::Duration;

use bytes::Bytes;
use futures::stream;
use praxis_proto::envoy::service::ext_proc::v3::{
    BodyResponse, HeadersResponse, HttpBody, HttpHeaders, ImmediateResponse, ProcessingRequest, body_mutation,
    external_processor_client::ExternalProcessorClient, processing_request, processing_response,
};
use tonic::transport::Channel;

use crate::{callout::response_variant_name, mutations::header_value_string};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Header key used by the Go EPP to communicate the selected endpoint.
pub const DESTINATION_ENDPOINT_HEADER: &str = "x-gateway-destination-endpoint";

// -----------------------------------------------------------------------------
// RequestPhaseResult
// -----------------------------------------------------------------------------

/// Structured output from a request-phase `ext_proc` exchange.
///
/// Contains all data the later `llmd_external_epp` filter needs to
/// apply header mutations, set the upstream endpoint, replace the
/// request body, or short-circuit with an immediate response.
#[derive(Debug, Default)]
pub struct RequestPhaseResult {
    /// Header-phase response from the external processor.
    pub headers_response: Option<HeadersResponse>,

    /// Last body-phase response from the external processor.
    ///
    /// When the EPP sends multiple `RequestBody` responses (streamed
    /// chunks), this contains only the final one. The reassembled
    /// body is in [`mutated_body`](Self::mutated_body).
    pub body_response: Option<BodyResponse>,

    /// Selected endpoint extracted from the
    /// [`DESTINATION_ENDPOINT_HEADER`] header in the header-phase
    /// response mutation. `None` when the header is absent.
    pub selected_endpoint: Option<String>,

    /// Mutated request body bytes. Populated when the body-phase
    /// response contains a [`Body`] replacement, a [`ClearBody`]
    /// directive, or one or more [`StreamedResponse`] chunks
    /// (reassembled in order).
    ///
    /// [`Body`]: body_mutation::Mutation::Body
    /// [`ClearBody`]: body_mutation::Mutation::ClearBody
    /// [`StreamedResponse`]: body_mutation::Mutation::StreamedResponse
    pub mutated_body: Option<Bytes>,

    /// Immediate response from the external processor, if it chose
    /// to short-circuit request processing.
    pub immediate_response: Option<ImmediateResponse>,
}

// -----------------------------------------------------------------------------
// RequestPhaseError
// -----------------------------------------------------------------------------

/// Errors from a request-phase `ext_proc` exchange.
#[derive(Debug, thiserror::Error)]
pub enum RequestPhaseError {
    /// gRPC transport or protocol error.
    #[error("ext_proc request phase gRPC error: {0}")]
    Grpc(#[from] tonic::Status),

    /// The overall request-phase timeout expired.
    #[error("ext_proc request phase timed out")]
    Timeout,

    /// The server closed the stream without sending any response.
    #[error("ext_proc server closed stream without response")]
    EmptyStream,

    /// The server sent a response type that does not belong in the
    /// request phase (e.g. `ResponseHeaders` when we expected
    /// `RequestHeaders` or `RequestBody`).
    #[error("ext_proc unexpected response type '{0}' during request phase")]
    UnexpectedResponse(String),

    /// A streamed body sequence was incomplete or mixed with
    /// non-streamed mutation types before `end_of_stream = true`.
    #[error("ext_proc incomplete body stream: {0}")]
    IncompleteBodyStream(String),
}

// -----------------------------------------------------------------------------
// BodyAction (internal dispatch)
// -----------------------------------------------------------------------------

/// Classified action from a single body-phase response.
enum BodyAction {
    /// A `StreamedResponse` chunk with its `end_of_stream` flag.
    Streamed {
        /// Raw body bytes for this chunk.
        body: Vec<u8>,
        /// Whether this is the final chunk.
        end_of_stream: bool,
    },

    /// A direct `Body(bytes)` replacement (single message).
    Replace(Vec<u8>),

    /// A `ClearBody(true)` directive.
    Clear,

    /// No body mutation present in the response.
    None,
}

// -----------------------------------------------------------------------------
// Public API
// -----------------------------------------------------------------------------

/// Execute the request-phase `ext_proc` exchange on a single stream.
///
/// Opens one [`ExternalProcessor::Process`] bidirectional stream and
/// sends two messages in order:
///
/// 1. `RequestHeaders` with `end_of_stream = false`.
/// 2. `RequestBody` with `end_of_stream = true`.
///
/// Then reads responses: one `RequestHeaders` response (or
/// `ImmediateResponse`), followed by one or more `RequestBody`
/// responses. The Go EPP may send multiple `RequestBody` responses
/// with [`StreamedResponse`] chunks; these are reassembled into
/// [`RequestPhaseResult::mutated_body`].
///
/// The timeout covers the entire exchange including stream setup.
///
/// # Errors
///
/// Returns [`RequestPhaseError::Grpc`] on transport errors,
/// [`RequestPhaseError::Timeout`] if the deadline expires,
/// [`RequestPhaseError::EmptyStream`] if the server closes without
/// responding, [`RequestPhaseError::UnexpectedResponse`] for wrong
/// response types, or [`RequestPhaseError::IncompleteBodyStream`]
/// when a streamed body sequence ends without `end_of_stream = true`
/// or is mixed with non-streamed mutation types.
///
/// [`ExternalProcessor::Process`]: ExternalProcessorClient::process
/// [`StreamedResponse`]: praxis_proto::envoy::service::ext_proc::v3::StreamedBodyResponse
pub async fn process_request_phase(
    channel: Channel,
    headers: HttpHeaders,
    body: Bytes,
    timeout: Duration,
) -> Result<RequestPhaseResult, RequestPhaseError> {
    let requests = build_requests(headers, &body);

    let inner = tokio::time::timeout(timeout, execute_stream(channel, requests)).await;

    match inner {
        Ok(r) => r,
        Err(_elapsed) => Err(RequestPhaseError::Timeout),
    }
}

// -----------------------------------------------------------------------------
// Private helpers
// -----------------------------------------------------------------------------

/// Build the header and body request pair.
fn build_requests(headers: HttpHeaders, body: &Bytes) -> Vec<ProcessingRequest> {
    let header_request = ProcessingRequest {
        request: Some(processing_request::Request::RequestHeaders(HttpHeaders {
            end_of_stream: false,
            ..headers
        })),
        ..Default::default()
    };
    let body_request = ProcessingRequest {
        request: Some(processing_request::Request::RequestBody(HttpBody {
            body: body.to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    };
    vec![header_request, body_request]
}

/// Open the stream and read responses.
async fn execute_stream(
    channel: Channel,
    requests: Vec<ProcessingRequest>,
) -> Result<RequestPhaseResult, RequestPhaseError> {
    let mut client = ExternalProcessorClient::new(channel);
    let request_stream = stream::iter(requests);
    let response = client.process(request_stream).await.map_err(RequestPhaseError::Grpc)?;
    let mut resp_stream = response.into_inner();

    let mut result = RequestPhaseResult::default();

    read_header_response(&mut resp_stream, &mut result).await?;

    if result.immediate_response.is_none() {
        read_body_responses(&mut resp_stream, &mut result).await?;
    }

    drain_stream(&mut resp_stream).await;

    Ok(result)
}

/// Consume remaining messages so the h2 stream closes cleanly
/// instead of being reset. Prevents `too_many_internal_resets`
/// GOAWAY under sustained load on a shared connection.
///
/// Uses a short timeout to avoid blocking on servers that keep
/// the stream open for response-phase processing.
async fn drain_stream(stream: &mut tonic::Streaming<praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse>) {
    let drain = async { while stream.message().await.is_ok_and(|m| m.is_some()) {} };
    drop(tokio::time::timeout(Duration::from_millis(5), drain).await);
}

/// Read and dispatch the header-phase response from the stream.
async fn read_header_response(
    stream: &mut tonic::Streaming<praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse>,
    result: &mut RequestPhaseResult,
) -> Result<(), RequestPhaseError> {
    let msg = stream
        .message()
        .await
        .map_err(RequestPhaseError::Grpc)?
        .ok_or(RequestPhaseError::EmptyStream)?;

    let Some(resp) = msg.response else {
        return Ok(());
    };

    match resp {
        processing_response::Response::RequestHeaders(hr) => {
            result.selected_endpoint = extract_selected_endpoint(&hr);
            result.headers_response = Some(hr);
        },
        processing_response::Response::ImmediateResponse(imm) => {
            result.immediate_response = Some(imm);
        },
        other => {
            return Err(RequestPhaseError::UnexpectedResponse(
                response_variant_name(&other).to_owned(),
            ));
        },
    }

    Ok(())
}

/// Read body-phase responses from the stream.
///
/// The Go EPP may send one or more `RequestBody` responses:
///
/// - A single `Body(bytes)` or `ClearBody(true)` — handled as a one-shot replacement.
/// - One or more `StreamedResponse` chunks — accumulated and reassembled into a single contiguous body.
///
/// The server may also close the stream after the header response
/// without sending a body response. This is not an error.
///
/// Fail-closed: once a streamed chunk is received, the sequence must
/// complete with `end_of_stream = true`. Stream closure, a
/// non-streamed mutation type, or a no-mutation response before that
/// flag is an error.
async fn read_body_responses(
    stream: &mut tonic::Streaming<praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse>,
    result: &mut RequestPhaseResult,
) -> Result<(), RequestPhaseError> {
    let mut chunks: Vec<Vec<u8>> = Vec::new();

    while let BodyMessageOutcome::Chunk { body, done } =
        read_one_body_message(stream, result, !chunks.is_empty()).await?
    {
        chunks.push(body);
        if done {
            break;
        }
    }

    if !chunks.is_empty() && result.immediate_response.is_none() {
        result.mutated_body = Some(assemble_chunks(&chunks));
    }

    Ok(())
}

/// Outcome from reading a single body-phase message.
enum BodyMessageOutcome {
    /// A streamed chunk was received. If `done`, stop reading.
    Chunk {
        /// Raw bytes for this chunk.
        body: Vec<u8>,
        /// Whether this is the final chunk.
        done: bool,
    },

    /// Body phase is complete (stream closed without prior chunks,
    /// immediate response, one-shot `Body`/`ClearBody`, or no
    /// mutation).
    Complete,
}

/// Read one body-phase message and update result accordingly.
///
/// When `in_streamed_sequence` is true, stream closure and
/// non-streamed body actions are protocol violations.
async fn read_one_body_message(
    stream: &mut tonic::Streaming<praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse>,
    result: &mut RequestPhaseResult,
    in_streamed_sequence: bool,
) -> Result<BodyMessageOutcome, RequestPhaseError> {
    let Some(msg) = stream.message().await.map_err(RequestPhaseError::Grpc)? else {
        if in_streamed_sequence {
            return Err(RequestPhaseError::IncompleteBodyStream(
                "stream closed before end_of_stream".to_owned(),
            ));
        }
        return Ok(BodyMessageOutcome::Complete);
    };

    let Some(resp) = msg.response else {
        if in_streamed_sequence {
            return Err(RequestPhaseError::IncompleteBodyStream(
                "empty response during streamed body sequence".to_owned(),
            ));
        }
        return Ok(BodyMessageOutcome::Complete);
    };

    dispatch_body_response(resp, result, in_streamed_sequence)
}

/// Dispatch a body-phase response variant.
fn dispatch_body_response(
    resp: processing_response::Response,
    result: &mut RequestPhaseResult,
    in_streamed_sequence: bool,
) -> Result<BodyMessageOutcome, RequestPhaseError> {
    match resp {
        processing_response::Response::RequestBody(br) => {
            let outcome = apply_body_action(classify_body_mutation(&br), result, in_streamed_sequence)?;
            result.body_response = Some(br);
            Ok(outcome)
        },
        processing_response::Response::ImmediateResponse(imm) => {
            result.immediate_response = Some(imm);
            Ok(BodyMessageOutcome::Complete)
        },
        other => Err(RequestPhaseError::UnexpectedResponse(
            response_variant_name(&other).to_owned(),
        )),
    }
}

/// Apply a classified body action and return the read-loop outcome.
///
/// When `in_streamed_sequence` is true, only `Streamed` actions are
/// valid. `Replace`, `Clear`, and `None` are protocol violations.
fn apply_body_action(
    action: BodyAction,
    result: &mut RequestPhaseResult,
    in_streamed_sequence: bool,
) -> Result<BodyMessageOutcome, RequestPhaseError> {
    match action {
        BodyAction::Streamed { body, end_of_stream } => Ok(BodyMessageOutcome::Chunk {
            body,
            done: end_of_stream,
        }),
        BodyAction::Replace(data) => {
            reject_if_mid_stream(in_streamed_sequence, "Body")?;
            result.mutated_body = Some(Bytes::from(data));
            Ok(BodyMessageOutcome::Complete)
        },
        BodyAction::Clear => {
            reject_if_mid_stream(in_streamed_sequence, "ClearBody")?;
            result.mutated_body = Some(Bytes::new());
            Ok(BodyMessageOutcome::Complete)
        },
        BodyAction::None => {
            reject_if_mid_stream(in_streamed_sequence, "no-mutation response")?;
            Ok(BodyMessageOutcome::Complete)
        },
    }
}

/// Return an error if a non-streamed action appears mid-sequence.
fn reject_if_mid_stream(in_streamed_sequence: bool, action: &str) -> Result<(), RequestPhaseError> {
    if in_streamed_sequence {
        return Err(RequestPhaseError::IncompleteBodyStream(format!(
            "{action} received during streamed sequence"
        )));
    }
    Ok(())
}

/// Classify a body-phase response into a [`BodyAction`].
fn classify_body_mutation(br: &BodyResponse) -> BodyAction {
    let Some(common) = &br.response else {
        return BodyAction::None;
    };
    let Some(bm) = &common.body_mutation else {
        return BodyAction::None;
    };

    match &bm.mutation {
        Some(body_mutation::Mutation::StreamedResponse(sr)) => BodyAction::Streamed {
            body: sr.body.clone(),
            end_of_stream: sr.end_of_stream,
        },
        Some(body_mutation::Mutation::Body(data)) => BodyAction::Replace(data.clone()),
        Some(body_mutation::Mutation::ClearBody(true)) => BodyAction::Clear,
        _ => BodyAction::None,
    }
}

/// Concatenate streamed chunks into a single [`Bytes`].
fn assemble_chunks(chunks: &[Vec<u8>]) -> Bytes {
    let total: usize = chunks.iter().map(Vec::len).sum();
    let mut assembled = Vec::with_capacity(total);
    for chunk in chunks {
        assembled.extend_from_slice(chunk);
    }
    Bytes::from(assembled)
}

/// Extract the selected endpoint from the header-phase response.
///
/// Searches the `set_headers` list in the response's header mutation
/// for [`DESTINATION_ENDPOINT_HEADER`] (case-insensitive). Returns
/// the first non-empty value found, or `None`.
fn extract_selected_endpoint(hr: &HeadersResponse) -> Option<String> {
    let common = hr.response.as_ref()?;
    let mutation = common.header_mutation.as_ref()?;

    for hvo in &mutation.set_headers {
        if let Some(hv) = &hvo.header
            && hv.key.eq_ignore_ascii_case(DESTINATION_ENDPOINT_HEADER)
        {
            let value = header_value_string(hv);
            if !value.is_empty() {
                return Some(value);
            }
        }
    }

    None
}
