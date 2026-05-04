// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Configuration types for the MCP gateway filter.

use serde::Deserialize;

use crate::FilterError;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Default maximum request body size for `StreamBuffer` mode (64 `KiB`).
pub(crate) const DEFAULT_MAX_BODY_BYTES: usize = 65_536;

// -----------------------------------------------------------------------------
// ToolConfig
// -----------------------------------------------------------------------------

/// Tool definition in static config.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolConfig {
    /// Tool name on the backend.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Optional input schema.
    pub schema: Option<serde_json::Value>,
}

// -----------------------------------------------------------------------------
// McpServerConfig
// -----------------------------------------------------------------------------

/// MCP backend server configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpServerConfig {
    /// Unique server name.
    pub name: String,
    /// Backend cluster name.
    pub cluster: String,
    /// Backend MCP path.
    #[serde(default = "default_path")]
    pub path: String,
    /// Tool prefix for this server.
    pub tool_prefix: Option<String>,
    /// Statically defined tools.
    #[serde(default)]
    pub tools: Vec<ToolConfig>,
}

// -----------------------------------------------------------------------------
// McpGatewayConfig
// -----------------------------------------------------------------------------

/// MCP gateway filter configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpGatewayConfig {
    /// Public MCP path (reserved for future path-based matching).
    #[serde(default = "default_path")]
    #[allow(dead_code, reason = "reserved for future path-based request matching")]
    pub path: String,
    /// Maximum body size in bytes.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    /// Backend server definitions.
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

// -----------------------------------------------------------------------------
// Defaults
// -----------------------------------------------------------------------------

/// Default MCP path.
fn default_path() -> String {
    "/mcp".to_owned()
}

/// Default max body bytes.
fn default_max_body_bytes() -> usize {
    DEFAULT_MAX_BODY_BYTES
}

// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

/// Validate and build the configuration.
pub(crate) fn build_config(cfg: McpGatewayConfig) -> Result<(usize, McpGatewayConfig), FilterError> {
    if cfg.max_body_bytes == 0 {
        return Err("mcp_gateway: 'max_body_bytes' must be greater than 0".into());
    }

    validate_unique_server_names(&cfg.servers)?;
    validate_server_paths(&cfg.servers)?;
    validate_unique_tool_names(&cfg.servers)?;

    Ok((cfg.max_body_bytes, cfg))
}

/// Validate server backend paths match the rules enforced by
/// `apply_rewritten_path` at runtime.
fn validate_server_paths(servers: &[McpServerConfig]) -> Result<(), FilterError> {
    for server in servers {
        let path = &server.path;
        if !path.starts_with('/') {
            return Err(format!(
                "mcp_gateway: server '{}' path must start with /: '{path}'",
                server.name
            )
            .into());
        }
        if path.starts_with("//") {
            return Err(format!(
                "mcp_gateway: server '{}' path must not start with //: '{path}'",
                server.name
            )
            .into());
        }
        if path.split('/').any(|seg| seg == "..") {
            return Err(format!(
                "mcp_gateway: server '{}' path contains '..' traversal: '{path}'",
                server.name
            )
            .into());
        }
    }
    Ok(())
}

/// Validate that all server names are unique and non-empty.
fn validate_unique_server_names(servers: &[McpServerConfig]) -> Result<(), FilterError> {
    let mut seen = std::collections::HashSet::new();
    for server in servers {
        if server.name.is_empty() {
            return Err("mcp_gateway: server name must not be empty".into());
        }
        if !seen.insert(&server.name) {
            return Err(format!("mcp_gateway: duplicate server name: '{}'", server.name).into());
        }
    }
    Ok(())
}

/// Validate that no two tools produce the same exposed name after prefixing.
fn validate_unique_tool_names(servers: &[McpServerConfig]) -> Result<(), FilterError> {
    let mut seen = std::collections::HashSet::new();
    for server in servers {
        for tool in &server.tools {
            let exposed = if let Some(prefix) = &server.tool_prefix {
                format!("{prefix}{}", tool.name)
            } else {
                tool.name.clone()
            };
            if !seen.insert(exposed.clone()) {
                return Err(format!("mcp_gateway: duplicate exposed tool name: '{exposed}'").into());
            }
        }
    }
    Ok(())
}
