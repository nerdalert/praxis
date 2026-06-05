#!/usr/bin/env bash
# GuideLLM benchmark matrix: profiles x traffic patterns x stream modes.
#
# Runs a matrix of GuideLLM benchmarks against all three llm-d profiles
# using llm-d-inference-sim echo backend.
#
# Usage:
#   ./benchmarks/llm-d/run-guidellm-matrix.sh [MAX_SECONDS]
#
# Positional arguments:
#   MAX_SECONDS  per-benchmark duration (default: 30)
#
# Environment:
#   GUIDELLM_BIN         path to guidellm binary
#   LLM_D_SIM_BIN        path to llm-d-inference-sim binary
#   LLM_D_SIM_REPO       path to llm-d-inference-sim source
#   LLM_D_EPP_BIN        path to Go EPP binary
#   LLM_D_ROUTER_REPO    path to llm-d-router source
#   BENCHMARK_PROFILES   space-separated llm-d profiles (default: all three)
#   GUIDELLM_KINDS       space-separated GuideLLM traffic kinds to run
#                         (default: "concurrent-stream concurrent-nostream sweep")
#
# No proxy config changes are made. GuideLLM runs with:
#   --backend-kwargs '{"validate_backend": false}'  (or stream: false variant)
#   --model test-model
#   --data benchmarks/llm-d/data/guidellm-prompts.json

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RESULTS_BASE="$REPO_ROOT/target/criterion/llmd-guidellm-matrix"
LOGS_DIR="$RESULTS_BASE/logs"
MAX_SECONDS="${1:-30}"
MODEL_NAME="test-model"
DATA_FILE="$REPO_ROOT/benchmarks/llm-d/data/guidellm-prompts.json"

BACKEND_PORT=18080
PROXY_PORT=18090
ENVOY_PORT=18091
ADMIN_PORT=9901
EPP_GRPC_PORT=9002
EPP_HEALTH_PORT=9003
EPP_METRICS_PORT=9090
ENVOY_ADMIN_PORT=19000
ENVOY_IMAGE="envoyproxy/envoy:distroless-v1.33.2"
ENVOY_CONTAINER="llmd-bench-envoy"

BENCHMARK_PROFILES="${BENCHMARK_PROFILES:-praxis-simple praxis-native envoy-go-epp}"
GUIDELLM_KINDS="${GUIDELLM_KINDS:-concurrent-stream concurrent-nostream constant poisson sweep}"

LLM_D_SIM_BIN="${LLM_D_SIM_BIN:-}"
LLM_D_SIM_REPO="${LLM_D_SIM_REPO:-$REPO_ROOT/../../repos/llm-d-inference-sim}"
LLM_D_EPP_BIN="${LLM_D_EPP_BIN:-}"
LLM_D_ROUTER_REPO="${LLM_D_ROUTER_REPO:-$REPO_ROOT/../../repos/llm-d-router}"
GUIDELLM_BIN="${GUIDELLM_BIN:-}"

