#!/usr/bin/env bash
# Track B GuideLLM benchmark against llm-d-inference-sim.
#
# Runs GuideLLM concurrent profile against Track B profiles:
#   - praxis-simple (local control)
#   - praxis-go-epp
#   - envoy-go-epp
#
# Does NOT include praxis-native (Track A only).
#
# Usage:
#   ./benchmarks/llm-d/run-track-b-guidellm-sim.sh [MAX_SECONDS] [CONCURRENCY]
#
# Environment:
#   GUIDELLM_BIN         path to guidellm
#   LLM_D_SIM_BIN        path to sim binary
#   LLM_D_SIM_REPO       path to sim source
#   LLM_D_EPP_BIN        path to Go EPP binary
#   LLM_D_ROUTER_REPO    path to llm-d-router source

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RESULTS_BASE="$REPO_ROOT/target/criterion/llmd-track-b-guidellm"
LOGS_DIR="$RESULTS_BASE/logs"
MAX_SECONDS="${1:-30}"
CONCURRENCY="${2:-4}"
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
ENVOY_IMAGE="${ENVOY_IMAGE:-envoyproxy/envoy:distroless-v1.33.2}"
ENVOY_CONTAINER="${ENVOY_CONTAINER:-llmd-track-b-guidellm-envoy}"

LLM_D_SIM_BIN="${LLM_D_SIM_BIN:-}"
LLM_D_SIM_REPO="${LLM_D_SIM_REPO:-$REPO_ROOT/../../llm-d-benchmarks/repos/llm-d-inference-sim}"
LLM_D_EPP_BIN="${LLM_D_EPP_BIN:-}"
LLM_D_ROUTER_REPO="${LLM_D_ROUTER_REPO:-$REPO_ROOT/../../repos/llm-d-router}"
GUIDELLM_BIN="${GUIDELLM_BIN:-}"

SIM_PID=""
PROXY_PID=""
EPP_PID=""

cleanup() {
    local status=$?
    echo "Cleaning up..."
    if [ -n "$PROXY_PID" ]; then kill "$PROXY_PID" 2>/dev/null || true; fi
    if [ -n "$EPP_PID" ]; then kill "$EPP_PID" 2>/dev/null || true; fi
    if [ -n "$SIM_PID" ]; then kill "$SIM_PID" 2>/dev/null || true; fi
    docker rm -f "$ENVOY_CONTAINER" >/dev/null 2>&1 || true
    wait 2>/dev/null || true
    exit "$status"
}
trap cleanup EXIT

check_tool() { command -v "$1" >/dev/null 2>&1 || { echo "error: $1 not found"; exit 1; }; }
assert_port_free() {
    if ss -tlnH "sport = :$1" 2>/dev/null | grep -q LISTEN; then
        echo "error: port $1 in use"; exit 1
    fi
}
wait_ready() {
    local label="$1" url="$2" pid="$3" timeout="${4:-60}"
    for _ in $(seq 1 "$timeout"); do
        if ! kill -0 "$pid" 2>/dev/null; then echo "error: $label exited"; return 1; fi
        if curl -sf "$url" >/dev/null 2>&1; then return 0; fi; sleep 0.2
    done; echo "error: $label not ready"; return 1
}
wait_http_ready() {
    local label="$1" url="$2" timeout="${3:-60}"
    for _ in $(seq 1 "$timeout"); do if curl -sf "$url" >/dev/null 2>&1; then return 0; fi; sleep 0.2; done
    echo "error: $label not ready"; return 1
}
stop_praxis() { if [ -n "$PROXY_PID" ]; then kill "$PROXY_PID" 2>/dev/null || true; wait "$PROXY_PID" 2>/dev/null || true; PROXY_PID=""; fi; }
stop_epp() { if [ -n "$EPP_PID" ]; then kill "$EPP_PID" 2>/dev/null || true; wait "$EPP_PID" 2>/dev/null || true; EPP_PID=""; fi; }
stop_envoy() { docker rm -f "$ENVOY_CONTAINER" >/dev/null 2>&1 || true; }

