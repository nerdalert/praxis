#!/usr/bin/env bash
# Track B benchmark with llm-d-inference-sim echo backend.
#
# Runs praxis-simple, praxis-go-epp, and envoy-go-epp against
# llm-d-inference-sim in echo mode. Methodology matches Track A
# sim benchmark (3x30s, Vegeta rate 0, max-workers 16).
#
# Usage:
#   ./benchmarks/llm-d/run-track-b-sim-benchmark.sh [DURATION] [WARMUP] [RUNS]
#
# Environment:
#   LLM_D_SIM_BIN       path to sim binary (default: builds from LLM_D_SIM_REPO)
#   LLM_D_SIM_REPO      path to sim source
#   LLM_D_EPP_BIN       path to Go EPP binary
#   LLM_D_ROUTER_REPO   path to llm-d-router source

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RESULTS_DIR="$REPO_ROOT/target/criterion/llmd-track-b-sim"
LOGS_DIR="$RESULTS_DIR/logs"
DURATION="${1:-30}"
WARMUP="${2:-5}"
RUNS="${3:-3}"

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
ENVOY_CONTAINER="${ENVOY_CONTAINER:-llmd-track-b-sim-envoy}"
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
assert_port_free() {
    if ss -tlnH "sport = :$1" 2>/dev/null | grep -q LISTEN; then
        echo "error: port $1 is already in use"; exit 1
    fi
}
wait_ready() {
    local label="$1" url="$2" pid="$3" timeout="${4:-60}"
    for _ in $(seq 1 "$timeout"); do
        if ! kill -0 "$pid" 2>/dev/null; then echo "error: $label exited"; return 1; fi
        if curl -sf "$url" >/dev/null 2>&1; then return 0; fi
        sleep 0.2
    done
    echo "error: $label not ready"; return 1
}
wait_http_ready() {
    local label="$1" url="$2" timeout="${3:-60}"
    for _ in $(seq 1 "$timeout"); do
        if curl -sf "$url" >/dev/null 2>&1; then return 0; fi
        sleep 0.2
    done
    echo "error: $label not ready"; return 1
}
stop_praxis() { if [ -n "$PROXY_PID" ]; then kill "$PROXY_PID" 2>/dev/null || true; wait "$PROXY_PID" 2>/dev/null || true; PROXY_PID=""; fi; }
stop_epp() { if [ -n "$EPP_PID" ]; then kill "$EPP_PID" 2>/dev/null || true; wait "$EPP_PID" 2>/dev/null || true; EPP_PID=""; fi; }
stop_envoy() {
    if [ -n "$DOCKER_LOG_PID" ]; then kill "$DOCKER_LOG_PID" 2>/dev/null || true; wait "$DOCKER_LOG_PID" 2>/dev/null || true; DOCKER_LOG_PID=""; fi
    docker rm -f "$ENVOY_CONTAINER" >/dev/null 2>&1 || true
}

run_iteration() {
    local profile="$1" port="$2" run_num="$3"
    local targets_file="$BENCH_TMPDIR/targets-${port}.txt"
    if [ "$WARMUP" -gt 0 ]; then
        vegeta attack -targets "$targets_file" -rate 0 -max-workers 8 -duration "${WARMUP}s" >/dev/null 2>&1 || true
    fi
    local bin_file="$RESULTS_DIR/${profile}-run${run_num}.bin"
    vegeta attack -targets "$targets_file" -rate 0 -max-workers 16 -duration "${DURATION}s" > "$bin_file"
    vegeta report --type=json < "$bin_file" > "$RESULTS_DIR/${profile}-run${run_num}.json"
    vegeta report --type=text < "$bin_file" > "$RESULTS_DIR/${profile}-run${run_num}.txt"
    python3 - "$RESULTS_DIR/${profile}-run${run_num}.json" << 'PY'
import json, sys
with open(sys.argv[1]) as f: d = json.load(f)
lat = d.get("latencies", {}); codes = d.get("status_codes", {}); success = d.get("success", 0)
print(f"  RPS: {d.get('throughput', 0):.0f}  p50: {lat.get('50th', 0)/1e6:.2f}ms  p95: {lat.get('95th', 0)/1e6:.2f}ms  p99: {lat.get('99th', 0)/1e6:.2f}ms  success: {success*100:.4f}%")
print(f"  status_codes: {codes}")
if success < 1.0: print(f"  FAIL: {success*100:.4f}%"); sys.exit(1)
if set(codes.keys()) != {"200"}: print(f"  FAIL: non-200: {codes}"); sys.exit(1)
PY
}

