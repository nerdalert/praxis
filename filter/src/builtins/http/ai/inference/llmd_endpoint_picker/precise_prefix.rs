// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Precise prefix-cache scoring for the llm-d endpoint picker.
//!
//! Matches the Go EPP's token-level prefix-cache behavior: tokenizes
//! request text with a HuggingFace tokenizer, chunks into exact
//! `block_size_tokens` blocks (no partial blocks), and computes
//! chained FNV-64a hashes over CBOR-encoded `[parent, tokens, extra]`
//! payloads. This produces block keys that are bit-identical to those
//! produced by the Go `llm-d-kv-cache` `TokenProcessor`.
//!
//! Speculative indexing seeds the selected endpoint with computed
//! block keys after routing, allowing subsequent requests sharing the
//! same prefix to benefit from the prediction before KV-events
//! confirm the actual cache state.
//!
//! **Note:** KV-events ingestion (ZMQ subscriber) is deferred to a
//! follow-up PR. The current implementation relies on speculative
//! indexing and the approximate-mode LRU for warm-up.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Instant,
};

use serde::Deserialize;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;

/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

/// Default speculative entry TTL in milliseconds.
const DEFAULT_SPECULATIVE_TTL_MS: u64 = 30_000; // 30 seconds

// -----------------------------------------------------------------------------
// Config
// -----------------------------------------------------------------------------

/// Configuration for precise prefix-cache scoring.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PrecisePrefixConfig {
    /// Path-based tokenizer configuration.
    pub tokenizer: TokenizerConfig,

    /// Whether to seed the selected endpoint with the request's block
    /// keys after routing (speculative indexing).
    #[serde(default)]
    pub speculative_indexing: bool,

    /// Time-to-live for speculative entries in milliseconds.
    #[serde(default = "default_speculative_ttl_ms")]
    pub speculative_ttl_ms: u64,

    /// Hash seed string, aligned with vLLM's `PYTHONHASHSEED`.
    #[serde(default)]
    pub hash_seed: String,

    /// Number of tokens per hash block.
    #[serde(default = "default_block_size_tokens")]
    pub block_size_tokens: usize,
}

/// Tokenizer file path configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TokenizerConfig {
    /// Filesystem path to the HuggingFace `tokenizer.json`.
    pub path: String,
}

/// Default speculative TTL.
fn default_speculative_ttl_ms() -> u64 {
    DEFAULT_SPECULATIVE_TTL_MS
}

/// Default block size in tokens.
fn default_block_size_tokens() -> usize {
    super::prefix_cache::DEFAULT_BLOCK_SIZE_TOKENS
}

// -----------------------------------------------------------------------------
// Tokenizer Wrapper
// -----------------------------------------------------------------------------

/// Thin wrapper around a HuggingFace tokenizer loaded from a file.
pub(super) struct PreciseTokenizer {
    /// The underlying tokenizer instance.
    tokenizer: tokenizers::Tokenizer,
}

impl PreciseTokenizer {
    /// Load a tokenizer from a `tokenizer.json` file.
    ///
    /// Returns `None` if the file cannot be loaded.
    pub fn from_file(path: &str) -> Option<Self> {
        let tokenizer = tokenizers::Tokenizer::from_file(path).ok()?;
        Some(Self { tokenizer })
    }

    /// Encode text into token IDs without adding special tokens.
    ///
    /// Returns `None` if encoding fails.
    pub fn encode(&self, text: &str) -> Option<Vec<u32>> {
        let encoding = self.tokenizer.encode(text, false).ok()?;
        Some(encoding.get_ids().to_vec())
    }
}

// -----------------------------------------------------------------------------
// Block Key Computation (matching Go EPP)
// -----------------------------------------------------------------------------

