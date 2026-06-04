// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Approximate prefix-cache scoring for the llm-d endpoint picker.
//!
//! Hashes request prefix material into fixed-size blocks and tracks
//! which endpoints have previously seen each block. When a new request
//! arrives, the longest contiguous prefix match is used to bias routing
//! toward endpoints that already have the prefix cached in their
//! KV-cache.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

use serde::Deserialize;
use tracing::trace;

use crate::{FilterAction, FilterError};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Default weight for prefix-cache scoring.
const DEFAULT_PREFIX_WEIGHT: f64 = 1.0;

/// Default block size in tokens.
pub(super) const DEFAULT_BLOCK_SIZE_TOKENS: usize = 16;

/// Default maximum number of prefix blocks to match.
const DEFAULT_MAX_PREFIX_BLOCKS: usize = 256;

/// Default LRU capacity per endpoint (in block hashes).
const DEFAULT_LRU_CAPACITY: usize = 31_250; // ~500k tokens at 16 tokens/block

/// Approximate number of characters per token for byte-level
/// block segmentation.
const CHARS_PER_TOKEN: usize = 4;

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;

/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

/// Compute a deterministic FNV-1a 64-bit hash of `data`.
///
/// This is used instead of `DefaultHasher` to guarantee hash
/// stability across Rust toolchain versions.
fn fnv1a_hash(data: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// -----------------------------------------------------------------------------
// Config
// -----------------------------------------------------------------------------

/// Prefix-cache mode selector.
///
/// Determines whether approximate (byte-level) or precise
/// (tokenizer-level) prefix-cache scoring is used.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub(super) enum PrefixCacheMode {
    /// Byte-level hashing without a tokenizer (default).
    #[default]
    Approximate,
    /// Token-level hashing matching the Go EPP algorithm.
    Precise,
}

/// Configuration for prefix-cache scoring.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub(super) struct PrefixCacheConfig {
    /// Whether prefix-cache scoring is enabled.
    pub enabled: bool,

    /// Prefix-cache mode: `approximate` (default) or `precise`.
    #[serde(default)]
    pub mode: Option<PrefixCacheMode>,

    /// Weight applied to the prefix-match score when combining with
    /// queue and KV-cache scores.
    #[serde(default = "default_prefix_weight")]
    pub weight: f64,

    /// Number of tokens per hash block.
    #[serde(default = "default_block_size_tokens")]
    pub block_size_tokens: usize,

    /// Maximum number of prefix blocks to consider when matching.
    /// Ignored when `max_prefix_tokens_to_match` is positive.
    #[serde(default = "default_max_prefix_blocks")]
    pub max_prefix_blocks_to_match: usize,

    /// Maximum prefix length in tokens. When positive, overrides
    /// `max_prefix_blocks_to_match` with `max(1, tokens / block_size)`.
    #[serde(default)]
    pub max_prefix_tokens_to_match: usize,

    /// LRU capacity per endpoint in block hashes.
    #[serde(default = "default_lru_capacity")]
    pub lru_capacity_per_endpoint: usize,

    /// Precise mode configuration (required when `mode` is `precise`).
    #[serde(default)]
    pub precise: Option<super::precise_prefix::PrecisePrefixConfig>,
}

/// Default prefix weight.
fn default_prefix_weight() -> f64 {
    DEFAULT_PREFIX_WEIGHT
}

/// Default block size in tokens.
fn default_block_size_tokens() -> usize {
    DEFAULT_BLOCK_SIZE_TOKENS
}

/// Default maximum prefix blocks.
fn default_max_prefix_blocks() -> usize {
    DEFAULT_MAX_PREFIX_BLOCKS
}

/// Default LRU capacity per endpoint.
fn default_lru_capacity() -> usize {
    DEFAULT_LRU_CAPACITY
}

impl PrefixCacheConfig {
    /// Return the effective maximum number of blocks to match.
    ///
    /// When `max_prefix_tokens_to_match` is positive, the block cap is
    /// computed as `max(1, tokens / block_size_tokens)`. Otherwise the
    /// configured `max_prefix_blocks_to_match` is returned directly.
    pub fn effective_max_blocks(&self) -> usize {
        if self.max_prefix_tokens_to_match > 0 {
            (self.max_prefix_tokens_to_match / self.block_size_tokens).max(1)
        } else {
            self.max_prefix_blocks_to_match
        }
    }
}

