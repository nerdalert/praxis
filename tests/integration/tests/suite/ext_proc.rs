// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Praxis Contributors

//! Integration tests for ext_proc full-duplex request routing.

use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
};

use async_trait::async_trait;
use ext_proc_proto::envoy::service::ext_proc::v3::{
    BodyResponse, HeadersResponse, ImmediateResponse, ProcessingRequest, ProcessingResponse,
    external_processor_server::{ExternalProcessor, ExternalProcessorServer},
    processing_request, processing_response,
};
use praxis_test_utils::{free_port, http_post, http_send, parse_body, parse_status};
use tokio::sync::oneshot;
use tonic::transport::Server;

mod ext_proc_proto {
    pub(crate) mod envoy {
        pub(crate) mod service {
            pub(crate) mod common {
                #[allow(
                    dead_code,
                    missing_docs,
                    unreachable_pub,
                    trivial_casts,
                    unused_qualifications,
                    clippy::allow_attributes,
                    clippy::allow_attributes_without_reason,
                    clippy::clone_on_ref_ptr,
                    clippy::default_trait_access,
                    clippy::derive_partial_eq_without_eq,
                    clippy::doc_lazy_continuation,
                    clippy::doc_markdown,
                    clippy::enum_variant_names,
                    clippy::missing_docs_in_private_items,
                    clippy::needless_borrows_for_generic_args,
                    clippy::too_many_lines,
                    clippy::trivially_copy_pass_by_ref,
                    reason = "generated protobuf code used by test mock"
                )]
                pub(crate) mod v3 {
                    tonic::include_proto!("envoy.service.common.v3");
                }
            }

            pub(crate) mod ext_proc {
                #[allow(
                    dead_code,
                    missing_docs,
                    unreachable_pub,
                    trivial_casts,
                    unused_qualifications,
                    clippy::allow_attributes,
                    clippy::allow_attributes_without_reason,
                    clippy::clone_on_ref_ptr,
                    clippy::default_trait_access,
                    clippy::derive_partial_eq_without_eq,
                    clippy::doc_lazy_continuation,
                    clippy::doc_markdown,
                    clippy::enum_variant_names,
                    clippy::missing_docs_in_private_items,
                    clippy::needless_borrows_for_generic_args,
                    clippy::too_many_lines,
                    clippy::trivially_copy_pass_by_ref,
                    reason = "generated protobuf code used by test mock"
                )]
                pub(crate) mod v3 {
                    tonic::include_proto!("envoy.service.ext_proc.v3");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Mock Processor
// ---------------------------------------------------------------------------

/// Lifecycle event recorded by the mock processor per stream.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProcessorEvent {
    RequestHeaders,
    RequestBody { end_of_stream: bool },
}

struct MockProcessor {
    behavior: MockBehavior,
    stream_count: Arc<AtomicU32>,
    events: Arc<Mutex<Vec<ProcessorEvent>>>,
}

pub(crate) enum MockBehavior {
    RouteOnRequestEos { destination: String },
    Immediate { status: i32, body: String },
    MissingDestination,
    InvalidDestination { value: String },
    AmbiguousDestination { value_a: String, value_b: String },
    MutationPrecedence { headers_dest: String, body_dest: String },
}

#[async_trait]
impl ExternalProcessor for MockProcessor {
    type ProcessStream =
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<ProcessingResponse, tonic::Status>> + Send>>;