cleanup() {
    if [ -n "${SIM_PID:-}" ]; then kill "$SIM_PID" 2>/dev/null || true; fi
    if [ -n "${PROXY_PID:-}" ]; then kill "$PROXY_PID" 2>/dev/null || true; fi
    if [ -n "${EPP_PID:-}" ]; then kill "$EPP_PID" 2>/dev/null || true; fi
    docker rm -f "$ENVOY_CONTAINER" 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT

# --- Resolve binaries ---
if [ -z "$GUIDELLM_BIN" ]; then
    if command -v guidellm &>/dev/null; then GUIDELLM_BIN="guidellm"
    elif [ -x /tmp/guidellm-venv/bin/guidellm ]; then GUIDELLM_BIN="/tmp/guidellm-venv/bin/guidellm"
    else echo "error: guidellm not found."; exit 1; fi
fi
if [ -z "$LLM_D_SIM_BIN" ]; then
    if [ -x /tmp/llm-d-inference-sim ]; then LLM_D_SIM_BIN="/tmp/llm-d-inference-sim"
    elif [ -d "$LLM_D_SIM_REPO" ]; then
        (cd "$LLM_D_SIM_REPO" && make build 2>&1 | tail -2)
        cp "$LLM_D_SIM_REPO/bin/llm-d-inference-sim" /tmp/; LLM_D_SIM_BIN="/tmp/llm-d-inference-sim"
    else echo "error: no sim binary."; exit 1; fi
fi
if [ -z "$LLM_D_EPP_BIN" ]; then
    if [ -x /tmp/epp ]; then LLM_D_EPP_BIN="/tmp/epp"
    elif [ -d "$LLM_D_ROUTER_REPO" ]; then
        (cd "$LLM_D_ROUTER_REPO" && go build -o /tmp/epp ./cmd/epp 2>&1 | tail -2)
        LLM_D_EPP_BIN="/tmp/epp"
    else echo "error: no EPP binary."; exit 1; fi
fi

echo "=== GuideLLM Benchmark Matrix ==="
echo "Duration: ${MAX_SECONDS}s per benchmark"
echo "Profiles: ${BENCHMARK_PROFILES}"
echo "Kinds: ${GUIDELLM_KINDS}"
echo ""

mkdir -p "$RESULTS_BASE" "$LOGS_DIR"

# --- Start simulator ---
"$LLM_D_SIM_BIN" --model "$MODEL_NAME" --port "$BACKEND_PORT" --mode echo \
    --max-num-seqs 256 >"$LOGS_DIR/simulator.log" 2>&1 &
SIM_PID=$!
for _ in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:$BACKEND_PORT/health" >/dev/null 2>&1; then break; fi; sleep 0.2
done
echo "Simulator ready"

if [ ! -x "$REPO_ROOT/target/release/praxis" ]; then
    echo "error: build target/release/praxis first."; exit 1
fi

# --- GuideLLM runner ---
run_one() {
    local bench_profile="$1" target_url="$2" kind="$3"
    local out_dir="$RESULTS_BASE/${bench_profile}/${kind}"
    local log_file="$LOGS_DIR/${bench_profile}-${kind}.log"
    mkdir -p "$out_dir"

    # Parse kind into GuideLLM flags.
    local guidellm_profile="" rate_arg="" backend_kwargs='{"validate_backend": false}'
    case "$kind" in
        concurrent-stream)
            guidellm_profile="concurrent"; rate_arg="${CONCURRENCY_RATES:-4,16}" ;;
        concurrent-nostream)
            guidellm_profile="concurrent"; rate_arg="${CONCURRENCY_RATES:-4,16}"
            backend_kwargs='{"validate_backend": false, "stream": false}' ;;
        constant)
            guidellm_profile="constant"; rate_arg="500" ;;
        poisson)
            guidellm_profile="poisson"; rate_arg="500" ;;
        throughput)
            guidellm_profile="throughput"; rate_arg="1000" ;;
        sweep)
            guidellm_profile="sweep"; rate_arg="5" ;;
        *)
            echo "    Unknown kind: $kind"; return 1 ;;
    esac

    echo "  $kind ($guidellm_profile, rate=$rate_arg)..."

    local rate_flags=()
    if [ -n "$rate_arg" ]; then rate_flags=(--rate "$rate_arg"); fi

    "$GUIDELLM_BIN" benchmark run \
        --target="$target_url" \
        --model="$MODEL_NAME" \
        --data="$DATA_FILE" \
        --backend-kwargs "$backend_kwargs" \
        --profile="$guidellm_profile" \
        "${rate_flags[@]}" \
        --max-seconds="$MAX_SECONDS" \
        --output-dir="$out_dir" \
        --outputs=benchmark-results.json \
        --disable-console-interactive \
        >"$log_file" 2>&1

    if [ -f "$out_dir/benchmark-results.json" ]; then
        echo "    OK"
    else
        echo "    FAILED (see $log_file)"
    fi
}

preflight() {
    local url="$1"
    local status
    status=$(curl -sf -o /dev/null -w "%{http_code}" -X POST "$url/v1/chat/completions" \
        -H "Content-Type: application/json" \
        -d "{\"model\":\"$MODEL_NAME\",\"messages\":[{\"role\":\"user\",\"content\":\"ping\"}],\"max_tokens\":5}" 2>/dev/null) || status="000"
    if [ "$status" != "200" ]; then echo "  Preflight FAILED ($status)"; return 1; fi
}

start_praxis_profile() {
    local config="$1"
    PRAXIS_CONFIG="$config" "$REPO_ROOT/target/release/praxis" >"$LOGS_DIR/praxis-current.log" 2>&1 &
    PROXY_PID=$!
    for _ in $(seq 1 30); do
        if curl -sf "http://127.0.0.1:$ADMIN_PORT/ready" >/dev/null 2>&1; then break; fi; sleep 0.2
    done
}

stop_praxis() {
    if [ -n "${PROXY_PID:-}" ]; then
        kill "$PROXY_PID" 2>/dev/null || true; wait "$PROXY_PID" 2>/dev/null || true; unset PROXY_PID
    fi
    sleep 1
}

