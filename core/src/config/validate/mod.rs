// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Configuration validation rules.

mod branch_chain;
pub use branch_chain::{MAX_BRANCH_DEPTH, MAX_ITERATIONS_CEILING};
mod cluster;
mod filter_chain;
mod listener;
mod rules;
