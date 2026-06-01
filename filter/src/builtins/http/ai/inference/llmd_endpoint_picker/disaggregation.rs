// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Prefill/decode disaggregation types and helpers for the llm-d endpoint picker.

use serde::Deserialize;

use crate::FilterError;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Default header name for the prefill endpoint address.
const DEFAULT_PREFILL_HEADER: &str = "x-prefiller-host-port";

// -----------------------------------------------------------------------------
// Config
// -----------------------------------------------------------------------------

/// Configuration for prefill/decode disaggregation.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DisaggregationConfig {
    /// Whether disaggregation is active.
    pub enabled: bool,

    /// Header name injected into upstream requests with the prefill address.
    #[serde(default = "default_prefill_header")]
    pub prefill_header: String,

    /// Controls when a prefill endpoint is selected alongside the decode target.
    #[serde(default = "default_prefill_mode")]
    pub prefill_mode: PrefillMode,

    /// Whether to inject `kv_transfer_params` into the request body
    /// so the decode-side sidecar knows to run remote prefill.
    #[serde(default = "default_inject_kv_transfer")]
    pub inject_kv_transfer_params: bool,
}

/// Controls when the proxy injects a prefill endpoint header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum PrefillMode {
    /// Always attempt to find a prefill endpoint.
    Always,
    /// Never inject a prefill header (decode-only).
    Never,
}

/// Role that an endpoint plays in a disaggregated serving topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum EndpointRole {
    /// Endpoint handles only prefill (prompt processing).
    Prefill,
    /// Endpoint handles only decode (token generation).
    Decode,
    /// Endpoint handles both prefill and decode.
    #[serde(rename = "prefill-decode")]
    PrefillDecode,
}

// -----------------------------------------------------------------------------
// Defaults
// -----------------------------------------------------------------------------

/// Default prefill header name.
fn default_prefill_header() -> String {
    DEFAULT_PREFILL_HEADER.to_owned()
}

/// Default prefill mode.
fn default_prefill_mode() -> PrefillMode {
    PrefillMode::Always
}

/// Default value for `inject_kv_transfer_params`.
fn default_inject_kv_transfer() -> bool {
    true
}

/// Default endpoint role for backward compatibility.
pub(super) fn default_endpoint_role() -> EndpointRole {
    EndpointRole::PrefillDecode
}

// -----------------------------------------------------------------------------
// Selection Helpers
// -----------------------------------------------------------------------------

/// Whether an endpoint with the given role can serve decode requests.
pub(super) fn is_decode_candidate(role: EndpointRole) -> bool {
    matches!(role, EndpointRole::Decode | EndpointRole::PrefillDecode)
}

/// Whether an endpoint with the given role can serve prefill requests.
pub(super) fn is_prefill_candidate(role: EndpointRole) -> bool {
    matches!(role, EndpointRole::Prefill | EndpointRole::PrefillDecode)
}

// -----------------------------------------------------------------------------
// Body Mutation
// -----------------------------------------------------------------------------

/// Inject `kv_transfer_params` into a JSON request body.
///
/// Merges into an existing `kv_transfer_params` object (preserving
/// extra keys) or replaces a non-object value. Adds
/// `do_remote_decode`, `do_remote_prefill`, and `remote_host` so
/// the decode-side sidecar knows to fetch KV-cache data from the
/// prefill endpoint.
///
/// Returns `None` if the body is not valid JSON or not an object.
pub(super) fn inject_kv_transfer_params(body: &[u8], prefill_address: &str) -> Option<Vec<u8>> {
    let mut value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let obj = value.as_object_mut()?;
    let kv_obj = get_or_create_kv_object(obj)?;
    kv_obj.insert("do_remote_decode".to_owned(), serde_json::json!(true));
    kv_obj.insert("do_remote_prefill".to_owned(), serde_json::json!(false));
    kv_obj.insert("remote_host".to_owned(), serde_json::json!(prefill_address));
    serde_json::to_vec(&value).ok()
}

