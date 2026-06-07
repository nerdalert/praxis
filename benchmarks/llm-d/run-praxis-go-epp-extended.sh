#!/usr/bin/env bash
# Praxis + Go EPP extended benchmark (Track B).
#
# Runs the praxis-go-epp profile with the llmd-chat-small workload
# against a minimal Python mock backend. Methodology matches the
# existing extended benchmark: 3 runs, 30s each, median selected.
#
# Usage:
#   ./benchmarks/llm-d/run-praxis-go-epp-extended.sh [DURATION] [WARMUP] [RUNS]
#
# Positional arguments:
#   DURATION  measurement duration per run in seconds (default: 30)
#   WARMUP   warmup duration in seconds (default: 5)
#   RUNS     number of measurement runs (default: 3)
#
# Prerequisites:
#   - vegeta
#   - python3 (stdlib only)
#   - Praxis built with --features ext-proc
#   - Go EPP binary: LLM_D_EPP_BIN or built from LLM_D_ROUTER_REPO
#
# Request path:
#   Vegeta -> Praxis llmd_external_epp (port 18090)
#     -> ext_proc gRPC -> Go EPP (port 9002)
#     -> x-gateway-destination-endpoint -> mock backend (port 18080)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RESULTS_DIR="$REPO_ROOT/target/criterion/llmd-extended"
LOGS_DIR="$RESULTS_DIR/logs"
DURATION="${1:-30}"
WARMUP="${2:-5}"
RUNS="${3:-3}"

PROFILE="praxis-go-epp"
BACKEND_PORT=18080
PROXY_PORT=18090
ADMIN_PORT=9901
EPP_GRPC_PORT=9002
EPP_HEALTH_PORT=9003
EPP_METRICS_PORT=9090

LLM_D_EPP_BIN="${LLM_D_EPP_BIN:-}"
LLM_D_ROUTER_REPO="${LLM_D_ROUTER_REPO:-$REPO_ROOT/../../repos/llm-d-router}"

BACKEND_PID=""
EPP_PID=""
PROXY_PID=""

cleanup() {
    echo "Cleaning up..."
    if [ -n "$PROXY_PID" ]; then kill "$PROXY_PID" 2>/dev/null || true; fi
    if [ -n "$EPP_PID" ]; then kill "$EPP_PID" 2>/dev/null || true; fi
    if [ -n "$BACKEND_PID" ]; then kill "$BACKEND_PID" 2>/dev/null || true; fi
    wait 2>/dev/null || true
}
trap cleanup EXIT

check_tool() {
    command -v "$1" &>/dev/null || { echo "error: $1 not found"; exit 1; }
}

wait_ready() {
    local label="$1" url="$2" pid="$3" timeout="${4:-60}"
    for _ in $(seq 1 "$timeout"); do
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "error: $label (PID $pid) exited during startup"
            return 1
        fi
        if curl -sf "$url" >/dev/null 2>&1; then return 0; fi
        sleep 0.2
    done
    echo "error: $label did not become ready within ${timeout} iterations"
    return 1
}

assert_port_free() {
    if ss -tlnH "sport = :$1" 2>/dev/null | grep -q LISTEN; then
        echo "error: port $1 is already in use"
        exit 1
    fi
}

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

check_tool vegeta
check_tool python3

for p in $BACKEND_PORT $PROXY_PORT $ADMIN_PORT $EPP_GRPC_PORT $EPP_HEALTH_PORT $EPP_METRICS_PORT; do
    assert_port_free "$p"
done

echo "=== Praxis + Go EPP Extended Benchmark (Track B) ==="
echo "Duration: ${DURATION}s  Warmup: ${WARMUP}s  Runs: ${RUNS}"
echo ""

mkdir -p "$RESULTS_DIR" "$LOGS_DIR"

# ---------------------------------------------------------------------------
# Record metadata
# ---------------------------------------------------------------------------

PRAXIS_COMMIT="$(cd "$REPO_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo unknown)"
PRAXIS_BRANCH="$(cd "$REPO_ROOT" && git branch --show-current 2>/dev/null || echo unknown)"
EPP_COMMIT=""
if [ -d "$LLM_D_ROUTER_REPO" ]; then
    EPP_COMMIT="$(cd "$LLM_D_ROUTER_REPO" && git rev-parse --short HEAD 2>/dev/null || echo unknown)"