preflight() {
    local url="$1" name="$2"
    local status
    status=$(curl -sf -o /dev/null -w "%{http_code}" -X POST "$url/v1/chat/completions" \
        -H "Content-Type: application/json" \
        -d "{\"model\":\"$MODEL_NAME\",\"messages\":[{\"role\":\"user\",\"content\":\"ping\"}],\"max_tokens\":5}" 2>/dev/null) || status="000"
    if [ "$status" = "200" ]; then echo "  Preflight $name: OK"
    else echo "  Preflight $name: FAILED (HTTP $status)"; return 1; fi
}

run_guidellm() {
    local profile_name="$1" target_url="$2"
    local out_dir="$RESULTS_BASE/${profile_name}"
    local log_file="$LOGS_DIR/${profile_name}-guidellm.log"
    mkdir -p "$out_dir"

    echo "  GuideLLM concurrent (concurrency=$CONCURRENCY, ${MAX_SECONDS}s)..."

    "$GUIDELLM_BIN" benchmark run \
        --target="$target_url" \
        --model="$MODEL_NAME" \
        --data="$DATA_FILE" \
        --backend-kwargs '{"validate_backend": false}' \
        --profile=concurrent \
        --rate="$CONCURRENCY" \
        --max-seconds="$MAX_SECONDS" \
        --output-dir="$out_dir" \
        --outputs=benchmark-results.json \
        --disable-console-interactive \
        >"$log_file" 2>&1

    if [ -f "$out_dir/benchmark-results.json" ]; then
        echo "    OK: $out_dir/benchmark-results.json"
    else
        echo "    FAILED (see $log_file)"
        tail -10 "$log_file"
        return 1
    fi
}

# --- Preflight ---
check_tool docker; check_tool curl
for port in "$BACKEND_PORT" "$PROXY_PORT" "$ENVOY_PORT" "$ADMIN_PORT" "$EPP_GRPC_PORT" "$EPP_HEALTH_PORT" "$EPP_METRICS_PORT" "$ENVOY_ADMIN_PORT"; do
    assert_port_free "$port"
done

# --- Resolve binaries ---
if [ -z "$GUIDELLM_BIN" ]; then
    if command -v guidellm &>/dev/null; then GUIDELLM_BIN="guidellm"
    elif [ -x /tmp/guidellm-venv/bin/guidellm ]; then GUIDELLM_BIN="/tmp/guidellm-venv/bin/guidellm"
    else echo "error: guidellm not found"; exit 1; fi
fi
if [ -z "$LLM_D_SIM_BIN" ]; then
    if [ -x "$LLM_D_SIM_REPO/bin/llm-d-inference-sim" ]; then LLM_D_SIM_BIN="$LLM_D_SIM_REPO/bin/llm-d-inference-sim"
    else echo "error: sim not found"; exit 1; fi
fi
if [ -z "$LLM_D_EPP_BIN" ]; then
    if [ -x /tmp/epp ]; then LLM_D_EPP_BIN="/tmp/epp"
    elif [ -d "$LLM_D_ROUTER_REPO" ]; then (cd "$LLM_D_ROUTER_REPO" && go build -o /tmp/epp ./cmd/epp); LLM_D_EPP_BIN="/tmp/epp"
    else echo "error: no EPP"; exit 1; fi
fi

PRAXIS_BIN="$REPO_ROOT/target/release/praxis"
if [ ! -x "$PRAXIS_BIN" ]; then echo "error: Praxis binary not found"; exit 1; fi

echo "=== Track B GuideLLM Simulator Benchmark ==="
echo "Duration: ${MAX_SECONDS}s  Concurrency: $CONCURRENCY"
echo "GuideLLM: $GUIDELLM_BIN ($($GUIDELLM_BIN --version 2>&1 | head -1))"
echo ""

mkdir -p "$RESULTS_BASE" "$LOGS_DIR"

# --- Start simulator ---
echo "Starting llm-d-inference-sim..."
"$LLM_D_SIM_BIN" --model "$MODEL_NAME" --served-model-name "$MODEL_NAME" \
    --port "$BACKEND_PORT" --logtostderr=true >"$LOGS_DIR/sim.log" 2>&1 &
SIM_PID=$!
wait_ready "simulator" "http://127.0.0.1:$BACKEND_PORT/health" "$SIM_PID" 30
echo "Simulator ready"

