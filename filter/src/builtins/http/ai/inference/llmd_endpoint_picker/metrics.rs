// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! vLLM Prometheus metrics parsing and plain HTTP scraping.

use std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Maximum metrics response body size (1 MiB).
const METRICS_MAX_BODY_BYTES: usize = 1_048_576; // 1 MiB

// -----------------------------------------------------------------------------
// Parsed Metrics
// -----------------------------------------------------------------------------

/// Metrics extracted from a vLLM Prometheus `/metrics` response.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct EndpointMetrics {
    /// Number of currently running requests.
    pub running_requests: Option<u64>,
    /// Number of waiting (queued) requests.
    pub waiting_requests: Option<u64>,
    /// KV-cache utilization as a percentage (0-100).
    pub kv_cache_usage_percent: Option<f64>,
}

// -----------------------------------------------------------------------------
// Prometheus Parser
// -----------------------------------------------------------------------------

/// Parse vLLM metrics from Prometheus text exposition format.
///
/// Recognizes both colon and underscore metric name variants:
/// - `vllm:num_requests_running` / `vllm_num_requests_running`
/// - `vllm:num_requests_waiting` / `vllm_num_requests_waiting`
/// - `vllm:gpu_cache_usage_perc` / `vllm_gpu_cache_usage_perc` / `vllm:kv_cache_usage_perc` /
///   `vllm_kv_cache_usage_perc`
pub(super) fn parse_vllm_metrics(body: &str) -> EndpointMetrics {
    let mut result = EndpointMetrics {
        running_requests: None,
        waiting_requests: None,
        kv_cache_usage_percent: None,
    };

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((metric_name, value_str)) = parse_metric_line(line) else {
            continue;
        };
        apply_metric(&mut result, metric_name, value_str);
    }

    result
}

/// Apply a single parsed metric line to the result.
fn apply_metric(result: &mut EndpointMetrics, name: &str, value_str: &str) {
    match name {
        "vllm:num_requests_running" | "vllm_num_requests_running" => {
            if let Some(v) = parse_gauge_as_u64(value_str) {
                result.running_requests = Some(v);
            }
        },
        "vllm:num_requests_waiting" | "vllm_num_requests_waiting" => {
            if let Some(v) = parse_gauge_as_u64(value_str) {
                result.waiting_requests = Some(v);
            }
        },
        "vllm:gpu_cache_usage_perc"
        | "vllm_gpu_cache_usage_perc"
        | "vllm:kv_cache_usage_perc"
        | "vllm_kv_cache_usage_perc" => {
            if let Ok(v) = value_str.parse::<f64>()
                && v.is_finite()
            {
                let pct = if v <= 1.0 { v * 100.0 } else { v };
                result.kv_cache_usage_percent = Some(pct.clamp(0.0, 100.0));
            }
        },
        _ => {},
    }
}

/// Extract metric name and value from a Prometheus text line.
///
/// The metric name is the text before `{` or before the first whitespace.
/// The value is the last whitespace-separated token.
fn parse_metric_line(line: &str) -> Option<(&str, &str)> {
    let name_end = line.find(|c: char| c == '{' || c.is_ascii_whitespace())?;
    let name = &line[..name_end];
    let value_str = line.split_ascii_whitespace().last()?;
    if value_str == name {
        return None;
    }
    Some((name, value_str))
}

/// Parse a gauge value as u64, truncating the fractional part.
fn parse_gauge_as_u64(s: &str) -> Option<u64> {
    if let Ok(v) = s.parse::<u64>() {
        return Some(v);
    }
    let v = s.parse::<f64>().ok()?;
    if v.is_finite() && v >= 0.0 {
        #[allow(
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation,
            reason = "gauge values are non-negative after the check"
        )]
        return Some(v as u64);
    }
    None
}

// -----------------------------------------------------------------------------
// HTTP Scraper
// -----------------------------------------------------------------------------

/// Parsed components of an `http://host:port/path` URL.
#[derive(Debug)]
pub(super) struct ParsedUrl {
    /// Host and port in `host:port` form.
    pub host_port: String,
    /// Request path including leading `/`.
    pub path: String,
}

