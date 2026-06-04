#!/usr/bin/env bash
# llm-d benchmark smoke test.
#
# Runs praxis-simple and praxis-native profiles with the
# llmd-chat-small workload against a minimal Python mock backend.
#
# Usage:
#   ./benchmarks/llm-d/run-smoke.sh [DURATION_SECS] [WARMUP_SECS]
#
# Positional arguments:
#   DURATION_SECS  measurement duration per profile (default: 5)
#   WARMUP_SECS    warmup duration per profile (default: 1)
#
# Prerequisites:
#   - vegeta (https://github.com/tsenart/vegeta)
#   - python3 (stdlib only, no pip packages)
#   - cargo (Rust toolchain) or a pre-built target/release/praxis
#
# The mock backend returns a static OpenAI chat completion response.
# These numbers are control-path smoke only and do not reflect real
# inference latency or llm-d-inference-sim behavior.

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

cleanup() {
    if [ -n "${BACKEND_PID:-}" ]; then kill "$BACKEND_PID" 2>/dev/null || true; fi
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
check_tool cargo

echo "=== llm-d Benchmark Smoke Test ==="
echo "Duration: ${DURATION}s  Warmup: ${WARMUP}s"
echo ""

mkdir -p "$RESULTS_DIR" "$LOGS_DIR"

# Start minimal mock backend (stdlib only, no pip packages).
echo "Starting mock backend on port $BACKEND_PORT..."
python3 -c "
import http.server, json

RESPONSE = json.dumps({
    'id': 'chatcmpl-smoke',
    'object': 'chat.completion',
    'model': 'test-model',
    'choices': [{
        'index': 0,
        'message': {'role': 'assistant', 'content': 'Hello! I am a mock response.'},
        'finish_reason': 'stop'
    }],
    'usage': {'prompt_tokens': 10, 'completion_tokens': 8, 'total_tokens': 18}
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
        self.send_response(200)
        self.send_header('Content-Type', 'text/plain')
        self.end_headers()
        self.wfile.write(b'ok')
    def log_message(self, format, *args):
        pass

http.server.HTTPServer(('127.0.0.1', $BACKEND_PORT), Handler).serve_forever()
" &
BACKEND_PID=$!

for _ in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:$BACKEND_PORT/" >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done
echo "Backend ready (PID $BACKEND_PID)"

# Build Praxis (skip if binary already exists).
if [ -x "$REPO_ROOT/target/release/praxis" ]; then
    echo "Using existing Praxis binary."
else
    echo "Building Praxis..."
    cargo build --release -p praxis --quiet 2>&1 | tail -3
fi

run_profile() {
    local profile="$1"
    local config="$2"

    echo ""
    echo "--- Profile: $profile ---"

    PRAXIS_CONFIG="$config" "$REPO_ROOT/target/release/praxis" \
        >"$LOGS_DIR/${profile}.log" 2>&1 &
    PROXY_PID=$!

    for _ in $(seq 1 60); do
        if curl -sf "http://127.0.0.1:$ADMIN_PORT/ready" >/dev/null 2>&1; then
            break
        fi
        if ! kill -0 "$PROXY_PID" 2>/dev/null; then
            echo "error: Praxis exited during startup for profile $profile"
            echo "  log: $LOGS_DIR/${profile}.log"
            return 1
        fi
        sleep 0.1
    done

    if ! curl -sf "http://127.0.0.1:$ADMIN_PORT/ready" >/dev/null 2>&1; then
        echo "error: Praxis did not become ready for profile $profile"
        echo "  log: $LOGS_DIR/${profile}.log"
        kill "$PROXY_PID" 2>/dev/null || true
        wait "$PROXY_PID" 2>/dev/null || true
        return 1
    fi

    echo "Praxis ready (PID $PROXY_PID)"

    local tmpdir
    tmpdir=$(mktemp -d)
    cat > "$tmpdir/body.json" << 'BODY'
{"model":"test-model","messages":[{"role":"user","content":"Hello, how are you?"}],"max_tokens":50}
BODY

    cat > "$tmpdir/targets.txt" << TARGETS
POST http://127.0.0.1:$PROXY_PORT/v1/chat/completions
Content-Type: application/json
@${tmpdir}/body.json
TARGETS

    # Warmup (discard output).
    if [ "$WARMUP" -gt 0 ]; then
        echo "Warmup (${WARMUP}s)..."
        vegeta attack -targets "$tmpdir/targets.txt" \
            -rate 0 -max-workers 8 \
            -duration "${WARMUP}s" > /dev/null 2>&1 || true
    fi

    # Single measurement attack — save binary output once.
    echo "Measuring (${DURATION}s)..."
    local bin_file="$RESULTS_DIR/${profile}.bin"
    vegeta attack -targets "$tmpdir/targets.txt" \
        -rate 0 -max-workers 16 \
        -duration "${DURATION}s" > "$bin_file"

    # Generate all output formats from the same sample.
    vegeta report --type=json   < "$bin_file" > "$RESULTS_DIR/${profile}.json"
    vegeta report --type=text   < "$bin_file" > "$RESULTS_DIR/${profile}.txt"

    # Generate structured YAML from the JSON (stdlib only).
    python3 -c "
import json, sys
from datetime import datetime, timezone

with open('$RESULTS_DIR/${profile}.json') as f:
    data = json.load(f)

lat = data.get('latencies', {})

lines = []
lines.append('timestamp: \"' + datetime.now(timezone.utc).isoformat() + '\"')
lines.append('commit: \"$(cd "$REPO_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo unknown)\"')
lines.append('profile: \"$profile\"')
lines.append('workload: \"llmd-chat-small\"')
lines.append('execution_mode: \"local\"')
lines.append('backend_type: \"minimal-mock\"')
lines.append('metrics_type: \"mock-static\"')
lines.append('proxy: \"praxis\"')
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
lines.append('command: \"benchmarks/llm-d/run-smoke.sh $DURATION $WARMUP\"')

with open('$RESULTS_DIR/${profile}.yaml', 'w') as f:
    f.write('\n'.join(lines) + '\n')
"

    echo ""
    cat "$RESULTS_DIR/${profile}.txt"
    echo ""
    echo "Artifacts:"
    echo "  JSON: $RESULTS_DIR/${profile}.json"
    echo "  YAML: $RESULTS_DIR/${profile}.yaml"
    echo "  text: $RESULTS_DIR/${profile}.txt"
    echo "  log:  $LOGS_DIR/${profile}.log"

    kill "$PROXY_PID" 2>/dev/null || true
    wait "$PROXY_PID" 2>/dev/null || true
    unset PROXY_PID

    rm -rf "$tmpdir"
}

run_profile "praxis-simple" "$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-simple.yaml"
run_profile "praxis-native" "$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-native.yaml"

echo ""
echo "=== Smoke Test Complete ==="
echo "Results in: $RESULTS_DIR/"
ls -la "$RESULTS_DIR/" 2>/dev/null