# ====================== PRAXIS-SIMPLE ======================
echo ""; echo "=== praxis-simple ==="
PRAXIS_CONFIG="$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-simple.yaml" \
    "$PRAXIS_BIN" >"$LOGS_DIR/praxis-simple.log" 2>&1 &
PROXY_PID=$!
wait_ready "Praxis simple" "http://127.0.0.1:$ADMIN_PORT/ready" "$PROXY_PID" 60
preflight "http://127.0.0.1:$PROXY_PORT" "praxis-simple"
run_guidellm "praxis-simple" "http://127.0.0.1:$PROXY_PORT"
stop_praxis; sleep 1

# ====================== PRAXIS-GO-EPP ======================
echo ""; echo "=== praxis-go-epp ==="
EPP_TMPDIR=$(mktemp -d)
sed "s|PLACEHOLDER_ENDPOINTS_PATH|$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-endpoints.yaml|" \
    "$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-config.yaml" > "$EPP_TMPDIR/epp-config.yaml"
"$LLM_D_EPP_BIN" --pool-name bench-pool --config-file "$EPP_TMPDIR/epp-config.yaml" \
    --grpc-port "$EPP_GRPC_PORT" --grpc-health-port "$EPP_HEALTH_PORT" --metrics-port "$EPP_METRICS_PORT" \
    --secure-serving=false --health-checking=false --tracing=false --metrics-endpoint-auth=false \
    >"$LOGS_DIR/praxis-go-epp-epp.log" 2>&1 &
EPP_PID=$!
wait_ready "Go EPP" "http://127.0.0.1:$EPP_METRICS_PORT/metrics" "$EPP_PID" 60
PRAXIS_CONFIG="$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-go-epp.yaml" \
    "$PRAXIS_BIN" >"$LOGS_DIR/praxis-go-epp.log" 2>&1 &
PROXY_PID=$!
wait_ready "Praxis go-epp" "http://127.0.0.1:$ADMIN_PORT/ready" "$PROXY_PID" 60
preflight "http://127.0.0.1:$PROXY_PORT" "praxis-go-epp"
run_guidellm "praxis-go-epp" "http://127.0.0.1:$PROXY_PORT"
stop_praxis; stop_epp; rm -rf "$EPP_TMPDIR"; sleep 1

# ====================== ENVOY + GO EPP ======================
echo ""; echo "=== envoy-go-epp ==="
EPP_TMPDIR=$(mktemp -d)
sed "s|PLACEHOLDER_ENDPOINTS_PATH|$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-endpoints.yaml|" \
    "$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-config.yaml" > "$EPP_TMPDIR/epp-config.yaml"
"$LLM_D_EPP_BIN" --pool-name bench-pool --config-file "$EPP_TMPDIR/epp-config.yaml" \
    --grpc-port "$EPP_GRPC_PORT" --grpc-health-port "$EPP_HEALTH_PORT" --metrics-port "$EPP_METRICS_PORT" \
    --secure-serving=false --health-checking=false --tracing=false --metrics-endpoint-auth=false \
    >"$LOGS_DIR/envoy-go-epp-epp.log" 2>&1 &
EPP_PID=$!
wait_ready "Go EPP" "http://127.0.0.1:$EPP_METRICS_PORT/metrics" "$EPP_PID" 60
docker rm -f "$ENVOY_CONTAINER" >/dev/null 2>&1 || true
docker run --rm -d --name "$ENVOY_CONTAINER" --network host \
    -v "$REPO_ROOT/benchmarks/comparison/configs/llmd/envoy-go-epp.yaml:/etc/envoy/envoy.yaml:ro" \
    "$ENVOY_IMAGE" -c /etc/envoy/envoy.yaml --log-level warn >"$LOGS_DIR/envoy-start.log" 2>&1
wait_http_ready "Envoy" "http://127.0.0.1:$ENVOY_ADMIN_PORT/ready" 60
preflight "http://127.0.0.1:$ENVOY_PORT" "envoy-go-epp"
run_guidellm "envoy-go-epp" "http://127.0.0.1:$ENVOY_PORT"
stop_envoy; stop_epp; rm -rf "$EPP_TMPDIR"; sleep 1

echo ""; echo "=== Track B GuideLLM Complete ==="
echo "Results: $RESULTS_BASE/"