/// Parse a plain HTTP URL into host:port and path components.
pub(super) fn parse_http_url(url: &str) -> Option<ParsedUrl> {
    let rest = url.strip_prefix("http://")?;
    let (host_port, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    if host_port.is_empty() {
        return None;
    }
    Some(ParsedUrl {
        host_port: host_port.to_owned(),
        path: path.to_owned(),
    })
}

/// Scrape a plain HTTP metrics endpoint and return the response body.
///
/// Sends an HTTP/1.1 GET with `Connection: close` and reads until EOF
/// or [`METRICS_MAX_BODY_BYTES`], whichever comes first. Returns `None`
/// on connection failure, timeout, non-2xx status, oversized response,
/// or truncated body (when `Content-Length` is present and the received
/// body is shorter than declared).
pub(super) fn scrape_http_metrics(url: &str, timeout: Duration) -> Option<String> {
    let parsed = parse_http_url(url)?;
    let mut stream = connect_with_timeout(&parsed.host_port, timeout)?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        parsed.path, parsed.host_port
    );
    stream.write_all(request.as_bytes()).ok()?;

    let body_bytes = read_bounded(&mut stream, METRICS_MAX_BODY_BYTES)?;
    let response = String::from_utf8_lossy(&body_bytes);
    let (headers, body) = response.split_once("\r\n\r\n")?;

    let status_line = headers.lines().next()?;
    let status_code = status_line.split_ascii_whitespace().nth(1)?.parse::<u16>().ok()?;

    if !(200..300).contains(&status_code) {
        return None;
    }

    if parse_content_length(headers).is_some_and(|expected| body.len() < expected) {
        return None;
    }

    Some(body.to_owned())
}

/// Extract `Content-Length` value from response headers, if present
/// and valid.
fn parse_content_length(headers: &str) -> Option<usize> {
    for line in headers.lines().skip(1) {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

/// Connect to a host:port string, resolving hostnames via DNS.
fn connect_with_timeout(host_port: &str, timeout: Duration) -> Option<TcpStream> {
    let addrs = host_port.to_socket_addrs().ok()?;
    for addr in addrs {
        if let Ok(stream) = TcpStream::connect_timeout(&addr, timeout) {
            return Some(stream);
        }
    }
    None
}

/// Read up to `max_bytes` from `reader`, returning `None` if the limit
/// is exceeded or a read error occurs.
fn read_bounded(reader: &mut impl Read, max_bytes: usize) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(chunk.get(..n).unwrap_or(&chunk));
                if buf.len() > max_bytes {
                    return None;
                }
            },
            Err(_) => return None,
        }
    }
    Some(buf)
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests"
)]
mod tests {
    use super::*;

    // -- Prometheus parser tests --

    #[test]
    fn parses_colon_metric_names() {
        let body = "\
            vllm:num_requests_running 3\n\
            vllm:num_requests_waiting 2\n\
            vllm:gpu_cache_usage_perc 0.75\n";

        let m = parse_vllm_metrics(body);

        assert_eq!(m.running_requests, Some(3), "running_requests");
        assert_eq!(m.waiting_requests, Some(2), "waiting_requests");
        assert_eq!(
            m.kv_cache_usage_percent,
            Some(75.0),
            "cache ratio 0.75 should become 75.0%"
        );
    }

    #[test]
    fn parses_underscore_metric_names() {
        let body = "\
            vllm_num_requests_running 4\n\
            vllm_num_requests_waiting 5\n\
            vllm_gpu_cache_usage_perc 61.5\n";

        let m = parse_vllm_metrics(body);

        assert_eq!(m.running_requests, Some(4), "running_requests");
        assert_eq!(m.waiting_requests, Some(5), "waiting_requests");
        assert_eq!(
            m.kv_cache_usage_percent,
            Some(61.5),
            "cache percent > 1.0 treated as percent directly"
        );
    }

    #[test]
    fn parses_labelled_metric_lines() {
        let body = r#"vllm:num_requests_running{model_name="fake-model"} 1"#;

        let m = parse_vllm_metrics(body);

        assert_eq!(m.running_requests, Some(1), "labelled running_requests");
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let body = "\
            # HELP vllm:num_requests_running Number of running requests.\n\
            # TYPE vllm:num_requests_running gauge\n\
            \n\
            vllm:num_requests_running 7\n\
            \n";

        let m = parse_vllm_metrics(body);

        assert_eq!(m.running_requests, Some(7), "running_requests after comments");
        assert_eq!(m.waiting_requests, None, "absent metric stays None");
    }

    #[test]
    fn ignores_bad_values() {
        let body = "\
            vllm:num_requests_running not_a_number\n\
            vllm:num_requests_waiting -1\n\
            vllm:gpu_cache_usage_perc NaN\n";

        let m = parse_vllm_metrics(body);

        assert_eq!(m.running_requests, None, "bad value should not parse");
        assert_eq!(m.waiting_requests, None, "negative u64 should not parse");
        assert_eq!(m.kv_cache_usage_percent, None, "NaN should not parse");
    }

    #[test]
    fn clamps_cache_usage_to_range() {
        let body = "vllm:gpu_cache_usage_perc 150.0\n";

        let m = parse_vllm_metrics(body);

        assert_eq!(
            m.kv_cache_usage_percent,
            Some(100.0),
            "cache usage above 100 should clamp to 100"
        );
    }

    #[test]
    fn cache_ratio_boundary_at_one() {
        let body = "vllm:gpu_cache_usage_perc 1.0\n";

        let m = parse_vllm_metrics(body);

        assert_eq!(
            m.kv_cache_usage_percent,
            Some(100.0),
            "exactly 1.0 is treated as ratio → 100%"
        );
    }

