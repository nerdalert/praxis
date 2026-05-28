// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Responses API parser for non-streaming JSON responses.
//!
//! Provides pure parsing functions for Responses API
//! output items, including [`function_call`] detection,
//! assistant messages, and opaque/unknown item handling.
//!
//! This module is independent of the proxy pipeline and
//! can be tested without a running server.
//!
//! [`function_call`]: DetectedFunctionCall

pub mod parser;
pub mod sse;
