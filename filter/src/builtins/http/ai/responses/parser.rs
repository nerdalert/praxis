// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Pure parser for Responses API JSON output items.
//!
//! Extracts [`DetectedFunctionCall`] items from a model
//! response without executing them. Unknown or opaque
//! item types (reasoning, custom provider items) are
//! preserved as [`OutputItem::Unknown`].

use serde_json::Value;

// -----------------------------------------------------------------------------
// Types
// -----------------------------------------------------------------------------

/// A function call detected in the model response.
///
/// Correlation with tool results uses [`call_id`], not
/// the item [`id`]. The [`arguments`] field is the raw
/// JSON string emitted by the model.
///
/// [`call_id`]: DetectedFunctionCall::call_id
/// [`id`]: the output item's own unique ID (not used for correlation)
/// [`arguments`]: DetectedFunctionCall::arguments
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetectedFunctionCall {
    /// The JSON string arguments emitted by the model.
    pub arguments: String,

    /// The model-generated correlation ID for result
    /// injection via `function_call_output`.
    pub call_id: String,

    /// The function name the model wants to invoke.
    pub name: String,
}

/// A parsed output item from a Responses API response.
#[derive(Clone, Debug)]
#[allow(dead_code, reason = "variants consumed by later checkpoints")]
pub enum OutputItem {
    /// An assistant text message (final answer).
    Message(Value),

    /// A model-emitted function call request.
    FunctionCall(DetectedFunctionCall),

    /// Any item type not explicitly handled.
    Unknown(Value),
}

/// Result of parsing a full Responses API response body.
#[derive(Clone, Debug)]
pub struct ParsedModelResponse {
    /// All detected function calls.
    pub function_calls: Vec<DetectedFunctionCall>,

    /// The response status field (e.g. "completed",
    /// "incomplete", "failed").
    #[allow(dead_code, reason = "consumed by later checkpoints")]
    pub status: String,
}

// -----------------------------------------------------------------------------
// Parsing
// -----------------------------------------------------------------------------

/// Parse a Responses API JSON response body and extract
/// function calls.
///
/// Returns `None` if the body is not valid JSON or does
/// not contain an `output` array.
pub fn parse_model_response(body: &[u8]) -> Option<ParsedModelResponse> {
    let json: Value = serde_json::from_slice(body).ok()?;

    let status = json
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();

    let output = json.get("output")?.as_array()?;

    let mut function_calls = Vec::new();

    for item in output {
        if let OutputItem::FunctionCall(fc) = parse_output_item(item) {
            function_calls.push(fc);
        }
    }

    Some(ParsedModelResponse { function_calls, status })
}

/// Classify a single output item by its `type` field.
pub fn parse_output_item(item: &Value) -> OutputItem {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");

    match item_type {
        "function_call" => parse_function_call_item(item),
        "message" => OutputItem::Message(item.clone()),
        _ => OutputItem::Unknown(item.clone()),
    }
}

// -----------------------------------------------------------------------------
// Utilities
// -----------------------------------------------------------------------------