/// Compute precise block keys matching the Go EPP algorithm.
///
/// 1. Compute `initHash = hash(fnv64a(hash_seed), [], model_name)`.
/// 2. Chunk tokens into exact `block_size` blocks (no partial).
/// 3. Chain: each block's hash uses the previous as parent.
pub(super) fn compute_precise_block_keys(
    tokens: &[u32],
    model_name: &str,
    block_size: usize,
    init_hash_seed: u64,
) -> Vec<u64> {
    let init_hash = hash_cbor_payload(init_hash_seed, &[], model_name);
    let chunks = chunk_tokens_exact(tokens, block_size);
    build_hash_chain(init_hash, &chunks)
}

/// Build the chained hash sequence from token chunks.
fn build_hash_chain(init_hash: u64, chunks: &[&[u32]]) -> Vec<u64> {
    let mut parent = init_hash;
    let mut keys = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        parent = hash_cbor_payload(parent, chunk, "");
        keys.push(parent);
    }
    keys
}

/// Compute FNV-64a of the hash seed string.
///
/// Matches the Go EPP's `fnv.New64a()` + `Write([]byte(hashSeed))`.
pub(super) fn compute_init_hash_seed(hash_seed: &str) -> u64 {
    fnv1a_hash(hash_seed.as_bytes())
}

/// CBOR-encode `[parent, tokens, extra]` and FNV-64a the result.
///
/// When `extra` is empty, CBOR null is used (matching Go `nil`).
/// When non-empty, a CBOR text string is used.
fn hash_cbor_payload(parent: u64, tokens: &[u32], extra: &str) -> u64 {
    let mut payload = Vec::new();
    let cbor_value = build_cbor_array(parent, tokens, extra);
    // Encoding to a Vec cannot fail for well-formed values.
    let _unused = ciborium::into_writer(&cbor_value, &mut payload);
    fnv1a_hash(&payload)
}

/// Build the CBOR array value `[parent, tokens, extra]`.
fn build_cbor_array(
    parent: u64,
    tokens: &[u32],
    extra: &str,
) -> ciborium::Value {
    let token_values: Vec<ciborium::Value> = tokens
        .iter()
        .map(|t| ciborium::Value::Integer((*t).into()))
        .collect();

    let extra_value = if extra.is_empty() {
        ciborium::Value::Null
    } else {
        ciborium::Value::Text(extra.to_owned())
    };

    ciborium::Value::Array(vec![
        ciborium::Value::Integer(parent.into()),
        ciborium::Value::Array(token_values),
        extra_value,
    ])
}