/// Return a mutable reference to the `kv_transfer_params` sub-object,
/// creating or replacing it as needed.
///
/// If the existing value is already an object, returns it for merging.
/// Otherwise inserts a fresh empty object.
fn get_or_create_kv_object(
    obj: &mut serde_json::Map<String, serde_json::Value>,
) -> Option<&mut serde_json::Map<String, serde_json::Value>> {
    let is_existing_object = obj.get("kv_transfer_params").is_some_and(serde_json::Value::is_object);
    if !is_existing_object {
        obj.insert("kv_transfer_params".to_owned(), serde_json::json!({}));
    }
    obj.get_mut("kv_transfer_params")?.as_object_mut()
}

// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

/// Validate disaggregation configuration.
pub(super) fn validate_disaggregation_config(cfg: &DisaggregationConfig) -> Result<(), FilterError> {
    if cfg.prefill_header.trim().is_empty() {
        return Err("llmd_endpoint_picker: disaggregation.prefill_header must not be empty".into());
    }
    if http::header::HeaderName::from_bytes(cfg.prefill_header.as_bytes()).is_err() {
        return Err("llmd_endpoint_picker: disaggregation.prefill_header is not a valid HTTP header name".into());
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

    #[test]
    fn endpoint_role_deserializes_prefill() {
        let role: EndpointRole = serde_yaml::from_str("prefill").unwrap();
        assert_eq!(role, EndpointRole::Prefill, "should deserialize 'prefill'");
    }

    #[test]
    fn endpoint_role_deserializes_decode() {
        let role: EndpointRole = serde_yaml::from_str("decode").unwrap();
        assert_eq!(role, EndpointRole::Decode, "should deserialize 'decode'");
    }

    #[test]
    fn endpoint_role_deserializes_prefill_decode() {
        let role: EndpointRole = serde_yaml::from_str("prefill-decode").unwrap();
        assert_eq!(role, EndpointRole::PrefillDecode, "should deserialize 'prefill-decode'");
    }

    #[test]
    fn endpoint_role_rejects_unknown() {
        let result = serde_yaml::from_str::<EndpointRole>("unknown");
        assert!(result.is_err(), "unknown role should be rejected");
    }

    #[test]
    fn prefill_mode_deserializes_always() {
        let mode: PrefillMode = serde_yaml::from_str("always").unwrap();
        assert_eq!(mode, PrefillMode::Always, "should deserialize 'always'");
    }

    #[test]
    fn prefill_mode_deserializes_never() {
        let mode: PrefillMode = serde_yaml::from_str("never").unwrap();
        assert_eq!(mode, PrefillMode::Never, "should deserialize 'never'");
    }

    #[test]
    fn prefill_mode_rejects_unknown() {
        let result = serde_yaml::from_str::<PrefillMode>("auto");
        assert!(result.is_err(), "unknown prefill mode should be rejected");
    }

    #[test]
    fn is_decode_candidate_accepts_decode() {
        assert!(
            is_decode_candidate(EndpointRole::Decode),
            "Decode role should be a decode candidate"
        );
    }

    #[test]
    fn is_decode_candidate_accepts_prefill_decode() {
        assert!(
            is_decode_candidate(EndpointRole::PrefillDecode),
            "PrefillDecode role should be a decode candidate"
        );
    }

    #[test]
    fn is_decode_candidate_rejects_prefill() {
        assert!(
            !is_decode_candidate(EndpointRole::Prefill),
            "Prefill role should not be a decode candidate"
        );
    }

    #[test]
    fn is_prefill_candidate_accepts_prefill() {
        assert!(
            is_prefill_candidate(EndpointRole::Prefill),
            "Prefill role should be a prefill candidate"
        );
    }

    #[test]
    fn is_prefill_candidate_accepts_prefill_decode() {
        assert!(
            is_prefill_candidate(EndpointRole::PrefillDecode),
            "PrefillDecode role should be a prefill candidate"
        );
    }

    #[test]
    fn is_prefill_candidate_rejects_decode() {
        assert!(
            !is_prefill_candidate(EndpointRole::Decode),
            "Decode role should not be a prefill candidate"
        );
    }

    #[test]
    fn validate_rejects_empty_prefill_header() {
        let cfg = DisaggregationConfig {
            enabled: true,
            prefill_header: String::new(),
            prefill_mode: PrefillMode::Always,
            inject_kv_transfer_params: true,
        };
        assert!(
            validate_disaggregation_config(&cfg).is_err(),
            "empty prefill_header should be rejected"
        );
    }

    #[test]
    fn validate_rejects_whitespace_prefill_header() {
        let cfg = DisaggregationConfig {
            enabled: true,
            prefill_header: "   ".to_owned(),
            prefill_mode: PrefillMode::Always,
            inject_kv_transfer_params: true,
        };
        assert!(
            validate_disaggregation_config(&cfg).is_err(),
            "whitespace-only prefill_header should be rejected"
        );
    }

    #[test]
    fn validate_accepts_valid_config() {
        let cfg = DisaggregationConfig {
            enabled: true,
            prefill_header: "x-prefiller-host-port".to_owned(),
            prefill_mode: PrefillMode::Always,
            inject_kv_transfer_params: true,
        };
        assert!(
            validate_disaggregation_config(&cfg).is_ok(),
            "valid config should be accepted"
        );
    }

    #[test]
    fn default_prefill_header_value() {
        assert_eq!(
            default_prefill_header(),
            "x-prefiller-host-port",
            "default prefill header should be x-prefiller-host-port"
        );
    }

    #[test]
    fn default_prefill_mode_is_always() {
        assert_eq!(
            default_prefill_mode(),
            PrefillMode::Always,
            "default prefill mode should be Always"
        );
    }

    #[test]
    fn default_endpoint_role_is_prefill_decode() {
        assert_eq!(
            default_endpoint_role(),
            EndpointRole::PrefillDecode,
            "default endpoint role should be PrefillDecode"
        );
    }

    #[test]
    fn config_deserializes_with_defaults() {
        let cfg: DisaggregationConfig = serde_yaml::from_str("enabled: true").unwrap();
        assert!(cfg.enabled, "enabled should be true");
        assert_eq!(cfg.prefill_header, "x-prefiller-host-port", "should use default header");
        assert_eq!(cfg.prefill_mode, PrefillMode::Always, "should use default mode");
    }

    #[test]
    fn config_rejects_unknown_fields() {
        let result = serde_yaml::from_str::<DisaggregationConfig>("enabled: true\nunknown_field: 42");
        assert!(result.is_err(), "unknown fields should be rejected");
    }

    #[test]
    fn validate_rejects_header_with_spaces() {
        let cfg = DisaggregationConfig {
            enabled: true,
            prefill_header: "invalid header".to_owned(),
            prefill_mode: PrefillMode::Always,
            inject_kv_transfer_params: true,
        };
        assert!(
            validate_disaggregation_config(&cfg).is_err(),
            "header name with spaces should be rejected"
        );
    }

    #[test]
    fn validate_rejects_header_with_newlines() {
        let cfg = DisaggregationConfig {
            enabled: true,
            prefill_header: "invalid\nheader".to_owned(),
            prefill_mode: PrefillMode::Always,
            inject_kv_transfer_params: true,
        };
        assert!(
            validate_disaggregation_config(&cfg).is_err(),
            "header name with newlines should be rejected"
        );
    }

    #[test]
    fn validate_accepts_default_prefill_header() {
        let cfg = DisaggregationConfig {
            enabled: true,
            prefill_header: "x-prefiller-host-port".to_owned(),
            prefill_mode: PrefillMode::Always,
            inject_kv_transfer_params: true,
        };
        assert!(
            validate_disaggregation_config(&cfg).is_ok(),
            "x-prefiller-host-port should be accepted"
        );
    }

    // -- kv_transfer_params body mutation tests --

    #[test]
    fn inject_kv_transfer_params_produces_correct_json() {
        let body = br#"{"model":"fake-model","messages":[]}"#;

        let result = inject_kv_transfer_params(body, "10.0.0.1:8000").unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&result).unwrap();

        let kv = &parsed["kv_transfer_params"];
        assert_eq!(kv["do_remote_decode"], true, "do_remote_decode should be true");
        assert_eq!(kv["do_remote_prefill"], false, "do_remote_prefill should be false");
        assert_eq!(
            kv["remote_host"].as_str(),
            Some("10.0.0.1:8000"),
            "remote_host should be the prefill address"
        );
    }

    #[test]
    fn inject_kv_transfer_params_preserves_existing_fields() {
        let body = br#"{"model":"fake-model","messages":[{"role":"user","content":"hi"}],"temperature":0.7}"#;

        let result = inject_kv_transfer_params(body, "10.0.0.1:8000").unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&result).unwrap();

        assert_eq!(
            parsed["model"].as_str(),
            Some("fake-model"),
            "model field should be preserved"
        );
        assert!(parsed["messages"].is_array(), "messages field should be preserved");
        assert_eq!(parsed["temperature"], 0.7, "temperature field should be preserved");
        assert!(
            parsed["kv_transfer_params"].is_object(),
            "kv_transfer_params should be present"
        );
    }

    #[test]
    fn inject_kv_transfer_params_invalid_json_returns_none() {
        assert!(
            inject_kv_transfer_params(b"not json", "10.0.0.1:8000").is_none(),
            "invalid JSON should return None"
        );
    }

    #[test]
    fn inject_kv_transfer_params_non_object_returns_none() {
        assert!(
            inject_kv_transfer_params(b"[1,2,3]", "10.0.0.1:8000").is_none(),
            "JSON array should return None"
        );
    }

    #[test]
    fn default_inject_kv_transfer_is_true() {
        assert!(
            default_inject_kv_transfer(),
            "default inject_kv_transfer_params should be true"
        );
    }

    #[test]
    fn config_deserializes_inject_kv_transfer_default() {
        let cfg: DisaggregationConfig = serde_yaml::from_str("enabled: true").unwrap();
        assert!(
            cfg.inject_kv_transfer_params,
            "inject_kv_transfer_params should default to true"
        );
    }

    #[test]
    fn inject_kv_transfer_params_merges_with_existing_object() {
        let body = br#"{"model":"m","kv_transfer_params":{"custom_key":"keep_me","do_remote_decode":false}}"#;

        let result = inject_kv_transfer_params(body, "10.0.0.1:8000").unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&result).unwrap();

        let kv = &parsed["kv_transfer_params"];
        assert_eq!(
            kv["do_remote_decode"], true,
            "do_remote_decode should be overwritten to true"
        );
        assert_eq!(kv["do_remote_prefill"], false, "do_remote_prefill should be injected");
        assert_eq!(
            kv["remote_host"].as_str(),
            Some("10.0.0.1:8000"),
            "remote_host should be set"
        );
        assert_eq!(
            kv["custom_key"].as_str(),
            Some("keep_me"),
            "extra keys in existing kv_transfer_params should be preserved"
        );
    }

    #[test]
    fn inject_kv_transfer_params_replaces_non_object_value() {
        let body = br#"{"model":"m","kv_transfer_params":"not-an-object"}"#;

        let result = inject_kv_transfer_params(body, "10.0.0.1:8000").unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&result).unwrap();

        let kv = &parsed["kv_transfer_params"];
        assert!(
            kv.is_object(),
            "non-object kv_transfer_params should be replaced with object"
        );
        assert_eq!(kv["do_remote_decode"], true, "do_remote_decode should be true");
        assert_eq!(kv["do_remote_prefill"], false, "do_remote_prefill should be false");
        assert_eq!(
            kv["remote_host"].as_str(),
            Some("10.0.0.1:8000"),
            "remote_host should be the prefill address"
        );
    }

    #[test]
    fn config_deserializes_inject_kv_transfer_false() {
        let cfg: DisaggregationConfig =
            serde_yaml::from_str("enabled: true\ninject_kv_transfer_params: false").unwrap();
        assert!(
            !cfg.inject_kv_transfer_params,
            "inject_kv_transfer_params should be false when explicitly set"
        );
    }
}
