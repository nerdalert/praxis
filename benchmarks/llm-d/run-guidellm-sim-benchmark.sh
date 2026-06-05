#!/usr/bin/env bash
# GuideLLM benchmark against llm-d-inference-sim through all three profiles.
#
# Runs GuideLLM with multiple traffic profiles (concurrent, constant, poisson,
# sweep) against praxis-simple, praxis-native, and envoy-go-epp.
#
# Usage:
#   ./benchmarks/llm-d/run-guidellm-sim-benchmark.sh [MAX_SECONDS]
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
#   GUIDELLM_PROFILES    space-separated list of GuideLLM profiles to run
#                         (default: "concurrent constant poisson sweep")
#   CONCURRENCY_RATES    rates for concurrent profile (default: "1,4,16,32")
#   CONSTANT_RATE        rate for constant profile (default: 500)
#   POISSON_RATE         rate for poisson profile (default: 500)
#   SWEEP_STEPS          steps for sweep profile (default: 5)
#
# Prerequisites:
#   - guidellm (pip install guidellm)
#   - docker (for Envoy)
#   - llm-d-inference-sim binary or source
#   - Go EPP binary or llm-d-router source
#   - Pre-built target/release/praxis
#
# No proxy config changes are needed. GuideLLM runs with:
#   --backend-kwargs '{"validate_backend": false}'
#   --model test-model (explicit, prevents /v1/models call)
#   --data (JSON prompt file, no HuggingFace tokenizer needed)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RESULTS_BASE="$REPO_ROOT/target/criterion/llmd-guidellm"
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

GUIDELLM_PROFILES="${GUIDELLM_PROFILES:-concurrent constant poisson sweep}"
CONCURRENCY_RATES="${CONCURRENCY_RATES:-1,4,16,32}"
CONSTANT_RATE="${CONSTANT_RATE:-500}"
POISSON_RATE="${POISSON_RATE:-500}"
SWEEP_STEPS="${SWEEP_STEPS:-5}"

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
resolve_bin() {
    local name="$1" cached="$2" repo="$3" build_cmd="$4" var_ref="$5"
    if [ -n "${!var_ref}" ]; then return; fi
    if [ -x "$cached" ]; then eval "$var_ref=$cached"; return; fi
    if [ -d "$repo" ]; then
        echo "Building $name..."
        eval "$build_cmd"
        eval "$var_ref=$cached"
    else
        echo "error: no $name binary. Set $var_ref or provide repo."
        exit 1
    fi
}

if [ -z "$GUIDELLM_BIN" ]; then
    if command -v guidellm &>/dev/null; then GUIDELLM_BIN="guidellm"
    elif [ -x /tmp/guidellm-venv/bin/guidellm ]; then GUIDELLM_BIN="/tmp/guidellm-venv/bin/guidellm"
    else echo "error: guidellm not found. pip install guidellm or set GUIDELLM_BIN."; exit 1; fi
fi

resolve_bin "llm-d-inference-sim" "/tmp/llm-d-inference-sim" "$LLM_D_SIM_REPO" \
    "(cd $LLM_D_SIM_REPO && make build 2>&1 | tail -3) && cp $LLM_D_SIM_REPO/bin/llm-d-inference-sim /tmp/" \
    "LLM_D_SIM_BIN"
resolve_bin "Go EPP" "/tmp/epp" "$LLM_D_ROUTER_REPO" \
    "(cd $LLM_D_ROUTER_REPO && go build -o /tmp/epp ./cmd/epp 2>&1 | tail -3)" \
    "LLM_D_EPP_BIN"

echo "=== GuideLLM Simulator Benchmark ==="
echo "Duration: ${MAX_SECONDS}s per benchmark"
echo "GuideLLM profiles: ${GUIDELLM_PROFILES}"
echo "GuideLLM: $GUIDELLM_BIN"
echo ""

mkdir -p "$RESULTS_BASE" "$LOGS_DIR"

