#!/usr/bin/env bash
# Envoy + Go EPP benchmark smoke test.
#
# Runs the envoy-go-epp profile with the llmd-chat-small workload
# against a minimal Python mock backend.
#
# Usage:
#   ./benchmarks/llm-d/run-envoy-go-epp-smoke.sh [DURATION_SECS] [WARMUP_SECS]
#
# Positional arguments:
#   DURATION_SECS  measurement duration (default: 5)
#   WARMUP_SECS    warmup duration (default: 1)
#
# Prerequisites:
#   - vegeta
#   - python3 (stdlib only)
#   - docker (for Envoy)
#   - Go EPP binary: either at LLM_D_EPP_BIN or built from
#     the llm-d-router repo at LLM_D_ROUTER_REPO
#
# The request path is:
#   Vegeta -> Envoy (Docker, port 18091) -> ext_proc -> Go EPP (port 9002)
#     -> ORIGINAL_DST -> mock backend (port 18080)
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
ENVOY_PORT=18091
EPP_GRPC_PORT=9002
EPP_HEALTH_PORT=9003
EPP_METRICS_PORT=9090
ENVOY_ADMIN_PORT=19000

ENVOY_IMAGE="envoyproxy/envoy:distroless-v1.33.2"
ENVOY_CONTAINER="llmd-bench-envoy"

# EPP binary: use env var, or build from llm-d-router repo.
LLM_D_EPP_BIN="${LLM_D_EPP_BIN:-}"
LLM_D_ROUTER_REPO="${LLM_D_ROUTER_REPO:-$REPO_ROOT/../../repos/llm-d-router}"

