// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Kubernetes endpoint discovery for the llm-d endpoint picker.
//!
//! Reads an `InferencePool` resource and discovers matching pods to
//! build endpoint state. Supports both `inference.networking.k8s.io/v1`
//! and `inference.networking.x-k8s.io/v1alpha2` API versions.

use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpStream,
    sync::Arc,
    time::Duration,
};

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::Deserialize;
use tracing::{debug, warn};

/// Characters that must be percent-encoded inside a Kubernetes
/// `labelSelector` query-parameter value. Equals (`=`) and comma (`,`)
/// are NOT included because they serve as the selector syntax delimiters
/// that the API server expects literally.
const LABEL_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'/')
    .add(b':')
    .add(b'#')
    .add(b'?')
    .add(b'&')
    .add(b'+')
    .add(b'%');

use super::{
    config::{GatewayApiConfig, InferencePoolConfig},
    state::EndpointState,
};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Default Kubernetes API server port.
const DEFAULT_K8S_PORT: &str = "443";
/// In-cluster service account token path.
const SA_TOKEN_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";
/// In-cluster service account CA cert path.
const SA_CA_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";
/// Maximum response body size for K8s API calls.
const K8S_MAX_BODY_BYTES: usize = 4_194_304; // 4 MiB
/// Default in-cluster service account namespace path.
const SA_NAMESPACE_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";

/// Return the service account token path, allowing an environment
/// variable override for testing.
fn sa_token_path() -> String {
    std::env::var("PRAXIS_K8S_SERVICEACCOUNT_TOKEN_PATH").unwrap_or_else(|_| SA_TOKEN_PATH.to_owned())
}

/// Return the service account CA certificate path, allowing an
/// environment variable override for testing.
fn sa_ca_path() -> String {
    std::env::var("PRAXIS_K8S_SERVICEACCOUNT_CA_PATH").unwrap_or_else(|_| SA_CA_PATH.to_owned())
}

/// Return the service account namespace path, allowing an environment
/// variable override for testing.
pub(super) fn sa_namespace_path() -> String {
    std::env::var("PRAXIS_K8S_SERVICEACCOUNT_NAMESPACE_PATH").unwrap_or_else(|_| SA_NAMESPACE_PATH.to_owned())
}

// -----------------------------------------------------------------------------
// K8s API Types — InferencePool
// -----------------------------------------------------------------------------

/// Top-level `InferencePool` response from the Kubernetes API.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InferencePoolResponse {
    /// Resource spec.
    spec: InferencePoolSpec,
}

/// `InferencePool` spec covering both v1 and v1alpha2 shapes.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InferencePoolSpec {
    /// v1 style: `{ matchLabels: { ... } }`
    /// v1alpha2 style: `{ key: value, ... }` (plain map)
    selector: serde_json::Value,

    /// v1 style: `[{ number: 8000 }]`
    target_ports: Option<Vec<TargetPort>>,

    /// v1alpha2 style: `8000`
    target_port_number: Option<u16>,
}

/// Port entry in the v1 `targetPorts` array.
#[derive(Debug, Deserialize)]
struct TargetPort {
    /// Port number.
    number: u16,
}

/// Extracted pool configuration from either API version.
#[derive(Debug, Clone)]
pub(super) struct PoolInfo {
    /// Label selector as key-value pairs.
    pub selector: BTreeMap<String, String>,
    /// Target ports to generate endpoints for.
    pub target_ports: Vec<u16>,
}

/// Parse an `InferencePool` JSON response into a [`PoolInfo`].
///
/// Returns `None` when the JSON is malformed, the selector is empty,
/// or no target ports are specified.
pub(super) fn parse_inference_pool(json: &str) -> Option<PoolInfo> {
    let pool: InferencePoolResponse = serde_json::from_str(json).ok()?;
    let selector = parse_selector(&pool.spec.selector)?;
    let target_ports = parse_target_ports(&pool.spec)?;
    Some(PoolInfo { selector, target_ports })
}

/// Parse an `InferencePool` JSON allowing missing target ports.
///
/// Used by gateway discovery where a backendRef port can substitute
/// for absent pool-level target ports.
fn parse_inference_pool_lenient(json: &str) -> Option<PoolInfo> {
    let pool: InferencePoolResponse = serde_json::from_str(json).ok()?;
    let selector = parse_selector(&pool.spec.selector)?;
    let target_ports = parse_target_ports(&pool.spec).unwrap_or_default();
    Some(PoolInfo { selector, target_ports })
}

/// Extract label selector from either v1 `matchLabels` or v1alpha2
/// plain map format.
fn parse_selector(value: &serde_json::Value) -> Option<BTreeMap<String, String>> {
    if let Some(match_labels) = value.get("matchLabels") {
        return parse_string_map(match_labels);
    }
    parse_string_map(value)
}

/// Parse a JSON object into a sorted string map.
fn parse_string_map(value: &serde_json::Value) -> Option<BTreeMap<String, String>> {
    let obj = value.as_object()?;
    let mut map = BTreeMap::new();
    for (k, v) in obj {
        map.insert(k.clone(), v.as_str()?.to_owned());
    }
    if map.is_empty() {
        return None;
    }
    Some(map)
}

/// Extract target ports from either v1 `targetPorts` array or v1alpha2
/// `targetPortNumber` scalar.
fn parse_target_ports(spec: &InferencePoolSpec) -> Option<Vec<u16>> {
    if let Some(ref ports) = spec.target_ports {
        let nums: Vec<u16> = ports.iter().map(|p| p.number).collect();
        if !nums.is_empty() {
            return Some(nums);
        }
    }
    spec.target_port_number.map(|p| vec![p])
}

/// Resolve target ports from pool info with an optional backendRef
/// port fallback.
///
/// Pool-level target ports always take priority. The fallback port
/// is only used when the pool has no target ports of its own.
fn resolve_target_ports(pool_info: &PoolInfo, fallback_port: Option<u16>) -> Option<Vec<u16>> {
    if !pool_info.target_ports.is_empty() {
        return Some(pool_info.target_ports.clone());
    }
    fallback_port.map(|p| vec![p])
}

// -----------------------------------------------------------------------------
// K8s API Types — HTTPRoute
// -----------------------------------------------------------------------------

/// Top-level `HTTPRoute` response from the Gateway API.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HttpRouteResponse {
    /// Route spec.
    spec: HttpRouteSpec,
}

