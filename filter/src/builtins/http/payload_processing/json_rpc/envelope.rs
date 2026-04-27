// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! JSON-RPC 2.0 envelope parsing and metadata extraction.

use serde_json::Value;

use super::config::{BatchPolicy, InvalidJsonRpcBehavior, JsonRpcConfig};

// -----------------------------------------------------------------------------
// JSON-RPC Types
// -----------------------------------------------------------------------------

/// JSON-RPC message kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum JsonRpcKind {
    /// Request with id (expects response).
    Request,
    /// Notification without id (no response expected).
    Notification,
    /// Response with id and result/error.
    Response,
    /// Batch array of requests/notifications/responses.
    Batch,
}

impl JsonRpcKind {
    /// Get the string representation for headers and filter results.
    pub(super) fn as_str(&self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Notification => "notification",
            Self::Response => "response",
            Self::Batch => "batch",
        }
    }
}

/// JSON-RPC id type for proper string conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum JsonRpcIdKind {
    /// String id.
    String,
    /// Integer id (i64/u64).
    Integer,
    /// Numeric id (f64).
    Number,
    /// Null id.
    Null,
    /// Missing id (notification).
    Missing,
}

impl JsonRpcIdKind {
    /// Get the string representation for filter results.
    pub(super) fn as_str(&self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Null => "null",
            Self::Missing => "missing",
        }
    }
}

/// Parsed JSON-RPC envelope metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct JsonRpcEnvelope {
    /// Message kind (request/notification/response/batch).
    pub kind: JsonRpcKind,
    /// Method name (for requests and notifications).
    pub method: Option<String>,
    /// ID as string (for requests and responses).
    pub id: Option<String>,
    /// ID type classification.
    pub id_kind: JsonRpcIdKind,
    /// Batch length (for batches only).
    pub batch_len: Option<usize>,
}

// -----------------------------------------------------------------------------
// Parser Errors
// -----------------------------------------------------------------------------

/// JSON-RPC parsing error.
#[derive(Debug, Clone)]
pub(super) enum JsonRpcParseError {
    /// Invalid JSON.
    InvalidJson(String),
    /// Missing required jsonrpc field.
    MissingVersion,
    /// Wrong jsonrpc version.
    WrongVersion(String),
    /// Missing method for request/notification.
    MissingMethod,
    /// Method is not a string.
    InvalidMethod,
    /// Invalid id type (JSON-RPC 2.0 only allows string, number, or null).
    InvalidId,
    /// Unsupported batch (based on policy).
    UnsupportedBatch,
    /// Empty batch array.
    EmptyBatch,
}

impl std::fmt::Display for JsonRpcParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidJson(e) => write!(f, "invalid JSON: {e}"),
            Self::MissingVersion => write!(f, "missing 'jsonrpc' field"),
            Self::WrongVersion(v) => write!(f, "wrong jsonrpc version: '{v}', expected '2.0'"),
            Self::MissingMethod => write!(f, "missing 'method' field for request/notification"),
            Self::InvalidMethod => write!(f, "'method' must be a string"),
            Self::InvalidId => write!(f, "'id' must be string, number, or null"),
            Self::UnsupportedBatch => write!(f, "batch requests not supported by current policy"),
            Self::EmptyBatch => write!(f, "batch array is empty"),
        }
    }
}

impl std::error::Error for JsonRpcParseError {}

// -----------------------------------------------------------------------------
// Parser Function
// -----------------------------------------------------------------------------

/// Parse JSON-RPC 2.0 envelope from request body bytes.
///
/// Returns:
/// - `Ok(Some(envelope))` for valid JSON-RPC 2.0
/// - `Ok(None)` for valid JSON but not JSON-RPC (when `on_invalid` allows continuing)
/// - `Err(error)` for invalid JSON or JSON-RPC violations
pub(super) fn parse_json_rpc_envelope(
    input: &[u8],
    config: &JsonRpcConfig,
) -> Result<Option<JsonRpcEnvelope>, JsonRpcParseError> {
    // Parse JSON
    let value: Value = serde_json::from_slice(input).map_err(|e| JsonRpcParseError::InvalidJson(e.to_string()))?;

    // Handle batch (array) vs single message
    match value {
        Value::Array(ref items) => parse_batch(items, config),
        Value::Object(_) => match parse_single_message(&value) {
            Ok(envelope) => Ok(Some(envelope)),
            Err(JsonRpcParseError::MissingVersion) => handle_non_json_rpc(config),
            Err(e) => Err(e),
        },
        _ => {
            // Top-level non-object/array JSON is not JSON-RPC
            handle_non_json_rpc(config)
        },
    }
}

