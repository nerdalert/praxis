// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! SSE parser for Responses API streaming events.
//!
//! Buffers partial lines across chunks, extracts typed
//! events, and assembles complete [`DetectedFunctionCall`]
//! items from `function_call_arguments.delta` events.
//!
//! Argument buffers are keyed by `output_index` from the
//! SSE event to correctly handle multiple concurrent
//! function calls.
//!
//! [`DetectedFunctionCall`]: super::parser::DetectedFunctionCall

use std::collections::HashMap;

use serde_json::Value;

use super::parser::DetectedFunctionCall;

// -----------------------------------------------------------------------------
// SseParser
// -----------------------------------------------------------------------------

/// Stateful SSE parser that buffers partial lines and
/// assembles function-call arguments from deltas.
pub struct SseParser {
    /// Argument buffers keyed by `output_index`.
    argument_buffers: HashMap<u64, ArgumentBuffer>,

    /// Partial line buffer for incomplete chunks.
    line_buffer: String,

    /// Model name from `response.completed`.
    model: Option<String>,

    /// Whether a terminal event was seen.
    response_completed: bool,

    /// Response ID from `response.completed`.
    response_id: Option<String>,

    /// Accumulated output text deltas for synthesis.
    text_content: String,
}

/// Accumulates argument deltas for a single function call.
struct ArgumentBuffer {
    /// The function-call arguments accumulated so far.
    arguments: String,

    /// The call_id for correlation.
    call_id: String,

    /// Whether argument assembly is complete.
    done: bool,

    /// The function name.
    name: String,
}

/// Result of parsing all buffered SSE events.
pub struct SseParseResult {
    /// Completed function calls with full arguments.
    pub function_calls: Vec<DetectedFunctionCall>,

    /// Whether any function-call items were started but
    /// never received `arguments.done`.
    pub has_incomplete_function_calls: bool,

    /// Model name from `response.completed` metadata.
    pub model: Option<String>,

    /// Whether a terminal response event was received.
    pub response_completed: bool,

    /// Response ID from `response.completed` metadata.
    pub response_id: Option<String>,

    /// Synthesized text content from output text deltas.
    pub synthesized_text: String,
}

impl SseParser {
    /// Create a new parser.
    pub fn new() -> Self {
        Self {
            argument_buffers: HashMap::new(),
            line_buffer: String::new(),
            model: None,
            response_completed: false,
            response_id: None,
            text_content: String::new(),
        }
    }

    /// Feed a chunk of SSE data into the parser.
    pub fn feed(&mut self, chunk: &str) {
        self.line_buffer.push_str(chunk);

        while let Some(line_end) = find_line_end(&self.line_buffer) {
            let line = self.line_buffer[..line_end].to_owned();
            let skip = if self.line_buffer[line_end..].starts_with("\r\n") {
                2
            } else {
                1
            };
            self.line_buffer = self.line_buffer[line_end + skip..].to_owned();
            self.process_line(&line);
        }
    }

    /// Extract the final parse result.
    pub fn finish(self) -> SseParseResult {
        let has_incomplete = self.argument_buffers.values().any(|b| !b.done);

        let mut function_calls: Vec<DetectedFunctionCall> = self
            .argument_buffers
            .into_values()
            .filter(|b| b.done)
            .map(|b| DetectedFunctionCall {
                arguments: b.arguments,
                call_id: b.call_id,
                name: b.name,
            })
            .collect();

        function_calls.sort_by(|a, b| a.call_id.cmp(&b.call_id));

        SseParseResult {
            function_calls,
            has_incomplete_function_calls: has_incomplete,
            model: self.model,
            response_completed: self.response_completed,
            response_id: self.response_id,
            synthesized_text: self.text_content,
        }
    }

    /// Process a single complete SSE line.
    fn process_line(&mut self, line: &str) {
        let Some(data) = line.strip_prefix("data: ") else {
            return;
        };

        if data == "[DONE]" {
            self.response_completed = true;
            return;
        }

        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return;
        };

        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");