# --- Start simulator ---
echo "Starting llm-d-inference-sim..."
"$LLM_D_SIM_BIN" --model "$MODEL_NAME" --port "$BACKEND_PORT" --mode echo \
    --max-num-seqs 256 >"$LOGS_DIR/simulator.log" 2>&1 &
SIM_PID=$!
for _ in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:$BACKEND_PORT/health" >/dev/null 2>&1; then break; fi
    sleep 0.2
done
echo "Simulator ready"

# --- Build Praxis ---
if [ ! -x "$REPO_ROOT/target/release/praxis" ]; then
    echo "error: Praxis binary not found at target/release/praxis. Build it first."
    exit 1
fi

# --- GuideLLM runner ---
run_guidellm() {
    local profile_name="$1" target_url="$2" guidellm_kind="$3" rate_arg="$4"
    local out_dir="$RESULTS_BASE/${profile_name}/${guidellm_kind}"
    local log_file="$LOGS_DIR/${profile_name}-${guidellm_kind}.log"
    mkdir -p "$out_dir"

    echo "  GuideLLM $guidellm_kind (rate=$rate_arg, ${MAX_SECONDS}s)..."

    local rate_flags=()
    if [ -n "$rate_arg" ]; then rate_flags=(--rate "$rate_arg"); fi

    "$GUIDELLM_BIN" benchmark run \
        --target="$target_url" \
        --model="$MODEL_NAME" \
        --data="$DATA_FILE" \
        --backend-kwargs '{"validate_backend": false}' \
        --profile="$guidellm_kind" \
        "${rate_flags[@]}" \
        --max-seconds="$MAX_SECONDS" \
        --output-dir="$out_dir" \
        --outputs=benchmark-results.json \
        --disable-console-interactive \
        >"$log_file" 2>&1

    if [ -f "$out_dir/benchmark-results.json" ]; then
        echo "    OK: $out_dir/benchmark-results.json"
    else
        echo "    FAILED (see $log_file)"
    fi
}

run_all_guidellm_profiles() {
    local profile_name="$1" target_url="$2"
    for kind in $GUIDELLM_PROFILES; do
        case "$kind" in
            concurrent) run_guidellm "$profile_name" "$target_url" "concurrent" "$CONCURRENCY_RATES" ;;
            constant)   run_guidellm "$profile_name" "$target_url" "constant" "$CONSTANT_RATE" ;;
            poisson)    run_guidellm "$profile_name" "$target_url" "poisson" "$POISSON_RATE" ;;
            sweep)      run_guidellm "$profile_name" "$target_url" "sweep" "$SWEEP_STEPS" ;;
            throughput) run_guidellm "$profile_name" "$target_url" "throughput" "$CONSTANT_RATE" ;;
            synchronous) run_guidellm "$profile_name" "$target_url" "synchronous" "" ;;
            *) echo "  Skipping unknown profile: $kind" ;;
        esac
    done
}

preflight() {
    local url="$1" name="$2"
    local status
    status=$(curl -sf -o /dev/null -w "%{http_code}" -X POST "$url/v1/chat/completions" \
        -H "Content-Type: application/json" \
        -d "{\"model\":\"$MODEL_NAME\",\"messages\":[{\"role\":\"user\",\"content\":\"ping\"}],\"max_tokens\":5}" 2>/dev/null) || status="000"
    if [ "$status" = "200" ]; then
        echo "  Preflight $name: OK"
    else
        echo "  Preflight $name: FAILED (HTTP $status)"
        return 1
    fi
}

# ====================== PRAXIS-SIMPLE ======================
echo ""
echo "=== praxis-simple ==="
PRAXIS_CONFIG="$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-simple.yaml" \
    "$REPO_ROOT/target/release/praxis" >"$LOGS_DIR/praxis-simple.log" 2>&1 &
