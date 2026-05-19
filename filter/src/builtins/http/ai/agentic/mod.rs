// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Agentic protocol filters: JSON-RPC 2.0 extraction, MCP and A2A classification.

mod a2a;
pub(crate) mod json_rpc;
mod mcp;

pub use a2a::A2aFilter;
pub use json_rpc::JsonRpcFilter;
pub use mcp::McpFilter;