fi
VEGETA_VERSION="$(vegeta --version 2>&1 | head -1 || echo unknown)"
CPU_MODEL="$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2 | xargs || echo unknown)"
OS_VERSION="$(uname -r)"

echo "Praxis: ${PRAXIS_COMMIT} (${PRAXIS_BRANCH})"
echo "Go EPP: ${EPP_COMMIT}"
echo "Vegeta: ${VEGETA_VERSION}"
echo "CPU: ${CPU_MODEL}"
echo "OS: Linux ${OS_VERSION}"
echo ""

# ---------------------------------------------------------------------------
# Verify Praxis has ext-proc
# ---------------------------------------------------------------------------

PRAXIS_BIN="$REPO_ROOT/target/release/praxis"
if [ ! -x "$PRAXIS_BIN" ]; then
    echo "Building Praxis (ext-proc)..."
    (cd "$REPO_ROOT" && cargo build --release -p praxis --features ext-proc 2>&1 | tail -3)
fi

# Validate the binary can parse llmd_external_epp config.
CONFIG_FILE="$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-go-epp.yaml"
if ! "$PRAXIS_BIN" -t -c "$CONFIG_FILE" 2>/dev/null; then
    echo "error: Praxis binary does not support llmd_external_epp"
    echo "  Rebuild with: cargo build --release -p praxis --features ext-proc"
    exit 1
fi
echo "Praxis binary validated (ext-proc enabled)"

# ---------------------------------------------------------------------------
# Resolve EPP binary
# ---------------------------------------------------------------------------

if [ -z "$LLM_D_EPP_BIN" ]; then
    if [ -x /tmp/epp ]; then
        LLM_D_EPP_BIN="/tmp/epp"
    elif [ -d "$LLM_D_ROUTER_REPO" ]; then
        echo "Building EPP from $LLM_D_ROUTER_REPO..."
        (cd "$LLM_D_ROUTER_REPO" && go build -o /tmp/epp ./cmd/epp 2>&1 | tail -3)
        LLM_D_EPP_BIN="/tmp/epp"
    else
        echo "error: no EPP binary. Set LLM_D_EPP_BIN or LLM_D_ROUTER_REPO"
        exit 1
    fi
fi
echo "EPP binary: $LLM_D_EPP_BIN"

# ---------------------------------------------------------------------------
# Start mock backend
# ---------------------------------------------------------------------------

BENCH_TMPDIR=$(mktemp -d)

# Build a Go mock backend (handles high concurrency without 502s).
MOCK_SRC="$BENCH_TMPDIR/mock.go"
MOCK_BIN="$BENCH_TMPDIR/mock"
cat > "$MOCK_SRC" << 'GOSRC'
package main
import (
	"io"
	"net/http"
	"os"
)
const resp = `{"id":"chatcmpl-bench","object":"chat.completion","model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"Hello from mock."},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}`
func main() {
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		io.Copy(io.Discard, r.Body)
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(200)
		io.WriteString(w, resp)
	})
	if err := http.ListenAndServe("127.0.0.1:"+os.Args[1], nil); err != nil {
		os.Exit(1)
	}
}
GOSRC
echo "Building Go mock backend..."
go build -o "$MOCK_BIN" "$MOCK_SRC"
echo "Starting mock backend on port $BACKEND_PORT..."
"$MOCK_BIN" "$BACKEND_PORT" &
BACKEND_PID=$!
wait_ready "backend" "http://127.0.0.1:$BACKEND_PORT/" "$BACKEND_PID" 30
echo "Backend ready (PID $BACKEND_PID)"

# ---------------------------------------------------------------------------
# Start Go EPP
# ---------------------------------------------------------------------------

EPP_TMPDIR=$(mktemp -d)
ENDPOINTS_ABS="$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-endpoints.yaml"
sed "s|PLACEHOLDER_ENDPOINTS_PATH|$ENDPOINTS_ABS|" \
    "$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-config.yaml" \
    > "$EPP_TMPDIR/epp-config.yaml"