    async fn process(
        &self,
        request: tonic::Request<tonic::Streaming<ProcessingRequest>>,
    ) -> Result<tonic::Response<Self::ProcessStream>, tonic::Status> {
        self.stream_count.fetch_add(1, Ordering::Relaxed);
        let events = Arc::clone(&self.events);
        let mut stream = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(8);

        match &self.behavior {
            MockBehavior::RouteOnRequestEos { destination } => {
                let dest = destination.clone();
                let ev = Arc::clone(&events);
                tokio::spawn(async move {
                    expect_request_headers(&mut stream, &ev).await;
                    wait_for_body_eos(&mut stream, &ev).await;

                    let header_resp = build_routing_response(&dest);
                    drop(tx.send(Ok(header_resp)).await);

                    let body_resp = build_noop_body_response();
                    drop(tx.send(Ok(body_resp)).await);
                });
            },
            MockBehavior::Immediate { status, body } => {
                let s = *status;
                let b = body.clone();
                let ev = Arc::clone(&events);
                tokio::spawn(async move {
                    expect_request_headers(&mut stream, &ev).await;
                    wait_for_body_eos(&mut stream, &ev).await;
                    use ext_proc_proto::envoy::service::common::v3::HttpStatus;
                    let resp = ProcessingResponse {
                        response: Some(processing_response::Response::ImmediateResponse(ImmediateResponse {
                            status: Some(HttpStatus { code: s }),
                            body: b,
                            ..Default::default()
                        })),
                        ..Default::default()
                    };
                    drop(tx.send(Ok(resp)).await);
                });
            },
            MockBehavior::MissingDestination => {
                let ev = Arc::clone(&events);
                tokio::spawn(async move {
                    expect_request_headers(&mut stream, &ev).await;
                    wait_for_body_eos(&mut stream, &ev).await;

                    let header_resp = ProcessingResponse {
                        response: Some(processing_response::Response::RequestHeaders(HeadersResponse {
                            response: None,
                        })),
                        ..Default::default()
                    };
                    drop(tx.send(Ok(header_resp)).await);

                    let body_resp = build_noop_body_response();
                    drop(tx.send(Ok(body_resp)).await);
                });
            },
            MockBehavior::InvalidDestination { value } => {
                let val = value.clone();
                let ev = Arc::clone(&events);
                tokio::spawn(async move {
                    expect_request_headers(&mut stream, &ev).await;
                    wait_for_body_eos(&mut stream, &ev).await;

                    let header_resp = build_routing_response(&val);
                    drop(tx.send(Ok(header_resp)).await);

                    let body_resp = build_noop_body_response();
                    drop(tx.send(Ok(body_resp)).await);
                });
            },
            MockBehavior::AmbiguousDestination { value_a, value_b } => {
                let a = value_a.clone();
                let b = value_b.clone();
                let ev = Arc::clone(&events);
                tokio::spawn(async move {
                    expect_request_headers(&mut stream, &ev).await;
                    wait_for_body_eos(&mut stream, &ev).await;

                    let header_resp = build_dual_routing_response(&a, &b);
                    drop(tx.send(Ok(header_resp)).await);

                    let body_resp = build_noop_body_response();
                    drop(tx.send(Ok(body_resp)).await);
                });
            },
            MockBehavior::MutationPrecedence {
                headers_dest,
                body_dest,
            } => {
                let hd = headers_dest.clone();
                let bd = body_dest.clone();
                let ev = Arc::clone(&events);
                tokio::spawn(async move {
                    expect_request_headers(&mut stream, &ev).await;
                    wait_for_body_eos(&mut stream, &ev).await;

                    let header_resp = build_append_routing_response(&hd);
                    drop(tx.send(Ok(header_resp)).await);

                    let body_resp = build_body_response_with_remove_and_set(&bd);
                    drop(tx.send(Ok(body_resp)).await);
                });
            },
        }

        Ok(tonic::Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
    }
}

async fn expect_request_headers(
    stream: &mut tonic::Streaming<ProcessingRequest>,
    events: &Arc<Mutex<Vec<ProcessorEvent>>>,
) {
    let msg = stream
        .message()
        .await
        .expect("stream read failed")
        .expect("stream closed before RequestHeaders");
    match msg.request {
        Some(processing_request::Request::RequestHeaders(_)) => {
            events.lock().unwrap().push(ProcessorEvent::RequestHeaders);
        },
        Some(other) => panic!(
            "expected RequestHeaders as first message, got {:?}",
            std::mem::discriminant(&other)
        ),
        None => panic!("first message has no request variant"),
    }
}

async fn wait_for_body_eos(stream: &mut tonic::Streaming<ProcessingRequest>, events: &Arc<Mutex<Vec<ProcessorEvent>>>) {
    loop {
        match stream.message().await {
            Ok(Some(msg)) => {
                if let Some(processing_request::Request::RequestBody(b)) = msg.request {
                    let eos = b.end_of_stream;
                    events
                        .lock()
                        .unwrap()
                        .push(ProcessorEvent::RequestBody { end_of_stream: eos });
                    if eos {
                        break;
                    }
                }
            },
            _ => return,
        }
    }
}

fn build_routing_response(destination: &str) -> ProcessingResponse {
    use ext_proc_proto::envoy::service::{
        common::v3::{HeaderValue, HeaderValueOption, header_value_option::HeaderAppendAction},
        ext_proc::v3::{CommonResponse, HeaderMutation},
    };
    ProcessingResponse {
        response: Some(processing_response::Response::RequestHeaders(HeadersResponse {
            response: Some(CommonResponse {
                header_mutation: Some(HeaderMutation {
                    set_headers: vec![HeaderValueOption {
                        header: Some(HeaderValue {
                            key: "x-gateway-destination-endpoint".to_owned(),
                            raw_value: destination.as_bytes().to_vec(),
                            ..Default::default()
                        }),
                        append_action: HeaderAppendAction::OverwriteIfExistsOrAdd.into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            }),
        })),
        ..Default::default()
    }
}

fn build_dual_routing_response(value_a: &str, value_b: &str) -> ProcessingResponse {
    use ext_proc_proto::envoy::service::{
        common::v3::{HeaderValue, HeaderValueOption},
        ext_proc::v3::{CommonResponse, HeaderMutation},
    };
    ProcessingResponse {
        response: Some(processing_response::Response::RequestHeaders(HeadersResponse {
            response: Some(CommonResponse {
                header_mutation: Some(HeaderMutation {
                    set_headers: vec![
                        HeaderValueOption {
                            header: Some(HeaderValue {
                                key: "x-gateway-destination-endpoint".to_owned(),
                                raw_value: value_a.as_bytes().to_vec(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        HeaderValueOption {
                            header: Some(HeaderValue {
                                key: "x-gateway-destination-endpoint".to_owned(),
                                raw_value: value_b.as_bytes().to_vec(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }),
                ..Default::default()
            }),
        })),
        ..Default::default()
    }
}

fn build_append_routing_response(destination: &str) -> ProcessingResponse {
    use ext_proc_proto::envoy::service::{
        common::v3::{HeaderValue, HeaderValueOption, header_value_option::HeaderAppendAction},
        ext_proc::v3::{CommonResponse, HeaderMutation},
    };
    ProcessingResponse {
        response: Some(processing_response::Response::RequestHeaders(HeadersResponse {
            response: Some(CommonResponse {
                header_mutation: Some(HeaderMutation {
                    set_headers: vec![HeaderValueOption {
                        header: Some(HeaderValue {
                            key: "x-gateway-destination-endpoint".to_owned(),
                            raw_value: destination.as_bytes().to_vec(),
                            ..Default::default()
                        }),
                        append_action: HeaderAppendAction::AppendIfExistsOrAdd.into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            }),
        })),
        ..Default::default()
    }
}

fn build_body_response_with_remove_and_set(destination: &str) -> ProcessingResponse {
    use ext_proc_proto::envoy::service::{
        common::v3::{HeaderValue, HeaderValueOption, header_value_option::HeaderAppendAction},
        ext_proc::v3::{CommonResponse, HeaderMutation},
    };
    ProcessingResponse {
        response: Some(processing_response::Response::RequestBody(BodyResponse {
            response: Some(CommonResponse {
                header_mutation: Some(HeaderMutation {
                    remove_headers: vec!["x-gateway-destination-endpoint".to_owned()],
                    set_headers: vec![HeaderValueOption {
                        header: Some(HeaderValue {
                            key: "x-gateway-destination-endpoint".to_owned(),
                            raw_value: destination.as_bytes().to_vec(),
                            ..Default::default()
                        }),
                        append_action: HeaderAppendAction::OverwriteIfExistsOrAdd.into(),
                        ..Default::default()
                    }],
                }),
                body_mutation: None,
                ..Default::default()
            }),
        })),
        ..Default::default()
    }
}

fn build_noop_body_response() -> ProcessingResponse {
    ProcessingResponse {
        response: Some(processing_response::Response::RequestBody(BodyResponse {
            response: None,
        })),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Mock Processor Lifecycle
// ---------------------------------------------------------------------------

pub(crate) struct ProcessorGuard {
    pub(crate) addr: SocketAddr,
    stream_count: Arc<AtomicU32>,
    events: Arc<Mutex<Vec<ProcessorEvent>>>,
    _shutdown: oneshot::Sender<()>,
}

impl ProcessorGuard {
    pub(crate) fn stream_count(&self) -> u32 {
        self.stream_count.load(Ordering::Relaxed)
    }

    fn events(&self) -> Vec<ProcessorEvent> {
        self.events.lock().unwrap().clone()
    }
}

pub(crate) fn start_mock_processor(behavior: MockBehavior) -> ProcessorGuard {
    let stream_count = Arc::new(AtomicU32::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let processor = MockProcessor {
        behavior,
        stream_count: Arc::clone(&stream_count),
        events: Arc::clone(&events),
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    std::thread::spawn(move || {
        rt.block_on(async {
            Server::builder()
                .add_service(ExternalProcessorServer::new(processor))
                .serve_with_incoming_shutdown(tokio_stream::wrappers::TcpListenerStream::new(listener), async {
                    drop(shutdown_rx.await);
                })
                .await
                .unwrap();
        });
    });

    praxis_test_utils::wait_for_tcp(&format!("127.0.0.1:{}", addr.port()));

    ProcessorGuard {
        addr,
        stream_count,
        events,
        _shutdown: shutdown_tx,
    }
}

// ---------------------------------------------------------------------------
// Recording Backend
// ---------------------------------------------------------------------------

struct RecordingBackend {
    addr: SocketAddr,
    request_count: Arc<AtomicU32>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl RecordingBackend {
    fn count(&self) -> u32 {
        self.request_count.load(Ordering::Relaxed)
    }

    fn destination(&self) -> String {
        self.addr.to_string()
    }
}

impl Drop for RecordingBackend {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        drop(std::net::TcpStream::connect(self.addr));
    }
}

fn start_recording_backend() -> RecordingBackend {
    use std::{
        io::{Read as _, Write as _},
        sync::atomic::AtomicBool,
    };

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(AtomicU32::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));

    let counter = Arc::clone(&count);
    let flag = Arc::clone(&shutdown);
    std::thread::spawn(move || {
        listener.set_nonblocking(false).unwrap();
        for stream in listener.incoming() {
            if flag.load(Ordering::Acquire) {
                break;
            }
            if let Ok(mut s) = stream {
                drop(s.set_read_timeout(Some(std::time::Duration::from_secs(5))));
                // Keep enough room for future tests with larger header sets
                // while still avoiding a full HTTP parser in this recording backend.
                let mut buf = vec![0_u8; 16 * 1024];
                drop(s.read(&mut buf));
                counter.fetch_add(1, Ordering::Relaxed);
                drop(s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nrecorded"));
            }
        }
    });

    RecordingBackend {
        addr,
        request_count: count,
        shutdown,
    }
}

// ---------------------------------------------------------------------------
// Proxy Helpers
// ---------------------------------------------------------------------------

fn start_ext_proc_proxy(proxy_port: u16, processor_port: u16, config_yaml: &str) -> praxis_test_utils::ProxyGuard {
    let patched = config_yaml
        .replace("127.0.0.1:8080", &format!("127.0.0.1:{proxy_port}"))
        .replace("0.0.0.0:8080", &format!("127.0.0.1:{proxy_port}"))
        .replace("127.0.0.1:9002", &format!("127.0.0.1:{processor_port}"));
    let config = praxis_core::config::Config::from_yaml(&patched).expect("ext_proc test config should parse");
    let registry = praxis::build_full_registry();
    praxis_test_utils::start_proxy_with_registry(&config, &registry)
}

fn load_example_yaml() -> String {
    let path = praxis_test_utils::example_config_path("traffic-management/ext-proc-endpoint-selector.yaml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn ext_proc_routes_after_eos() {
    let backend = praxis_test_utils::start_echo_backend();
    let proc_guard = start_mock_processor(MockBehavior::RouteOnRequestEos {
        destination: format!("127.0.0.1:{}", backend.port()),
    });
    let proxy_port = free_port();
    let yaml = load_example_yaml();
    let proxy = start_ext_proc_proxy(proxy_port, proc_guard.addr.port(), &yaml);

    let baseline = proc_guard.stream_count();
    let (status, body) = http_post(proxy.addr(), "/route-test", "hello from client");
    assert_eq!(status, 200, "routed request should succeed");
    assert_eq!(body, "hello from client", "backend should echo original body");
    assert_eq!(
        proc_guard.stream_count() - baseline,
        1,
        "exactly one Process stream for one HTTP request"
    );
    let events = proc_guard.events();
    assert!(
        events.contains(&ProcessorEvent::RequestHeaders),
        "processor must observe RequestHeaders"
    );
    assert!(
        events.contains(&ProcessorEvent::RequestBody { end_of_stream: false }),
        "processor must observe a non-terminal body chunk before EOS"
    );
    assert!(
        events.contains(&ProcessorEvent::RequestBody { end_of_stream: true }),
        "processor must observe terminal RequestBody EOS"
    );
}

#[test]
fn ext_proc_destination_header_stripped() {
    let backend = praxis_test_utils::start_header_echo_backend();
    let proc_guard = start_mock_processor(MockBehavior::RouteOnRequestEos {
        destination: format!("127.0.0.1:{}", backend.port()),
    });
    let proxy_port = free_port();
    let yaml = load_example_yaml();
    let proxy = start_ext_proc_proxy(proxy_port, proc_guard.addr.port(), &yaml);

    let (status, body) = http_post(proxy.addr(), "/strip-test", "body");
    assert_eq!(status, 200);
    let lower = body.to_lowercase();
    assert!(
        !lower.contains("x-gateway-destination-endpoint"),
        "destination header must be stripped before reaching backend, got headers: {body}"
    );
}

#[test]
fn ext_proc_client_header_cannot_select_upstream() {
    let legit_backend = praxis_test_utils::start_backend_with_shutdown("legit");
    let evil_backend = start_recording_backend();
    let proc_guard = start_mock_processor(MockBehavior::RouteOnRequestEos {
        destination: format!("127.0.0.1:{}", legit_backend.port()),
    });
    let proxy_port = free_port();
    let yaml = load_example_yaml();
    let proxy = start_ext_proc_proxy(proxy_port, proc_guard.addr.port(), &yaml);

    let request = format!(
        "POST /ssrf HTTP/1.1\r\nHost: localhost\r\nx-gateway-destination-endpoint: {}\r\nContent-Length: 4\r\n\r\ntest",
        evil_backend.destination()
    );
    let raw = http_send(proxy.addr(), &request);
    let status = parse_status(&raw);
    let body = parse_body(&raw);
    assert_eq!(status, 200, "request should succeed via legit backend");
    assert_eq!(
        body, "legit",
        "must route to processor-chosen backend, not client header"
    );
    assert_eq!(evil_backend.count(), 0, "spoofed backend must not be contacted");
}

#[test]
fn ext_proc_processor_failure_returns_status_on_error() {
    let unused_port = free_port();
    let spoof_backend = start_recording_backend();
    let proxy_port = free_port();
    let yaml = load_example_yaml();
    let proxy = start_ext_proc_proxy(proxy_port, unused_port, &yaml);

    let request = format!(
        "POST /fail-test HTTP/1.1\r\nHost: localhost\r\nx-gateway-destination-endpoint: {}\r\nContent-Length: 4\r\n\r\nbody",
        spoof_backend.destination()
    );
    let raw = http_send(proxy.addr(), &request);
    let status = parse_status(&raw);
    assert_eq!(status, 503, "processor failure should return status_on_error 503");
    assert_eq!(
        spoof_backend.count(),
        0,
        "spoofed backend must not be contacted on processor failure"
    );
}

#[test]
fn ext_proc_immediate_response() {
    let proc_guard = start_mock_processor(MockBehavior::Immediate {
        status: 403,
        body: "forbidden by processor".to_owned(),
    });
    let proxy_port = free_port();
    let yaml = load_example_yaml();
    let proxy = start_ext_proc_proxy(proxy_port, proc_guard.addr.port(), &yaml);

    let (status, body) = http_post(proxy.addr(), "/immediate", "body");
    assert_eq!(status, 403, "immediate response status should reach client");
    assert_eq!(
        body, "forbidden by processor",
        "immediate response body should reach client"
    );
}

#[test]
fn ext_proc_required_missing_destination_rejects() {
    let spoof_backend = start_recording_backend();
    let proc_guard = start_mock_processor(MockBehavior::MissingDestination);
    let proxy_port = free_port();
    let yaml = load_example_yaml();
    let proxy = start_ext_proc_proxy(proxy_port, proc_guard.addr.port(), &yaml);

    let request = format!(
        "POST /missing-dest HTTP/1.1\r\nHost: localhost\r\nx-gateway-destination-endpoint: {}\r\nContent-Length: 4\r\n\r\nbody",
        spoof_backend.destination()
    );
    let raw = http_send(proxy.addr(), &request);
    let status = parse_status(&raw);
    assert_eq!(
        status, 503,
        "required endpoint_selector should reject when destination is absent"
    );
    assert_eq!(
        spoof_backend.count(),
        0,
        "spoofed backend must not be contacted when destination is missing"
    );
}

#[test]
fn ext_proc_bodyless_request_routes() {
    let backend = praxis_test_utils::start_backend_with_shutdown("bodyless-ok");
    let proc_guard = start_mock_processor(MockBehavior::RouteOnRequestEos {
        destination: format!("127.0.0.1:{}", backend.port()),
    });
    let proxy_port = free_port();
    let yaml = load_example_yaml();
    let proxy = start_ext_proc_proxy(proxy_port, proc_guard.addr.port(), &yaml);

    let request = "POST /bodyless HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n";
    let raw = http_send(proxy.addr(), request);
    let status = parse_status(&raw);
    let body = parse_body(&raw);
    assert_eq!(status, 200, "bodyless request should route successfully");
    assert_eq!(body, "bodyless-ok");
}

#[test]
fn ext_proc_repeated_requests_no_crosstalk() {
    let backend = praxis_test_utils::start_echo_backend();
    let proc_guard = start_mock_processor(MockBehavior::RouteOnRequestEos {
        destination: format!("127.0.0.1:{}", backend.port()),
    });
    let proxy_port = free_port();
    let yaml = load_example_yaml();
    let proxy = start_ext_proc_proxy(proxy_port, proc_guard.addr.port(), &yaml);

    let baseline = proc_guard.stream_count();
    for i in 0..3 {
        let marker = format!("request-{i}");
        let (status, body) = http_post(proxy.addr(), &format!("/repeat-{i}"), &marker);
        assert_eq!(status, 200, "request {i} should succeed");
        assert_eq!(body, marker, "request {i} body should match");
    }
    let streams_used = proc_guard.stream_count() - baseline;
    assert_eq!(
        streams_used, 3,
        "each request should use exactly one new Process stream (used {streams_used})"
    );
}

#[test]
fn ext_proc_invalid_destination_rejects() {
    let backend = start_recording_backend();
    let proc_guard = start_mock_processor(MockBehavior::InvalidDestination {
        value: "http://not-a-valid-authority:999/path".to_owned(),
    });
    let proxy_port = free_port();
    let yaml = load_example_yaml();
    let proxy = start_ext_proc_proxy(proxy_port, proc_guard.addr.port(), &yaml);

    let request = format!(
        "POST /invalid-dest HTTP/1.1\r\nHost: localhost\r\nx-gateway-destination-endpoint: {}\r\nContent-Length: 4\r\n\r\nbody",
        backend.destination()
    );
    let raw = http_send(proxy.addr(), &request);
    let status = parse_status(&raw);
    assert_eq!(
        status, 503,
        "invalid destination should be rejected by endpoint_selector"
    );
    assert_eq!(
        backend.count(),
        0,
        "no backend request should occur for invalid destination"
    );
}

#[test]
fn ext_proc_ambiguous_destination_rejects() {
    let backend = start_recording_backend();
    let proc_guard = start_mock_processor(MockBehavior::AmbiguousDestination {
        value_a: "host-a:8080".to_owned(),
        value_b: "host-b:9090".to_owned(),
    });
    let proxy_port = free_port();
    let yaml = load_example_yaml();
    let proxy = start_ext_proc_proxy(proxy_port, proc_guard.addr.port(), &yaml);

    let request = format!(
        "POST /ambiguous-dest HTTP/1.1\r\nHost: localhost\r\nx-gateway-destination-endpoint: {}\r\nContent-Length: 4\r\n\r\nbody",
        backend.destination()
    );
    let raw = http_send(proxy.addr(), &request);
    let status = parse_status(&raw);
    assert_eq!(
        status, 503,
        "ambiguous destination should be rejected by endpoint_selector"
    );
    assert_eq!(
        backend.count(),
        0,
        "no backend request should occur for ambiguous destination"
    );
}

#[test]
fn ext_proc_immediate_response_no_backend_hit() {
    let backend = start_recording_backend();
    let proc_guard = start_mock_processor(MockBehavior::Immediate {
        status: 403,
        body: "blocked".to_owned(),
    });
    let proxy_port = free_port();
    let yaml = load_example_yaml();
    let proxy = start_ext_proc_proxy(proxy_port, proc_guard.addr.port(), &yaml);

    let request = format!(
        "POST /immediate-no-backend HTTP/1.1\r\nHost: localhost\r\nx-gateway-destination-endpoint: {}\r\nContent-Length: 4\r\n\r\nbody",
        backend.destination()
    );
    let raw = http_send(proxy.addr(), &request);
    let status = parse_status(&raw);
    let body = parse_body(&raw);
    assert_eq!(status, 403, "immediate response should reach client");
    assert_eq!(body, "blocked");
    assert_eq!(
        backend.count(),
        0,
        "no backend request should occur for immediate response"
    );
}

#[test]
fn ext_proc_mutation_precedence_later_set_overrides() {
    let initial_backend = praxis_test_utils::start_backend_with_shutdown("initial");
    let final_backend = praxis_test_utils::start_backend_with_shutdown("final");
    let proc_guard = start_mock_processor(MockBehavior::MutationPrecedence {
        headers_dest: format!("127.0.0.1:{}", initial_backend.port()),
        body_dest: format!("127.0.0.1:{}", final_backend.port()),
    });
    let proxy_port = free_port();
    let yaml = load_example_yaml();
    let proxy = start_ext_proc_proxy(proxy_port, proc_guard.addr.port(), &yaml);

    let (status, body) = http_post(proxy.addr(), "/precedence", "precedence-body");
    assert_eq!(status, 200, "request should succeed");
    assert_eq!(
        body, "final",
        "body response Remove+Set must override earlier Add from headers response"
    );
}
