// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! AI filters for HTTP workloads.

#[cfg(feature = "ai-inference")]
mod inference;
#[cfg(feature = "ai-inference")]
mod prompt_enrich;
#[cfg(feature = "ai-inference")]
pub(crate) mod responses;
#[cfg(feature = "ai-inference")]
mod responses_orchestrator;

#[cfg(feature = "ai-inference")]
pub use inference::ModelToHeaderFilter;
#[cfg(feature = "ai-inference")]
pub use prompt_enrich::PromptEnrichFilter;
#[cfg(feature = "ai-inference")]
pub use responses_orchestrator::ResponsesOrchestratorFilter;
