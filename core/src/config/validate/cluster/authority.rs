// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Praxis Contributors

//! HTTP authority validation for per-cluster upstream authority override.

use crate::errors::ProxyError;

/// Maximum DNS hostname length without a trailing root label.
const MAX_HOSTNAME_LEN: usize = 253;

/// Validate a per-cluster authority override value.
///
/// The supported form is `host [ ":" port ]`, where `host` is an
/// ASCII DNS hostname or bracketed IPv6 address. Schemes, paths,
/// userinfo, query strings, and fragments are rejected.
pub(super) fn validate_authority(authority: &str, cluster_name: &str) -> Result<(), ProxyError> {
    if authority.is_empty() {
        return Err(ProxyError::Config(format!(
            "cluster '{cluster_name}': authority must not be empty"
        )));
    }

    reject_control_chars(authority, cluster_name)?;
    reject_whitespace(authority, cluster_name)?;
    reject_uri_components(authority, cluster_name)?;
    validate_host_port(authority, cluster_name)
}

/// Reject ASCII control characters (C0 range and DEL).
fn reject_control_chars(authority: &str, cluster_name: &str) -> Result<(), ProxyError> {
    if authority.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return Err(ProxyError::Config(format!(
            "cluster '{cluster_name}': authority contains control characters"
        )));
    }
    Ok(())
}

/// Reject space and tab characters.
fn reject_whitespace(authority: &str, cluster_name: &str) -> Result<(), ProxyError> {
    if authority.bytes().any(|b| b == b' ' || b == b'\t') {
        return Err(ProxyError::Config(format!(
            "cluster '{cluster_name}': authority contains whitespace"
        )));
    }
    Ok(())
}

/// Reject URI components that are not part of a bare authority.
fn reject_uri_components(authority: &str, cluster_name: &str) -> Result<(), ProxyError> {
    if authority.contains("://") {
        return Err(ProxyError::Config(format!(
            "cluster '{cluster_name}': authority must not contain a URI scheme"
        )));
    }
    if authority.contains('/') {
        return Err(ProxyError::Config(format!(
            "cluster '{cluster_name}': authority must not contain a path"
        )));
    }
    if authority.contains('@') {
        return Err(ProxyError::Config(format!(
            "cluster '{cluster_name}': authority must not contain userinfo"
        )));
    }
    if authority.contains('#') {
        return Err(ProxyError::Config(format!(
            "cluster '{cluster_name}': authority must not contain a fragment"
        )));
    }
    if authority.contains('?') {
        return Err(ProxyError::Config(format!(
            "cluster '{cluster_name}': authority must not contain a query string"
        )));
    }
    Ok(())
}

/// Validate the host and optional port components.
fn validate_host_port(authority: &str, cluster_name: &str) -> Result<(), ProxyError> {
    if let Some(rest) = authority.strip_prefix('[') {
        validate_bracketed_ipv6(rest, cluster_name)
    } else {
        validate_hostname_port(authority, cluster_name)
    }
}

/// Validate `[ipv6]:port` or `[ipv6]` form.
fn validate_bracketed_ipv6(rest: &str, cluster_name: &str) -> Result<(), ProxyError> {
    let Some(bracket_end) = rest.find(']') else {
        return Err(ProxyError::Config(format!(
            "cluster '{cluster_name}': authority has unclosed IPv6 bracket"
        )));
    };
    let ipv6 = rest.get(..bracket_end).unwrap_or_default();
    if ipv6.parse::<std::net::Ipv6Addr>().is_err() {
        return Err(ProxyError::Config(format!(
            "cluster '{cluster_name}': authority contains invalid IPv6 address"
        )));
    }
    let after = rest.get(bracket_end + 1..).unwrap_or_default();
    if after.is_empty() {
        return Ok(());
    }
    if let Some(port_str) = after.strip_prefix(':') {
        validate_port(port_str, cluster_name)
    } else {
        Err(ProxyError::Config(format!(
            "cluster '{cluster_name}': authority has unexpected characters after IPv6 address"
        )))
    }
}

/// Validate `hostname:port` or `hostname` form.
fn validate_hostname_port(authority: &str, cluster_name: &str) -> Result<(), ProxyError> {
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) && !p.is_empty() => (h, Some(p)),
        _ => (authority, None),
    };

    if host.is_empty() {
        return Err(ProxyError::Config(format!(
            "cluster '{cluster_name}': authority hostname is empty"
        )));
    }

    for b in host.bytes() {
        if !b.is_ascii_alphanumeric() && b != b'-' && b != b'.' {
            return Err(ProxyError::Config(format!(
                "cluster '{cluster_name}': authority hostname contains invalid characters"
            )));
        }
    }

    validate_dns_labels(host, cluster_name)?;

    if let Some(p) = port {
        validate_port(p, cluster_name)?;
    }

    Ok(())
}

/// Validate DNS label rules (RFC 1035 §2.3.1).
///
/// Each label must be 1-63 characters and must not start or end with a hyphen.
fn validate_dns_labels(host: &str, cluster_name: &str) -> Result<(), ProxyError> {
    let hostname = host.strip_suffix('.').unwrap_or(host);
    if hostname.is_empty() {
        return Err(ProxyError::Config(format!(
            "cluster '{cluster_name}': authority hostname is empty"
        )));
    }
    if hostname.len() > MAX_HOSTNAME_LEN {
        return Err(ProxyError::Config(format!(
            "cluster '{cluster_name}': authority hostname exceeds {MAX_HOSTNAME_LEN} characters"
        )));
    }
    for label in hostname.split('.') {
        if label.is_empty() {
            return Err(ProxyError::Config(format!(
                "cluster '{cluster_name}': authority hostname contains an empty label"
            )));
        }
        if label.len() > 63 {
            return Err(ProxyError::Config(format!(
                "cluster '{cluster_name}': authority hostname label exceeds 63 characters"
            )));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(ProxyError::Config(format!(
                "cluster '{cluster_name}': authority hostname label must not start or end with a hyphen"
            )));
        }
    }
    Ok(())
}