/// Validate prefix-cache configuration.
///
/// # Errors
///
/// Returns [`FilterError`] when any field has an invalid value.
pub(super) fn validate_prefix_cache_config(cfg: &PrefixCacheConfig) -> Result<(), FilterError> {
    validate_prefix_cache_weight(cfg)?;
    if matches!(cfg.mode, Some(PrefixCacheMode::Precise)) {
        return validate_precise_mode(cfg);
    }
    validate_approximate_mode(cfg)
}

/// Validate weight field common to both modes.
fn validate_prefix_cache_weight(cfg: &PrefixCacheConfig) -> Result<(), FilterError> {
    if !cfg.weight.is_finite() || cfg.weight < 0.0 {
        return Err("llmd_endpoint_picker: prefix_cache.weight must be a finite non-negative number".into());
    }
    Ok(())
}

/// Validate approximate-mode-specific fields.
fn validate_approximate_mode(cfg: &PrefixCacheConfig) -> Result<(), FilterError> {
    if cfg.block_size_tokens == 0 {
        return Err("llmd_endpoint_picker: prefix_cache.block_size_tokens must be greater than zero".into());
    }
    if cfg.lru_capacity_per_endpoint == 0 {
        return Err("llmd_endpoint_picker: prefix_cache.lru_capacity_per_endpoint must be greater than zero".into());
    }
    if cfg.max_prefix_blocks_to_match == 0 && cfg.max_prefix_tokens_to_match == 0 {
        return Err(
            "llmd_endpoint_picker: prefix_cache must have at least one of max_prefix_blocks_to_match or max_prefix_tokens_to_match > 0"
                .into(),
        );
    }
    Ok(())
}

/// Validate precise-mode-specific fields.
fn validate_precise_mode(cfg: &PrefixCacheConfig) -> Result<(), FilterError> {
    let Some(ref precise) = cfg.precise else {
        return Err("llmd_endpoint_picker: prefix_cache.precise is required when mode is precise".into());
    };
    super::precise_prefix::validate_precise_config(precise)
}

// -----------------------------------------------------------------------------
// Request Parsing
// -----------------------------------------------------------------------------

/// Extracted request information for prefix-cache scoring.
pub(super) struct RequestInfo {
    /// The model name from the request body.
    pub model: String,

    /// Concatenated prefix material for block hashing, if available.
    pub prefix_material: Option<Vec<u8>>,
}

/// Parse a JSON request body to extract the model and prefix material.
///
/// # Errors
///
/// Returns a 400 rejection for invalid JSON or a missing model field.
pub(super) fn extract_request_info(body: &[u8]) -> Result<RequestInfo, FilterAction> {
    let value = serde_json::from_slice::<serde_json::Value>(body)
        .map_err(|_err| reject(400, "llmd_endpoint_picker: invalid JSON request body"))?;

    let model = extract_model_from_value(&value)?;
    let prefix_material = extract_prefix_material(&value);

    trace!(
        model = model,
        has_prefix = prefix_material.is_some(),
        "request info extracted"
    );
    Ok(RequestInfo { model, prefix_material })
}

/// Extract the model string from a parsed JSON value.
fn extract_model_from_value(value: &serde_json::Value) -> Result<String, FilterAction> {
    value
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| reject(400, "llmd_endpoint_picker: missing model field"))
}

/// Extract prefix material from the parsed JSON body.
///
/// Supports chat completions (`messages`), completions (`prompt`),
/// and responses API (`input` with optional `instructions`/`tools`).
///
/// For `prompt`, both string and non-string (array/object) values
/// are supported. Non-string prompts are serialized to stable JSON.
fn extract_prefix_material(value: &serde_json::Value) -> Option<Vec<u8>> {
    if let Some(messages) = value.get("messages").and_then(serde_json::Value::as_array) {
        return extract_chat_prefix(messages);
    }
    if let Some(prompt) = value.get("prompt") {
        return extract_prompt_prefix(prompt);
    }
    if value.get("input").is_some() {
        return extract_responses_prefix(value);
    }
    None
}

/// Extract prefix material from a `prompt` field.
///
/// String prompts are included as raw bytes. Array or object
/// prompts are serialized to stable JSON.
fn extract_prompt_prefix(prompt: &serde_json::Value) -> Option<Vec<u8>> {
    if let Some(s) = prompt.as_str() {
        if s.is_empty() {
            return None;
        }
        return Some(s.as_bytes().to_vec());
    }
    serde_json::to_vec(prompt).ok().filter(|v| !v.is_empty())
}