        match event_type {
            "response.output_item.added" => self.handle_output_item_added(&event),
            "response.function_call_arguments.delta" => self.handle_arguments_delta(&event),
            "response.function_call_arguments.done" => self.handle_arguments_done(&event),
            "response.output_text.delta" => self.handle_text_delta(&event),
            "response.completed" => {
                self.response_completed = true;
                if let Some(resp) = event.get("response") {
                    self.response_id = resp.get("id").and_then(Value::as_str).map(str::to_owned);
                    self.model = resp.get("model").and_then(Value::as_str).map(str::to_owned);
                }
            },
            _ => {},
        }
    }

    /// Handle a new output item being added.
    fn handle_output_item_added(&mut self, event: &Value) {
        let item = match event.get("item") {
            Some(i) => i,
            None => return,
        };

        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }

        let output_index = event.get("output_index").and_then(Value::as_u64).unwrap_or(0);
        let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or_default();
        let name = item.get("name").and_then(Value::as_str).unwrap_or_default();

        self.argument_buffers.insert(
            output_index,
            ArgumentBuffer {
                arguments: String::new(),
                call_id: call_id.to_owned(),
                done: false,
                name: name.to_owned(),
            },
        );
    }

    /// Handle an argument delta chunk, keyed by
    /// `output_index`.
    fn handle_arguments_delta(&mut self, event: &Value) {
        let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
        let output_index = event.get("output_index").and_then(Value::as_u64).unwrap_or(0);

        if let Some(buf) = self.argument_buffers.get_mut(&output_index) {
            buf.arguments.push_str(delta);
        }
    }

    /// Handle argument assembly completion, keyed by
    /// `output_index`.
    fn handle_arguments_done(&mut self, event: &Value) {
        let output_index = event.get("output_index").and_then(Value::as_u64).unwrap_or(0);

        if let Some(buf) = self.argument_buffers.get_mut(&output_index) {
            buf.done = true;
        }
    }

    /// Accumulate output text for synthesis.
    fn handle_text_delta(&mut self, event: &Value) {
        let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
        self.text_content.push_str(delta);
    }
}

// -----------------------------------------------------------------------------
// Utilities
// -----------------------------------------------------------------------------

/// Find the first LF or CRLF in the buffer.
fn find_line_end(buf: &str) -> Option<usize> {
    buf.find('\n').map(|pos| {
        if pos > 0 && buf.as_bytes().get(pos.wrapping_sub(1)) == Some(&b'\r') {
            pos - 1
        } else {
            pos
        }
    })
}