cleanup() {
    echo "Cleaning up..."
    if [ -n "${BACKEND_PID:-}" ]; then kill "$BACKEND_PID" 2>/dev/null || true; fi
    if [ -n "${EPP_PID:-}" ]; then kill "$EPP_PID" 2>/dev/null || true; fi
    docker rm -f "$ENVOY_CONTAINER" 2>/dev/null || true
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
check_tool docker

echo "=== Envoy + Go EPP Benchmark Smoke Test ==="
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

http.server.HTTPServer(('127.0.0.1', $BACKEND_PORT), Handler).serve_forever()
" &
BACKEND_PID=$!

for _ in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:$BACKEND_PORT/" >/dev/null 2>&1; then break; fi
    sleep 0.1
done
echo "Backend ready (PID $BACKEND_PID)"

# --- Prepare EPP config with resolved path ---
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
    >"$LOGS_DIR/go-epp.log" 2>&1 &
EPP_PID=$!

# Wait for EPP gRPC to be ready.
for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$EPP_METRICS_PORT/metrics" >/dev/null 2>&1; then break; fi
    if ! kill -0 "$EPP_PID" 2>/dev/null; then
        echo "error: EPP exited during startup"
        echo "  log: $LOGS_DIR/go-epp.log"
        tail -20 "$LOGS_DIR/go-epp.log"
        exit 1
    fi
    sleep 0.2
done
echo "EPP ready (PID $EPP_PID)"

# --- Start Envoy ---
echo "Starting Envoy on port $ENVOY_PORT..."
docker rm -f "$ENVOY_CONTAINER" 2>/dev/null || true

ENVOY_CONFIG_ABS="$REPO_ROOT/benchmarks/comparison/configs/llmd/envoy-go-epp.yaml"
docker run --rm -d \
    --name "$ENVOY_CONTAINER" \
    --network host \
    -v "$ENVOY_CONFIG_ABS:/etc/envoy/envoy.yaml:ro" \
    "$ENVOY_IMAGE" \
    -c /etc/envoy/envoy.yaml --log-level warn \
    >"$LOGS_DIR/envoy-start.log" 2>&1

# Wait for Envoy ready.
for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$ENVOY_ADMIN_PORT/ready" >/dev/null 2>&1; then break; fi
    sleep 0.2
done

if ! curl -sf "http://127.0.0.1:$ENVOY_ADMIN_PORT/ready" >/dev/null 2>&1; then
    echo "error: Envoy did not become ready"
    echo "  container logs:"
    docker logs "$ENVOY_CONTAINER" 2>&1 | tail -20
    exit 1
fi
echo "Envoy ready (container $ENVOY_CONTAINER)"

# Capture Envoy logs in background.
docker logs -f "$ENVOY_CONTAINER" >"$LOGS_DIR/envoy.log" 2>&1 &

# --- Verify end-to-end path ---
echo "Verifying end-to-end: Vegeta -> Envoy -> EPP -> backend..."
E2E_RESPONSE=$(curl -sf -X POST "http://127.0.0.1:$ENVOY_PORT/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -d '{"model":"test-model","messages":[{"role":"user","content":"ping"}],"max_tokens":5}' 2>&1) || true

if echo "$E2E_RESPONSE" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d.get('model')=='test-model'" 2>/dev/null; then
    echo "End-to-end path verified: got valid chat completion response."
else
    echo "error: end-to-end verification failed"
    echo "  response: $E2E_RESPONSE"
    echo "  EPP log tail:"
    tail -10 "$LOGS_DIR/go-epp.log"
    exit 1
fi

# --- Benchmark ---
TMPDIR_BENCH=$(mktemp -d)
cat > "$TMPDIR_BENCH/body.json" << 'BODY'
{"model":"test-model","messages":[{"role":"user","content":"Hello, how are you?"}],"max_tokens":50}
BODY

cat > "$TMPDIR_BENCH/targets.txt" << TARGETS
POST http://127.0.0.1:$ENVOY_PORT/v1/chat/completions
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
BIN_FILE="$RESULTS_DIR/envoy-go-epp.bin"
vegeta attack -targets "$TMPDIR_BENCH/targets.txt" \
    -rate 0 -max-workers 16 \
    -duration "${DURATION}s" > "$BIN_FILE"

vegeta report --type=json < "$BIN_FILE" > "$RESULTS_DIR/envoy-go-epp.json"
vegeta report --type=text < "$BIN_FILE" > "$RESULTS_DIR/envoy-go-epp.txt"

# Generate YAML (stdlib only).
python3 -c "
import json
from datetime import datetime, timezone

with open('$RESULTS_DIR/envoy-go-epp.json') as f:
    data = json.load(f)

lat = data.get('latencies', {})
lines = []
lines.append('timestamp: \"' + datetime.now(timezone.utc).isoformat() + '\"')
lines.append('commit: \"n/a\"')
lines.append('profile: \"envoy-go-epp\"')
lines.append('workload: \"llmd-chat-small\"')
lines.append('execution_mode: \"local\"')
lines.append('backend_type: \"minimal-mock\"')
lines.append('metrics_type: \"mock-static\"')
lines.append('proxy: \"envoy-go-epp\"')
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
lines.append('command: \"benchmarks/llm-d/run-envoy-go-epp-smoke.sh $DURATION $WARMUP\"')
lines.append('envoy_image: \"$ENVOY_IMAGE\"')

with open('$RESULTS_DIR/envoy-go-epp.yaml', 'w') as f:
    f.write('\n'.join(lines) + '\n')
"

echo ""
cat "$RESULTS_DIR/envoy-go-epp.txt"
echo ""
echo "Artifacts:"
echo "  JSON: $RESULTS_DIR/envoy-go-epp.json"
echo "  YAML: $RESULTS_DIR/envoy-go-epp.yaml"
echo "  text: $RESULTS_DIR/envoy-go-epp.txt"
echo "  Envoy log: $LOGS_DIR/envoy.log"
echo "  EPP log:   $LOGS_DIR/go-epp.log"

rm -rf "$TMPDIR_BENCH" "$EPP_TMPDIR"

echo ""
echo "=== Envoy + Go EPP Smoke Test Complete ==="