/// Concatenate role and content from each message in a chat request.
///
/// Content may be a plain string or an array of content blocks.
/// For array content, each block is serialized according to its
/// type: `text` blocks contribute their text, `image_url` blocks
/// contribute the URL string, and other block types are serialized
/// as stable JSON.
fn extract_chat_prefix(messages: &[serde_json::Value]) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    for msg in messages {
        if let Some(role) = msg.get("role").and_then(serde_json::Value::as_str) {
            buf.extend_from_slice(role.as_bytes());
        }
        if let Some(content) = msg.get("content") {
            append_content(&mut buf, content);
        }
    }
    if buf.is_empty() { None } else { Some(buf) }
}

/// Append content to the prefix buffer.
///
/// Handles both string content and structured content arrays.
fn append_content(buf: &mut Vec<u8>, content: &serde_json::Value) {
    if let Some(s) = content.as_str() {
        buf.extend_from_slice(s.as_bytes());
        return;
    }
    if let Some(arr) = content.as_array() {
        for block in arr {
            append_content_block(buf, block);
        }
    }
}

/// Append a single content block to the prefix buffer.
///
/// Dispatches on the `type` field: `text` blocks contribute their
/// text, `image_url` blocks contribute the URL, and all other
/// types are serialized as stable JSON.
fn append_content_block(buf: &mut Vec<u8>, block: &serde_json::Value) {
    let block_type = block.get("type").and_then(serde_json::Value::as_str);
    match block_type {
        Some("text") => {
            if let Some(text) = block.get("text").and_then(serde_json::Value::as_str) {
                buf.extend_from_slice(text.as_bytes());
            }
        },
        Some("image_url") => {
            if let Some(url) = block
                .get("image_url")
                .and_then(|v| v.get("url"))
                .and_then(serde_json::Value::as_str)
            {
                buf.extend_from_slice(url.as_bytes());
            }
        },
        _ => {
            if let Ok(serialized) = serde_json::to_vec(block) {
                buf.extend_from_slice(&serialized);
            }
        },
    }
}

/// Extract prefix material from a responses API request.
fn extract_responses_prefix(value: &serde_json::Value) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    if let Some(instructions) = value.get("instructions").and_then(serde_json::Value::as_str) {
        buf.extend_from_slice(instructions.as_bytes());
    }
    if let Some(tools) = value.get("tools")
        && let Ok(serialized) = serde_json::to_vec(tools)
    {
        buf.extend_from_slice(&serialized);
    }
    if let Some(input) = value.get("input")
        && let Ok(serialized) = serde_json::to_vec(input)
    {
        buf.extend_from_slice(&serialized);
    }
    if buf.is_empty() { None } else { Some(buf) }
}

// -----------------------------------------------------------------------------
// Block Hashing
// -----------------------------------------------------------------------------

/// Compute chained block hashes for prefix material.
///
/// The hash chain is seeded with the model name so that identical
/// text under different models produces different hashes. Each block
/// hash depends on the previous, ensuring position-sensitivity.
///
/// Cache salt is not yet supported.
pub(super) fn compute_block_hashes(
    model: &str,
    prefix_material: &[u8],
    block_size_tokens: usize,
    max_blocks: usize,
) -> Vec<u64> {
    let block_size_bytes = block_size_tokens * CHARS_PER_TOKEN;

    let mut hashes = Vec::new();
    let mut prev_hash = seed_hash(model);
    let mut offset = 0;

    while offset < prefix_material.len() && hashes.len() < max_blocks {
        let end = (offset + block_size_bytes).min(prefix_material.len());
        let Some(block) = prefix_material.get(offset..end) else {
            break;
        };
        prev_hash = chain_hash(block, prev_hash);
        hashes.push(prev_hash);
        offset = end;
    }

    hashes
}

/// Produce a seed hash from the model name.
fn seed_hash(model: &str) -> u64 {
    fnv1a_hash(model.as_bytes())
}

