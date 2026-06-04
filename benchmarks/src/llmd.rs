// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! llm-d benchmark profiles and request body generators.
//!
//! Provides OpenAI-compatible chat completion request bodies for
//! llm-d benchmark workloads and profile name definitions.

use serde::{Deserialize, Serialize};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Default model name for llm-d benchmark requests.
const DEFAULT_MODEL: &str = "test-model";

/// Default small prompt content.
const SMALL_PROMPT: &str = "Hello, how are you?";

/// Default max tokens for small chat requests.
const DEFAULT_MAX_TOKENS: u32 = 50;

// -----------------------------------------------------------------------------
// Benchmark Profile
// -----------------------------------------------------------------------------

/// llm-d benchmark profile defining the request path under test.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LlmdProfile {
    /// Client -> Praxis generic proxy -> simulator.
    PraxisSimple,

    /// Client -> Praxis `llmd_endpoint_picker` -> simulator.
    PraxisNative,

    /// Client -> Envoy `ext_proc` -> Go EPP -> simulator.
    EnvoyGoEpp,

    /// Client -> Envoy -> Praxis native -> simulator.
    EnvoyPraxisNative,
}

impl LlmdProfile {
    /// Parse a profile name string.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "praxis-simple" => Some(Self::PraxisSimple),
            "praxis-native" => Some(Self::PraxisNative),
            "envoy-go-epp" => Some(Self::EnvoyGoEpp),
            "envoy-praxis-native" => Some(Self::EnvoyPraxisNative),
            _ => None,
        }
    }

    /// Return the kebab-case name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::PraxisSimple => "praxis-simple",
            Self::PraxisNative => "praxis-native",
            Self::EnvoyGoEpp => "envoy-go-epp",
            Self::EnvoyPraxisNative => "envoy-praxis-native",
        }
    }
}

// -----------------------------------------------------------------------------
// Request Body Generators
// -----------------------------------------------------------------------------

/// Generate a small OpenAI-compatible chat completion request body.
pub fn chat_small_body() -> Vec<u8> {
    let body = serde_json::json!({
        "model": DEFAULT_MODEL,
        "messages": [
            {"role": "user", "content": SMALL_PROMPT}
        ],
        "max_tokens": DEFAULT_MAX_TOKENS
    });
    serde_json::to_vec(&body).unwrap_or_default()
}

/// Generate a large-prompt chat completion request body.
///
/// Fills the prompt content to approximately `target_size` bytes.
pub fn chat_large_prompt_body(target_size: usize) -> Vec<u8> {
    let padding = "x".repeat(target_size);
    let body = serde_json::json!({
        "model": DEFAULT_MODEL,
        "messages": [
            {"role": "user", "content": padding}
        ],
        "max_tokens": DEFAULT_MAX_TOKENS
    });
    serde_json::to_vec(&body).unwrap_or_default()
}

/// Generate a streaming chat completion request body.
pub fn chat_streaming_body() -> Vec<u8> {
    let body = serde_json::json!({
        "model": DEFAULT_MODEL,
        "messages": [
            {"role": "user", "content": SMALL_PROMPT}
        ],
        "max_tokens": DEFAULT_MAX_TOKENS,
        "stream": true
    });
    serde_json::to_vec(&body).unwrap_or_default()
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn chat_small_body_is_valid_json() {
        let body = chat_small_body();
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("small body should be valid JSON");
        assert_eq!(parsed["model"], "test-model", "model field should be test-model");
        assert!(parsed["messages"].is_array(), "messages should be an array");
        assert_eq!(parsed["max_tokens"], 50, "max_tokens should be 50");
    }

    #[test]
    fn chat_large_prompt_body_reaches_target_size() {
        let target = 16_384; // 16 KiB
        let body = chat_large_prompt_body(target);
        assert!(
            body.len() >= target,
            "large prompt body ({}) should be at least {target} bytes",
            body.len()
        );
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("large body should be valid JSON");
        assert_eq!(parsed["model"], "test-model", "model field should be test-model");
    }

    #[test]
    fn chat_streaming_body_includes_stream_flag() {
        let body = chat_streaming_body();
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("streaming body should be valid JSON");
        assert_eq!(parsed["stream"], true, "stream field should be true");
    }

    #[test]
    fn profile_parse_valid() {
        assert_eq!(
            LlmdProfile::parse("praxis-simple"),
            Some(LlmdProfile::PraxisSimple),
            "should parse praxis-simple"
        );
        assert_eq!(
            LlmdProfile::parse("praxis-native"),
            Some(LlmdProfile::PraxisNative),
            "should parse praxis-native"
        );
        assert_eq!(
            LlmdProfile::parse("envoy-go-epp"),
            Some(LlmdProfile::EnvoyGoEpp),
            "should parse envoy-go-epp"
        );
    }

    #[test]
    fn profile_parse_invalid() {
        assert_eq!(
            LlmdProfile::parse("unknown"),
            None,
            "should return None for unknown profile"
        );
    }

    #[test]
    fn profile_name_round_trip() {
        for name in &["praxis-simple", "praxis-native", "envoy-go-epp", "envoy-praxis-native"] {
            let profile = LlmdProfile::parse(name).unwrap();
            assert_eq!(profile.name(), *name, "name() should round-trip with parse()");
        }
    }
}