/// `HTTPRoute` spec containing routing rules.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HttpRouteSpec {
    /// Ordered list of routing rules.
    #[serde(default)]
    rules: Vec<HttpRouteRule>,
}

/// A single routing rule within an `HTTPRoute`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HttpRouteRule {
    /// Backend references for this rule.
    #[serde(default)]
    backend_refs: Vec<BackendRef>,
}

/// A backend reference within an `HTTPRoute` rule.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackendRef {
    /// API group of the referenced resource.
    #[serde(default)]
    group: Option<String>,

    /// Kind of the referenced resource.
    #[serde(default)]
    kind: Option<String>,

    /// Name of the referenced resource.
    name: String,

    /// Optional port number.
    #[serde(default)]
    port: Option<u16>,
}

/// Extracted pool reference from an `HTTPRoute` `backendRef`.
#[derive(Debug, Clone)]
pub(super) struct PoolBackendRef {
    /// Name of the referenced `InferencePool`.
    pub name: String,
    /// Optional port from the `backendRef`, used as fallback when the
    /// `InferencePool` has no target ports.
    pub port: Option<u16>,
}

/// Parse an `HTTPRoute` JSON response to find the first `InferencePool`
/// backend reference.
pub(super) fn parse_httproute_pool_ref(json: &str) -> Option<PoolBackendRef> {
    let route: HttpRouteResponse = serde_json::from_str(json).ok()?;
    find_inference_pool_ref(&route.spec.rules)
}

/// Search rules for the first backendRef targeting an `InferencePool`.
fn find_inference_pool_ref(rules: &[HttpRouteRule]) -> Option<PoolBackendRef> {
    for rule in rules {
        for br in &rule.backend_refs {
            if is_inference_pool_ref(br) {
                return Some(PoolBackendRef {
                    name: br.name.clone(),
                    port: br.port,
                });
            }
        }
    }
    None
}

/// Check whether a backendRef points to an `InferencePool` resource.
///
/// Requires an exact group match against the two known Gateway API
/// inference groups and a non-empty resource name.
fn is_inference_pool_ref(br: &BackendRef) -> bool {
    let kind_match = br.kind.as_deref() == Some("InferencePool");
    let group_match = matches!(
        br.group.as_deref(),
        Some("inference.networking.k8s.io" | "inference.networking.x-k8s.io")
    );
    let name_valid = !br.name.trim().is_empty();
    kind_match && group_match && name_valid
}

// -----------------------------------------------------------------------------
// K8s API Types — Pod List
// -----------------------------------------------------------------------------

/// Pod list response from the Kubernetes API.
#[derive(Debug, Deserialize)]
struct PodList {
    /// List of pods.
    items: Vec<Pod>,
}

/// Minimal pod representation.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pod {
    /// Pod metadata.
    metadata: PodMetadata,
    /// Pod status.
    status: Option<PodStatus>,
}

/// Pod metadata fields.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PodMetadata {
    /// Pod name.
    name: Option<String>,
    /// Deletion timestamp (non-null means terminating).
    deletion_timestamp: Option<String>,
    /// Pod labels.
    #[serde(default)]
    labels: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Pod status fields.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PodStatus {
    /// Pod phase (Running, Pending, etc.).
    phase: Option<String>,
    /// Pod IP address. K8s uses `podIP` (not camelCase `podIp`).
    #[serde(alias = "podIP")]
    pod_ip: Option<String>,
    /// Container conditions.
    conditions: Option<Vec<PodCondition>>,
}

/// Pod condition entry.
#[derive(Debug, Deserialize)]
struct PodCondition {
    /// Condition type (Ready, Initialized, etc.).
    #[serde(rename = "type")]
    condition_type: String,
    /// Condition status ("True", "False", "Unknown").
    status: String,
}

/// Parse a pod list JSON and extract ready, running pods with IPs.
pub(super) fn parse_ready_pods(json: &str) -> Vec<ReadyPod> {
    let list: PodList = match serde_json::from_str(json) {
        Ok(l) => l,
        Err(_) => return Vec::new(),
    };
    list.items.into_iter().filter_map(pod_to_ready).collect()
}

/// Convert a pod to a [`ReadyPod`] if it is running, ready, and has
/// an IP.
fn pod_to_ready(pod: Pod) -> Option<ReadyPod> {
    if pod.metadata.deletion_timestamp.is_some() {
        return None;
    }
    let status = pod.status.as_ref()?;
    if status.phase.as_deref() != Some("Running") {
        return None;
    }
    let ip = status.pod_ip.as_deref()?.to_owned();
    if ip.is_empty() {
        return None;
    }
    if !is_pod_ready(status) {
        return None;
    }
    let name = pod.metadata.name.filter(|n| !n.is_empty())?;
    let role = extract_role_label(&pod.metadata.labels);
    Some(ReadyPod { name, ip, role })
}

/// Check whether a pod has the `Ready` condition set to `True`.
fn is_pod_ready(status: &PodStatus) -> bool {
    status
        .conditions
        .as_ref()
        .is_some_and(|conds| conds.iter().any(|c| c.condition_type == "Ready" && c.status == "True"))
}

/// Extract the `llm-d.ai/role` label value from pod labels.
fn extract_role_label(labels: &Option<serde_json::Map<String, serde_json::Value>>) -> Option<String> {
    labels.as_ref()?.get("llm-d.ai/role")?.as_str().map(str::to_owned)
}

/// A pod that passed readiness filtering.
#[derive(Debug, Clone)]
pub(super) struct ReadyPod {
    /// Pod name.
    pub name: String,
    /// Pod IP address.
    pub ip: String,
    /// Serving role extracted from the `llm-d.ai/role` label.
    pub role: Option<String>,
}

// -----------------------------------------------------------------------------
// Label Selector Encoding
// -----------------------------------------------------------------------------

/// Encode a label selector map as a Kubernetes API query parameter.
///
/// Each key and value is individually percent-encoded so that label
/// keys containing slashes (e.g. `llm-d.ai/model`) are safe to embed
/// in a URL query string. Keys are sorted for deterministic output.
pub(super) fn encode_label_selector(selector: &BTreeMap<String, String>) -> String {
    selector
        .iter()
        .map(|(k, v)| {
            let ek = utf8_percent_encode(k, LABEL_ENCODE_SET);
            let ev = utf8_percent_encode(v, LABEL_ENCODE_SET);
            format!("{ek}={ev}")
        })
        .collect::<Vec<_>>()
        .join(",")
}

// -----------------------------------------------------------------------------
// Endpoint Building
// -----------------------------------------------------------------------------

