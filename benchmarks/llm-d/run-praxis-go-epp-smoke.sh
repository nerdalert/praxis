#!/usr/bin/env bash
# Praxis + Go EPP benchmark smoke test (Track B).
#
# Runs the praxis-go-epp profile with the llmd-chat-small workload
# against a minimal Python mock backend.
#
# Usage:
#   ./benchmarks/llm-d/run-praxis-go-epp-smoke.sh [DURATION_SECS] [WARMUP_SECS]
#
# Positional arguments:
#   DURATION_SECS  measurement duration (default: 5)
#   WARMUP_SECS    warmup duration (default: 1)
#
# Prerequisites:
#   - vegeta
#   - python3 (stdlib only)
#   - Praxis built with --features ext-proc
#   - Go EPP binary: either at LLM_D_EPP_BIN or built from
#     the llm-d-router repo at LLM_D_ROUTER_REPO
#
# The request path is:
#   Vegeta -> Praxis llmd_external_epp (port 18090)
#     -> ext_proc gRPC -> Go EPP (port 9002)
#     -> x-gateway-destination-endpoint -> mock backend (port 18080)
#
# These numbers are control-path smoke only.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RESULTS_DIR="$REPO_ROOT/target/criterion/llmd-smoke"
LOGS_DIR="$RESULTS_DIR/logs"
DURATION="${1:-5}"
WARMUP="${2:-1}"

BACKEND_PORT=18080
PROXY_PORT=18090
ADMIN_PORT=9901
EPP_GRPC_PORT=9002
EPP_HEALTH_PORT=9003
EPP_METRICS_PORT=9090

LLM_D_EPP_BIN="${LLM_D_EPP_BIN:-}"
LLM_D_ROUTER_REPO="${LLM_D_ROUTER_REPO:-$REPO_ROOT/../../repos/llm-d-router}"

cleanup() {
    echo "Cleaning up..."
    if [ -n "${BACKEND_PID:-}" ]; then kill "$BACKEND_PID" 2>/dev/null || true; fi
    if [ -n "${EPP_PID:-}" ]; then kill "$EPP_PID" 2>/dev/null || true; fi
    if [ -n "${PROXY_PID:-}" ]; then kill "$PROXY_PID" 2>/dev/null || true; fi
    wait 2>/dev/null || true
}
trap cleanup EXIT

check_tool() {
    if ! command -v "$1" &>/dev/null; then
        echo "error: $1 not found. Install it first."
        exit 1
    fi
}

check_tool vegeta
check_tool python3

echo "=== Praxis + Go EPP Benchmark Smoke Test (Track B) ==="
echo "Duration: ${DURATION}s  Warmup: ${WARMUP}s"
echo ""

mkdir -p "$RESULTS_DIR" "$LOGS_DIR"

# --- Resolve EPP binary ---
if [ -z "$LLM_D_EPP_BIN" ]; then
    if [ -x /tmp/epp ]; then
        LLM_D_EPP_BIN="/tmp/epp"
        echo "Using cached EPP binary at $LLM_D_EPP_BIN"
    elif [ -d "$LLM_D_ROUTER_REPO" ]; then
        echo "Building EPP from $LLM_D_ROUTER_REPO..."
        (cd "$LLM_D_ROUTER_REPO" && go build -o /tmp/epp ./cmd/epp 2>&1 | tail -3)
        LLM_D_EPP_BIN="/tmp/epp"
    else
        echo "error: no EPP binary found."
        echo "  Set LLM_D_EPP_BIN=/path/to/epp or"
        echo "  Set LLM_D_ROUTER_REPO=/path/to/llm-d-router"
        exit 1
    fi
fi
echo "EPP binary: $LLM_D_EPP_BIN"

# --- Build Praxis (requires ext-proc feature) ---
if [ -x "$REPO_ROOT/target/release/praxis" ]; then
    echo "Using existing Praxis binary."
else
    echo "Building Praxis (ext-proc)..."
    cargo build --release -p praxis --features ext-proc --quiet 2>&1 | tail -3
fi

# --- Start mock backend ---
echo "Starting mock backend on port $BACKEND_PORT..."
python3 -c "
import http.server, json

RESPONSE = json.dumps({
    'id': 'chatcmpl-smoke',
    'object': 'chat.completion',
    'model': 'test-model',
    'choices': [{
        'index': 0,
        'message': {'role': 'assistant', 'content': 'Hello from mock.'},
        'finish_reason': 'stop'
    }],
    'usage': {'prompt_tokens': 10, 'completion_tokens': 5, 'total_tokens': 15}
}).encode()

class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        content_length = int(self.headers.get('Content-Length', 0))
        self.rfile.read(content_length)
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(RESPONSE)))
        self.end_headers()
        self.wfile.write(RESPONSE)
    def do_GET(self):
        body = b'ok'
        self.send_response(200)
        self.send_header('Content-Type', 'text/plain')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, format, *args):
        pass

server = http.server.ThreadingHTTPServer(('127.0.0.1', $BACKEND_PORT), Handler)
server.request_queue_size = 128
server.serve_forever()
" &
BACKEND_PID=$!

for _ in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:$BACKEND_PORT/" >/dev/null 2>&1; then break; fi
    sleep 0.1
done
echo "Backend ready (PID $BACKEND_PID)"

# --- Prepare EPP config ---
EPP_TMPDIR=$(mktemp -d)
ENDPOINTS_ABS="$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-endpoints.yaml"
sed "s|PLACEHOLDER_ENDPOINTS_PATH|$ENDPOINTS_ABS|" \
    "$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-config.yaml" \
    > "$EPP_TMPDIR/epp-config.yaml"

