#!/usr/bin/env bash
# Extended llm-d benchmark: multiple runs, longer duration, median selection.
#
# Runs praxis-simple, praxis-native, and envoy-go-epp profiles
# with configurable duration and repetitions. Computes median
# throughput and p99 across runs.
#
# Usage:
#   ./benchmarks/llm-d/run-extended-benchmark.sh [DURATION] [WARMUP] [RUNS]
#
# Positional arguments:
#   DURATION  measurement duration per run in seconds (default: 30)
#   WARMUP    warmup duration in seconds (default: 5)
#   RUNS      number of repetitions per profile (default: 3)
#
# Prerequisites: same as run-smoke.sh and run-envoy-go-epp-smoke.sh
# Results go to target/criterion/llmd-extended/

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RESULTS_DIR="$REPO_ROOT/target/criterion/llmd-extended"
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
ENVOY_IMAGE="envoyproxy/envoy:distroless-v1.33.2"
ENVOY_CONTAINER="llmd-bench-envoy"

LLM_D_EPP_BIN="${LLM_D_EPP_BIN:-}"
LLM_D_ROUTER_REPO="${LLM_D_ROUTER_REPO:-$REPO_ROOT/../../repos/llm-d-router}"

cleanup() {
    if [ -n "${BACKEND_PID:-}" ]; then kill "$BACKEND_PID" 2>/dev/null || true; fi
    if [ -n "${PROXY_PID:-}" ]; then kill "$PROXY_PID" 2>/dev/null || true; fi
    if [ -n "${EPP_PID:-}" ]; then kill "$EPP_PID" 2>/dev/null || true; fi
    docker rm -f "$ENVOY_CONTAINER" 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT

check_tool() {
    if ! command -v "$1" &>/dev/null; then
        echo "error: $1 not found."
        exit 1
    fi
}

check_tool vegeta
check_tool python3
check_tool docker

echo "=== llm-d Extended Benchmark ==="
echo "Duration: ${DURATION}s  Warmup: ${WARMUP}s  Runs: ${RUNS}"
echo ""

mkdir -p "$RESULTS_DIR" "$LOGS_DIR"

# --- Resolve EPP binary ---
if [ -z "$LLM_D_EPP_BIN" ]; then
    if [ -x /tmp/epp ]; then
        LLM_D_EPP_BIN="/tmp/epp"
    elif [ -d "$LLM_D_ROUTER_REPO" ]; then
        echo "Building EPP..."
        (cd "$LLM_D_ROUTER_REPO" && go build -o /tmp/epp ./cmd/epp 2>&1 | tail -3)
        LLM_D_EPP_BIN="/tmp/epp"
    else
        echo "error: no EPP binary. Set LLM_D_EPP_BIN or LLM_D_ROUTER_REPO."
        exit 1
    fi
fi

# --- Start mock backend ---
echo "Starting mock backend..."
python3 -c "
import http.server, json
RESPONSE = json.dumps({
    'id': 'chatcmpl-bench', 'object': 'chat.completion', 'model': 'test-model',
    'choices': [{'index': 0, 'message': {'role': 'assistant', 'content': 'Hello from mock.'}, 'finish_reason': 'stop'}],
    'usage': {'prompt_tokens': 10, 'completion_tokens': 5, 'total_tokens': 15}
}).encode()
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get('Content-Length', 0)))
        self.send_response(200); self.send_header('Content-Type','application/json')
        self.send_header('Content-Length',str(len(RESPONSE))); self.end_headers(); self.wfile.write(RESPONSE)
    def do_GET(self):
        self.send_response(200); self.send_header('Content-Type','text/plain'); self.end_headers(); self.wfile.write(b'ok')
    def log_message(self, *a): pass
http.server.HTTPServer(('127.0.0.1', $BACKEND_PORT), H).serve_forever()
" &
BACKEND_PID=$!
for _ in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:$BACKEND_PORT/" >/dev/null 2>&1; then break; fi
    sleep 0.1
done
echo "Backend ready"

# --- Build Praxis ---
if [ -x "$REPO_ROOT/target/release/praxis" ]; then
    echo "Using existing Praxis binary."
else
    echo "Building Praxis..."
    cargo build --release -p praxis --quiet 2>&1 | tail -3
fi

# --- Vegeta targets ---
BENCH_TMP=$(mktemp -d)
cat > "$BENCH_TMP/body.json" << 'BODY'
{"model":"test-model","messages":[{"role":"user","content":"Hello, how are you?"}],"max_tokens":50}
BODY

make_targets() {
    local port="$1"
    local tfile="$BENCH_TMP/targets-${port}.txt"
    cat > "$tfile" << TARGETS
POST http://127.0.0.1:${port}/v1/chat/completions
Content-Type: application/json
@${BENCH_TMP}/body.json
TARGETS
    echo "$tfile"
}