    #[test]
    fn cache_just_above_one_is_percent() {
        let body = "vllm:gpu_cache_usage_perc 1.01\n";

        let m = parse_vllm_metrics(body);

        assert_eq!(
            m.kv_cache_usage_percent,
            Some(1.01),
            "1.01 > 1.0 treated as percent directly"
        );
    }

    #[test]
    fn parses_kv_cache_usage_perc_colon() {
        let body = "vllm:kv_cache_usage_perc{model_name=\"fake-model\"} 0.5\n";

        let m = parse_vllm_metrics(body);

        assert_eq!(
            m.kv_cache_usage_percent,
            Some(50.0),
            "vllm:kv_cache_usage_perc 0.5 as ratio → 50%"
        );
    }

    #[test]
    fn parses_kv_cache_usage_perc_underscore() {
        let body = "vllm_kv_cache_usage_perc 62.5\n";

        let m = parse_vllm_metrics(body);

        assert_eq!(
            m.kv_cache_usage_percent,
            Some(62.5),
            "vllm_kv_cache_usage_perc 62.5 > 1.0 treated as percent"
        );
    }

    #[test]
    fn float_running_requests_truncated() {
        let body = "vllm:num_requests_running 3.7\n";

        let m = parse_vllm_metrics(body);

        assert_eq!(m.running_requests, Some(3), "float gauge should truncate to u64");
    }

    // -- URL parser tests --

    #[test]
    fn parses_http_url_with_path() {
        let parsed = parse_http_url("http://127.0.0.1:8000/metrics").unwrap();

        assert_eq!(parsed.host_port, "127.0.0.1:8000", "host_port");
        assert_eq!(parsed.path, "/metrics", "path");
    }

    #[test]
    fn parses_http_url_without_path() {
        let parsed = parse_http_url("http://myhost:9090").unwrap();

        assert_eq!(parsed.host_port, "myhost:9090", "host_port");
        assert_eq!(parsed.path, "/", "default path");
    }

    #[test]
    fn rejects_non_http_url() {
        assert!(
            parse_http_url("https://foo:443/bar").is_none(),
            "https should not parse"
        );
    }

    #[test]
    fn rejects_empty_host() {
        assert!(parse_http_url("http:///path").is_none(), "empty host should not parse");
    }

    #[test]
    fn rejects_bare_scheme() {
        assert!(parse_http_url("http://").is_none(), "bare scheme should not parse");
    }

    #[test]
    fn rejects_empty_string() {
        assert!(parse_http_url("").is_none(), "empty string should not parse");
    }

    // -- Scraper tests --

    #[test]
    fn scrape_http_metrics_supports_localhost_hostname() {
        let metrics_body = "vllm:num_requests_running 42\n";
        let (port, _guard) = start_test_metrics_server(metrics_body);

        let url = format!("http://localhost:{port}/metrics");
        let body = scrape_http_metrics(&url, Duration::from_secs(2));

        assert!(body.is_some(), "scraping via localhost hostname should succeed");
        let m = parse_vllm_metrics(&body.unwrap());
        assert_eq!(
            m.running_requests,
            Some(42),
            "scraped value should match server response"
        );
    }

    #[test]
    fn scrape_http_metrics_rejects_oversized_response() {
        let oversized = "x".repeat(METRICS_MAX_BODY_BYTES + 1);
        let (port, _guard) = start_test_metrics_server(&oversized);

        let url = format!("http://127.0.0.1:{port}/metrics");
        let body = scrape_http_metrics(&url, Duration::from_secs(2));

        assert!(body.is_none(), "scraping an oversized response should return None");
    }

    #[test]
    fn scrape_http_metrics_rejects_truncated_response() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let join = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let mut buf = [0u8; 512];
            drop(stream.read(&mut buf));
            drop(stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial"));
            drop(stream.shutdown(std::net::Shutdown::Both));
        });

        let url = format!("http://127.0.0.1:{port}/metrics");
        let result = scrape_http_metrics(&url, Duration::from_millis(500));
        let _join = join.join();

        assert!(
            result.is_none(),
            "truncated response with Content-Length mismatch must be rejected"
        );
    }

    // -- Test Utilities --

    fn start_test_metrics_server(body: &str) -> (u16, TestServerGuard) {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = body.to_owned();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_clone = std::sync::Arc::clone(&stop);

        let join = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            while !stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                        let mut buf = [0u8; 1024];
                        drop(stream.read(&mut buf));
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        drop(stream.write_all(response.as_bytes()));
                    },
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    },
                    Err(_) => break,
                }
            }
        });

        (port, TestServerGuard { stop, join: Some(join) })
    }

    struct TestServerGuard {
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for TestServerGuard {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(join) = self.join.take() {
                let _result = join.join();
            }
        }
    }
}
