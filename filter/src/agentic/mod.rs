// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Agentic protocol support: MCP gateway, A2A routing, shared state.

pub(crate) mod mcp_gateway;
#[allow(dead_code, reason = "foundational types for upcoming A2A routing and session management")]
pub(crate) mod state;