/// Hash a block of bytes chained with the previous hash.
fn chain_hash(block: &[u8], prev: u64) -> u64 {
    let prev_bytes = prev.to_le_bytes();
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in block {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    for &byte in &prev_bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// -----------------------------------------------------------------------------
// Prefix Index
// -----------------------------------------------------------------------------

/// In-memory index mapping block hashes to the endpoints that have
/// previously served requests with those prefixes.
pub(super) struct PrefixIndex {
    /// Map from block hash to the set of endpoint names that have it.
    hash_to_endpoints: HashMap<u64, HashSet<Arc<str>>>,

    /// Per-endpoint LRU of block hashes (most recent at back).
    endpoint_lru: HashMap<Arc<str>, VecDeque<u64>>,

    /// Maximum number of block hashes to retain per endpoint.
    lru_capacity: usize,
}

impl PrefixIndex {
    /// Create a new prefix index with the given per-endpoint capacity.
    pub fn new(lru_capacity: usize) -> Self {
        Self {
            hash_to_endpoints: HashMap::new(),
            endpoint_lru: HashMap::new(),
            lru_capacity,
        }
    }

    /// Record that an endpoint has served a request with the given
    /// block hashes.
    ///
    /// If a hash already exists in this endpoint's LRU it is removed
    /// first so it moves to the back without creating a duplicate
    /// that would cause premature eviction.
    pub fn record(&mut self, endpoint_name: &Arc<str>, block_hashes: &[u64]) {
        let lru = self.endpoint_lru.entry(Arc::clone(endpoint_name)).or_default();

        for &hash in block_hashes {
            // Remove existing entry to avoid duplicates.
            lru.retain(|h| *h != hash);

            if lru.len() >= self.lru_capacity
                && let Some(evicted) = lru.pop_front()
            {
                evict_from_map(&mut self.hash_to_endpoints, evicted, endpoint_name);
            }
            lru.push_back(hash);
            self.hash_to_endpoints
                .entry(hash)
                .or_default()
                .insert(Arc::clone(endpoint_name));
        }
    }

    /// Count contiguous prefix matches for each endpoint.
    ///
    /// Returns a map from endpoint name to the number of leading
    /// blocks that matched contiguously. Iteration stops as soon
    /// as a hash has no matching endpoints at all.
    pub fn longest_prefix_match(&self, block_hashes: &[u64], endpoint_names: &[Arc<str>]) -> HashMap<Arc<str>, usize> {
        let mut counts: HashMap<Arc<str>, usize> = HashMap::new();

        for hash in block_hashes {
            let Some(endpoints) = self.hash_to_endpoints.get(hash) else {
                break;
            };
            if endpoints.is_empty() {
                break;
            }
            for ep in endpoints {
                if endpoint_names.iter().any(|n| n.as_ref() == ep.as_ref()) {
                    *counts.entry(Arc::clone(ep)).or_insert(0) += 1;
                }
            }
        }

        counts
    }

    /// Remove state for endpoints that are no longer active.
    pub fn cleanup_stale(&mut self, active_names: &HashSet<Arc<str>>) {
        let stale: Vec<Arc<str>> = self
            .endpoint_lru
            .keys()
            .filter(|k| !active_names.contains(k.as_ref()))
            .cloned()
            .collect();

        for name in &stale {
            self.remove_endpoint(name);
        }
    }

    /// Remove all state for an endpoint.
    fn remove_endpoint(&mut self, name: &Arc<str>) {
        if let Some(lru) = self.endpoint_lru.remove(name) {
            for hash in lru {
                evict_from_map(&mut self.hash_to_endpoints, hash, name);
            }
        }
    }
}

/// Remove a hash-to-endpoint mapping from the global map, cleaning up
/// empty sets to prevent unbounded memory growth.
fn evict_from_map(map: &mut HashMap<u64, HashSet<Arc<str>>>, hash: u64, endpoint_name: &Arc<str>) {
    if let Some(set) = map.get_mut(&hash) {
        set.remove(endpoint_name);
        if set.is_empty() {
            map.remove(&hash);
        }
    }
}

// -----------------------------------------------------------------------------
// Scoring Integration
// -----------------------------------------------------------------------------

/// Compute prefix scores for each endpoint.
///
/// Returns per-endpoint scores in the range `[0.0, 1.0]`, where 1.0
/// means all blocks matched contiguously.
pub(super) fn compute_prefix_scores(
    index: &PrefixIndex,
    block_hashes: &[u64],
    total_blocks: usize,
    endpoints: &[Arc<str>],
) -> HashMap<Arc<str>, f64> {
    if total_blocks == 0 || block_hashes.is_empty() {
        return HashMap::new();
    }

    let matches = index.longest_prefix_match(block_hashes, endpoints);
    let mut scores = HashMap::with_capacity(matches.len());

    for (name, matched) in &matches {
        #[allow(
            clippy::cast_precision_loss,
            reason = "block counts are small enough that f64 is lossless"
        )]
        let score = *matched as f64 / total_blocks as f64;
        scores.insert(Arc::clone(name), score);
    }

    scores
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// Build a plain-text rejection response.
fn reject(status: u16, message: &'static str) -> FilterAction {
    FilterAction::Reject(
        crate::Rejection::status(status)
            .with_header("content-type", "text/plain; charset=utf-8")
            .with_body(bytes::Bytes::from_static(message.as_bytes())),
    )
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

    // -- Config validation --

    #[test]
    fn config_defaults_are_valid() {
        let cfg = PrefixCacheConfig {
            enabled: true,
            weight: default_prefix_weight(),
            block_size_tokens: default_block_size_tokens(),
            max_prefix_blocks_to_match: default_max_prefix_blocks(),
            max_prefix_tokens_to_match: 0,
            lru_capacity_per_endpoint: default_lru_capacity(),
        };

        assert!(
            validate_prefix_cache_config(&cfg).is_ok(),
            "default config should be valid"
        );
    }

    #[test]
    fn config_rejects_negative_weight() {
        let cfg = PrefixCacheConfig {
            enabled: true,
            weight: -1.0,
            block_size_tokens: 16,
            max_prefix_blocks_to_match: 256,
            max_prefix_tokens_to_match: 0,
            lru_capacity_per_endpoint: 1000,
        };

        assert!(
            validate_prefix_cache_config(&cfg).is_err(),
            "negative weight should be rejected"
        );
    }

    #[test]
    fn config_rejects_nan_weight() {
        let cfg = PrefixCacheConfig {
            enabled: true,
            weight: f64::NAN,
            block_size_tokens: 16,
            max_prefix_blocks_to_match: 256,
            max_prefix_tokens_to_match: 0,
            lru_capacity_per_endpoint: 1000,
        };

        assert!(
            validate_prefix_cache_config(&cfg).is_err(),
            "NaN weight should be rejected"
        );
    }

    #[test]
    fn config_rejects_infinite_weight() {
        let cfg = PrefixCacheConfig {
            enabled: true,
            weight: f64::INFINITY,
            block_size_tokens: 16,
            max_prefix_blocks_to_match: 256,
            max_prefix_tokens_to_match: 0,
            lru_capacity_per_endpoint: 1000,
        };

        assert!(
            validate_prefix_cache_config(&cfg).is_err(),
            "infinite weight should be rejected"
        );
    }

    #[test]
    fn config_rejects_zero_block_size() {
        let cfg = PrefixCacheConfig {
            enabled: true,
            weight: 1.0,
            block_size_tokens: 0,
            max_prefix_blocks_to_match: 256,
            max_prefix_tokens_to_match: 0,
            lru_capacity_per_endpoint: 1000,
        };

        assert!(
            validate_prefix_cache_config(&cfg).is_err(),
            "zero block_size_tokens should be rejected"
        );
    }

    #[test]
    fn config_rejects_zero_lru_capacity() {
        let cfg = PrefixCacheConfig {
            enabled: true,
            weight: 1.0,
            block_size_tokens: 16,
            max_prefix_blocks_to_match: 256,
            max_prefix_tokens_to_match: 0,
            lru_capacity_per_endpoint: 0,
        };

        assert!(
            validate_prefix_cache_config(&cfg).is_err(),
            "zero lru_capacity should be rejected"
        );
    }

    #[test]
    fn effective_max_blocks_uses_token_override() {
        let cfg = PrefixCacheConfig {
            enabled: true,
            weight: 1.0,
            block_size_tokens: 16,
            max_prefix_blocks_to_match: 256,
            max_prefix_tokens_to_match: 64,
            lru_capacity_per_endpoint: 1000,
        };

        assert_eq!(cfg.effective_max_blocks(), 4, "64 tokens / 16 block_size = 4 blocks");
    }

    #[test]
    fn effective_max_blocks_minimum_one() {
        let cfg = PrefixCacheConfig {
            enabled: true,
            weight: 1.0,
            block_size_tokens: 16,
            max_prefix_blocks_to_match: 256,
            max_prefix_tokens_to_match: 1,
            lru_capacity_per_endpoint: 1000,
        };

        assert_eq!(
            cfg.effective_max_blocks(),
            1,
            "tokens < block_size should still give at least 1 block"
        );
    }

    #[test]
    fn effective_max_blocks_uses_configured_when_zero_tokens() {
        let cfg = PrefixCacheConfig {
            enabled: true,
            weight: 1.0,
            block_size_tokens: 16,
            max_prefix_blocks_to_match: 128,
            max_prefix_tokens_to_match: 0,
            lru_capacity_per_endpoint: 1000,
        };

        assert_eq!(
            cfg.effective_max_blocks(),
            128,
            "zero tokens should fall back to max_prefix_blocks_to_match"
        );
    }

    // -- Request parsing: chat completions --

    #[test]
    fn extracts_chat_completions_prefix() {
        let body = br#"{"model":"gpt-4","messages":[{"role":"system","content":"You are helpful."},{"role":"user","content":"Hello"}]}"#;

        let info = extract_request_info(body).unwrap();

        assert_eq!(info.model, "gpt-4", "model name");
        let prefix = info.prefix_material.unwrap();
        let expected = b"systemYou are helpful.userHello";
        assert_eq!(prefix, expected.to_vec(), "chat prefix should concatenate role+content");
    }

    #[test]
    fn extracts_completions_prefix() {
        let body = br#"{"model":"gpt-3","prompt":"Once upon a time"}"#;

        let info = extract_request_info(body).unwrap();

        assert_eq!(info.model, "gpt-3", "model name");
        let prefix = info.prefix_material.unwrap();
        assert_eq!(
            prefix,
            b"Once upon a time".to_vec(),
            "completions prefix should be prompt bytes"
        );
    }

    #[test]
    fn extracts_responses_prefix() {
        let body = br#"{"model":"gpt-4","input":"describe the sky","instructions":"Be poetic"}"#;

        let info = extract_request_info(body).unwrap();

        assert_eq!(info.model, "gpt-4", "model name");
        assert!(
            info.prefix_material.is_some(),
            "responses API should produce prefix material"
        );
    }

    #[test]
    fn no_prefix_when_no_messages_or_prompt() {
        let body = br#"{"model":"gpt-4","stream":true}"#;

        let info = extract_request_info(body).unwrap();

        assert_eq!(info.model, "gpt-4", "model name");
        assert!(
            info.prefix_material.is_none(),
            "no messages/prompt/input should yield None"
        );
    }

    #[test]
    fn rejects_invalid_json() {
        let body = b"not json";

        let result = extract_request_info(body);

        assert!(result.is_err(), "invalid JSON should be rejected");
    }

    #[test]
    fn rejects_missing_model() {
        let body = br#"{"messages":[]}"#;

        let result = extract_request_info(body);

        assert!(result.is_err(), "missing model should be rejected");
    }

    // -- Block hashing --

    #[test]
    fn block_hashes_are_deterministic() {
        let material = vec![b'a'; 256];

        let h1 = compute_block_hashes("model", &material, 16, 256);
        let h2 = compute_block_hashes("model", &material, 16, 256);

        assert_eq!(h1, h2, "hashes should be deterministic within a process");
    }

    #[test]
    fn model_name_affects_hashes() {
        let material = vec![b'a'; 256];

        let h1 = compute_block_hashes("model-a", &material, 16, 256);
        let h2 = compute_block_hashes("model-b", &material, 16, 256);

        assert_ne!(h1, h2, "different models should produce different hashes");
    }

    #[test]
    fn block_cap_is_applied() {
        let material = vec![b'a'; 1024];

        let hashes = compute_block_hashes("model", &material, 16, 2);

        assert_eq!(hashes.len(), 2, "should cap at max_blocks=2");
    }

    #[test]
    fn short_material_produces_partial_block_hash() {
        let material = vec![b'a'; 10];

        let hashes = compute_block_hashes("model", &material, 16, 256);

        assert_eq!(
            hashes.len(),
            1,
            "material shorter than one full block should still produce one hash"
        );
    }

    // -- Prefix index --

    #[test]
    fn longest_contiguous_prefix_matching() {
        let mut index = PrefixIndex::new(1000);
        let ep: Arc<str> = Arc::from("ep-a");
        let hashes = vec![100, 200, 300, 400];

        index.record(&ep, &hashes);
        let matches = index.longest_prefix_match(&hashes, &[Arc::clone(&ep)]);

        assert_eq!(
            matches.get(&ep).copied(),
            Some(4),
            "all 4 blocks should match contiguously"
        );
    }

    #[test]
    fn prefix_match_stops_at_gap() {
        let mut index = PrefixIndex::new(1000);
        let ep: Arc<str> = Arc::from("ep-a");

        index.record(&ep, &[100, 200]);
        let query = vec![100, 200, 300, 400];
        let matches = index.longest_prefix_match(&query, &[Arc::clone(&ep)]);

        assert_eq!(matches.get(&ep).copied(), Some(2), "should match first 2 blocks only");
    }

    #[test]
    fn missing_endpoint_scores_zero() {
        let index = PrefixIndex::new(1000);
        let ep: Arc<str> = Arc::from("ep-unknown");
        let hashes = vec![100, 200];

        let matches = index.longest_prefix_match(&hashes, &[ep]);

        assert!(matches.is_empty(), "unrecorded endpoint should have no matches");
    }

    #[test]
    fn lru_eviction_works() {
        let mut index = PrefixIndex::new(3);
        let ep: Arc<str> = Arc::from("ep-a");

        index.record(&ep, &[1, 2, 3]);
        assert!(
            index.hash_to_endpoints.contains_key(&1),
            "hash 1 should be present before eviction"
        );

        index.record(&ep, &[4]);
        assert!(
            !index.hash_to_endpoints.contains_key(&1),
            "hash 1 should be evicted after capacity exceeded"
        );
        assert!(
            index.hash_to_endpoints.contains_key(&4),
            "hash 4 should be present after recording"
        );
    }

    #[test]
    fn stale_endpoint_cleanup() {
        let mut index = PrefixIndex::new(1000);
        let ep_a: Arc<str> = Arc::from("ep-a");
        let ep_b: Arc<str> = Arc::from("ep-b");

        index.record(&ep_a, &[100, 200]);
        index.record(&ep_b, &[300, 400]);

        let active: HashSet<Arc<str>> = [Arc::clone(&ep_a)].into_iter().collect();
        index.cleanup_stale(&active);

        assert!(
            index.endpoint_lru.contains_key(&ep_a),
            "active endpoint should be retained"
        );
        assert!(
            !index.endpoint_lru.contains_key(&ep_b),
            "stale endpoint should be removed"
        );
        assert!(
            !index.hash_to_endpoints.contains_key(&300),
            "stale endpoint's hashes should be cleaned up"
        );
    }

    // -- Scoring --

    #[test]
    fn scoring_prefers_prefix_hit_endpoint() {
        let mut index = PrefixIndex::new(1000);
        let ep_hit: Arc<str> = Arc::from("ep-hit");
        let ep_miss: Arc<str> = Arc::from("ep-miss");

        let hashes = vec![10, 20, 30, 40];
        index.record(&ep_hit, &hashes);

        let endpoints = vec![Arc::clone(&ep_hit), Arc::clone(&ep_miss)];
        let scores = compute_prefix_scores(&index, &hashes, hashes.len(), &endpoints);

        let hit_score = scores.get(&ep_hit).copied().unwrap_or(0.0);
        let miss_score = scores.get(&ep_miss).copied().unwrap_or(0.0);

        assert!(
            hit_score > miss_score,
            "endpoint with prefix hit ({hit_score}) should score higher than miss ({miss_score})"
        );
        assert!(
            (hit_score - 1.0).abs() < f64::EPSILON,
            "full match should give score 1.0, got {hit_score}"
        );
    }

    #[test]
    fn scoring_returns_empty_for_zero_blocks() {
        let index = PrefixIndex::new(1000);
        let ep: Arc<str> = Arc::from("ep-a");

        let scores = compute_prefix_scores(&index, &[], 0, &[ep]);

        assert!(scores.is_empty(), "zero blocks should produce empty scores");
    }

    #[test]
    fn partial_prefix_match_score() {
        let mut index = PrefixIndex::new(1000);
        let ep: Arc<str> = Arc::from("ep-a");

        index.record(&ep, &[10, 20]);
        let query = vec![10, 20, 30, 40];
        let scores = compute_prefix_scores(&index, &query, query.len(), &[Arc::clone(&ep)]);

        let score = scores.get(&ep).copied().unwrap_or(0.0);
        assert!(
            (score - 0.5).abs() < f64::EPSILON,
            "2/4 match should give 0.5, got {score}"
        );
    }

    // -- Item 2: Structured content and non-string prompts --

    #[test]
    fn extracts_chat_prefix_with_content_array() {
        let body = br#"{"model":"gpt-4","messages":[{"role":"user","content":[{"type":"text","text":"hello"},{"type":"image_url","image_url":{"url":"https://img.example.com/cat.png"}}]}]}"#;

        let info = extract_request_info(body).unwrap();

        let prefix = info.prefix_material.unwrap();
        let expected = b"userhellohttps://img.example.com/cat.png";
        assert_eq!(
            prefix,
            expected.to_vec(),
            "should extract text and image_url from content array"
        );
    }

    #[test]
    fn extracts_chat_prefix_with_unknown_content_block_type() {
        let body = br#"{"model":"gpt-4","messages":[{"role":"user","content":[{"type":"audio","data":"abc123"}]}]}"#;

        let info = extract_request_info(body).unwrap();

        let prefix = info.prefix_material.unwrap();
        let prefix_str = String::from_utf8_lossy(&prefix);
        assert!(prefix_str.starts_with("user"), "should include role, got: {prefix_str}");
        assert!(
            prefix_str.contains("audio"),
            "unknown block type should be JSON-serialized, got: {prefix_str}"
        );
    }

    #[test]
    fn extracts_completions_prefix_array_prompt() {
        let body = br#"{"model":"gpt-3","prompt":["Once upon","a time"]}"#;

        let info = extract_request_info(body).unwrap();

        let prefix = info.prefix_material.unwrap();
        let expected = br#"["Once upon","a time"]"#;
        assert_eq!(prefix, expected.to_vec(), "array prompt should be JSON-serialized");
    }

    #[test]
    fn extracts_completions_prefix_object_prompt() {
        let body = br#"{"model":"gpt-3","prompt":{"text":"hello"}}"#;

        let info = extract_request_info(body).unwrap();

        let prefix = info.prefix_material.unwrap();
        let expected = br#"{"text":"hello"}"#;
        assert_eq!(prefix, expected.to_vec(), "object prompt should be JSON-serialized");
    }

    // -- Item 4: Partial final block --

    #[test]
    fn partial_final_block_produces_hash() {
        // 80 bytes = 1 full block (64 bytes at 16 tokens * 4 chars) + 16 partial
        let material = vec![b'x'; 80];

        let hashes = compute_block_hashes("model", &material, 16, 256);

        assert_eq!(
            hashes.len(),
            2,
            "80 bytes should produce 2 hashes: 1 full block + 1 partial"
        );
    }

    // -- Item 5: FNV-1a hash stability --

    #[test]
    fn fnv1a_hash_is_stable() {
        let result = fnv1a_hash(b"hello");
        assert_eq!(
            result, 0xA430_D846_80AA_BD0B,
            "FNV-1a of 'hello' must produce the known stable value"
        );
    }

    #[test]
    fn fnv1a_hash_empty_input() {
        let result = fnv1a_hash(b"");
        assert_eq!(
            result, FNV_OFFSET_BASIS,
            "FNV-1a of empty input should be the offset basis"
        );
    }

    // -- Item 6: LRU duplicate regression --

    #[test]
    fn record_same_hash_twice_no_duplicate() {
        let mut index = PrefixIndex::new(1000);
        let ep: Arc<str> = Arc::from("ep-a");

        index.record(&ep, &[42]);
        index.record(&ep, &[42]);

        let lru = index.endpoint_lru.get(&ep).unwrap();
        let count = lru.iter().filter(|&&h| h == 42).count();
        assert_eq!(count, 1, "hash 42 should appear only once in LRU after recording twice");
    }

    // -- Item 7: longest_prefix_match with repeated hash values --

    #[test]
    fn longest_prefix_match_with_repeated_hashes() {
        let mut index = PrefixIndex::new(1000);
        let ep: Arc<str> = Arc::from("ep-a");

        // Record a sequence with a repeated hash value.
        index.record(&ep, &[100, 100, 200]);
        let query = vec![100, 100, 200];
        let matches = index.longest_prefix_match(&query, &[Arc::clone(&ep)]);

        assert!(
            matches.get(&ep).copied().unwrap_or(0) > 0,
            "repeated hashes should still produce matches"
        );
    }

    // -- Item 8: Config validation rejects both max values zero --

    #[test]
    fn config_rejects_both_max_blocks_and_tokens_zero() {
        let cfg = PrefixCacheConfig {
            enabled: true,
            weight: 1.0,
            block_size_tokens: 16,
            max_prefix_blocks_to_match: 0,
            max_prefix_tokens_to_match: 0,
            lru_capacity_per_endpoint: 1000,
        };

        assert!(
            validate_prefix_cache_config(&cfg).is_err(),
            "both max_prefix_blocks_to_match and max_prefix_tokens_to_match being zero should be rejected"
        );
    }
}