start_envoy_epp() {
    local epp_tmp
    epp_tmp=$(mktemp -d)
    ENDPOINTS_ABS="$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-endpoints.yaml"
    sed "s|PLACEHOLDER_ENDPOINTS_PATH|$ENDPOINTS_ABS|" \
        "$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-config.yaml" > "$epp_tmp/epp-config.yaml"

    "$LLM_D_EPP_BIN" --pool-name bench --config-file "$epp_tmp/epp-config.yaml" \
        --grpc-port "$EPP_GRPC_PORT" --grpc-health-port "$EPP_HEALTH_PORT" --metrics-port "$EPP_METRICS_PORT" \
        --secure-serving=false --health-checking=false --tracing=false --metrics-endpoint-auth=false \
        --grpc-max-recv-msg-size 10MiB --grpc-max-send-msg-size 10MiB \
        >"$LOGS_DIR/go-epp.log" 2>&1 &
    EPP_PID=$!
    for _ in $(seq 1 30); do
        if curl -sf "http://127.0.0.1:$EPP_METRICS_PORT/metrics" >/dev/null 2>&1; then break; fi
        if ! kill -0 "$EPP_PID" 2>/dev/null; then echo "EPP failed"; return 1; fi; sleep 0.2
    done

    docker rm -f "$ENVOY_CONTAINER" 2>/dev/null || true
    docker run --rm -d --name "$ENVOY_CONTAINER" --network host \
        -v "$REPO_ROOT/benchmarks/comparison/configs/llmd/envoy-go-epp.yaml:/etc/envoy/envoy.yaml:ro" \
        "$ENVOY_IMAGE" -c /etc/envoy/envoy.yaml --log-level warn >/dev/null 2>&1
    for _ in $(seq 1 30); do
        if curl -sf "http://127.0.0.1:$ENVOY_ADMIN_PORT/ready" >/dev/null 2>&1; then break; fi; sleep 0.2
    done
    docker logs -f "$ENVOY_CONTAINER" >"$LOGS_DIR/envoy.log" 2>&1 &

    rm -rf "$epp_tmp"
}

stop_envoy_epp() {
    if [ -n "${EPP_PID:-}" ]; then kill "$EPP_PID" 2>/dev/null || true; wait "$EPP_PID" 2>/dev/null || true; unset EPP_PID; fi
    docker rm -f "$ENVOY_CONTAINER" 2>/dev/null || true
    sleep 1
}

# ====================== RUN MATRIX ======================
for bench_profile in $BENCHMARK_PROFILES; do
    echo ""
    echo "=== $bench_profile ==="

    case "$bench_profile" in
        praxis-simple)
            start_praxis_profile "$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-simple.yaml"
            TARGET="http://127.0.0.1:$PROXY_PORT"
            ;;
        praxis-native)
            start_praxis_profile "$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-native.yaml"
            TARGET="http://127.0.0.1:$PROXY_PORT"
            ;;
        envoy-go-epp)
            start_envoy_epp
            TARGET="http://127.0.0.1:$ENVOY_PORT"
            ;;
        *) echo "Unknown profile: $bench_profile"; continue ;;
    esac

    preflight "$TARGET" "$bench_profile" || { stop_praxis; stop_envoy_epp; continue; }

    for kind in $GUIDELLM_KINDS; do
        run_one "$bench_profile" "$TARGET" "$kind"
    done

    case "$bench_profile" in
        praxis-simple|praxis-native) stop_praxis ;;
        envoy-go-epp) stop_envoy_epp ;;
    esac
done

# ====================== SUMMARY ======================
echo ""
echo "=== Matrix Summary ==="
echo ""

RESULTS_BASE_FOR_SUMMARY="$RESULTS_BASE" python3 << 'PYEOF'
import json, os

base = os.environ.get("RESULTS_BASE_FOR_SUMMARY", "")
if not base:
    import sys
    print("(no RESULTS_BASE_FOR_SUMMARY set)")
    sys.exit(0)

# Walk all result files and build rows.
rows = []
for dirpath, _, filenames in os.walk(base):
    for fn in filenames:
        if fn != "benchmark-results.json":
            continue
        fpath = os.path.join(dirpath, fn)
        rel = os.path.relpath(dirpath, base)
        parts = rel.split(os.sep)
        if len(parts) < 2:
            continue
        profile, kind = parts[0], parts[1]
        stream = "yes" if "nostream" not in kind else "no"

        with open(fpath) as f:
            d = json.load(f)
        for b in d.get("benchmarks", []):
            m = b["metrics"]
            rps = m["requests_per_second"]["successful"]["mean"]
            ttft_val = m["time_to_first_token_ms"]["successful"]["median"]
            ttft = f"{ttft_val:.2f}ms" if stream == "yes" else "n/a"
            lat = m["request_latency"]["successful"]["median"] * 1000
            reqs = m["request_totals"]["successful"]
            cfg = b.get("config", {})
            strat = cfg.get("strategy", {})
            conc = strat.get("max_concurrency", strat.get("rate", "?"))
            rows.append((profile, stream, conc, rps, ttft, lat, reqs))

# Sort: profile, stream=yes first, concurrency.
order = {"praxis-simple": 0, "praxis-native": 1, "envoy-go-epp": 2}
rows.sort(key=lambda r: (order.get(r[0], 9), r[1] == "no", r[2]))

hdr = f"{'Profile':>16s}  {'Stream':>6s}  {'Conc':>4s}  {'RPS':>6s}  {'TTFT':>8s}  {'E2E':>8s}  {'Reqs':>5s}"
print(hdr)
print("-" * len(hdr))
for profile, stream, conc, rps, ttft, lat, reqs in rows:
    print(f"{profile:>16s}  {stream:>6s}  {conc:>4}  {rps:6.0f}  {ttft:>8s}  {lat:7.1f}ms  {reqs:5d}")
PYEOF

echo ""
echo "Results: $RESULTS_BASE/"
echo "=== Matrix Complete ==="