compute_median() {
    local profile="$1"
    python3 - "$RESULTS_DIR" "$profile" "$RUNS" << 'PY'
import json, statistics, sys
rd, profile, runs_s = sys.argv[1:]
runs = [json.load(open(f"{rd}/{profile}-run{i}.json")) for i in range(1, int(runs_s)+1)]
lat_keys = ["mean","50th","90th","95th","99th","max"]
result = {"profile": profile, "runs": len(runs),
    "throughput": statistics.median([r["throughput"] for r in runs]),
    "success": statistics.median([r["success"] for r in runs]),
    "latencies": {k: statistics.median([r["latencies"][k] for r in runs]) for k in lat_keys}}
json.dump(result, open(f"{rd}/{profile}-median.json","w"), indent=2)
lat = result["latencies"]
print(f"  Median: {result['throughput']:.0f} req/s, p50={lat['50th']/1e6:.2f}ms, p95={lat['95th']/1e6:.2f}ms, p99={lat['99th']/1e6:.2f}ms")
PY
}

# --- Preflight ---
check_tool vegeta; check_tool python3; check_tool go; check_tool docker
for port in "$BACKEND_PORT" "$PROXY_PORT" "$ENVOY_PORT" "$ADMIN_PORT" "$EPP_GRPC_PORT" "$EPP_HEALTH_PORT" "$EPP_METRICS_PORT" "$ENVOY_ADMIN_PORT"; do
    assert_port_free "$port"
done

mkdir -p "$RESULTS_DIR" "$LOGS_DIR"
BENCH_TMPDIR=$(mktemp -d)

# --- Resolve binaries ---
PRAXIS_BIN="$REPO_ROOT/target/release/praxis"
if [ ! -x "$PRAXIS_BIN" ]; then cargo build --release -p praxis --features ext-proc; fi

if [ -z "$LLM_D_SIM_BIN" ]; then
    if [ -x "$LLM_D_SIM_REPO/bin/llm-d-inference-sim" ]; then
        LLM_D_SIM_BIN="$LLM_D_SIM_REPO/bin/llm-d-inference-sim"
    else
        echo "error: sim binary not found. Set LLM_D_SIM_BIN or build in LLM_D_SIM_REPO"
        exit 1
    fi
fi

if [ -z "$LLM_D_EPP_BIN" ]; then
    if [ -x /tmp/epp ]; then LLM_D_EPP_BIN="/tmp/epp"
    elif [ -d "$LLM_D_ROUTER_REPO" ]; then (cd "$LLM_D_ROUTER_REPO" && go build -o /tmp/epp ./cmd/epp); LLM_D_EPP_BIN="/tmp/epp"
    else echo "error: no EPP binary"; exit 1; fi
fi

echo "=== Track B Simulator Echo Benchmark ==="
echo "Duration: ${DURATION}s  Warmup: ${WARMUP}s  Runs: ${RUNS}"
echo "Simulator: $LLM_D_SIM_BIN"
echo "EPP: $LLM_D_EPP_BIN"
echo ""

# --- Start simulator ---
echo "Starting llm-d-inference-sim on port $BACKEND_PORT..."
"$LLM_D_SIM_BIN" --model "$MODEL_NAME" --served-model-name "$MODEL_NAME" \
    --port "$BACKEND_PORT" --logtostderr=true \
    >"$LOGS_DIR/sim.log" 2>&1 &
SIM_PID=$!
wait_ready "simulator" "http://127.0.0.1:$BACKEND_PORT/health" "$SIM_PID" 30
echo "Simulator ready (PID $SIM_PID)"

# --- Prepare targets and EPP config ---
cat > "$BENCH_TMPDIR/body.json" << 'BODY'
{"model":"test-model","messages":[{"role":"user","content":"Hello, how are you?"}],"max_tokens":50}
BODY

for port in "$PROXY_PORT" "$ENVOY_PORT"; do
    cat > "$BENCH_TMPDIR/targets-${port}.txt" << TARGETS
POST http://127.0.0.1:${port}/v1/chat/completions
Content-Type: application/json
@${BENCH_TMPDIR}/body.json
TARGETS
done

# --- praxis-simple ---
echo ""; echo "--- praxis-simple (${RUNS} runs x ${DURATION}s) ---"
PRAXIS_CONFIG="$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-simple.yaml" "$PRAXIS_BIN" >"$LOGS_DIR/praxis-simple.log" 2>&1 &
PROXY_PID=$!; wait_ready "Praxis simple" "http://127.0.0.1:$ADMIN_PORT/ready" "$PROXY_PID" 60
for run in $(seq 1 "$RUNS"); do echo "Run $run:"; run_iteration "praxis-simple" "$PROXY_PORT" "$run"; done
compute_median "praxis-simple"; stop_praxis; sleep 1

