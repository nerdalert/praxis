// SPDX-License-Identifier: LGPL-3.0-only
// Copyright (c) 2024 Shane Utt

//! Resilience, fault-tolerance, and throughput test suite for Praxis.

mod backend_failure;
mod backend_recovery;
mod concurrent_load;
mod large_payload;
mod multi_listener_isolation;
mod rate_limit_burst;
mod throughput_body;
mod throughput_filter_chain;
mod throughput_production;
mod throughput_simple;
mod throughput_tcp;
mod throughput_utils;
