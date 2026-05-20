// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Agentic protocol filters: JSON-RPC 2.0 extraction, MCP classification.

pub(crate) mod json_rpc;
mod mcp;

pub use json_rpc::JsonRpcFilter;
pub use mcp::McpFilter;