/// Parse a batch array according to the configured policy.
fn parse_batch(items: &[Value], config: &JsonRpcConfig) -> Result<Option<JsonRpcEnvelope>, JsonRpcParseError> {
    if items.is_empty() {
        return Err(JsonRpcParseError::EmptyBatch);
    }

    match config.batch_policy {
        BatchPolicy::Reject => Err(JsonRpcParseError::UnsupportedBatch),
        BatchPolicy::First => {
            // Find the first valid JSON-RPC message in the batch
            for item in items {
                if let Ok(mut envelope) = parse_single_message(item) {
                    // Override to batch kind and set batch_len
                    envelope.kind = JsonRpcKind::Batch;
                    envelope.batch_len = Some(items.len());
                    return Ok(Some(envelope));
                }
            }
            // No valid JSON-RPC messages in batch
            handle_non_json_rpc(config)
        },
    }
}

/// Parse a single JSON-RPC message (request/notification/response).
fn parse_single_message(value: &Value) -> Result<JsonRpcEnvelope, JsonRpcParseError> {
    let obj = value
        .as_object()
        .ok_or_else(|| JsonRpcParseError::InvalidJson("expected object".to_owned()))?;

    validate_version(obj)?;

    let (id, id_kind) = extract_id(obj)?;

    if obj.contains_key("result") || obj.contains_key("error") {
        return Ok(JsonRpcEnvelope {
            kind: JsonRpcKind::Response,
            method: None,
            id,
            id_kind,
            batch_len: None,
        });
    }

    build_request_or_notification(obj, id, id_kind)
}

/// Validate the `jsonrpc` version field is present and equals `"2.0"`.
fn validate_version(obj: &serde_json::Map<String, Value>) -> Result<(), JsonRpcParseError> {
    let version = obj
        .get("jsonrpc")
        .and_then(|v| v.as_str())
        .ok_or(JsonRpcParseError::MissingVersion)?;

    if version != "2.0" {
        return Err(JsonRpcParseError::WrongVersion(version.to_owned()));
    }

    Ok(())
}

/// Build a request or notification envelope from a validated JSON-RPC object.
fn build_request_or_notification(
    obj: &serde_json::Map<String, Value>,
    id: Option<String>,
    id_kind: JsonRpcIdKind,
) -> Result<JsonRpcEnvelope, JsonRpcParseError> {
    let method = obj
        .get("method")
        .ok_or(JsonRpcParseError::MissingMethod)?
        .as_str()
        .ok_or(JsonRpcParseError::InvalidMethod)?
        .to_owned();

    let kind = if id.is_some() {
        JsonRpcKind::Request
    } else {
        JsonRpcKind::Notification
    };

    Ok(JsonRpcEnvelope {
        kind,
        method: Some(method),
        id,
        id_kind,
        batch_len: None,
    })
}

/// Extract and classify the JSON-RPC id field.
fn extract_id(obj: &serde_json::Map<String, Value>) -> Result<(Option<String>, JsonRpcIdKind), JsonRpcParseError> {
    match obj.get("id") {
        None => Ok((None, JsonRpcIdKind::Missing)),
        Some(Value::Null) => Ok((Some("null".to_owned()), JsonRpcIdKind::Null)),
        Some(Value::String(s)) => Ok((Some(s.clone()), JsonRpcIdKind::String)),
        Some(Value::Number(n)) => {
            if n.is_i64() || n.is_u64() {
                Ok((Some(n.to_string()), JsonRpcIdKind::Integer))
            } else {
                Ok((Some(n.to_string()), JsonRpcIdKind::Number))
            }
        },
        Some(Value::Bool(_) | Value::Object(_) | Value::Array(_)) => {
            // JSON-RPC 2.0 only allows string, number, or null IDs
            Err(JsonRpcParseError::InvalidId)
        },
    }
}

/// Handle non-JSON-RPC input based on `on_invalid` config.
fn handle_non_json_rpc(config: &JsonRpcConfig) -> Result<Option<JsonRpcEnvelope>, JsonRpcParseError> {
    match config.on_invalid {
        InvalidJsonRpcBehavior::Continue => Ok(None),
        InvalidJsonRpcBehavior::Reject | InvalidJsonRpcBehavior::Error => Err(JsonRpcParseError::MissingVersion),
    }
}