/// Build `EndpointState` entries from discovered pods and pool config.
///
/// Pods with an unrecognized `llm-d.ai/role` label are skipped and
/// a warning is logged.
pub(super) fn build_discovered_endpoints(
    pods: &[ReadyPod],
    target_ports: &[u16],
    models: &[String],
    metrics_path: &str,
) -> Vec<EndpointState> {
    let model_arcs: Vec<Arc<str>> = models.iter().map(|m| Arc::from(m.as_str())).collect();
    let mut endpoints = Vec::with_capacity(pods.len() * target_ports.len());

    for pod in pods {
        let Some(role) = resolve_pod_role(pod) else {
            continue;
        };
        for &port in target_ports {
            endpoints.push(make_discovered_endpoint(pod, port, &model_arcs, metrics_path, role));
        }
    }

    endpoints.sort_by(|a, b| a.name.cmp(&b.name));
    endpoints
}

/// Resolve the endpoint role for a pod, logging a warning and
/// returning `None` for unrecognized role labels.
fn resolve_pod_role(pod: &ReadyPod) -> Option<super::disaggregation::EndpointRole> {
    use super::disaggregation::EndpointRole;

    match pod.role.as_deref() {
        Some("prefill") => Some(EndpointRole::Prefill),
        Some("decode") => Some(EndpointRole::Decode),
        Some("prefill-decode") => Some(EndpointRole::PrefillDecode),
        None => Some(super::disaggregation::default_endpoint_role()),
        Some(unknown) => {
            warn!(pod = %pod.name, role = unknown, "unknown llm-d.ai/role label; skipping pod");
            None
        },
    }
}

/// Create a single discovered endpoint entry.
fn make_discovered_endpoint(
    pod: &ReadyPod,
    port: u16,
    model_arcs: &[Arc<str>],
    metrics_path: &str,
    role: super::disaggregation::EndpointRole,
) -> EndpointState {
    let name = format!("{}:{port}", pod.name);
    let address = format!("{}:{port}", pod.ip);
    let metrics_url = format!("http://{address}{metrics_path}");

    EndpointState {
        name: Arc::from(name),
        address: Arc::from(address),
        models: model_arcs.to_vec(),
        running_requests: 0,
        waiting_requests: 0,
        kv_cache_usage_percent: 0.0,
        healthy: true,
        metrics_url: Some(Arc::from(metrics_url)),
        role,
    }
}

// -----------------------------------------------------------------------------
// In-Cluster K8s Client
// -----------------------------------------------------------------------------

/// Blocking Kubernetes API client for in-cluster use.
pub(super) struct KubeClient {
    /// K8s API server host.
    host: String,
    /// K8s API server port.
    port: String,
    /// Bearer token for authentication.
    token: String,
    /// TLS config using service account CA.
    tls_config: Arc<rustls::ClientConfig>,
    /// Request timeout.
    timeout: Duration,
}

impl KubeClient {
    /// Create a client from in-cluster service account credentials.
    ///
    /// Returns `None` if credentials are unavailable.
    pub fn from_in_cluster(timeout: Duration) -> Option<Self> {
        let host = std::env::var("KUBERNETES_SERVICE_HOST").ok()?;
        let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| DEFAULT_K8S_PORT.to_owned());
        let token = std::fs::read_to_string(sa_token_path()).ok()?;
        let ca_pem = std::fs::read(sa_ca_path()).ok()?;

        let tls_config = build_tls_config(&ca_pem)?;

        Some(Self {
            host,
            port,
            token: token.trim().to_owned(),
            tls_config,
            timeout,
        })
    }

    /// GET a Kubernetes API path and return the response body.
    pub fn get(&self, path: &str) -> Option<String> {
        let mut tls_stream = self.connect_tls(path)?;
        let raw = self.send_request(&mut tls_stream, path)?;
        parse_http_response(&raw, path)
    }

    /// Establish a TLS connection to the API server.
    fn connect_tls(&self, path: &str) -> Option<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
        let addr = format!("{}:{}", self.host, self.port);
        let Some(stream) = connect_tcp(&addr, self.timeout) else {
            warn!(addr = addr, path = path, "K8s API: TCP connect failed");
            return None;
        };
        stream.set_read_timeout(Some(self.timeout)).ok()?;
        stream.set_write_timeout(Some(self.timeout)).ok()?;

        // KUBERNETES_SERVICE_HOST may be an IP. Fall back to
        // `kubernetes.default.svc` if server name parsing fails.
        let server_name = k8s_server_name(&self.host)?;
        let tls_conn = rustls::ClientConnection::new(Arc::clone(&self.tls_config), server_name).ok()?;
        Some(rustls::StreamOwned::new(tls_conn, stream))
    }

    /// Send an HTTP GET and return the raw response.
    fn send_request(
        &self,
        tls_stream: &mut rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
        path: &str,
    ) -> Option<String> {
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {}\r\n\
             Authorization: Bearer {}\r\n\
             Accept: application/json\r\n\
             Connection: close\r\n\r\n",
            self.host, self.token
        );
        tls_stream.write_all(request.as_bytes()).ok()?;
        let raw_bytes = read_k8s_response(tls_stream)?;
        Some(String::from_utf8_lossy(&raw_bytes).into_owned())
    }

    /// Discover endpoints via a Gateway API `HTTPRoute`.
    ///
    /// Reads the `HTTPRoute`, extracts the first `InferencePool`
    /// backendRef, and discovers pods. When the `InferencePool`
    /// has no target ports, the backendRef port is used as fallback.
    pub fn discover_via_gateway(&self, gw: &GatewayApiConfig) -> Option<Vec<EndpointState>> {
        let namespace = gw.effective_namespace();
        let route_json = self.get_httproute_json(gw, &namespace)?;
        let pool_ref = parse_httproute_pool_ref(&route_json)?;

        let pool_config = build_pool_config_from_gateway(gw, &pool_ref, &namespace);
        self.discover_gateway_pool(&pool_config, pool_ref.port)
    }

    /// Fetch the `HTTPRoute` JSON from the Kubernetes API.
    fn get_httproute_json(&self, gw: &GatewayApiConfig, namespace: &str) -> Option<String> {
        let route_path = format!(
            "/apis/gateway.networking.k8s.io/v1/namespaces/{namespace}/httproutes/{}",
            gw.http_route.name
        );
        self.get(&route_path)
    }

    /// Discover endpoints from an `InferencePool` with backendRef
    /// port fallback for gateway-sourced discovery.
    fn discover_gateway_pool(
        &self,
        pool_config: &InferencePoolConfig,
        fallback_port: Option<u16>,
    ) -> Option<Vec<EndpointState>> {
        let (group, version) = parse_api_version(&pool_config.api_version)?;
        let namespace = pool_config.effective_namespace();
        let pool_path = format!(
            "/apis/{group}/{version}/namespaces/{namespace}/inferencepools/{}",
            pool_config.name
        );
        let pool_json = self.get(&pool_path)?;
        let pool_info = parse_inference_pool_lenient(&pool_json)?;

        let target_ports = resolve_target_ports(&pool_info, fallback_port)?;
        self.discover_pods(&namespace, &pool_info, &target_ports, pool_config)
    }

    /// Discover endpoints from an `InferencePool`.
    pub fn discover(&self, pool_config: &InferencePoolConfig) -> Option<Vec<EndpointState>> {
        let (group, version) = parse_api_version(&pool_config.api_version)?;
        let namespace = pool_config.effective_namespace();

        let pool_path = format!(
            "/apis/{group}/{version}/namespaces/{namespace}/inferencepools/{}",
            pool_config.name
        );
        let pool_json = self.get(&pool_path)?;
        let pool_info = parse_inference_pool(&pool_json)?;

        self.discover_pods(&namespace, &pool_info, &pool_info.target_ports, pool_config)
    }

    /// Scan pods matching the pool selector and build endpoint state.
    fn discover_pods(
        &self,
        namespace: &str,
        pool_info: &PoolInfo,
        target_ports: &[u16],
        pool_config: &InferencePoolConfig,
    ) -> Option<Vec<EndpointState>> {
        let selector_str = encode_label_selector(&pool_info.selector);
        let pod_path = format!("/api/v1/namespaces/{namespace}/pods?labelSelector={selector_str}");
        let pod_json = self.get(&pod_path)?;
        let ready_pods = parse_ready_pods(&pod_json);

        debug!(
            selector = selector_str,
            ready_pod_count = ready_pods.len(),
            target_ports = ?target_ports,
            "K8s discovery pod scan complete"
        );

        let endpoints = build_discovered_endpoints(
            &ready_pods,
            target_ports,
            &pool_config.models,
            &pool_config.metrics_path,
        );

        Some(endpoints)
    }
}