/// Compute a deterministic FNV-1a 64-bit hash of `data`.
fn fnv1a_hash(data: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Split tokens into exact `block_size` chunks, discarding any
/// trailing partial block.
fn chunk_tokens_exact(tokens: &[u32], block_size: usize) -> Vec<&[u32]> {
    tokens
        .chunks(block_size)
        .filter(|c| c.len() == block_size)
        .collect()
}

// -----------------------------------------------------------------------------
// Precise Prefix Index
// -----------------------------------------------------------------------------

/// In-memory index mapping precise block keys (uint64) to endpoints.
///
/// Structurally identical to the approximate `PrefixIndex` but
/// operates on tokenizer-derived keys rather than byte-level hashes.
pub(super) struct PrecisePrefixIndex {
    /// Map from block key to endpoints that have it cached.
    hash_to_endpoints: HashMap<u64, HashSet<Arc<str>>>,

    /// Per-endpoint LRU of block keys (most recent at back).
    endpoint_lru: HashMap<Arc<str>, VecDeque<u64>>,

    /// Maximum block keys to retain per endpoint.
    lru_capacity: usize,

    /// Pending speculative entries awaiting confirmation or expiry.
    speculative: Vec<SpeculativeEntry>,
}

/// A speculative cache entry seeded after endpoint selection.
struct SpeculativeEntry {
    /// Block keys speculatively assigned to the endpoint.
    block_keys: Vec<u64>,

    /// The endpoint name this prediction is for.
    endpoint: Arc<str>,

    /// When this entry expires and should be evicted.
    expires_at: Instant,
}

impl PrecisePrefixIndex {
    /// Create a new precise prefix index with the given capacity.
    pub fn new(lru_capacity: usize) -> Self {
        Self {
            hash_to_endpoints: HashMap::new(),
            endpoint_lru: HashMap::new(),
            lru_capacity,
            speculative: Vec::new(),
        }
    }

    /// Record that an endpoint has block keys cached.
    ///
    /// Deduplicates within the LRU and evicts oldest entries when
    /// capacity is exceeded.
    pub fn record(&mut self, endpoint: &Arc<str>, block_keys: &[u64]) {
        let lru = self.endpoint_lru.entry(Arc::clone(endpoint)).or_default();

        for &key in block_keys {
            lru.retain(|h| *h != key);
            if lru.len() >= self.lru_capacity {
                evict_oldest(lru, &mut self.hash_to_endpoints, endpoint);
            }
            lru.push_back(key);
            self.hash_to_endpoints
                .entry(key)
                .or_default()
                .insert(Arc::clone(endpoint));
        }
    }

    /// Record speculative block keys with a TTL.
    pub fn record_speculative(
        &mut self,
        endpoint: &Arc<str>,
        block_keys: Vec<u64>,
        ttl_ms: u64,
    ) {
        let expires_at = Instant::now() + std::time::Duration::from_millis(ttl_ms);
        self.record(endpoint, &block_keys);
        self.speculative.push(SpeculativeEntry {
            block_keys,
            endpoint: Arc::clone(endpoint),
            expires_at,
        });
    }

    /// Evict expired speculative entries.
    pub fn evict_expired_speculative(&mut self) {
        let now = Instant::now();
        let expired: Vec<SpeculativeEntry> = self
            .speculative
            .extract_if(.., |e| e.expires_at <= now)
            .collect();

        for entry in &expired {
            for &key in &entry.block_keys {
                remove_hash_mapping(
                    &mut self.hash_to_endpoints,
                    key,
                    &entry.endpoint,
                );
            }
        }
    }

    /// Count contiguous prefix matches for each endpoint.
    ///
    /// Stops at the first block key with no matching endpoints.
    pub fn longest_prefix_match(
        &self,
        block_keys: &[u64],
        candidates: &[Arc<str>],
    ) -> HashMap<Arc<str>, usize> {
        let mut counts: HashMap<Arc<str>, usize> = HashMap::new();

        for key in block_keys {
            let Some(endpoints) = self.hash_to_endpoints.get(key) else {
                break;
            };
            if endpoints.is_empty() {
                break;
            }
            for ep in endpoints {
                if candidates.iter().any(|c| c.as_ref() == ep.as_ref()) {
                    *counts.entry(Arc::clone(ep)).or_insert(0) += 1;
                }
            }
        }

        counts
    }

    /// Remove state for endpoints no longer in the active set.
    pub fn cleanup_stale(&mut self, active: &HashSet<Arc<str>>) {
        let stale: Vec<Arc<str>> = self
            .endpoint_lru
            .keys()
            .filter(|k| !active.contains(k.as_ref()))
            .cloned()
            .collect();

        for name in &stale {
            self.remove_endpoint(name);
        }
        self.speculative.retain(|e| active.contains(&e.endpoint));
    }

    /// Remove all state for a single endpoint.
    fn remove_endpoint(&mut self, name: &Arc<str>) {
        if let Some(lru) = self.endpoint_lru.remove(name) {
            for key in lru {
                remove_hash_mapping(&mut self.hash_to_endpoints, key, name);
            }
        }
    }
}

/// Evict the oldest entry from the LRU and clean up the hash map.
fn evict_oldest(
    lru: &mut VecDeque<u64>,
    map: &mut HashMap<u64, HashSet<Arc<str>>>,
    endpoint: &Arc<str>,
) {
    if let Some(evicted) = lru.pop_front() {
        remove_hash_mapping(map, evicted, endpoint);
    }
}

/// Remove one endpoint from a hash-to-endpoints mapping entry,
/// deleting the entry if empty.
fn remove_hash_mapping(
    map: &mut HashMap<u64, HashSet<Arc<str>>>,
    key: u64,
    endpoint: &Arc<str>,
) {
    if let Some(set) = map.get_mut(&key) {
        set.remove(endpoint);
        if set.is_empty() {
            map.remove(&key);
        }
    }
}

// -----------------------------------------------------------------------------
// Scoring
// -----------------------------------------------------------------------------

/// Compute precise prefix scores for each endpoint.
///
/// Returns per-endpoint scores in `[0.0, 1.0]`, where 1.0 means all
/// blocks matched contiguously.
pub(super) fn compute_precise_prefix_scores(
    index: &PrecisePrefixIndex,
    block_keys: &[u64],
    total_blocks: usize,
    endpoints: &[Arc<str>],
) -> HashMap<Arc<str>, f64> {
    if total_blocks == 0 || block_keys.is_empty() {
        return HashMap::new();
    }

    let matches = index.longest_prefix_match(block_keys, endpoints);
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
// Validation
// -----------------------------------------------------------------------------

/// Validate precise prefix-cache configuration.
///
/// # Errors
///
/// Returns [`crate::FilterError`] when any field has an invalid value.
pub(super) fn validate_precise_config(
    cfg: &PrecisePrefixConfig,
) -> Result<(), crate::FilterError> {
    if cfg.tokenizer.path.trim().is_empty() {
        return Err(
            "llmd_endpoint_picker: precise.tokenizer.path must not be empty"
                .into(),
        );
    }
    if cfg.block_size_tokens == 0 {
        return Err(
            "llmd_endpoint_picker: precise.block_size_tokens must be > 0"
                .into(),
        );
    }
    if cfg.speculative_ttl_ms == 0 && cfg.speculative_indexing {
        return Err(
            "llmd_endpoint_picker: precise.speculative_ttl_ms must be > 0 when speculative_indexing is enabled"
                .into(),
        );
    }
    Ok(())
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

    // -- FNV-1a stability --

    #[test]
    fn fnv1a_hash_matches_known_value() {
        let result = fnv1a_hash(b"hello");
        assert_eq!(
            result, 0xA430_D846_80AA_BD0B,
            "FNV-1a of 'hello' must match known stable value"
        );
    }

    #[test]
    fn fnv1a_hash_empty_is_offset_basis() {
        let result = fnv1a_hash(b"");
        assert_eq!(
            result, FNV_OFFSET_BASIS,
            "FNV-1a of empty input should be offset basis"
        );
    }

    // -- CBOR + FNV hash stability --

    #[test]
    fn cbor_fnv_hash_is_deterministic() {
        let h1 = hash_cbor_payload(42, &[1, 2, 3], "model-a");
        let h2 = hash_cbor_payload(42, &[1, 2, 3], "model-a");
        assert_eq!(h1, h2, "same inputs must produce same hash");
    }

    #[test]
    fn cbor_fnv_hash_differs_with_different_parent() {
        let h1 = hash_cbor_payload(1, &[1, 2], "");
        let h2 = hash_cbor_payload(2, &[1, 2], "");
        assert_ne!(h1, h2, "different parents must produce different hashes");
    }

    #[test]
    fn cbor_fnv_hash_differs_with_different_tokens() {
        let h1 = hash_cbor_payload(0, &[1, 2, 3], "");
        let h2 = hash_cbor_payload(0, &[4, 5, 6], "");
        assert_ne!(h1, h2, "different tokens must produce different hashes");
    }

    #[test]
    fn cbor_fnv_hash_differs_with_different_extra() {
        let h1 = hash_cbor_payload(0, &[1], "model-a");
        let h2 = hash_cbor_payload(0, &[1], "model-b");
        assert_ne!(h1, h2, "different extra must produce different hashes");
    }

    #[test]
    fn cbor_fnv_hash_nil_vs_empty_string() {
        let h_nil = hash_cbor_payload(0, &[1], "");
        let h_str = hash_cbor_payload(0, &[1], "something");
        assert_ne!(
            h_nil, h_str,
            "nil extra vs string extra must differ"
        );
    }

    // -- Token chunking --

    #[test]
    fn chunk_tokens_exact_full_blocks_only() {
        let tokens: Vec<u32> = (0..20).collect();
        let chunks = chunk_tokens_exact(&tokens, 8);
        assert_eq!(chunks.len(), 2, "20 tokens / 8 = 2 full blocks, 4 discarded");
        assert_eq!(chunks[0].len(), 8, "first chunk is 8 tokens");
        assert_eq!(chunks[1].len(), 8, "second chunk is 8 tokens");
    }

    #[test]
    fn chunk_tokens_exact_no_partial() {
        let tokens: Vec<u32> = (0..7).collect();
        let chunks = chunk_tokens_exact(&tokens, 8);
        assert!(chunks.is_empty(), "7 tokens with block_size 8 produces no blocks");
    }

    #[test]
    fn chunk_tokens_exact_exact_boundary() {
        let tokens: Vec<u32> = (0..16).collect();
        let chunks = chunk_tokens_exact(&tokens, 16);
        assert_eq!(chunks.len(), 1, "exactly one full block");
    }

    // -- Block key computation --

    #[test]
    fn model_name_affects_block_keys() {
        let tokens: Vec<u32> = (1..=16).collect();
        let seed = compute_init_hash_seed("");
        let k1 = compute_precise_block_keys(&tokens, "model-a", 16, seed);
        let k2 = compute_precise_block_keys(&tokens, "model-b", 16, seed);
        assert_ne!(k1, k2, "different models must produce different keys");
    }

    #[test]
    fn hash_seed_affects_block_keys() {
        let tokens: Vec<u32> = (1..=16).collect();
        let seed_a = compute_init_hash_seed("seed-a");
        let seed_b = compute_init_hash_seed("seed-b");
        let k1 = compute_precise_block_keys(&tokens, "model", 16, seed_a);
        let k2 = compute_precise_block_keys(&tokens, "model", 16, seed_b);
        assert_ne!(k1, k2, "different seeds must produce different keys");
    }

    #[test]
    fn block_keys_are_deterministic() {
        let tokens: Vec<u32> = (1..=32).collect();
        let seed = compute_init_hash_seed("test");
        let k1 = compute_precise_block_keys(&tokens, "model", 16, seed);
        let k2 = compute_precise_block_keys(&tokens, "model", 16, seed);
        assert_eq!(k1, k2, "same inputs must produce same keys");
        assert_eq!(k1.len(), 2, "32 tokens / 16 = 2 blocks");
    }

    #[test]
    fn no_tokens_produces_no_keys() {
        let seed = compute_init_hash_seed("");
        let keys = compute_precise_block_keys(&[], "model", 16, seed);
        assert!(keys.is_empty(), "empty tokens should produce no keys");
    }

    #[test]
    fn partial_tokens_produce_no_keys() {
        let tokens: Vec<u32> = (1..=15).collect();
        let seed = compute_init_hash_seed("");
        let keys = compute_precise_block_keys(&tokens, "model", 16, seed);
        assert!(keys.is_empty(), "15 tokens with block_size 16 produces no keys");
    }

    // -- Precise prefix index --

    #[test]
    fn index_record_and_lookup() {
        let mut index = PrecisePrefixIndex::new(1000);
        let ep: Arc<str> = Arc::from("ep-a");
        let keys = vec![100, 200, 300];

        index.record(&ep, &keys);
        let matches = index.longest_prefix_match(&keys, &[Arc::clone(&ep)]);

        assert_eq!(
            matches.get(&ep).copied(),
            Some(3),
            "all 3 blocks should match"
        );
    }

    #[test]
    fn index_match_stops_at_gap() {
        let mut index = PrecisePrefixIndex::new(1000);
        let ep: Arc<str> = Arc::from("ep-a");

        index.record(&ep, &[100, 200]);
        let query = vec![100, 200, 300, 400];
        let matches = index.longest_prefix_match(&query, &[Arc::clone(&ep)]);

        assert_eq!(
            matches.get(&ep).copied(),
            Some(2),
            "should match first 2 blocks only"
        );
    }

    #[test]
    fn index_lru_eviction() {
        let mut index = PrecisePrefixIndex::new(3);
        let ep: Arc<str> = Arc::from("ep-a");

        index.record(&ep, &[1, 2, 3]);
        assert!(
            index.hash_to_endpoints.contains_key(&1),
            "key 1 should exist before eviction"
        );

        index.record(&ep, &[4]);
        assert!(
            !index.hash_to_endpoints.contains_key(&1),
            "key 1 should be evicted"
        );
        assert!(
            index.hash_to_endpoints.contains_key(&4),
            "key 4 should exist"
        );
    }

    #[test]
    fn index_cleanup_stale() {
        let mut index = PrecisePrefixIndex::new(1000);
        let ep_a: Arc<str> = Arc::from("ep-a");
        let ep_b: Arc<str> = Arc::from("ep-b");

        index.record(&ep_a, &[100]);
        index.record(&ep_b, &[200]);

        let active: HashSet<Arc<str>> = [Arc::clone(&ep_a)].into_iter().collect();
        index.cleanup_stale(&active);

        assert!(
            index.endpoint_lru.contains_key(&ep_a),
            "active endpoint retained"
        );
        assert!(
            !index.endpoint_lru.contains_key(&ep_b),
            "stale endpoint removed"
        );
    }

    // -- Speculative indexing --

    #[test]
    fn speculative_entries_expire() {
        let mut index = PrecisePrefixIndex::new(1000);
        let ep: Arc<str> = Arc::from("ep-a");

        // Record with 0ms TTL (already expired).
        index.record_speculative(&ep, vec![100, 200], 0);

        // Before eviction, keys should be present.
        assert!(
            index.hash_to_endpoints.contains_key(&100),
            "key should exist before evict"
        );

        // A tiny sleep is not needed since TTL=0 means instant expiry,
        // but Instant::now() may not have advanced. Wait for the
        // condition.
        std::thread::sleep(std::time::Duration::from_millis(1));
        index.evict_expired_speculative();

        assert!(
            !index.hash_to_endpoints.contains_key(&100),
            "key should be evicted after expiry"
        );
        assert!(
            !index.hash_to_endpoints.contains_key(&200),
            "key should be evicted after expiry"
        );
    }

    #[test]
    fn speculative_entries_not_expired_stay() {
        let mut index = PrecisePrefixIndex::new(1000);
        let ep: Arc<str> = Arc::from("ep-a");

        index.record_speculative(&ep, vec![100], 60_000);
        index.evict_expired_speculative();

        assert!(
            index.hash_to_endpoints.contains_key(&100),
            "non-expired speculative entry should remain"
        );
    }

    // -- Scoring --

    #[test]
    fn scoring_full_match() {
        let mut index = PrecisePrefixIndex::new(1000);
        let ep: Arc<str> = Arc::from("ep-a");
        let keys = vec![10, 20, 30, 40];

        index.record(&ep, &keys);
        let scores = compute_precise_prefix_scores(
            &index,
            &keys,
            keys.len(),
            &[Arc::clone(&ep)],
        );

        let score = scores.get(&ep).copied().unwrap_or(0.0);
        assert!(
            (score - 1.0).abs() < f64::EPSILON,
            "full match should give 1.0, got {score}"
        );
    }

    #[test]
    fn scoring_partial_match() {
        let mut index = PrecisePrefixIndex::new(1000);
        let ep: Arc<str> = Arc::from("ep-a");

        index.record(&ep, &[10, 20]);
        let query = vec![10, 20, 30, 40];
        let scores = compute_precise_prefix_scores(
            &index,
            &query,
            query.len(),
            &[Arc::clone(&ep)],
        );

        let score = scores.get(&ep).copied().unwrap_or(0.0);
        assert!(
            (score - 0.5).abs() < f64::EPSILON,
            "2/4 match should give 0.5, got {score}"
        );
    }

    #[test]
    fn scoring_empty_returns_empty() {
        let index = PrecisePrefixIndex::new(1000);
        let ep: Arc<str> = Arc::from("ep-a");

        let scores = compute_precise_prefix_scores(&index, &[], 0, &[ep]);
        assert!(scores.is_empty(), "zero blocks should produce empty scores");
    }

    // -- Validation --

    #[test]
    fn validate_rejects_empty_tokenizer_path() {
        let cfg = PrecisePrefixConfig {
            tokenizer: TokenizerConfig {
                path: String::new(),
            },
            speculative_indexing: false,
            speculative_ttl_ms: 30_000,
            hash_seed: String::new(),
            block_size_tokens: 16,
        };
        assert!(
            validate_precise_config(&cfg).is_err(),
            "empty tokenizer path should be rejected"
        );
    }

    #[test]
    fn validate_rejects_zero_block_size() {
        let cfg = PrecisePrefixConfig {
            tokenizer: TokenizerConfig {
                path: "/path/to/tokenizer.json".to_owned(),
            },
            speculative_indexing: false,
            speculative_ttl_ms: 30_000,
            hash_seed: String::new(),
            block_size_tokens: 0,
        };
        assert!(
            validate_precise_config(&cfg).is_err(),
            "zero block_size should be rejected"
        );
    }

    #[test]
    fn validate_rejects_zero_ttl_with_speculative() {
        let cfg = PrecisePrefixConfig {
            tokenizer: TokenizerConfig {
                path: "/path/to/tokenizer.json".to_owned(),
            },
            speculative_indexing: true,
            speculative_ttl_ms: 0,
            hash_seed: String::new(),
            block_size_tokens: 16,
        };
        assert!(
            validate_precise_config(&cfg).is_err(),
            "zero TTL with speculative enabled should be rejected"
        );
    }

    #[test]
    fn validate_accepts_valid_config() {
        let cfg = PrecisePrefixConfig {
            tokenizer: TokenizerConfig {
                path: "/path/to/tokenizer.json".to_owned(),
            },
            speculative_indexing: true,
            speculative_ttl_ms: 30_000,
            hash_seed: "my-seed".to_owned(),
            block_size_tokens: 16,
        };
        assert!(
            validate_precise_config(&cfg).is_ok(),
            "valid config should be accepted"
        );
    }

    // -- Init hash seed --

    #[test]
    fn init_hash_seed_empty_string() {
        let seed = compute_init_hash_seed("");
        assert_eq!(
            seed, FNV_OFFSET_BASIS,
            "empty seed should give FNV offset basis"
        );
    }

    #[test]
    fn init_hash_seed_differs_for_different_seeds() {
        let s1 = compute_init_hash_seed("seed-a");
        let s2 = compute_init_hash_seed("seed-b");
        assert_ne!(s1, s2, "different seed strings should give different hashes");
    }

    // -- Duplicate recording --

    #[test]
    fn record_same_key_twice_no_duplicate() {
        let mut index = PrecisePrefixIndex::new(1000);
        let ep: Arc<str> = Arc::from("ep-a");

        index.record(&ep, &[42]);
        index.record(&ep, &[42]);

        let lru = index.endpoint_lru.get(&ep).unwrap();
        let count = lru.iter().filter(|&&h| h == 42).count();
        assert_eq!(count, 1, "key 42 should appear once in LRU after recording twice");
    }
}