echo "Starting Go EPP on gRPC port $EPP_GRPC_PORT..."
"$LLM_D_EPP_BIN" \
    --pool-name bench-pool \
    --config-file "$EPP_TMPDIR/epp-config.yaml" \
    --grpc-port "$EPP_GRPC_PORT" \
    --grpc-health-port "$EPP_HEALTH_PORT" \
    --metrics-port "$EPP_METRICS_PORT" \
    --secure-serving=false \
    --health-checking=false \
    --tracing=false \
    --metrics-endpoint-auth=false \
    >"$LOGS_DIR/${PROFILE}-epp.log" 2>&1 &
EPP_PID=$!
wait_ready "EPP" "http://127.0.0.1:$EPP_METRICS_PORT/metrics" "$EPP_PID" 60
echo "EPP ready (PID $EPP_PID)"

# ---------------------------------------------------------------------------
# Start Praxis
# ---------------------------------------------------------------------------

echo "Starting Praxis ($PROFILE) on port $PROXY_PORT..."
PRAXIS_CONFIG="$CONFIG_FILE" "$PRAXIS_BIN" \
    >"$LOGS_DIR/${PROFILE}.log" 2>&1 &
PROXY_PID=$!
wait_ready "Praxis" "http://127.0.0.1:$ADMIN_PORT/ready" "$PROXY_PID" 60
echo "Praxis ready (PID $PROXY_PID)"

# ---------------------------------------------------------------------------
# Verify E2E path through Go EPP (fatal)
# ---------------------------------------------------------------------------

echo "Verifying E2E path..."
VERIFY=$(curl -sf -X POST \
    -H "Content-Type: application/json" \
    "http://127.0.0.1:$PROXY_PORT/v1/chat/completions" \
    -d '{"model":"test-model","messages":[{"role":"user","content":"verify"}],"max_tokens":5}' 2>&1)
if ! echo "$VERIFY" | grep -q "chat.completion"; then
    echo "error: E2E verification failed"
    exit 1
fi

# Fatal: EPP must have processed the verification request.
sleep 0.5
if ! grep -q "test-model" "$LOGS_DIR/${PROFILE}-epp.log" 2>/dev/null; then
    echo "error: EPP log does not contain 'test-model' after verification"
    echo "  EPP log: $LOGS_DIR/${PROFILE}-epp.log"
    tail -10 "$LOGS_DIR/${PROFILE}-epp.log"
    exit 1
fi
echo "E2E verified: Praxis -> Go EPP -> backend"

# ---------------------------------------------------------------------------
# Prepare Vegeta targets
# ---------------------------------------------------------------------------

cat > "$BENCH_TMPDIR/body.json" << 'BODY'
{"model":"test-model","messages":[{"role":"user","content":"Hello, how are you?"}],"max_tokens":50}
BODY

cat > "$BENCH_TMPDIR/targets.txt" << TARGETS
POST http://127.0.0.1:$PROXY_PORT/v1/chat/completions
Content-Type: application/json
@${BENCH_TMPDIR}/body.json
TARGETS

# ---------------------------------------------------------------------------
# Run benchmark: N runs with warmup
# ---------------------------------------------------------------------------

for run in $(seq 1 "$RUNS"); do
    echo ""
    echo "--- Run $run of $RUNS ---"

    if [ "$WARMUP" -gt 0 ]; then
        echo "Warmup (${WARMUP}s)..."
        vegeta attack -targets "$BENCH_TMPDIR/targets.txt" \
            -rate 0 -max-workers 8 \
            -duration "${WARMUP}s" > /dev/null 2>&1 || true
    fi

    echo "Measuring (${DURATION}s)..."
    BIN="$RESULTS_DIR/${PROFILE}-run${run}.bin"
    vegeta attack -targets "$BENCH_TMPDIR/targets.txt" \
        -rate 0 -max-workers 16 \
        -duration "${DURATION}s" > "$BIN"

    vegeta report --type=json < "$BIN" > "$RESULTS_DIR/${PROFILE}-run${run}.json"
    vegeta report --type=text < "$BIN" > "$RESULTS_DIR/${PROFILE}-run${run}.txt"

    # Print per-run summary and enforce 100% success.
    python3 -c "
import json, sys
with open('$RESULTS_DIR/${PROFILE}-run${run}.json') as f:
    d = json.load(f)
lat = d.get('latencies', {})
codes = d.get('status_codes', {})
success = d.get('success', 0)
errors = d.get('errors', [])