/// Build an `InferencePoolConfig` from a gateway API config and a
/// discovered pool backend reference.
fn build_pool_config_from_gateway(
    gw: &GatewayApiConfig,
    pool_ref: &PoolBackendRef,
    namespace: &str,
) -> InferencePoolConfig {
    InferencePoolConfig {
        name: pool_ref.name.clone(),
        namespace: Some(namespace.to_owned()),
        api_version: gw.inference_pool_api_version.clone(),
        models: gw.models.clone(),
        metrics_path: gw.metrics_path.clone(),
    }
}

/// Parse an HTTP response, handling status, chunked encoding, and
/// `Content-Length` validation.
fn parse_http_response(raw: &str, path: &str) -> Option<String> {
    let (headers, raw_body) = raw.split_once("\r\n\r\n")?;
    let status_line = headers.lines().next()?;
    let status_code = status_line.split_ascii_whitespace().nth(1)?.parse::<u16>().ok()?;

    if !(200..300).contains(&status_code) {
        warn!(path = path, status = status_code, "K8s API returned non-2xx status");
        return None;
    }

    let is_chunked = headers
        .lines()
        .any(|l| l.to_ascii_lowercase().contains("transfer-encoding: chunked"));

    if is_chunked {
        decode_chunked(raw_body)
    } else {
        if content_length_truncated(headers, raw_body) {
            warn!(path = path, "K8s API response body shorter than Content-Length");
            return None;
        }
        Some(raw_body.to_owned())
    }
}

/// Return `true` when a `Content-Length` header is present and the body
/// is shorter than declared.
fn content_length_truncated(headers: &str, body: &str) -> bool {
    parse_content_length(headers).is_some_and(|expected| body.len() < expected)
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

/// Decode a chunked transfer-encoded body.
///
/// Returns `None` if chunk sizes are malformed or the body is truncated.
fn decode_chunked(raw: &str) -> Option<String> {
    let mut result = String::new();
    let mut remaining = raw;
    while let Some((size_str, rest)) = remaining.split_once("\r\n") {
        let size = usize::from_str_radix(size_str.trim(), 16).ok()?;
        if size == 0 {
            break;
        }
        if rest.len() < size {
            return None;
        }
        result.push_str(rest.get(..size).unwrap_or(rest));
        remaining = rest.get(size..).unwrap_or("");
        remaining = remaining.strip_prefix("\r\n").unwrap_or(remaining);
    }
    Some(result)
}

/// Determine the TLS server name for the K8s API server.
///
/// Standard Kubernetes API server certificates include
/// `kubernetes.default.svc` as a SAN. When `KUBERNETES_SERVICE_HOST` is
/// an IP that cannot be parsed as a `ServerName` (e.g. due to a missing
/// IP SAN in the cert), this fallback allows the TLS handshake to
/// succeed using the DNS name that the cert is guaranteed to contain.
fn k8s_server_name(host: &str) -> Option<rustls::pki_types::ServerName<'static>> {
    rustls::pki_types::ServerName::try_from(host)
        .map(|s| s.to_owned())
        .or_else(|_| rustls::pki_types::ServerName::try_from("kubernetes.default.svc").map(|s| s.to_owned()))
        .ok()
}

/// Connect to a host:port string, resolving hostnames via DNS.
fn connect_tcp(addr: &str, timeout: Duration) -> Option<TcpStream> {
    use std::net::ToSocketAddrs;
    let addrs = addr.to_socket_addrs().ok()?;
    for a in addrs {
        if let Ok(stream) = TcpStream::connect_timeout(&a, timeout) {
            return Some(stream);
        }
    }
    None
}

/// Build a rustls `ClientConfig` from a CA PEM.
fn build_tls_config(ca_pem: &[u8]) -> Option<Arc<rustls::ClientConfig>> {
    if ca_pem.is_empty() {
        warn!("K8s API: empty CA certificate bundle");
        return None;
    }

    let mut root_store = rustls::RootCertStore::empty();
    let certs = rustls_pemfile::certs(&mut &ca_pem[..])
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    for cert in certs {
        root_store.add(cert).ok()?;
    }

    if root_store.is_empty() {
        warn!("K8s API: no valid certificates in CA bundle");
        return None;
    }

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Some(Arc::new(config))
}

