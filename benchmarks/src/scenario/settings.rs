// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Serializable scenario settings for benchmark reports.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Scenario, Workload};

// -----------------------------------------------------------------------------
// ScenarioSettings
// -----------------------------------------------------------------------------

/// Serializable snapshot of a scenario's configuration.
///
/// Included in benchmark reports so runs are reproducible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioSettings {
    /// Warmup duration in seconds.
    pub warmup_secs: u64,

    /// Measurement duration in seconds.
    pub duration_secs: u64,

    /// Number of runs.
    pub runs: u32,

    /// Workload-specific parameters.
    #[serde(flatten)]
    pub workload: BTreeMap<String, serde_json::Value>,
}

impl ScenarioSettings {
    /// Build settings from a [`Scenario`].
    pub fn from_scenario(s: &Scenario) -> Self {
        Self {
            warmup_secs: s.warmup.as_secs(),
            duration_secs: s.duration.as_secs(),
            runs: s.runs,
            workload: workload_params(&s.workload),
        }
    }
}

/// Extract workload-specific parameters into a map.
fn workload_params(workload: &Workload) -> BTreeMap<String, serde_json::Value> {
    let mut p = BTreeMap::new();
    match workload {
        Workload::SmallRequests { concurrency }
        | Workload::LlmdChatSmall { concurrency }
        | Workload::LlmdChatStreaming { concurrency } => insert_u32(&mut p, "concurrency", *concurrency),
        Workload::LargePayload { body_size } => insert_usize(&mut p, "body_size", *body_size),
        Workload::LargePayloadHighConcurrency { concurrency, body_size } => {
            insert_u32(&mut p, "concurrency", *concurrency);
            insert_usize(&mut p, "body_size", *body_size);
        },
        Workload::LlmdChatLargePrompt {
            concurrency,
            prompt_size,
        } => {
            insert_u32(&mut p, "concurrency", *concurrency);
            insert_usize(&mut p, "prompt_size", *prompt_size);
        },
        Workload::HighConnectionCount { connections } => insert_u32(&mut p, "connections", *connections),
        Workload::Ramp {
            start_qps,
            end_qps,
            step,
        } => {
            insert_u32(&mut p, "start_qps", *start_qps);
            insert_u32(&mut p, "end_qps", *end_qps);
            insert_u32(&mut p, "step", *step);
        },
        Workload::Sustained | Workload::TcpThroughput | Workload::TcpConnectionRate => {},
    }
    p
}

/// Insert a `u32` parameter.
fn insert_u32(params: &mut BTreeMap<String, serde_json::Value>, key: &str, value: u32) {
    params.insert(key.into(), value.into());
}

/// Insert a `usize` parameter.
fn insert_usize(params: &mut BTreeMap<String, serde_json::Value>, key: &str, value: usize) {
    params.insert(key.into(), value.into());
}

/// Build a settings map from a list of scenarios.
pub fn settings_map(scenarios: &[Scenario]) -> BTreeMap<String, ScenarioSettings> {
    scenarios
        .iter()
        .map(|s| (s.name.clone(), ScenarioSettings::from_scenario(s)))
        .collect()
}