# --- praxis-go-epp ---
echo ""; echo "--- praxis-go-epp (${RUNS} runs x ${DURATION}s) ---"
EPP_TMPDIR=$(mktemp -d)
sed "s|PLACEHOLDER_ENDPOINTS_PATH|$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-endpoints.yaml|" \
    "$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-config.yaml" > "$EPP_TMPDIR/epp-config.yaml"
"$LLM_D_EPP_BIN" --pool-name bench-pool --config-file "$EPP_TMPDIR/epp-config.yaml" \
    --grpc-port "$EPP_GRPC_PORT" --grpc-health-port "$EPP_HEALTH_PORT" --metrics-port "$EPP_METRICS_PORT" \
    --secure-serving=false --health-checking=false --tracing=false --metrics-endpoint-auth=false \
    >"$LOGS_DIR/praxis-go-epp-epp.log" 2>&1 &
EPP_PID=$!; wait_ready "Go EPP" "http://127.0.0.1:$EPP_METRICS_PORT/metrics" "$EPP_PID" 60
PRAXIS_CONFIG="$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-go-epp.yaml" "$PRAXIS_BIN" >"$LOGS_DIR/praxis-go-epp.log" 2>&1 &
PROXY_PID=$!; wait_ready "Praxis go-epp" "http://127.0.0.1:$ADMIN_PORT/ready" "$PROXY_PID" 60
for run in $(seq 1 "$RUNS"); do echo "Run $run:"; run_iteration "praxis-go-epp" "$PROXY_PORT" "$run"; done
compute_median "praxis-go-epp"; stop_praxis; stop_epp; rm -rf "$EPP_TMPDIR"; EPP_TMPDIR=""; sleep 1

# --- envoy-go-epp ---
echo ""; echo "--- envoy-go-epp (${RUNS} runs x ${DURATION}s) ---"
EPP_TMPDIR=$(mktemp -d)
sed "s|PLACEHOLDER_ENDPOINTS_PATH|$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-endpoints.yaml|" \
    "$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-config.yaml" > "$EPP_TMPDIR/epp-config.yaml"
"$LLM_D_EPP_BIN" --pool-name bench-pool --config-file "$EPP_TMPDIR/epp-config.yaml" \
    --grpc-port "$EPP_GRPC_PORT" --grpc-health-port "$EPP_HEALTH_PORT" --metrics-port "$EPP_METRICS_PORT" \
    --secure-serving=false --health-checking=false --tracing=false --metrics-endpoint-auth=false \
    >"$LOGS_DIR/envoy-go-epp-epp.log" 2>&1 &
EPP_PID=$!; wait_ready "Go EPP" "http://127.0.0.1:$EPP_METRICS_PORT/metrics" "$EPP_PID" 60
docker rm -f "$ENVOY_CONTAINER" >/dev/null 2>&1 || true
docker run --rm -d --name "$ENVOY_CONTAINER" --network host \
    -v "$REPO_ROOT/benchmarks/comparison/configs/llmd/envoy-go-epp.yaml:/etc/envoy/envoy.yaml:ro" \
    "$ENVOY_IMAGE" -c /etc/envoy/envoy.yaml --log-level warn >"$LOGS_DIR/envoy-start.log" 2>&1
wait_http_ready "Envoy" "http://127.0.0.1:$ENVOY_ADMIN_PORT/ready" 60
docker logs -f "$ENVOY_CONTAINER" >"$LOGS_DIR/envoy.log" 2>&1 &
DOCKER_LOG_PID=$!
for run in $(seq 1 "$RUNS"); do echo "Run $run:"; run_iteration "envoy-go-epp" "$ENVOY_PORT" "$run"; done
compute_median "envoy-go-epp"; stop_envoy; stop_epp; rm -rf "$EPP_TMPDIR"; EPP_TMPDIR=""; sleep 1

# --- Summary ---
echo ""; echo "=== Track B Simulator Echo Results ==="
python3 - "$RESULTS_DIR" << 'PY'
import json, sys
rd = sys.argv[1]
for p in ["praxis-simple", "praxis-go-epp", "envoy-go-epp"]:
    d = json.load(open(f"{rd}/{p}-median.json"))
    lat = d["latencies"]
    print(f"  {p}: {d['throughput']:.0f} RPS, p50={lat['50th']/1e6:.2f}ms, p95={lat['95th']/1e6:.2f}ms, p99={lat['99th']/1e6:.2f}ms")
PY
echo ""; echo "=== Simulator Echo Benchmark Complete ==="