/// Validate the port number (1..=65535).
fn validate_port(port_str: &str, cluster_name: &str) -> Result<(), ProxyError> {
    if port_str.is_empty() {
        return Err(ProxyError::Config(format!(
            "cluster '{cluster_name}': authority has empty port"
        )));
    }
    let port: u32 = port_str
        .parse()
        .map_err(|_e| ProxyError::Config(format!("cluster '{cluster_name}': authority has invalid port")))?;
    if port == 0 || port > 65535 {
        return Err(ProxyError::Config(format!(
            "cluster '{cluster_name}': authority port out of range"
        )));
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test suppressions")]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::needless_raw_strings,
    clippy::needless_raw_string_hashes,
    reason = "tests use unwrap/expect/panic/raw strings for brevity"
)]
mod tests {
    use super::*;

    fn ok(authority: &str) {
        validate_authority(authority, "test").unwrap_or_else(|e| panic!("expected Ok for {authority:?}, got: {e}"));
    }

    fn err(authority: &str) -> String {
        validate_authority(authority, "test").unwrap_err().to_string()
    }

    #[test]
    fn accept_simple_hostname() {
        ok("api.example.com");
    }

    #[test]
    fn accept_hostname_with_port() {
        ok("api.example.com:443");
    }

    #[test]
    fn accept_localhost() {
        ok("localhost");
    }

    #[test]
    fn accept_localhost_with_port() {
        ok("localhost:8080");
    }

    #[test]
    fn accept_single_label() {
        ok("backend");
    }

    #[test]
    fn accept_bracketed_ipv6() {
        ok("[::1]");
    }

    #[test]
    fn accept_bracketed_ipv6_with_port() {
        ok("[::1]:8443");
    }

    #[test]
    fn accept_full_ipv6() {
        ok("[2001:db8::1]:443");
    }

    #[test]
    fn reject_empty() {
        assert!(err("").contains("must not be empty"));
    }

    #[test]
    fn reject_control_char() {
        assert!(err("api\x00.example.com").contains("control characters"));
    }

    #[test]
    fn reject_newline() {
        assert!(err("api\nexample.com").contains("control characters"));
    }

    #[test]
    fn reject_space() {
        assert!(err("api example.com").contains("whitespace"));
    }

    #[test]
    fn reject_tab() {
        assert!(err("api\texample.com").contains("control characters"));
    }

    #[test]
    fn reject_scheme() {
        assert!(err("https://api.example.com").contains("URI scheme"));
    }

    #[test]
    fn reject_path() {
        assert!(err("api.example.com/v1").contains("path"));
    }

    #[test]
    fn reject_userinfo() {
        assert!(err("user@api.example.com").contains("userinfo"));
    }

    #[test]
    fn reject_fragment() {
        assert!(err("api.example.com#section").contains("fragment"));
    }

    #[test]
    fn reject_query() {
        assert!(err("api.example.com?key=val").contains("query"));
    }

    #[test]
    fn reject_overlong() {
        let long = "a".repeat(254);
        assert!(err(&long).contains("253"));
    }

    #[test]
    fn accept_at_253_chars() {
        let host = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61)
        );
        assert_eq!(host.len(), 253, "test host should exercise the boundary");
        ok(&host);
    }

    #[test]
    fn accept_253_character_hostname_with_port() {
        let host = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61)
        );
        ok(&format!("{host}:65535"));
    }

    #[test]
    fn accept_fully_qualified_hostname() {
        ok("api.example.com.");
    }

    #[test]
    fn reject_port_zero() {
        assert!(err("api.example.com:0").contains("port out of range"));
    }

    #[test]
    fn reject_port_too_large() {
        assert!(err("api.example.com:65536").contains("port out of range"));
    }

    #[test]
    fn accept_port_65535() {
        ok("api.example.com:65535");
    }

    #[test]
    fn reject_unclosed_ipv6_bracket() {
        assert!(err("[::1").contains("unclosed"));
    }

    #[test]
    fn reject_invalid_ipv6() {
        assert!(err("[not-ipv6]").contains("invalid IPv6"));
    }

    #[test]
    fn reject_underscore_in_hostname() {
        assert!(err("api_server.example.com").contains("invalid characters"));
    }

    #[test]
    fn reject_leading_hyphen() {
        assert!(err("-bad.example.com").contains("hyphen"));
    }

    #[test]
    fn reject_trailing_hyphen() {
        assert!(err("bad-.example.com").contains("hyphen"));
    }

    #[test]
    fn reject_leading_dot() {
        assert!(err(".example.com").contains("empty label"));
    }

    #[test]
    fn reject_consecutive_dots() {
        assert!(err("example..com").contains("empty label"));
    }

    #[test]
    fn reject_label_over_63_chars() {
        let long_label = "a".repeat(64);
        assert!(err(&format!("{long_label}.example.com")).contains("63 characters"));
    }

    #[test]
    fn accept_label_at_63_chars() {
        let label = "a".repeat(63);
        ok(&format!("{label}.example.com"));
    }

    #[test]
    fn accept_hyphen_in_middle() {
        ok("my-api.example.com");
    }
}
