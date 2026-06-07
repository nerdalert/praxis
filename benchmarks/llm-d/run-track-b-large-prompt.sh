#!/usr/bin/env bash
# Track B large-prompt benchmark with llm-d-inference-sim.
#
# Tests body-handling overhead as prompt size grows.
# Sizes match Track A: 16 KiB, 64 KiB, 256 KiB.
#
# Usage:
#   ./benchmarks/llm-d/run-track-b-large-prompt.sh [DURATION] [WARMUP] [RUNS]
#
# Environment:
#   PROMPT_SIZES  space-separated sizes in bytes (default: "16384 65536 262144")

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RESULTS_DIR="$REPO_ROOT/target/criterion/llmd-track-b-large-prompt"
LOGS_DIR="$RESULTS_DIR/logs"
DURATION="${1:-30}"
WARMUP="${2:-5}"
RUNS="${3:-3}"
PROMPT_SIZES="${PROMPT_SIZES:-16384 65536 262144}"

BACKEND_PORT=18080
PROXY_PORT=18090
ENVOY_PORT=18091
ADMIN_PORT=9901
EPP_GRPC_PORT=9002
EPP_HEALTH_PORT=9003
EPP_METRICS_PORT=9090
ENVOY_ADMIN_PORT=19000
MODEL_NAME="test-model"

ENVOY_IMAGE="${ENVOY_IMAGE:-envoyproxy/envoy:distroless-v1.33.2}"
ENVOY_CONTAINER="${ENVOY_CONTAINER:-llmd-track-b-lp-envoy}"
LLM_D_SIM_BIN="${LLM_D_SIM_BIN:-}"
LLM_D_SIM_REPO="${LLM_D_SIM_REPO:-$REPO_ROOT/../../llm-d-benchmarks/repos/llm-d-inference-sim}"
LLM_D_EPP_BIN="${LLM_D_EPP_BIN:-}"
LLM_D_ROUTER_REPO="${LLM_D_ROUTER_REPO:-$REPO_ROOT/../../repos/llm-d-router}"

SIM_PID=""
PROXY_PID=""
EPP_PID=""
DOCKER_LOG_PID=""
BENCH_TMPDIR=""
EPP_TMPDIR=""

cleanup() {
    local status=$?
    echo "Cleaning up..."
    if [ -n "$DOCKER_LOG_PID" ]; then kill "$DOCKER_LOG_PID" 2>/dev/null || true; fi
    if [ -n "$PROXY_PID" ]; then kill "$PROXY_PID" 2>/dev/null || true; fi
    if [ -n "$EPP_PID" ]; then kill "$EPP_PID" 2>/dev/null || true; fi
    if [ -n "$SIM_PID" ]; then kill "$SIM_PID" 2>/dev/null || true; fi
    docker rm -f "$ENVOY_CONTAINER" >/dev/null 2>&1 || true
    if [ -n "$BENCH_TMPDIR" ]; then rm -rf "$BENCH_TMPDIR"; fi
    if [ -n "$EPP_TMPDIR" ]; then rm -rf "$EPP_TMPDIR"; fi
    wait 2>/dev/null || true
    exit "$status"
}
trap cleanup EXIT

check_tool() { command -v "$1" >/dev/null 2>&1 || { echo "error: $1 not found"; exit 1; }; }
assert_port_free() { if ss -tlnH "sport = :$1" 2>/dev/null | grep -q LISTEN; then echo "error: port $1 in use"; exit 1; fi; }
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
stop_envoy() {
    if [ -n "$DOCKER_LOG_PID" ]; then kill "$DOCKER_LOG_PID" 2>/dev/null || true; wait "$DOCKER_LOG_PID" 2>/dev/null || true; DOCKER_LOG_PID=""; fi
    docker rm -f "$ENVOY_CONTAINER" >/dev/null 2>&1 || true
}

generate_body() {
    local size="$1" out="$2"
    python3 -c "
import json, sys
size = int(sys.argv[1])
padding = 'x' * max(0, size - 100)
obj = {'model': 'test-model', 'messages': [{'role': 'user', 'content': padding}], 'max_tokens': 10}
json.dump(obj, open(sys.argv[2], 'w'))
" "$size" "$out"
}

run_iteration() {
    local profile="$1" port="$2" run_num="$3" targets_file="$4"
    if [ "$WARMUP" -gt 0 ]; then
        vegeta attack -targets "$targets_file" -rate 0 -max-workers 8 -duration "${WARMUP}s" >/dev/null 2>&1 || true
    fi
    local bin_file="$RESULTS_DIR/${profile}-run${run_num}.bin"
    vegeta attack -targets "$targets_file" -rate 0 -max-workers 16 -duration "${DURATION}s" > "$bin_file"
    vegeta report --type=json < "$bin_file" > "$RESULTS_DIR/${profile}-run${run_num}.json"
    python3 - "$RESULTS_DIR/${profile}-run${run_num}.json" << 'PY'
import json, sys
with open(sys.argv[1]) as f: d = json.load(f)
lat = d.get("latencies", {}); codes = d.get("status_codes", {}); success = d.get("success", 0)
print(f"  RPS: {d.get('throughput',0):.0f}  p50: {lat.get('50th',0)/1e6:.2f}ms  p99: {lat.get('99th',0)/1e6:.2f}ms  success: {success*100:.4f}%")
if success < 1.0: print(f"  FAIL: {success*100:.4f}%"); sys.exit(1)
PY
}

