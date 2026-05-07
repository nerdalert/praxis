// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Reserved internal header helpers for proxy-owned routing metadata.

/// Built-in reserved header prefixes for Praxis agentic protocol routing.
///
/// Headers with these prefixes are proxy-internal metadata used for
/// body-derived routing decisions. Clients must not be able to inject
/// them directly, and they should not be forwarded to upstream backends.
///
/// Standard MCP protocol headers (`mcp-session-id`, `mcp-method`,
/// `mcp-name`, `mcp-protocol-version`, `mcp-param-*`) do NOT match these
/// prefixes because they lack the `x-` prefix.
///
/// TODO(#186) Spike: consider additive operator-managed reserved prefixes
/// once the broader config model defines global vs listener/filter-chain
/// scope and additive vs override semantics.
const RESERVED_HEADER_PREFIXES: &[&str] = &["x-praxis-", "x-mcp-", "x-a2a-"];

/// Return whether a header name belongs to Praxis reserved internal metadata.
pub(in crate::http::pingora::handler) fn is_reserved_internal_header(name: &http::HeaderName) -> bool {
    let name = name.as_str();
    RESERVED_HEADER_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}
