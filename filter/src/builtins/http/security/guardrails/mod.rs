// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Rejects requests matching string or regex guardrail rules.

mod config;
mod filter;
mod rule;

#[cfg(test)]
mod tests;

pub use self::{config::GuardrailsAction, filter::GuardrailsFilter};