/// Parse a `function_call` output item into a
/// [`DetectedFunctionCall`], falling back to
/// [`OutputItem::Unknown`] if required fields are
/// missing.
fn parse_function_call_item(item: &Value) -> OutputItem {
    let call_id = item.get("call_id").and_then(Value::as_str);
    let name = item.get("name").and_then(Value::as_str);
    let arguments = item.get("arguments").and_then(Value::as_str);

    match (call_id, name, arguments) {
        (Some(cid), Some(n), Some(args)) => OutputItem::FunctionCall(DetectedFunctionCall {
            arguments: args.to_owned(),
            call_id: cid.to_owned(),
            name: n.to_owned(),
        }),
        _ => OutputItem::Unknown(item.clone()),
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
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_function_call_item() {
        let item = json!({
            "type": "function_call",
            "id": "fc_item_001",
            "call_id": "call_weather_001",
            "name": "get_weather",
            "arguments": r#"{"city":"Boston"}"#,
            "status": "completed"
        });

        let parsed = parse_output_item(&item);

        match parsed {
            OutputItem::FunctionCall(fc) => {
                assert_eq!(fc.call_id, "call_weather_001", "call_id should match");
                assert_eq!(fc.name, "get_weather", "name should match");
                assert_eq!(fc.arguments, r#"{"city":"Boston"}"#, "arguments should match");
            },
            other => panic!("expected FunctionCall, got {other:?}"),
        }
    }

    #[test]
    fn extracts_call_id_name_arguments() {
        let body = json!({
            "id": "resp_001",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "id": "fc_001",
                "call_id": "call_abc",
                "name": "search",
                "arguments": r#"{"query":"rust"}"#,
                "status": "completed"
            }]
        });

        let parsed = parse_model_response(body.to_string().as_bytes()).unwrap();

        assert_eq!(parsed.function_calls.len(), 1, "should detect one function call");
        assert_eq!(parsed.function_calls[0].call_id, "call_abc", "call_id");
        assert_eq!(parsed.function_calls[0].name, "search", "name");
        assert_eq!(parsed.function_calls[0].arguments, r#"{"query":"rust"}"#, "arguments");
    }

    #[test]
    fn handles_multiple_output_items() {
        let body = json!({
            "id": "resp_001",
            "object": "response",
            "status": "completed",
            "output": [
                {
                    "type": "function_call",
                    "id": "fc_001",
                    "call_id": "call_a",
                    "name": "tool_a",
                    "arguments": "{}",
                    "status": "completed"
                },
                {
                    "type": "message",
                    "id": "msg_001",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "hello"}]
                },
                {
                    "type": "function_call",
                    "id": "fc_002",
                    "call_id": "call_b",
                    "name": "tool_b",
                    "arguments": r#"{"x":1}"#,
                    "status": "completed"
                }
            ]
        });

        let parsed = parse_model_response(body.to_string().as_bytes()).unwrap();

        assert_eq!(parsed.function_calls.len(), 2, "should detect two function calls");
        assert_eq!(parsed.function_calls[0].name, "tool_a", "first tool name");
        assert_eq!(parsed.function_calls[1].name, "tool_b", "second tool name");
        assert_eq!(parsed.function_calls[0].call_id, "call_a", "first call_id");
        assert_eq!(parsed.function_calls[1].call_id, "call_b", "second call_id");
    }

    #[test]
    fn preserves_unknown_item_types() {
        let item = json!({
            "type": "reasoning",
            "id": "rs_001",
            "summary": [{"type": "summary_text", "text": "thinking..."}]
        });

        let parsed = parse_output_item(&item);

        assert!(
            matches!(parsed, OutputItem::Unknown(_)),
            "reasoning should be Unknown, got {parsed:?}"
        );
    }

    #[test]
    fn no_function_call_returns_empty() {
        let body = json!({
            "id": "resp_001",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "message",
                "id": "msg_001",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "final answer"}]
            }]
        });

        let parsed = parse_model_response(body.to_string().as_bytes()).unwrap();

        assert!(parsed.function_calls.is_empty(), "no function calls should be detected");
    }

    #[test]
    fn function_call_missing_call_id_treated_as_unknown() {
        let item = json!({
            "type": "function_call",
            "id": "fc_001",
            "name": "get_weather",
            "arguments": "{}"
        });

        let parsed = parse_output_item(&item);

        assert!(
            matches!(parsed, OutputItem::Unknown(_)),
            "missing call_id should be Unknown"
        );
    }

    #[test]
    fn function_call_missing_name_treated_as_unknown() {
        let item = json!({
            "type": "function_call",
            "id": "fc_001",
            "call_id": "call_001",
            "arguments": "{}"
        });

        let parsed = parse_output_item(&item);

        assert!(
            matches!(parsed, OutputItem::Unknown(_)),
            "missing name should be Unknown"
        );
    }

    #[test]
    fn function_call_missing_arguments_treated_as_unknown() {
        let item = json!({
            "type": "function_call",
            "id": "fc_001",
            "call_id": "call_001",
            "name": "get_weather"
        });

        let parsed = parse_output_item(&item);

        assert!(
            matches!(parsed, OutputItem::Unknown(_)),
            "missing arguments should be Unknown"
        );
    }

    #[test]
    fn parses_response_status() {
        let body = json!({
            "id": "resp_001",
            "status": "incomplete",
            "output": []
        });

        let parsed = parse_model_response(body.to_string().as_bytes()).unwrap();

        assert_eq!(parsed.status, "incomplete", "status should be parsed");
    }

    #[test]
    fn invalid_json_returns_none() {
        let parsed = parse_model_response(b"not json");
        assert!(parsed.is_none(), "invalid JSON should return None");
    }

    #[test]
    fn missing_output_returns_none() {
        let body = json!({"id": "resp_001", "status": "completed"});
        let parsed = parse_model_response(body.to_string().as_bytes());
        assert!(parsed.is_none(), "missing output should return None");
    }

    #[test]
    fn item_without_type_is_unknown() {
        let item = json!({"id": "mystery", "data": 42});
        let parsed = parse_output_item(&item);

        assert!(
            matches!(parsed, OutputItem::Unknown(_)),
            "item without type should be Unknown"
        );
    }
}