# --- Start Go EPP ---
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
    >"$LOGS_DIR/praxis-go-epp-epp.log" 2>&1 &
EPP_PID=$!

for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$EPP_METRICS_PORT/metrics" >/dev/null 2>&1; then break; fi
    if ! kill -0 "$EPP_PID" 2>/dev/null; then
        echo "error: EPP exited during startup"
        tail -20 "$LOGS_DIR/praxis-go-epp-epp.log"
        exit 1
    fi
    sleep 0.2
done
echo "EPP ready (PID $EPP_PID)"

# --- Start Praxis ---
PROFILE="praxis-go-epp"
CONFIG="$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-go-epp.yaml"

echo "Starting Praxis (praxis-go-epp) on port $PROXY_PORT..."
PRAXIS_CONFIG="$CONFIG" "$REPO_ROOT/target/release/praxis" \
    >"$LOGS_DIR/${PROFILE}.log" 2>&1 &
PROXY_PID=$!

for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$ADMIN_PORT/ready" >/dev/null 2>&1; then break; fi
    if ! kill -0 "$PROXY_PID" 2>/dev/null; then
        echo "error: Praxis exited during startup"
        tail -20 "$LOGS_DIR/${PROFILE}.log"
        exit 1
    fi
    sleep 0.1
done
echo "Praxis ready (PID $PROXY_PID)"

# --- Verify E2E path ---
echo "Verifying E2E path..."
VERIFY=$(curl -sf -X POST \
    -H "Content-Type: application/json" \
    "http://127.0.0.1:$PROXY_PORT/v1/chat/completions" \
    -d '{"model":"test-model","messages":[{"role":"user","content":"verify"}],"max_tokens":5}' 2>&1)
if echo "$VERIFY" | grep -q "chat.completion"; then
    echo "E2E verified: response received through Praxis -> EPP -> backend"
else
    echo "error: E2E verification failed"
    echo "$VERIFY"
    exit 1
fi

# --- Vegeta benchmark ---
TMPDIR_BENCH=$(mktemp -d)
cat > "$TMPDIR_BENCH/body.json" << 'BODY'
{"model":"test-model","messages":[{"role":"user","content":"Hello, how are you?"}],"max_tokens":50}
BODY

cat > "$TMPDIR_BENCH/targets.txt" << TARGETS
POST http://127.0.0.1:$PROXY_PORT/v1/chat/completions
Content-Type: application/json
@${TMPDIR_BENCH}/body.json
TARGETS

if [ "$WARMUP" -gt 0 ]; then
    echo "Warmup (${WARMUP}s)..."
    vegeta attack -targets "$TMPDIR_BENCH/targets.txt" \
        -rate 0 -max-workers 8 \
        -duration "${WARMUP}s" > /dev/null 2>&1 || true
fi

echo "Measuring (${DURATION}s)..."
BIN_FILE="$RESULTS_DIR/${PROFILE}.bin"
vegeta attack -targets "$TMPDIR_BENCH/targets.txt" \
    -rate 0 -max-workers 16 \
    -duration "${DURATION}s" > "$BIN_FILE"

vegeta report --type=json < "$BIN_FILE" > "$RESULTS_DIR/${PROFILE}.json"
vegeta report --type=text < "$BIN_FILE" > "$RESULTS_DIR/${PROFILE}.txt"

# Generate structured YAML.
python3 -c "
import json, sys
from datetime import datetime, timezone

with open('$RESULTS_DIR/${PROFILE}.json') as f:
    data = json.load(f)

lat = data.get('latencies', {})

lines = []
lines.append('timestamp: \"' + datetime.now(timezone.utc).isoformat() + '\"')
lines.append('commit: \"$(cd "$REPO_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo unknown)\"')
lines.append('profile: \"$PROFILE\"')
lines.append('workload: \"llmd-chat-small\"')
lines.append('execution_mode: \"local\"')
lines.append('backend_type: \"minimal-mock\"')
lines.append('metrics_type: \"mock-static\"')
lines.append('proxy: \"praxis\"')
lines.append('epp: \"go-epp-external\"')
lines.append('tool: \"vegeta\"')
lines.append('latency:')
lines.append('  mean_ms: ' + str(lat.get('mean', 0) / 1e6))
lines.append('  p50_ms: ' + str(lat.get('50th', 0) / 1e6))
lines.append('  p90_ms: ' + str(lat.get('90th', 0) / 1e6))
lines.append('  p95_ms: ' + str(lat.get('95th', 0) / 1e6))
lines.append('  p99_ms: ' + str(lat.get('99th', 0) / 1e6))
lines.append('  max_ms: ' + str(lat.get('max', 0) / 1e6))
lines.append('throughput:')
lines.append('  requests_per_sec: ' + str(data.get('throughput', 0)))
lines.append('errors:')
lines.append('  success_rate: ' + str(data.get('success', 0)))
lines.append('duration_secs: $DURATION')
lines.append('warmup_secs: $WARMUP')
lines.append('command: \"benchmarks/llm-d/run-praxis-go-epp-smoke.sh $DURATION $WARMUP\"')

print('\n'.join(lines))
" > "$RESULTS_DIR/${PROFILE}.yaml"

echo ""
echo "=== Results: $PROFILE ==="
cat "$RESULTS_DIR/${PROFILE}.txt"
echo ""
echo "Results saved to: $RESULTS_DIR/${PROFILE}.{json,txt,yaml}"