/// Synthesize a minimal valid Responses JSON body from
/// SSE parse results when there are no tool calls.
pub fn synthesize_final_json(result: &SseParseResult) -> Vec<u8> {
    let id = result.response_id.as_deref().unwrap_or("resp_synthesized");
    let model = result.model.as_deref().unwrap_or("unknown");

    let response = serde_json::json!({
        "id": id,
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": model,
        "output": [
            {
                "type": "message",
                "id": "msg_synthesized",
                "role": "assistant",
                "status": "completed",
                "content": [
                    {
                        "type": "output_text",
                        "text": result.synthesized_text,
                        "annotations": []
                    }
                ]
            }
        ]
    });
    serde_json::to_vec(&response).unwrap_or_default()
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
    fn parses_complete_function_call_from_stream() {
        let mut parser = SseParser::new();
        parser.feed(&build_function_call_sse(
            0,
            "call_001",
            "get_weather",
            r#"{"city":"Boston"}"#,
        ));

        let result = parser.finish();
        assert_eq!(result.function_calls.len(), 1, "should detect one function call");
        assert_eq!(result.function_calls[0].call_id, "call_001", "call_id");
        assert_eq!(result.function_calls[0].name, "get_weather", "name");
        assert_eq!(result.function_calls[0].arguments, r#"{"city":"Boston"}"#, "arguments");
        assert!(result.response_completed, "response should be completed");
    }

    #[test]
    fn handles_arguments_split_across_chunks() {
        let mut parser = SseParser::new();

        parser.feed("data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_001\",\"name\":\"get_weather\"}}\n");
        parser.feed(
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"cit\"}\n",
        );
        parser.feed("data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"y\\\":\\\"Bos\"}\n");
        parser.feed(
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"ton\\\"}\"}\n",
        );
        parser.feed("data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":0,\"arguments\":\"{\\\"city\\\":\\\"Boston\\\"}\"}\n");
        parser.feed("data: {\"type\":\"response.completed\"}\n");

        let result = parser.finish();
        assert_eq!(result.function_calls.len(), 1, "should detect one function call");
        assert_eq!(
            result.function_calls[0].arguments, r#"{"city":"Boston"}"#,
            "arguments should be concatenated from deltas"
        );
    }

    #[test]
    fn handles_two_concurrent_function_calls() {
        let mut parser = SseParser::new();

        parser.feed("data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_a\",\"name\":\"tool_a\"}}\n");
        parser.feed("data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_b\",\"name\":\"tool_b\"}}\n");
        parser.feed("data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"x\\\":1}\"}\n");
        parser.feed("data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{\\\"y\\\":2}\"}\n");
        parser.feed("data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":0}\n");
        parser.feed("data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":1}\n");
        parser.feed("data: {\"type\":\"response.completed\"}\n");

        let result = parser.finish();
        assert_eq!(result.function_calls.len(), 2, "should detect two function calls");

        let a = result.function_calls.iter().find(|fc| fc.name == "tool_a").unwrap();
        assert_eq!(a.call_id, "call_a", "call_a id");
        assert_eq!(a.arguments, r#"{"x":1}"#, "call_a arguments");

        let b = result.function_calls.iter().find(|fc| fc.name == "tool_b").unwrap();
        assert_eq!(b.call_id, "call_b", "call_b id");
        assert_eq!(b.arguments, r#"{"y":2}"#, "call_b arguments");
    }

    #[test]
    fn handles_partial_lines_across_chunks() {
        let mut parser = SseParser::new();

        parser.feed("data: {\"type\":");
        parser.feed("\"response.completed\"}\n");

        let result = parser.finish();
        assert!(result.response_completed, "should handle partial line");
    }

    #[test]
    fn handles_crlf_line_endings() {
        let mut parser = SseParser::new();
        parser.feed("data: {\"type\":\"response.completed\"}\r\n");

        let result = parser.finish();
        assert!(result.response_completed, "should handle CRLF");
    }

    #[test]
    fn no_function_call_returns_empty() {
        let mut parser = SseParser::new();
        parser.feed("data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n");
        parser.feed("data: {\"type\":\"response.completed\"}\n");

        let result = parser.finish();
        assert!(result.function_calls.is_empty(), "no function calls");
        assert!(result.response_completed, "response completed");
        assert_eq!(result.synthesized_text, "hello", "should capture text delta");
    }

    #[test]
    fn handles_done_marker() {
        let mut parser = SseParser::new();
        parser.feed("data: [DONE]\n");

        let result = parser.finish();
        assert!(result.response_completed, "should handle [DONE]");
    }

    #[test]
    fn tolerates_unknown_event_types() {
        let mut parser = SseParser::new();
        parser.feed("data: {\"type\":\"response.unknown_event\",\"data\":42}\n");
        parser.feed("data: {\"type\":\"response.completed\"}\n");

        let result = parser.finish();
        assert!(result.response_completed, "unknown events should not break parser");
    }

    #[test]
    fn incomplete_arguments_not_returned() {
        let mut parser = SseParser::new();

        parser.feed("data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_001\",\"name\":\"get_weather\"}}\n");
        parser.feed("data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"partial\"}\n");

        let result = parser.finish();
        assert!(
            result.function_calls.is_empty(),
            "incomplete arguments should not be returned"
        );
    }

    // -------------------------------------------------------------------------
    // Test Utilities
    // -------------------------------------------------------------------------

    fn build_function_call_sse(output_index: u64, call_id: &str, name: &str, arguments: &str) -> String {
        let escaped_args = arguments.replace('\"', "\\\"");
        format!(
            "data: {{\"type\":\"response.output_item.added\",\"output_index\":{output_index},\"item\":{{\"type\":\"function_call\",\"call_id\":\"{call_id}\",\"name\":\"{name}\"}}}}\n\
             data: {{\"type\":\"response.function_call_arguments.delta\",\"output_index\":{output_index},\"delta\":\"{escaped_args}\"}}\n\
             data: {{\"type\":\"response.function_call_arguments.done\",\"output_index\":{output_index},\"arguments\":\"{escaped_args}\"}}\n\
             data: {{\"type\":\"response.output_item.done\"}}\n\
             data: {{\"type\":\"response.completed\"}}\n"
        )
    }
}