compute_median() {
    local profile="$1"
    python3 - "$RESULTS_DIR" "$profile" "$RUNS" << 'PY'
import json, statistics, sys
rd, profile, runs_s = sys.argv[1:]
runs = [json.load(open(f"{rd}/{profile}-run{i}.json")) for i in range(1, int(runs_s)+1)]
result = {"profile": profile, "throughput": statistics.median([r["throughput"] for r in runs]),
    "latencies": {k: statistics.median([r["latencies"][k] for r in runs]) for k in ["50th","95th","99th"]}}
json.dump(result, open(f"{rd}/{profile}-median.json","w"), indent=2)
lat = result["latencies"]
print(f"  Median: {result['throughput']:.0f} RPS, p99={lat['99th']/1e6:.2f}ms")
PY
}

# --- Preflight ---
check_tool vegeta; check_tool python3; check_tool go; check_tool docker
for port in "$BACKEND_PORT" "$PROXY_PORT" "$ENVOY_PORT" "$ADMIN_PORT" "$EPP_GRPC_PORT" "$EPP_HEALTH_PORT" "$EPP_METRICS_PORT" "$ENVOY_ADMIN_PORT"; do
    assert_port_free "$port"
done

mkdir -p "$RESULTS_DIR" "$LOGS_DIR"
BENCH_TMPDIR=$(mktemp -d)

PRAXIS_BIN="$REPO_ROOT/target/release/praxis"
if [ ! -x "$PRAXIS_BIN" ]; then cargo build --release -p praxis --features ext-proc; fi
if [ -z "$LLM_D_SIM_BIN" ]; then
    if [ -x "$LLM_D_SIM_REPO/bin/llm-d-inference-sim" ]; then LLM_D_SIM_BIN="$LLM_D_SIM_REPO/bin/llm-d-inference-sim"
    else echo "error: sim not found"; exit 1; fi
fi
if [ -z "$LLM_D_EPP_BIN" ]; then
    if [ -x /tmp/epp ]; then LLM_D_EPP_BIN="/tmp/epp"
    elif [ -d "$LLM_D_ROUTER_REPO" ]; then (cd "$LLM_D_ROUTER_REPO" && go build -o /tmp/epp ./cmd/epp); LLM_D_EPP_BIN="/tmp/epp"
    else echo "error: no EPP"; exit 1; fi
fi

echo "=== Track B Large-Prompt Benchmark ==="
echo "Duration: ${DURATION}s  Warmup: ${WARMUP}s  Runs: ${RUNS}"
echo "Prompt sizes: ${PROMPT_SIZES}"
echo ""

# --- Start simulator ---
"$LLM_D_SIM_BIN" --model "$MODEL_NAME" --served-model-name "$MODEL_NAME" \
    --port "$BACKEND_PORT" --logtostderr=true >"$LOGS_DIR/sim.log" 2>&1 &
SIM_PID=$!; wait_ready "simulator" "http://127.0.0.1:$BACKEND_PORT/health" "$SIM_PID" 30
echo "Simulator ready"

for SIZE in $PROMPT_SIZES; do
    SIZE_LABEL="${SIZE}B"
    echo ""; echo "========== Prompt size: ${SIZE_LABEL} =========="

    BODY_FILE="$BENCH_TMPDIR/body-${SIZE}.json"
    generate_body "$SIZE" "$BODY_FILE"
    ACTUAL=$(wc -c < "$BODY_FILE")
    echo "Body file: ${ACTUAL} bytes"

    for port in "$PROXY_PORT" "$ENVOY_PORT"; do
        cat > "$BENCH_TMPDIR/targets-${SIZE}-${port}.txt" << TARGETS