# --- Run a single benchmark iteration ---
run_iteration() {
    local profile="$1"
    local port="$2"
    local run_num="$3"
    local targets_file
    targets_file=$(make_targets "$port")

    if [ "$WARMUP" -gt 0 ]; then
        vegeta attack -targets "$targets_file" -rate 0 -max-workers 8 \
            -duration "${WARMUP}s" > /dev/null 2>&1 || true
    fi

    local bin_file="$RESULTS_DIR/${profile}-run${run_num}.bin"
    vegeta attack -targets "$targets_file" -rate 0 -max-workers 16 \
        -duration "${DURATION}s" > "$bin_file"

    vegeta report --type=json < "$bin_file" > "$RESULTS_DIR/${profile}-run${run_num}.json"
    vegeta report --type=text < "$bin_file" > "$RESULTS_DIR/${profile}-run${run_num}.txt"

    local rps p99
    rps=$(python3 -c "import json; print(f'{json.load(open(\"$RESULTS_DIR/${profile}-run${run_num}.json\"))[\"throughput\"]:.1f}')")
    p99=$(python3 -c "import json; print(f'{json.load(open(\"$RESULTS_DIR/${profile}-run${run_num}.json\"))[\"latencies\"][\"99th\"]/1e6:.2f}')")
    echo "  Run $run_num: ${rps} req/s, p99=${p99}ms"
}

