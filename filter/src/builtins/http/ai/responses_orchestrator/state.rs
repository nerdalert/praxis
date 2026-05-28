// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! In-memory Responses state store for the e2e spike.
//!
//! Stores completed response output items and maps
//! conversation IDs to their latest response ID.
//! Per-filter-instance, not global.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use serde_json::Value;

// -----------------------------------------------------------------------------
// StoredResponse
// -----------------------------------------------------------------------------

/// A persisted response entry.
#[derive(Clone, Debug)]
pub(super) struct StoredResponse {
    /// The full accumulated conversation transcript:
    /// prior input items + model output items. This is
    /// replayed as the input for continuations via
    /// `previous_response_id`.
    pub items: Vec<Value>,

    /// The model used for this response.
    #[allow(dead_code, reason = "used by later checkpoints for multi-model state")]
    pub model: String,
}

// -----------------------------------------------------------------------------
// ResponseStateStore
// -----------------------------------------------------------------------------

/// Thread-safe in-memory state store.
#[derive(Clone)]
pub(super) struct ResponseStateStore {
    /// Response ID → stored response.
    responses: Arc<Mutex<HashMap<String, StoredResponse>>>,

    /// Conversation ID → latest response ID.
    conversations: Arc<Mutex<HashMap<String, String>>>,
}

impl ResponseStateStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self {
            conversations: Arc::new(Mutex::new(HashMap::new())),
            responses: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Store a completed response.
    pub fn store_response(&self, response_id: &str, response: StoredResponse) {
        self.responses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(response_id.to_owned(), response);
    }

    /// Load a stored response by ID.
    pub fn load_response(&self, response_id: &str) -> Option<StoredResponse> {
        self.responses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(response_id)
            .cloned()
    }

    /// Update the latest response ID for a conversation.
    pub fn set_conversation_latest(&self, conversation_id: &str, response_id: &str) {
        self.conversations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(conversation_id.to_owned(), response_id.to_owned());
    }

    /// Get the latest response ID for a conversation.
    pub fn get_conversation_latest(&self, conversation_id: &str) -> Option<String> {
        self.conversations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(conversation_id)
            .cloned()
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, reason = "tests")]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn store_and_load_response() {
        let store = ResponseStateStore::new();

        store.store_response(
            "resp_001",
            StoredResponse {
                model: "test".to_owned(),
                items: vec![
                    json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}),
                    json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hi"}]}),
                ],
            },
        );

        let loaded = store.load_response("resp_001");
        assert!(loaded.is_some(), "stored response should be loadable");
        assert_eq!(loaded.unwrap().items.len(), 2, "should have two items (input + output)");
    }

    #[test]
    fn missing_response_returns_none() {
        let store = ResponseStateStore::new();
        assert!(
            store.load_response("nonexistent").is_none(),
            "missing response should return None"
        );
    }

    #[test]
    fn store_false_does_not_persist() {
        let store = ResponseStateStore::new();
        assert!(
            store.load_response("resp_never_stored").is_none(),
            "unstored response should not exist"
        );
    }

    #[test]
    fn conversation_latest_tracking() {
        let store = ResponseStateStore::new();

        store.set_conversation_latest("conv_1", "resp_001");
        assert_eq!(
            store.get_conversation_latest("conv_1"),
            Some("resp_001".to_owned()),
            "should track latest response"
        );

        store.set_conversation_latest("conv_1", "resp_002");
        assert_eq!(
            store.get_conversation_latest("conv_1"),
            Some("resp_002".to_owned()),
            "should update to latest"
        );
    }

    #[test]
    fn unknown_conversation_returns_none() {
        let store = ResponseStateStore::new();
        assert!(
            store.get_conversation_latest("conv_unknown").is_none(),
            "unknown conversation should return None"
        );
    }
}