POST http://127.0.0.1:${port}/v1/chat/completions
Content-Type: application/json
@${BODY_FILE}
TARGETS
    done

    # praxis-simple
    PRAXIS_CONFIG="$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-simple.yaml" "$PRAXIS_BIN" >"$LOGS_DIR/simple-${SIZE_LABEL}.log" 2>&1 &
    PROXY_PID=$!; wait_ready "Praxis simple" "http://127.0.0.1:$ADMIN_PORT/ready" "$PROXY_PID" 60
    PROFILE="praxis-simple-${SIZE_LABEL}"
    for run in $(seq 1 "$RUNS"); do echo "  $PROFILE run $run:"; run_iteration "$PROFILE" "$PROXY_PORT" "$run" "$BENCH_TMPDIR/targets-${SIZE}-${PROXY_PORT}.txt"; done
    compute_median "$PROFILE"; stop_praxis; sleep 1

    # praxis-go-epp
    EPP_TMPDIR=$(mktemp -d)
    sed "s|PLACEHOLDER_ENDPOINTS_PATH|$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-endpoints.yaml|" \
        "$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-config.yaml" > "$EPP_TMPDIR/epp-config.yaml"
    "$LLM_D_EPP_BIN" --pool-name bench-pool --config-file "$EPP_TMPDIR/epp-config.yaml" \
        --grpc-port "$EPP_GRPC_PORT" --grpc-health-port "$EPP_HEALTH_PORT" --metrics-port "$EPP_METRICS_PORT" \
        --secure-serving=false --health-checking=false --tracing=false --metrics-endpoint-auth=false \
        --grpc-max-recv-msg-size 10MiB --grpc-max-send-msg-size 10MiB \
        >"$LOGS_DIR/go-epp-${SIZE_LABEL}.log" 2>&1 &
    EPP_PID=$!; wait_ready "Go EPP" "http://127.0.0.1:$EPP_METRICS_PORT/metrics" "$EPP_PID" 60
    PRAXIS_CONFIG="$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-go-epp.yaml" "$PRAXIS_BIN" >"$LOGS_DIR/go-epp-praxis-${SIZE_LABEL}.log" 2>&1 &
    PROXY_PID=$!; wait_ready "Praxis go-epp" "http://127.0.0.1:$ADMIN_PORT/ready" "$PROXY_PID" 60
    PROFILE="praxis-go-epp-${SIZE_LABEL}"
    for run in $(seq 1 "$RUNS"); do echo "  $PROFILE run $run:"; run_iteration "$PROFILE" "$PROXY_PORT" "$run" "$BENCH_TMPDIR/targets-${SIZE}-${PROXY_PORT}.txt"; done
    compute_median "$PROFILE"; stop_praxis; stop_epp; rm -rf "$EPP_TMPDIR"; EPP_TMPDIR=""; sleep 1

    # envoy-go-epp
    EPP_TMPDIR=$(mktemp -d)
    sed "s|PLACEHOLDER_ENDPOINTS_PATH|$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-endpoints.yaml|" \
        "$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-config.yaml" > "$EPP_TMPDIR/epp-config.yaml"
    "$LLM_D_EPP_BIN" --pool-name bench-pool --config-file "$EPP_TMPDIR/epp-config.yaml" \
        --grpc-port "$EPP_GRPC_PORT" --grpc-health-port "$EPP_HEALTH_PORT" --metrics-port "$EPP_METRICS_PORT" \
        --secure-serving=false --health-checking=false --tracing=false --metrics-endpoint-auth=false \
        --grpc-max-recv-msg-size 10MiB --grpc-max-send-msg-size 10MiB \
        >"$LOGS_DIR/envoy-epp-${SIZE_LABEL}.log" 2>&1 &
    EPP_PID=$!; wait_ready "Go EPP" "http://127.0.0.1:$EPP_METRICS_PORT/metrics" "$EPP_PID" 60
    docker rm -f "$ENVOY_CONTAINER" >/dev/null 2>&1 || true
    docker run --rm -d --name "$ENVOY_CONTAINER" --network host \
        -v "$REPO_ROOT/benchmarks/comparison/configs/llmd/envoy-go-epp.yaml:/etc/envoy/envoy.yaml:ro" \
        "$ENVOY_IMAGE" -c /etc/envoy/envoy.yaml --log-level warn >"$LOGS_DIR/envoy-${SIZE_LABEL}-start.log" 2>&1
    wait_http_ready "Envoy" "http://127.0.0.1:$ENVOY_ADMIN_PORT/ready" 60
    docker logs -f "$ENVOY_CONTAINER" >"$LOGS_DIR/envoy-${SIZE_LABEL}.log" 2>&1 &
    DOCKER_LOG_PID=$!
    PROFILE="envoy-go-epp-${SIZE_LABEL}"
    for run in $(seq 1 "$RUNS"); do echo "  $PROFILE run $run:"; run_iteration "$PROFILE" "$ENVOY_PORT" "$run" "$BENCH_TMPDIR/targets-${SIZE}-${ENVOY_PORT}.txt"; done
    compute_median "$PROFILE"; stop_envoy; stop_epp; rm -rf "$EPP_TMPDIR"; EPP_TMPDIR=""; sleep 1
done

# --- Summary ---
echo ""; echo "=== Large-Prompt Summary ==="
for SIZE in $PROMPT_SIZES; do
    SIZE_LABEL="${SIZE}B"
    echo ""; echo "--- ${SIZE_LABEL} ---"
    for p in "praxis-simple-${SIZE_LABEL}" "praxis-go-epp-${SIZE_LABEL}" "envoy-go-epp-${SIZE_LABEL}"; do
        if [ -f "$RESULTS_DIR/${p}-median.json" ]; then
            python3 -c "import json; d=json.load(open('$RESULTS_DIR/${p}-median.json')); print(f'  ${p}: {d[\"throughput\"]:.0f} RPS, p99={d[\"latencies\"][\"99th\"]/1e6:.2f}ms')"
        fi
    done
done
echo ""; echo "=== Large-Prompt Benchmark Complete ==="