PROXY_PID=$!
for _ in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:$ADMIN_PORT/ready" >/dev/null 2>&1; then break; fi; sleep 0.2
done
preflight "http://127.0.0.1:$PROXY_PORT" "praxis-simple"
run_all_guidellm_profiles "praxis-simple" "http://127.0.0.1:$PROXY_PORT"
kill "$PROXY_PID" 2>/dev/null; wait "$PROXY_PID" 2>/dev/null || true; unset PROXY_PID
sleep 1

# ====================== PRAXIS-NATIVE ======================
echo ""
echo "=== praxis-native ==="
PRAXIS_CONFIG="$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-native.yaml" \
    "$REPO_ROOT/target/release/praxis" >"$LOGS_DIR/praxis-native.log" 2>&1 &
PROXY_PID=$!
for _ in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:$ADMIN_PORT/ready" >/dev/null 2>&1; then break; fi; sleep 0.2
done
preflight "http://127.0.0.1:$PROXY_PORT" "praxis-native"
run_all_guidellm_profiles "praxis-native" "http://127.0.0.1:$PROXY_PORT"
kill "$PROXY_PID" 2>/dev/null; wait "$PROXY_PID" 2>/dev/null || true; unset PROXY_PID
sleep 1

# ====================== ENVOY + GO EPP ======================
echo ""
echo "=== envoy-go-epp ==="
EPP_TMPDIR=$(mktemp -d)
ENDPOINTS_ABS="$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-endpoints.yaml"
sed "s|PLACEHOLDER_ENDPOINTS_PATH|$ENDPOINTS_ABS|" \
    "$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-config.yaml" > "$EPP_TMPDIR/epp-config.yaml"

"$LLM_D_EPP_BIN" --pool-name bench-pool --config-file "$EPP_TMPDIR/epp-config.yaml" \
    --grpc-port "$EPP_GRPC_PORT" --grpc-health-port "$EPP_HEALTH_PORT" --metrics-port "$EPP_METRICS_PORT" \
    --secure-serving=false --health-checking=false --tracing=false --metrics-endpoint-auth=false \
    --grpc-max-recv-msg-size 10MiB --grpc-max-send-msg-size 10MiB \
    >"$LOGS_DIR/go-epp.log" 2>&1 &
EPP_PID=$!
for _ in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:$EPP_METRICS_PORT/metrics" >/dev/null 2>&1; then break; fi
    if ! kill -0 "$EPP_PID" 2>/dev/null; then echo "error: EPP exited"; tail -5 "$LOGS_DIR/go-epp.log"; exit 1; fi
    sleep 0.2
done

docker rm -f "$ENVOY_CONTAINER" 2>/dev/null || true
ENVOY_CFG="$REPO_ROOT/benchmarks/comparison/configs/llmd/envoy-go-epp.yaml"
docker run --rm -d --name "$ENVOY_CONTAINER" --network host \
    -v "$ENVOY_CFG:/etc/envoy/envoy.yaml:ro" "$ENVOY_IMAGE" \
    -c /etc/envoy/envoy.yaml --log-level warn >/dev/null 2>&1
for _ in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:$ENVOY_ADMIN_PORT/ready" >/dev/null 2>&1; then break; fi; sleep 0.2
done
docker logs -f "$ENVOY_CONTAINER" >"$LOGS_DIR/envoy.log" 2>&1 &

preflight "http://127.0.0.1:$ENVOY_PORT" "envoy-go-epp"
run_all_guidellm_profiles "envoy-go-epp" "http://127.0.0.1:$ENVOY_PORT"

rm -rf "$EPP_TMPDIR"

# ====================== SUMMARY ======================
echo ""
echo "=== Summary ==="
echo ""
find "$RESULTS_BASE" -name "benchmark-results.json" -type f | sort | while read -r f; do
    rel="${f#"$RESULTS_BASE/"}"
    size=$(wc -c < "$f")
    echo "  $rel ($size bytes)"
done
echo ""
echo "Results: $RESULTS_BASE/"
echo "Logs: $LOGS_DIR/"
echo "=== GuideLLM Benchmark Complete ==="
