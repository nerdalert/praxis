// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Shane Utt

//! Criterion benchmarks for filter pipeline construction and execution.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::unnecessary_wraps,
    reason = "benchmarks; factory fns must return Result for http_builtin"
)]

mod common;

use std::{hint::black_box, sync::Arc};

use async_trait::async_trait;
use bytes::Bytes;
use common::{bench_runtime, make_ctx, make_request};
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use praxis_core::config::{PathMatch, Route};
use praxis_filter::{FailureMode, FilterEntry, FilterPipeline, FilterRegistry, HttpFilter, RouterFilter};

// -----------------------------------------------------------------------------
// Benchmarks
// -----------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_pipeline_build,
    bench_pipeline_execute_request,
    bench_filter_state,
    bench_pipeline_state_overhead,
    bench_pipeline_pinning,
);
criterion_main!(benches);

/// Benchmark pipeline construction from filter entries.
fn bench_pipeline_build(c: &mut Criterion) {
    let registry = FilterRegistry::with_builtins();
    let mut group = c.benchmark_group("pipeline_build");

    for size in [1, 5, 20] {
        let entries = make_entries(size);
        group.bench_with_input(BenchmarkId::from_parameter(size), &entries, |b, entries| {
            b.iter_batched(
                || entries.clone(),
                |mut cloned| {
                    let _result = black_box(FilterPipeline::build(black_box(&mut cloned), &registry).unwrap());
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

/// Benchmark async request execution through a realistic pipeline.
fn bench_pipeline_execute_request(c: &mut Criterion) {
    let rt = bench_runtime();

    let routes = vec![
        Route {
            path_match: PathMatch::Prefix {
                path_prefix: "/api/".to_owned(),
            },
            host: None,
            headers: None,
            cluster: "api".into(),
        },
        Route {
            path_match: PathMatch::Prefix {
                path_prefix: "/".to_owned(),
            },
            host: None,
            headers: None,
            cluster: "default".into(),
        },
    ];

    let router = RouterFilter::new(routes).expect("valid routes");
    let registry = FilterRegistry::with_builtins();
    let mut entries = vec![
        filter_entry(
            "router",
            "routes:\n  - path_prefix: /api/\n    cluster: api\n  - path_prefix: /\n    cluster: default",
        ),
        filter_entry("headers", "request_add:\n  - name: X-Via\n    value: praxis"),
    ];
    let pipeline = FilterPipeline::build(&mut entries, &registry).unwrap();

    // Also benchmark the router alone for comparison.
    let mut group = c.benchmark_group("pipeline_execute_request");
    group.bench_function("router_only", |b| {
        let router = &router;
        b.to_async(&rt).iter_batched(
            || make_request("/api/v1/users"),
            |req| async move {
                let mut ctx = make_ctx(&req);
                let _result = black_box(router.on_request(black_box(&mut ctx)).await.unwrap());
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("router_plus_headers", |b| {
        let pipeline = &pipeline;
        b.to_async(&rt).iter_batched(
            || make_request("/api/v1/users"),
            |req| async move {
                let mut ctx = make_ctx(&req);
                let _result = black_box(pipeline.execute_http_request(black_box(&mut ctx)).await.unwrap());
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark isolated filter state operations.
///
/// Each operation uses batched setup so context construction
/// and prior-state cleanup are outside the measurement.
fn bench_filter_state(c: &mut Criterion) {
    let mut group = c.benchmark_group("filter_state");

    group.bench_function("insert_into_empty", |b| {
        b.iter_batched(
            std::collections::HashMap::<usize, Box<dyn std::any::Any + Send + Sync>>::new,
            |mut map| {
                map.insert(0, Box::new(42u64));
                black_box(&map);
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("get_hit", |b| {
        let mut map: std::collections::HashMap<usize, Box<dyn std::any::Any + Send + Sync>> =
            std::collections::HashMap::new();
        map.insert(0, Box::new(42u64));
        b.iter(|| black_box(map.get(&0).and_then(|v| v.downcast_ref::<u64>())));
    });

    group.bench_function("get_miss", |b| {
        let map: std::collections::HashMap<usize, Box<dyn std::any::Any + Send + Sync>> =
            std::collections::HashMap::new();
        b.iter(|| black_box(map.get(&0).and_then(|v| v.downcast_ref::<u64>())));
    });

    group.bench_function("get_mut_hit", |b| {
        let mut map: std::collections::HashMap<usize, Box<dyn std::any::Any + Send + Sync>> =
            std::collections::HashMap::new();
        map.insert(0, Box::new(42u64));
        b.iter(|| {
            let val = map.get_mut(&0).and_then(|v| v.downcast_mut::<u64>());
            black_box(val.is_some());
        });
    });

    group.bench_function("remove_then_insert", |b| {
        let mut map: std::collections::HashMap<usize, Box<dyn std::any::Any + Send + Sync>> =
            std::collections::HashMap::new();
        map.insert(0, Box::new(42u64));
        b.iter(|| {
            drop(black_box(map.remove(&0)));
            map.insert(0, Box::new(42u64));
        });
    });

    group.finish();
}

/// Benchmark pipeline execution with and without filter state.
///
/// Both pipelines use two filters with the same body access so
/// the only behavioral difference is state insert/read.
fn bench_pipeline_state_overhead(c: &mut Criterion) {
    use praxis_filter::{FilterAction, FilterError, HttpFilterContext, http_builtin};

    fn noop_a(_: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        struct F;
        #[async_trait]
        impl HttpFilter for F {
            fn name(&self) -> &'static str {
                "bench_noop_a"
            }

            async fn on_request(&self, _ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
                Ok(FilterAction::Continue)
            }

            fn request_body_access(&self) -> praxis_filter::BodyAccess {
                praxis_filter::BodyAccess::ReadOnly
            }

            async fn on_request_body(
                &self,
                _ctx: &mut HttpFilterContext<'_>,
                _body: &mut Option<Bytes>,
                _eos: bool,
            ) -> Result<FilterAction, FilterError> {
                Ok(FilterAction::Continue)
            }
        }
        Ok(Box::new(F))
    }

    fn noop_b(_: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        struct F;
        #[async_trait]
        impl HttpFilter for F {
            fn name(&self) -> &'static str {
                "bench_noop_b"
            }

            async fn on_request(&self, _ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
                Ok(FilterAction::Continue)
            }

            fn request_body_access(&self) -> praxis_filter::BodyAccess {
                praxis_filter::BodyAccess::ReadOnly
            }

            async fn on_request_body(
                &self,
                _ctx: &mut HttpFilterContext<'_>,
                _body: &mut Option<Bytes>,
                _eos: bool,
            ) -> Result<FilterAction, FilterError> {
                Ok(FilterAction::Continue)
            }
        }
        Ok(Box::new(F))
    }

    fn stateful_writer(_: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        struct F;
        #[async_trait]
        impl HttpFilter for F {
            fn name(&self) -> &'static str {
                "bench_stateful_writer"
            }

            async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
                ctx.insert_filter_state(42u64);
                Ok(FilterAction::Continue)
            }

            fn request_body_access(&self) -> praxis_filter::BodyAccess {
                praxis_filter::BodyAccess::ReadOnly
            }

            async fn on_request_body(
                &self,
                _ctx: &mut HttpFilterContext<'_>,
                _body: &mut Option<Bytes>,
                _eos: bool,
            ) -> Result<FilterAction, FilterError> {
                Ok(FilterAction::Continue)
            }
        }
        Ok(Box::new(F))
    }

    fn stateful_reader(_: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        struct F;
        #[async_trait]
        impl HttpFilter for F {
            fn name(&self) -> &'static str {
                "bench_stateful_reader"
            }

            async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
                black_box(ctx.get_filter_state::<u64>());
                Ok(FilterAction::Continue)
            }

            fn request_body_access(&self) -> praxis_filter::BodyAccess {
                praxis_filter::BodyAccess::ReadOnly
            }

            async fn on_request_body(
                &self,
                ctx: &mut HttpFilterContext<'_>,
                _body: &mut Option<Bytes>,
                _eos: bool,
            ) -> Result<FilterAction, FilterError> {
                black_box(ctx.get_filter_state::<u64>());
                Ok(FilterAction::Continue)
            }
        }
        Ok(Box::new(F))
    }

    let rt = bench_runtime();

    let mut baseline_registry = FilterRegistry::with_builtins();
    baseline_registry
        .register("bench_noop_a", http_builtin(noop_a))
        .unwrap();
    baseline_registry
        .register("bench_noop_b", http_builtin(noop_b))
        .unwrap();

    let mut stateful_registry = FilterRegistry::with_builtins();
    stateful_registry
        .register("bench_stateful_writer", http_builtin(stateful_writer))
        .unwrap();
    stateful_registry
        .register("bench_stateful_reader", http_builtin(stateful_reader))
        .unwrap();

    let baseline = FilterPipeline::build(
        &mut [filter_entry("bench_noop_a", ""), filter_entry("bench_noop_b", "")],
        &baseline_registry,
    )
    .unwrap();

    let stateful = FilterPipeline::build(
        &mut [
            filter_entry("bench_stateful_writer", ""),
            filter_entry("bench_stateful_reader", ""),
        ],
        &stateful_registry,
    )
    .unwrap();

    let mut group = c.benchmark_group("pipeline_state_overhead");

    group.bench_function("request_baseline", |b| {
        let p = &baseline;
        b.to_async(&rt).iter_batched(
            || make_request("/"),
            |req| async move {
                let mut ctx = make_ctx(&req);
                drop(black_box(p.execute_http_request(&mut ctx).await.unwrap()));
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("request_stateful", |b| {
        let p = &stateful;
        b.to_async(&rt).iter_batched(
            || make_request("/"),
            |req| async move {
                let mut ctx = make_ctx(&req);
                drop(black_box(p.execute_http_request(&mut ctx).await.unwrap()));
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("request_plus_body_baseline", |b| {
        let p = &baseline;
        b.to_async(&rt).iter_batched(
            || make_request("/"),
            |req| async move {
                let mut ctx = make_ctx(&req);
                drop(p.execute_http_request(&mut ctx).await.unwrap());
                let mut body = Some(Bytes::from_static(b"chunk"));
                drop(black_box(
                    p.execute_http_request_body(&mut ctx, &mut body, true).await.unwrap(),
                ));
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("request_plus_body_stateful", |b| {
        let p = &stateful;
        b.to_async(&rt).iter_batched(
            || make_request("/"),
            |req| async move {
                let mut ctx = make_ctx(&req);
                drop(p.execute_http_request(&mut ctx).await.unwrap());
                let mut body = Some(Bytes::from_static(b"chunk"));
                drop(black_box(
                    p.execute_http_request_body(&mut ctx, &mut body, true).await.unwrap(),
                ));
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark the cost of pinning a pipeline vs repeated `ArcSwap` loads.
fn bench_pipeline_pinning(c: &mut Criterion) {
    use arc_swap::ArcSwap;

    let registry = FilterRegistry::with_builtins();
    let mut entries = vec![filter_entry(
        "router",
        "routes:\n  - path_prefix: /\n    cluster: default",
    )];
    let pipeline = Arc::new(FilterPipeline::build(&mut entries, &registry).unwrap());
    let swap = Arc::new(ArcSwap::from(Arc::clone(&pipeline)));

    let mut group = c.benchmark_group("pipeline_pinning");

    group.bench_function("arcswap_load_clone", |b| {
        let swap = &swap;
        b.iter(|| {
            let guard = swap.load();
            let _pinned = black_box(Arc::clone(&guard));
        });
    });

    group.bench_function("arc_clone_pinned", |b| {
        let pinned = Arc::clone(&pipeline);
        b.iter(|| {
            let _cloned = black_box(Arc::clone(&pinned));
        });
    });

    group.bench_function("arcswap_load_full", |b| {
        let swap = &swap;
        b.iter(|| {
            let _full = black_box(swap.load_full());
        });
    });

    group.finish();
}

// -----------------------------------------------------------------------------
// Benchmark Utilities
// -----------------------------------------------------------------------------

/// Build a [`FilterEntry`] from a filter type name and YAML config string.
fn filter_entry(filter_type: &str, yaml: &str) -> FilterEntry {
    FilterEntry {
        branch_chains: None,
        filter_type: filter_type.into(),
        config: serde_yaml::from_str(yaml).unwrap(),
        conditions: vec![],
        name: None,
        response_conditions: vec![],
        failure_mode: FailureMode::default(),
    }
}

/// Build a vector of `n` filter entries alternating between
/// router (even) and headers (odd).
fn make_entries(n: usize) -> Vec<FilterEntry> {
    (0..n)
        .map(|i| {
            if i % 2 == 0 {
                filter_entry("router", "routes: []")
            } else {
                filter_entry("headers", "response_add: []")
            }
        })
        .collect()
}