# --- Compute median from run files ---
compute_median() {
    local profile="$1"
    python3 -c "
import json, os

runs = []
for i in range(1, $RUNS + 1):
    path = '$RESULTS_DIR/${profile}-run' + str(i) + '.json'
    if os.path.exists(path):
        with open(path) as f:
            runs.append(json.load(f))

if not runs:
    print('  No runs found')
    exit(0)

def median(vals):
    s = sorted(vals)
    n = len(s)
    return s[n // 2]

rps_vals = [r['throughput'] for r in runs]
p50_vals = [r['latencies']['50th'] for r in runs]
p90_vals = [r['latencies']['90th'] for r in runs]
p95_vals = [r['latencies']['95th'] for r in runs]
p99_vals = [r['latencies']['99th'] for r in runs]
max_vals = [r['latencies']['max'] for r in runs]
mean_vals = [r['latencies']['mean'] for r in runs]
success_vals = [r['success'] for r in runs]

med = {
    'throughput': median(rps_vals),
    'latencies': {
        'mean': median(mean_vals),
        '50th': median(p50_vals),
        '90th': median(p90_vals),
        '95th': median(p95_vals),
        '99th': median(p99_vals),
        'max': median(max_vals),
    },
    'success': median(success_vals),
    'runs': len(runs),
    'duration_secs': $DURATION,
    'warmup_secs': $WARMUP,
    'profile': '$profile',
}

with open('$RESULTS_DIR/${profile}-median.json', 'w') as f:
    json.dump(med, f, indent=2)

rps = med['throughput']
p99 = med['latencies']['99th'] / 1e6
p50 = med['latencies']['50th'] / 1e6
success = med['success'] * 100
print(f'  Median ({len(runs)} runs): {rps:.0f} req/s, p50={p50:.2f}ms, p99={p99:.2f}ms, success={success:.2f}%')
"
}

# ====================== PRAXIS PROFILES ======================
echo ""
echo "--- praxis-simple (${RUNS} runs x ${DURATION}s) ---"
PRAXIS_CONFIG="$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-simple.yaml" \
    "$REPO_ROOT/target/release/praxis" >"$LOGS_DIR/praxis-simple.log" 2>&1 &
PROXY_PID=$!
for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$ADMIN_PORT/ready" >/dev/null 2>&1; then break; fi
    sleep 0.1
done
echo "Praxis ready"
for run in $(seq 1 "$RUNS"); do
    run_iteration "praxis-simple" "$PROXY_PORT" "$run"
done
compute_median "praxis-simple"
kill "$PROXY_PID" 2>/dev/null || true; wait "$PROXY_PID" 2>/dev/null || true; unset PROXY_PID
sleep 1

echo ""
echo "--- praxis-native (${RUNS} runs x ${DURATION}s) ---"
PRAXIS_CONFIG="$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-native.yaml" \
    "$REPO_ROOT/target/release/praxis" >"$LOGS_DIR/praxis-native.log" 2>&1 &
PROXY_PID=$!
for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$ADMIN_PORT/ready" >/dev/null 2>&1; then break; fi
    sleep 0.1
done
echo "Praxis ready"
for run in $(seq 1 "$RUNS"); do
    run_iteration "praxis-native" "$PROXY_PORT" "$run"
done
compute_median "praxis-native"
kill "$PROXY_PID" 2>/dev/null || true; wait "$PROXY_PID" 2>/dev/null || true; unset PROXY_PID
sleep 1

# ====================== ENVOY + GO EPP ======================
echo ""
echo "--- envoy-go-epp (${RUNS} runs x ${DURATION}s) ---"

EPP_TMPDIR=$(mktemp -d)
ENDPOINTS_ABS="$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-endpoints.yaml"
sed "s|PLACEHOLDER_ENDPOINTS_PATH|$ENDPOINTS_ABS|" \
    "$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-config.yaml" \
    > "$EPP_TMPDIR/epp-config.yaml"

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
    >"$LOGS_DIR/go-epp.log" 2>&1 &
EPP_PID=$!

for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$EPP_METRICS_PORT/metrics" >/dev/null 2>&1; then break; fi
    if ! kill -0 "$EPP_PID" 2>/dev/null; then
        echo "error: EPP exited"; tail -10 "$LOGS_DIR/go-epp.log"; exit 1
    fi
    sleep 0.2
done
echo "EPP ready"

docker rm -f "$ENVOY_CONTAINER" 2>/dev/null || true
ENVOY_CONFIG_ABS="$REPO_ROOT/benchmarks/comparison/configs/llmd/envoy-go-epp.yaml"
docker run --rm -d \
    --name "$ENVOY_CONTAINER" \
    --network host \
    -v "$ENVOY_CONFIG_ABS:/etc/envoy/envoy.yaml:ro" \
    "$ENVOY_IMAGE" \
    -c /etc/envoy/envoy.yaml --log-level warn \
    >/dev/null 2>&1

for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$ENVOY_ADMIN_PORT/ready" >/dev/null 2>&1; then break; fi
    sleep 0.2
done
echo "Envoy ready"
docker logs -f "$ENVOY_CONTAINER" >"$LOGS_DIR/envoy.log" 2>&1 &

# Verify e2e.
E2E=$(curl -sf -X POST "http://127.0.0.1:$ENVOY_PORT/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -d '{"model":"test-model","messages":[{"role":"user","content":"ping"}],"max_tokens":5}' 2>&1) || true
if echo "$E2E" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d.get('model')=='test-model'" 2>/dev/null; then
    echo "End-to-end verified"
else
    echo "error: e2e failed: $E2E"; exit 1
fi

for run in $(seq 1 "$RUNS"); do
    run_iteration "envoy-go-epp" "$ENVOY_PORT" "$run"
done
compute_median "envoy-go-epp"

rm -rf "$BENCH_TMP" "$EPP_TMPDIR"

# ====================== SUMMARY ======================
echo ""
echo "=== Summary (median of $RUNS runs, ${DURATION}s each) ==="
echo ""
python3 -c "
import json, os

profiles = ['praxis-simple', 'praxis-native', 'envoy-go-epp']
print(f'{'Profile':>16s}  {'RPS':>8s}  {'p50':>8s}  {'p95':>8s}  {'p99':>8s}  {'Success':>8s}')
print('-' * 60)
for p in profiles:
    path = '$RESULTS_DIR/' + p + '-median.json'
    if not os.path.exists(path):
        print(f'{p:>16s}  (no data)')
        continue
    with open(path) as f:
        d = json.load(f)
    rps = d['throughput']
    p50 = d['latencies']['50th'] / 1e6
    p95 = d['latencies']['95th'] / 1e6
    p99 = d['latencies']['99th'] / 1e6
    s = d['success'] * 100
    print(f'{p:>16s}  {rps:8.0f}  {p50:7.2f}ms {p95:7.2f}ms {p99:7.2f}ms  {s:7.2f}%')

# Comparison
simple = json.load(open('$RESULTS_DIR/praxis-simple-median.json')) if os.path.exists('$RESULTS_DIR/praxis-simple-median.json') else None
native = json.load(open('$RESULTS_DIR/praxis-native-median.json')) if os.path.exists('$RESULTS_DIR/praxis-native-median.json') else None
envoy = json.load(open('$RESULTS_DIR/envoy-go-epp-median.json')) if os.path.exists('$RESULTS_DIR/envoy-go-epp-median.json') else None

if native and envoy and envoy['throughput'] > 0:
    ratio = native['throughput'] / envoy['throughput']
    p99_ratio = envoy['latencies']['99th'] / native['latencies']['99th'] if native['latencies']['99th'] > 0 else 0
    print()
    print(f'praxis-native vs envoy-go-epp:')
    print(f'  Throughput: {ratio:.1f}x')
    print(f'  p99 latency: {p99_ratio:.1f}x lower')
"

echo ""
echo "Results in: $RESULTS_DIR/"
echo "=== Extended Benchmark Complete ==="