/// Read a bounded response from a TLS stream.
fn read_k8s_response(reader: &mut impl Read) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(chunk.get(..n).unwrap_or(&chunk));
                if buf.len() > K8S_MAX_BODY_BYTES {
                    return None;
                }
            },
            Err(_) => return None,
        }
    }
    Some(buf)
}

/// Split an `apiVersion` string like `inference.networking.k8s.io/v1`
/// into `(group, version)`.
///
/// Rejects empty components and strings with more than one slash.
fn parse_api_version(api_version: &str) -> Option<(&str, &str)> {
    let (group, version) = api_version.split_once('/')?;
    if group.is_empty() || version.is_empty() || version.contains('/') {
        return None;
    }
    Some((group, version))
}

// -----------------------------------------------------------------------------
// Discovery Without Client (For Testing)
// -----------------------------------------------------------------------------

/// Discover endpoints from raw JSON responses.
#[cfg(test)]
fn discover_from_json(
    pool_json: &str,
    pod_json: &str,
    models: &[String],
    metrics_path: &str,
) -> Option<Vec<EndpointState>> {
    let pool_info = parse_inference_pool(pool_json)?;
    let ready_pods = parse_ready_pods(pod_json);
    Some(build_discovered_endpoints(
        &ready_pods,
        &pool_info.target_ports,
        models,
        metrics_path,
    ))
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
    use crate::builtins::http::ai::inference::llmd_endpoint_picker::disaggregation::EndpointRole;

    // -- InferencePool parsing --

    #[test]
    fn parses_v1_inference_pool() {
        let json = r#"{
            "apiVersion": "inference.networking.k8s.io/v1",
            "kind": "InferencePool",
            "metadata": { "name": "test-pool", "namespace": "default" },
            "spec": {
                "selector": {
                    "matchLabels": { "app": "vllm", "pool": "test" }
                },
                "targetPorts": [{ "number": 8000 }]
            }
        }"#;

        let pool = parse_inference_pool(json).unwrap();

        assert_eq!(pool.selector.len(), 2, "should have two labels");
        assert_eq!(pool.selector.get("app").map(String::as_str), Some("vllm"), "app label");
        assert_eq!(
            pool.selector.get("pool").map(String::as_str),
            Some("test"),
            "pool label"
        );
        assert_eq!(pool.target_ports, vec![8000], "target ports");
    }

    #[test]
    fn parses_v1alpha2_inference_pool() {
        let json = r#"{
            "apiVersion": "inference.networking.x-k8s.io/v1alpha2",
            "kind": "InferencePool",
            "metadata": { "name": "sim-pool", "namespace": "default" },
            "spec": {
                "selector": { "app": "llm-d-sim", "pool": "sim-pool" },
                "targetPortNumber": 8000,
                "extensionRef": { "name": "praxis" }
            }
        }"#;

        let pool = parse_inference_pool(json).unwrap();

        assert_eq!(pool.selector.len(), 2, "should have two labels");
        assert_eq!(
            pool.selector.get("app").map(String::as_str),
            Some("llm-d-sim"),
            "app label"
        );
        assert_eq!(pool.target_ports, vec![8000], "target ports from scalar");
    }

    #[test]
    fn parses_v1_multiple_target_ports() {
        let json = r#"{
            "spec": {
                "selector": { "matchLabels": { "app": "multi" } },
                "targetPorts": [{ "number": 8000 }, { "number": 8001 }]
            }
        }"#;

        let pool = parse_inference_pool(json).unwrap();

        assert_eq!(pool.target_ports, vec![8000, 8001], "multiple ports");
    }

    // -- Label selector encoding --

    #[test]
    fn encodes_label_selector_deterministically() {
        let mut selector = BTreeMap::new();
        selector.insert("z-label".to_owned(), "last".to_owned());
        selector.insert("a-label".to_owned(), "first".to_owned());

        let encoded = encode_label_selector(&selector);

        assert_eq!(
            encoded, "a-label=first,z-label=last",
            "labels should be sorted alphabetically"
        );
    }

    // -- Pod list parsing --

    #[test]
    fn filters_to_ready_running_pods() {
        let pods = parse_ready_pods(MIXED_POD_LIST_JSON);

        assert_eq!(pods.len(), 1, "only one pod should pass filtering");
        assert_eq!(pods[0].name, "ready-pod", "pod name");
        assert_eq!(pods[0].ip, "10.0.0.1", "pod IP");
    }

    /// JSON fixture with one ready pod and several that should be filtered.
    const MIXED_POD_LIST_JSON: &str = r#"{
        "items": [
            {
                "metadata": { "name": "ready-pod" },
                "status": {
                    "phase": "Running",
                    "podIP": "10.0.0.1",
                    "conditions": [{ "type": "Ready", "status": "True" }]
                }
            },
            {
                "metadata": { "name": "pending-pod" },
                "status": { "phase": "Pending", "podIP": "10.0.0.2", "conditions": [] }
            },
            {
                "metadata": { "name": "not-ready-pod" },
                "status": {
                    "phase": "Running",
                    "podIP": "10.0.0.3",
                    "conditions": [{ "type": "Ready", "status": "False" }]
                }
            },
            {
                "metadata": { "name": "terminating-pod", "deletionTimestamp": "2026-05-29T00:00:00Z" },
                "status": {
                    "phase": "Running",
                    "podIP": "10.0.0.4",
                    "conditions": [{ "type": "Ready", "status": "True" }]
                }
            },
            {
                "metadata": { "name": "no-ip-pod" },
                "status": {
                    "phase": "Running",
                    "conditions": [{ "type": "Ready", "status": "True" }]
                }
            }
        ]
    }"#;

    // -- Endpoint building --

    #[test]
    fn builds_discovered_endpoints() {
        let pods = vec![
            ReadyPod {
                name: "sim-a-xyz".to_owned(),
                ip: "10.0.0.1".to_owned(),
                role: None,
            },
            ReadyPod {
                name: "sim-b-abc".to_owned(),
                ip: "10.0.0.2".to_owned(),
                role: None,
            },
        ];
        let models = vec!["fake-model".to_owned()];

        let endpoints = build_discovered_endpoints(&pods, &[8000], &models, "/metrics");

        assert_eq!(endpoints.len(), 2, "two endpoints from two pods");
        assert_eq!(
            endpoints[0].name.as_ref(),
            "sim-a-xyz:8000",
            "first endpoint name (sorted)"
        );
        assert_eq!(endpoints[0].address.as_ref(), "10.0.0.1:8000", "first endpoint address");
        assert_eq!(
            endpoints[0].metrics_url.as_deref(),
            Some("http://10.0.0.1:8000/metrics"),
            "metrics URL"
        );
        assert_eq!(endpoints[0].models[0].as_ref(), "fake-model", "model name");
        assert_eq!(endpoints[1].name.as_ref(), "sim-b-abc:8000", "second endpoint name");
    }

    #[test]
    fn builds_endpoints_for_multiple_ports() {
        let pods = vec![ReadyPod {
            name: "pod-a".to_owned(),
            ip: "10.0.0.1".to_owned(),
            role: None,
        }];
        let models = vec!["model".to_owned()];

        let endpoints = build_discovered_endpoints(&pods, &[8000, 8001], &models, "/metrics");

        assert_eq!(endpoints.len(), 2, "one pod with two ports produces two endpoints");
    }

    // -- Full discovery from JSON --

    #[test]
    fn discovers_endpoints_from_json_responses() {
        let pool_json = r#"{
            "spec": {
                "selector": { "matchLabels": { "app": "vllm" } },
                "targetPorts": [{ "number": 8000 }]
            }
        }"#;
        let pod_json = r#"{
            "items": [{
                "metadata": { "name": "vllm-pod-1" },
                "status": {
                    "phase": "Running",
                    "podIP": "10.0.1.5",
                    "conditions": [{ "type": "Ready", "status": "True" }]
                }
            }]
        }"#;

        let endpoints = discover_from_json(pool_json, pod_json, &["my-model".to_owned()], "/metrics").unwrap();

        assert_eq!(endpoints.len(), 1, "one discovered endpoint");
        assert_eq!(
            endpoints[0].address.as_ref(),
            "10.0.1.5:8000",
            "endpoint address is podIP:port"
        );
        assert_eq!(endpoints[0].models[0].as_ref(), "my-model", "model from config");
    }

    // -- API version parsing --

    #[test]
    fn parses_api_version_v1() {
        let (group, version) = parse_api_version("inference.networking.k8s.io/v1").unwrap();

        assert_eq!(group, "inference.networking.k8s.io", "group");
        assert_eq!(version, "v1", "version");
    }

    #[test]
    fn parses_api_version_v1alpha2() {
        let (group, version) = parse_api_version("inference.networking.x-k8s.io/v1alpha2").unwrap();

        assert_eq!(group, "inference.networking.x-k8s.io", "group");
        assert_eq!(version, "v1alpha2", "version");
    }

    #[test]
    fn rejects_api_version_empty_group() {
        assert!(parse_api_version("/v1").is_none(), "empty group should be rejected");
    }

    #[test]
    fn rejects_api_version_empty_version() {
        assert!(
            parse_api_version("group/").is_none(),
            "empty version should be rejected"
        );
    }

    #[test]
    fn rejects_api_version_no_slash() {
        assert!(parse_api_version("v1").is_none(), "no slash should be rejected");
    }

    #[test]
    fn rejects_api_version_extra_slash() {
        assert!(
            parse_api_version("group/version/extra").is_none(),
            "extra slash should be rejected"
        );
    }

    // -- Label selector encoding with percent-encoding --

    #[test]
    fn encodes_label_selector_with_prefix() {
        let mut selector = BTreeMap::new();
        selector.insert("llm-d.ai/model".to_owned(), "llama3".to_owned());
        selector.insert("app".to_owned(), "vllm".to_owned());

        let encoded = encode_label_selector(&selector);

        assert_eq!(
            encoded, "app=vllm,llm-d.ai%2Fmodel=llama3",
            "slash in label key should be percent-encoded"
        );
    }

    #[test]
    fn encodes_label_selector_plain_labels() {
        let mut selector = BTreeMap::new();
        selector.insert("app".to_owned(), "vllm".to_owned());
        selector.insert("pool".to_owned(), "test".to_owned());

        let encoded = encode_label_selector(&selector);

        assert_eq!(encoded, "app=vllm,pool=test", "plain labels should not be encoded");
    }

    #[test]
    fn encodes_label_selector_sorted_output() {
        let mut selector = BTreeMap::new();
        selector.insert("z".to_owned(), "3".to_owned());
        selector.insert("a".to_owned(), "1".to_owned());
        selector.insert("m".to_owned(), "2".to_owned());

        let encoded = encode_label_selector(&selector);

        assert_eq!(encoded, "a=1,m=2,z=3", "keys must be sorted");
    }

    // -- Pod name filtering --

    #[test]
    fn rejects_pod_with_missing_name() {
        let json = r#"{
            "items": [{
                "metadata": {},
                "status": {
                    "phase": "Running",
                    "podIP": "10.0.0.1",
                    "conditions": [{ "type": "Ready", "status": "True" }]
                }
            }]
        }"#;

        let pods = parse_ready_pods(json);

        assert!(pods.is_empty(), "pod with missing name should be filtered out");
    }

    #[test]
    fn rejects_pod_with_empty_name() {
        let json = r#"{
            "items": [{
                "metadata": { "name": "" },
                "status": {
                    "phase": "Running",
                    "podIP": "10.0.0.1",
                    "conditions": [{ "type": "Ready", "status": "True" }]
                }
            }]
        }"#;

        let pods = parse_ready_pods(json);

        assert!(pods.is_empty(), "pod with empty name should be filtered out");
    }

    // -- TLS config --

    #[test]
    fn rejects_empty_ca_bundle() {
        assert!(build_tls_config(b"").is_none(), "empty CA bundle should be rejected");
    }

    #[test]
    fn rejects_invalid_ca_bundle() {
        assert!(
            build_tls_config(b"not a certificate").is_none(),
            "invalid CA bundle should be rejected"
        );
    }

    // -- Chunked decoding --

    #[test]
    fn decode_chunked_valid() {
        let raw = "5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";

        let decoded = decode_chunked(raw).unwrap();

        assert_eq!(decoded, "hello world", "should decode chunked body");
    }

    #[test]
    fn decode_chunked_truncated_returns_none() {
        let raw = "ff\r\nshort\r\n";

        assert!(decode_chunked(raw).is_none(), "truncated chunk should return None");
    }

    #[test]
    fn decode_chunked_malformed_size_returns_none() {
        let raw = "zz\r\ndata\r\n";

        assert!(decode_chunked(raw).is_none(), "non-hex chunk size should return None");
    }

    // -- Role-aware pod discovery --

    #[test]
    fn pod_with_prefill_role_label() {
        let json = r#"{
            "items": [{
                "metadata": {
                    "name": "prefill-pod",
                    "labels": { "llm-d.ai/role": "prefill" }
                },
                "status": {
                    "phase": "Running",
                    "podIP": "10.0.0.1",
                    "conditions": [{ "type": "Ready", "status": "True" }]
                }
            }]
        }"#;

        let pods = parse_ready_pods(json);

        assert_eq!(pods.len(), 1, "one ready pod");
        assert_eq!(pods[0].role.as_deref(), Some("prefill"), "role label");

        let endpoints = build_discovered_endpoints(&pods, &[8000], &["m".to_owned()], "/metrics");

        assert_eq!(endpoints.len(), 1, "one endpoint");
        assert_eq!(endpoints[0].role, EndpointRole::Prefill, "prefill role mapped");
    }

    #[test]
    fn pod_with_decode_role_label() {
        let json = r#"{
            "items": [{
                "metadata": {
                    "name": "decode-pod",
                    "labels": { "llm-d.ai/role": "decode" }
                },
                "status": {
                    "phase": "Running",
                    "podIP": "10.0.0.2",
                    "conditions": [{ "type": "Ready", "status": "True" }]
                }
            }]
        }"#;

        let pods = parse_ready_pods(json);

        assert_eq!(pods.len(), 1, "one ready pod");
        assert_eq!(pods[0].role.as_deref(), Some("decode"), "role label");

        let endpoints = build_discovered_endpoints(&pods, &[8000], &["m".to_owned()], "/metrics");

        assert_eq!(endpoints.len(), 1, "one endpoint");
        assert_eq!(endpoints[0].role, EndpointRole::Decode, "decode role mapped");
    }

    #[test]
    fn pod_without_role_label_defaults_to_prefill_decode() {
        let json = r#"{
            "items": [{
                "metadata": { "name": "plain-pod" },
                "status": {
                    "phase": "Running",
                    "podIP": "10.0.0.3",
                    "conditions": [{ "type": "Ready", "status": "True" }]
                }
            }]
        }"#;

        let pods = parse_ready_pods(json);

        assert_eq!(pods.len(), 1, "one ready pod");
        assert!(pods[0].role.is_none(), "no role label");

        let endpoints = build_discovered_endpoints(&pods, &[8000], &["m".to_owned()], "/metrics");

        assert_eq!(endpoints.len(), 1, "one endpoint");
        assert_eq!(
            endpoints[0].role,
            EndpointRole::PrefillDecode,
            "absent role defaults to PrefillDecode"
        );
    }

    // -- HTTPRoute parsing --

    #[test]
    fn parse_httproute_with_inference_pool_ref() {
        let json = r#"{
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "HTTPRoute",
            "spec": {
                "rules": [{
                    "backendRefs": [{
                        "group": "inference.networking.k8s.io",
                        "kind": "InferencePool",
                        "name": "my-pool",
                        "port": 8000
                    }]
                }]
            }
        }"#;

        let pool_ref = parse_httproute_pool_ref(json).unwrap();

        assert_eq!(pool_ref.name, "my-pool", "pool name");
        assert_eq!(pool_ref.port, Some(8000), "port");
    }

    #[test]
    fn parse_httproute_accepts_x_k8s_inference_group() {
        let json = r#"{
            "spec": {
                "rules": [{
                    "backendRefs": [{
                        "group": "inference.networking.x-k8s.io",
                        "kind": "InferencePool",
                        "name": "alpha-pool"
                    }]
                }]
            }
        }"#;

        let pool_ref = parse_httproute_pool_ref(json).unwrap();

        assert_eq!(pool_ref.name, "alpha-pool", "accepts x-k8s group");
    }

    #[test]
    fn parse_httproute_ignores_service_backend_refs() {
        let json = r#"{
            "spec": {
                "rules": [{
                    "backendRefs": [{
                        "kind": "Service",
                        "name": "my-service",
                        "port": 80
                    }]
                }]
            }
        }"#;

        assert!(
            parse_httproute_pool_ref(json).is_none(),
            "Service backendRefs should be ignored"
        );
    }

    #[test]
    fn parse_httproute_takes_first_inference_pool() {
        let json = r#"{
            "spec": {
                "rules": [{
                    "backendRefs": [
                        {
                            "group": "inference.networking.k8s.io",
                            "kind": "InferencePool",
                            "name": "first-pool"
                        },
                        {
                            "group": "inference.networking.k8s.io",
                            "kind": "InferencePool",
                            "name": "second-pool"
                        }
                    ]
                }]
            }
        }"#;

        let pool_ref = parse_httproute_pool_ref(json).unwrap();

        assert_eq!(pool_ref.name, "first-pool", "should take first pool ref");
    }

    #[test]
    fn parse_httproute_no_usable_ref_returns_none() {
        let json = r#"{
            "spec": {
                "rules": [{
                    "backendRefs": [{
                        "group": "other.group.io",
                        "kind": "OtherKind",
                        "name": "something"
                    }]
                }]
            }
        }"#;

        assert!(
            parse_httproute_pool_ref(json).is_none(),
            "no InferencePool ref should return None"
        );
    }

    #[test]
    fn parse_httproute_empty_rules_returns_none() {
        let json = r#"{ "spec": { "rules": [] } }"#;

        assert!(
            parse_httproute_pool_ref(json).is_none(),
            "empty rules should return None"
        );
    }

    #[test]
    fn pod_with_unknown_role_label_is_skipped() {
        let json = r#"{
            "items": [{
                "metadata": {
                    "name": "unknown-role-pod",
                    "labels": { "llm-d.ai/role": "bogus" }
                },
                "status": {
                    "phase": "Running",
                    "podIP": "10.0.0.4",
                    "conditions": [{ "type": "Ready", "status": "True" }]
                }
            }]
        }"#;

        let pods = parse_ready_pods(json);

        assert_eq!(pods.len(), 1, "pod passes readiness filter");
        assert_eq!(pods[0].role.as_deref(), Some("bogus"), "unknown role label");

        let endpoints = build_discovered_endpoints(&pods, &[8000], &["m".to_owned()], "/metrics");

        assert!(endpoints.is_empty(), "pod with unknown role should be skipped");
    }

    // -- backendRef group matching (Fix 1) --

    #[test]
    fn accepts_k8s_io_inference_group() {
        let json = r#"{
            "spec": {
                "rules": [{
                    "backendRefs": [{
                        "group": "inference.networking.k8s.io",
                        "kind": "InferencePool",
                        "name": "pool-a"
                    }]
                }]
            }
        }"#;

        let pool_ref = parse_httproute_pool_ref(json).unwrap();

        assert_eq!(pool_ref.name, "pool-a", "k8s.io group accepted");
    }

    #[test]
    fn accepts_x_k8s_io_inference_group() {
        let json = r#"{
            "spec": {
                "rules": [{
                    "backendRefs": [{
                        "group": "inference.networking.x-k8s.io",
                        "kind": "InferencePool",
                        "name": "pool-b"
                    }]
                }]
            }
        }"#;

        let pool_ref = parse_httproute_pool_ref(json).unwrap();

        assert_eq!(pool_ref.name, "pool-b", "x-k8s.io group accepted");
    }

    #[test]
    fn rejects_similar_but_unsupported_inference_group() {
        let json = r#"{
            "spec": {
                "rules": [{
                    "backendRefs": [{
                        "group": "inference.networking.example.io",
                        "kind": "InferencePool",
                        "name": "pool-c"
                    }]
                }]
            }
        }"#;

        assert!(
            parse_httproute_pool_ref(json).is_none(),
            "unsupported inference group should be ignored"
        );
    }

    // -- backendRef empty name rejection (Fix 2) --

    #[test]
    fn rejects_backend_ref_with_empty_name() {
        let json = r#"{
            "spec": {
                "rules": [{
                    "backendRefs": [{
                        "group": "inference.networking.k8s.io",
                        "kind": "InferencePool",
                        "name": ""
                    }]
                }]
            }
        }"#;

        assert!(
            parse_httproute_pool_ref(json).is_none(),
            "backendRef with empty name should be ignored"
        );
    }

    #[test]
    fn rejects_backend_ref_with_whitespace_only_name() {
        let json = r#"{
            "spec": {
                "rules": [{
                    "backendRefs": [{
                        "group": "inference.networking.k8s.io",
                        "kind": "InferencePool",
                        "name": "   "
                    }]
                }]
            }
        }"#;

        assert!(
            parse_httproute_pool_ref(json).is_none(),
            "backendRef with whitespace-only name should be ignored"
        );
    }

    // -- backendRef port fallback (Fix 3) --

    #[test]
    fn pool_target_ports_win_over_backend_ref_port() {
        let pool_info = PoolInfo {
            selector: BTreeMap::from([("app".to_owned(), "vllm".to_owned())]),
            target_ports: vec![8000],
        };

        let result = resolve_target_ports(&pool_info, Some(9090)).unwrap();

        assert_eq!(result, vec![8000], "pool target ports should take priority");
    }

    #[test]
    fn backend_ref_port_used_when_pool_has_no_target_ports() {
        let pool_info = PoolInfo {
            selector: BTreeMap::from([("app".to_owned(), "vllm".to_owned())]),
            target_ports: vec![],
        };

        let result = resolve_target_ports(&pool_info, Some(9090)).unwrap();

        assert_eq!(result, vec![9090], "backendRef port should be used as fallback");
    }

    #[test]
    fn no_ports_anywhere_returns_none() {
        let pool_info = PoolInfo {
            selector: BTreeMap::from([("app".to_owned(), "vllm".to_owned())]),
            target_ports: vec![],
        };

        assert!(
            resolve_target_ports(&pool_info, None).is_none(),
            "no ports from pool or backendRef should return None"
        );
    }

    // -- Gateway-only discovery composition (Fix 4) --

    #[test]
    fn gateway_composition_parses_httproute_and_pool() {
        let pool_ref = parse_httproute_pool_ref(GW_ROUTE_JSON).unwrap();
        assert_eq!(pool_ref.name, "gw-pool", "pool name from HTTPRoute");

        let pool_info = parse_inference_pool(GW_POOL_JSON).unwrap();
        assert_eq!(pool_info.target_ports, vec![8080], "ports from pool spec");
    }

    #[test]
    fn gateway_composition_builds_endpoints() {
        let pool_info = parse_inference_pool(GW_POOL_JSON).unwrap();
        let ready_pods = parse_ready_pods(GW_POD_JSON);
        assert_eq!(ready_pods.len(), 1, "one ready pod");

        let endpoints = build_discovered_endpoints(
            &ready_pods,
            &pool_info.target_ports,
            &["llama3".to_owned()],
            "/stats/prometheus",
        );

        assert_eq!(endpoints.len(), 1, "one endpoint from composition");
        assert_eq!(endpoints[0].address.as_ref(), "10.0.5.1:8080", "podIP:port");
    }

    #[test]
    fn gateway_composition_preserves_config_and_role() {
        let pool_info = parse_inference_pool(GW_POOL_JSON).unwrap();
        let ready_pods = parse_ready_pods(GW_POD_JSON);
        let endpoints = build_discovered_endpoints(
            &ready_pods,
            &pool_info.target_ports,
            &["llama3".to_owned()],
            "/stats/prometheus",
        );

        assert_eq!(endpoints[0].models[0].as_ref(), "llama3", "models from gw config");
        assert_eq!(
            endpoints[0].metrics_url.as_deref(),
            Some("http://10.0.5.1:8080/stats/prometheus"),
            "metrics_url uses gateway metrics_path"
        );
        assert_eq!(endpoints[0].role, EndpointRole::Prefill, "role from pod label");
    }

    // -- Gateway composition test fixtures --

    const GW_ROUTE_JSON: &str = r#"{
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "HTTPRoute",
        "spec": {
            "rules": [{
                "backendRefs": [{
                    "group": "inference.networking.k8s.io",
                    "kind": "InferencePool",
                    "name": "gw-pool",
                    "port": 8000
                }]
            }]
        }
    }"#;

    const GW_POOL_JSON: &str = r#"{
        "spec": {
            "selector": { "matchLabels": { "app": "vllm" } },
            "targetPorts": [{ "number": 8080 }]
        }
    }"#;

    const GW_POD_JSON: &str = r#"{
        "items": [{
            "metadata": {
                "name": "vllm-pod-0",
                "labels": { "llm-d.ai/role": "prefill" }
            },
            "status": {
                "phase": "Running",
                "podIP": "10.0.5.1",
                "conditions": [{ "type": "Ready", "status": "True" }]
            }
        }]
    }"#;
}