print(f'  RPS: {d.get(\"throughput\", 0):.0f}  '
      f'p50: {lat.get(\"50th\", 0)/1e6:.2f}ms  '
      f'p95: {lat.get(\"95th\", 0)/1e6:.2f}ms  '
      f'p99: {lat.get(\"99th\", 0)/1e6:.2f}ms  '
      f'success: {success*100:.4f}%')
print(f'  status_codes: {codes}')

if success < 1.0:
    print(f'  FAIL: success rate {success*100:.4f}% is not 100%')
    if errors:
        print(f'  errors: {errors}')
    sys.exit(1)
"
done

# ---------------------------------------------------------------------------
# Compute median
# ---------------------------------------------------------------------------

echo ""
echo "=== Computing median across $RUNS runs ==="

python3 -c "
import json, statistics

runs = []
for i in range(1, $RUNS + 1):
    with open('$RESULTS_DIR/${PROFILE}-run' + str(i) + '.json') as f:
        runs.append(json.load(f))

def median_field(path_fn):
    vals = [path_fn(r) for r in runs]
    return statistics.median(vals)

lat_keys = ['mean', '50th', '90th', '95th', '99th', 'max']
median = {
    'throughput': median_field(lambda r: r.get('throughput', 0)),
    'success': median_field(lambda r: r.get('success', 0)),
    'latencies': {k: median_field(lambda r: r.get('latencies', {}).get(k, 0)) for k in lat_keys},
}

with open('$RESULTS_DIR/${PROFILE}-median.json', 'w') as f:
    json.dump(median, f, indent=2)

lat = median['latencies']
print(f'Median RPS: {median[\"throughput\"]:.0f}')
print(f'Median p50: {lat[\"50th\"]/1e6:.2f}ms')
print(f'Median p95: {lat[\"95th\"]/1e6:.2f}ms')
print(f'Median p99: {lat[\"99th\"]/1e6:.2f}ms')
print(f'Success: {median[\"success\"]*100:.1f}%')
"

# ---------------------------------------------------------------------------
# Generate median YAML
# ---------------------------------------------------------------------------

python3 -c "
import json
from datetime import datetime, timezone

with open('$RESULTS_DIR/${PROFILE}-median.json') as f:
    data = json.load(f)
lat = data.get('latencies', {})

lines = [
    'timestamp: \"' + datetime.now(timezone.utc).isoformat() + '\"',
    'praxis_commit: \"$PRAXIS_COMMIT\"',
    'praxis_branch: \"$PRAXIS_BRANCH\"',
    'epp_commit: \"$EPP_COMMIT\"',
    'vegeta_version: \"$VEGETA_VERSION\"',
    'cpu: \"$CPU_MODEL\"',
    'os: \"Linux $OS_VERSION\"',
    'profile: \"$PROFILE\"',
    'workload: \"llmd-chat-small\"',
    'execution_mode: \"local\"',
    'backend_type: \"go-mock\"',
    'proxy: \"praxis\"',
    'epp: \"go-epp-external\"',
    'tool: \"vegeta\"',
    'methodology: \"${RUNS} runs x ${DURATION}s, ${WARMUP}s warmup, median\"',
    'latency:',
    '  mean_ms: ' + str(lat.get('mean', 0) / 1e6),
    '  p50_ms: ' + str(lat.get('50th', 0) / 1e6),
    '  p90_ms: ' + str(lat.get('90th', 0) / 1e6),
    '  p95_ms: ' + str(lat.get('95th', 0) / 1e6),
    '  p99_ms: ' + str(lat.get('99th', 0) / 1e6),
    '  max_ms: ' + str(lat.get('max', 0) / 1e6),
    'throughput:',
    '  requests_per_sec: ' + str(data.get('throughput', 0)),
    'errors:',
    '  success_rate: ' + str(data.get('success', 0)),
    'duration_secs: $DURATION',
    'warmup_secs: $WARMUP',
    'runs: $RUNS',
    'workers: 16',
    'rate: 0',
    'command: \"benchmarks/llm-d/run-praxis-go-epp-extended.sh $DURATION $WARMUP $RUNS\"',
]
print('\n'.join(lines))
" > "$RESULTS_DIR/${PROFILE}-median.yaml"

echo ""
echo "=== Results saved ==="
echo "  Per-run: $RESULTS_DIR/${PROFILE}-run{1..$RUNS}.{bin,json,txt}"
echo "  Median:  $RESULTS_DIR/${PROFILE}-median.{json,yaml}"
