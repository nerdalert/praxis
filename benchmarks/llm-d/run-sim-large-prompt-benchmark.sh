#!/usr/bin/env bash
# llm-d large-prompt benchmark with llm-d-inference-sim backend.
#
# Runs praxis-simple, praxis-native, and envoy-go-epp profiles
# with configurable prompt sizes against llm-d-inference-sim.
#
# Usage:
#   ./benchmarks/llm-d/run-sim-large-prompt-benchmark.sh [DURATION] [WARMUP] [RUNS]
#
# Positional arguments:
#   DURATION  measurement duration per run in seconds (default: 30)
#   WARMUP    warmup duration in seconds (default: 5)
#   RUNS      number of repetitions per profile/size (default: 3)
#
# Environment:
#   LLM_D_SIM_BIN       path to llm-d-inference-sim binary
#   LLM_D_SIM_REPO      path to llm-d-inference-sim source (default: ../../repos/llm-d-inference-sim)
#   LLM_D_EPP_BIN       path to Go EPP binary
#   LLM_D_ROUTER_REPO   path to llm-d-router source (default: ../../repos/llm-d-router)
#   PROMPT_SIZES         space-separated sizes in bytes (default: "16384 65536 262144")

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RESULTS_DIR="$REPO_ROOT/target/criterion/llmd-sim-large-prompt"
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
ENVOY_IMAGE="envoyproxy/envoy:distroless-v1.33.2"
ENVOY_CONTAINER="llmd-bench-envoy"
MODEL_NAME="test-model"

LLM_D_SIM_BIN="${LLM_D_SIM_BIN:-}"
LLM_D_SIM_REPO="${LLM_D_SIM_REPO:-$REPO_ROOT/../../repos/llm-d-inference-sim}"
LLM_D_EPP_BIN="${LLM_D_EPP_BIN:-}"
LLM_D_ROUTER_REPO="${LLM_D_ROUTER_REPO:-$REPO_ROOT/../../repos/llm-d-router}"

cleanup() {
    if [ -n "${SIM_PID:-}" ]; then kill "$SIM_PID" 2>/dev/null || true; fi
    if [ -n "${PROXY_PID:-}" ]; then kill "$PROXY_PID" 2>/dev/null || true; fi
    if [ -n "${EPP_PID:-}" ]; then kill "$EPP_PID" 2>/dev/null || true; fi
    docker rm -f "$ENVOY_CONTAINER" 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT

check_tool() {
    if ! command -v "$1" &>/dev/null; then echo "error: $1 not found."; exit 1; fi
}

check_tool vegeta
check_tool python3
check_tool docker

# Human-readable size label.
size_label() {
    local bytes="$1"
    if [ "$bytes" -ge 262144 ]; then echo "256k"
    elif [ "$bytes" -ge 65536 ]; then echo "64k"
    elif [ "$bytes" -ge 16384 ]; then echo "16k"
    else echo "${bytes}b"; fi
}

echo "=== llm-d Large-Prompt Benchmark (llm-d-inference-sim) ==="
echo "Duration: ${DURATION}s  Warmup: ${WARMUP}s  Runs: ${RUNS}"
echo "Prompt sizes: ${PROMPT_SIZES}"
echo ""

mkdir -p "$RESULTS_DIR" "$LOGS_DIR"

# --- Resolve binaries ---
if [ -z "$LLM_D_SIM_BIN" ]; then
    if [ -x /tmp/llm-d-inference-sim ]; then LLM_D_SIM_BIN="/tmp/llm-d-inference-sim"
    elif [ -d "$LLM_D_SIM_REPO" ]; then
        echo "Building simulator..."; (cd "$LLM_D_SIM_REPO" && make build 2>&1 | tail -3)
        cp "$LLM_D_SIM_REPO/bin/llm-d-inference-sim" /tmp/llm-d-inference-sim; LLM_D_SIM_BIN="/tmp/llm-d-inference-sim"
    else echo "error: no simulator binary."; exit 1; fi
fi
if [ -z "$LLM_D_EPP_BIN" ]; then
    if [ -x /tmp/epp ]; then LLM_D_EPP_BIN="/tmp/epp"
    elif [ -d "$LLM_D_ROUTER_REPO" ]; then
        echo "Building EPP..."; (cd "$LLM_D_ROUTER_REPO" && go build -o /tmp/epp ./cmd/epp 2>&1 | tail -3)
        LLM_D_EPP_BIN="/tmp/epp"
    else echo "error: no EPP binary."; exit 1; fi
fi

# --- Start simulator ---
echo "Starting llm-d-inference-sim..."
"$LLM_D_SIM_BIN" --model "$MODEL_NAME" --port "$BACKEND_PORT" --mode echo \
    --max-num-seqs 256 --max-model-len 131072 \
    >"$LOGS_DIR/simulator.log" 2>&1 &
SIM_PID=$!
for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$BACKEND_PORT/health" >/dev/null 2>&1; then break; fi
    if ! kill -0 "$SIM_PID" 2>/dev/null; then echo "error: simulator exited"; tail -10 "$LOGS_DIR/simulator.log"; exit 1; fi
    sleep 0.2
done
echo "Simulator ready"

# --- Build Praxis ---
if [ -x "$REPO_ROOT/target/release/praxis" ]; then echo "Using existing Praxis binary."
else echo "Building Praxis..."; cargo build --release -p praxis --quiet 2>&1 | tail -3; fi

# --- Generate request bodies ---
BENCH_TMP=$(mktemp -d)
for size_bytes in $PROMPT_SIZES; do
    label=$(size_label "$size_bytes")
    python3 -c "
import json
padding = 'x' * $size_bytes
body = json.dumps({'model': '$MODEL_NAME', 'messages': [{'role': 'user', 'content': padding}], 'max_tokens': 10})
with open('$BENCH_TMP/body-${label}.json', 'w') as f: f.write(body)
print(f'Generated body-${label}.json: {len(body)} bytes')
"
done

make_targets() {
    local port="$1" label="$2"
    local tfile="$BENCH_TMP/targets-${port}-${label}.txt"
    cat > "$tfile" << TARGETS
POST http://127.0.0.1:${port}/v1/chat/completions
Content-Type: application/json
@${BENCH_TMP}/body-${label}.json
TARGETS
    echo "$tfile"
}

run_iteration() {
    local profile="$1" port="$2" label="$3" run_num="$4"
    local tag="${profile}-${label}"
    local targets_file
    targets_file=$(make_targets "$port" "$label")

    if [ "$WARMUP" -gt 0 ]; then
        vegeta attack -targets "$targets_file" -rate 0 -max-workers 8 \
            -duration "${WARMUP}s" > /dev/null 2>&1 || true
    fi

    local bin_file="$RESULTS_DIR/${tag}-run${run_num}.bin"
    vegeta attack -targets "$targets_file" -rate 0 -max-workers 16 \
        -duration "${DURATION}s" > "$bin_file"

    vegeta report --type=json < "$bin_file" > "$RESULTS_DIR/${tag}-run${run_num}.json"
    vegeta report --type=text < "$bin_file" > "$RESULTS_DIR/${tag}-run${run_num}.txt"

    local rps p99
    rps=$(python3 -c "import json; print(f'{json.load(open(\"$RESULTS_DIR/${tag}-run${run_num}.json\"))[\"throughput\"]:.1f}')")
    p99=$(python3 -c "import json; print(f'{json.load(open(\"$RESULTS_DIR/${tag}-run${run_num}.json\"))[\"latencies\"][\"99th\"]/1e6:.2f}')")
    echo "    Run $run_num: ${rps} req/s, p99=${p99}ms"
}

compute_median() {
    local tag="$1"
    python3 -c "
import json, os
runs = []
for i in range(1, $RUNS + 1):
    p = '$RESULTS_DIR/${tag}-run' + str(i) + '.json'
    if os.path.exists(p):
        with open(p) as f: runs.append(json.load(f))
if not runs: print('    No runs'); exit(0)
def med(v): s = sorted(v); return s[len(s)//2]
m = {
    'throughput': med([r['throughput'] for r in runs]),
    'latencies': {k: med([r['latencies'][k] for r in runs]) for k in ['mean','50th','90th','95th','99th','max']},
    'success': med([r['success'] for r in runs]),
    'runs': len(runs), 'duration_secs': $DURATION, 'warmup_secs': $WARMUP,
    'tag': '$tag', 'backend': 'llm-d-inference-sim', 'sim_mode': 'echo',
}
with open('$RESULTS_DIR/${tag}-median.json', 'w') as f: json.dump(m, f, indent=2)
rps=m['throughput']; p50=m['latencies']['50th']/1e6; p99=m['latencies']['99th']/1e6
print(f'    Median: {rps:.0f} req/s, p50={p50:.2f}ms, p99={p99:.2f}ms')
"
}

verify_request() {
    local port="$1" label="$2" profile="$3"
    local body_file="$BENCH_TMP/body-${label}.json"
    local status
    status=$(curl -sf -o /dev/null -w "%{http_code}" -X POST "http://127.0.0.1:${port}/v1/chat/completions" \
        -H "Content-Type: application/json" -d "@${body_file}" 2>&1) || true
    if [ "$status" = "200" ]; then
        echo "  Verified ${profile} ${label}: HTTP 200"
    else
        echo "  WARNING: ${profile} ${label} returned HTTP ${status}"
    fi
}

run_profile_all_sizes() {
    local profile="$1" port="$2"
    for size_bytes in $PROMPT_SIZES; do
        local label
        label=$(size_label "$size_bytes")
        echo "  --- ${profile} @ ${label} ---"
        verify_request "$port" "$label" "$profile"
        for run in $(seq 1 "$RUNS"); do
            run_iteration "$profile" "$port" "$label" "$run"
        done
        compute_median "${profile}-${label}"
    done
}

# ====================== PRAXIS-SIMPLE ======================
echo ""
echo "=== praxis-simple ==="
PRAXIS_CONFIG="$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-simple.yaml" \
    "$REPO_ROOT/target/release/praxis" >"$LOGS_DIR/praxis-simple.log" 2>&1 &
PROXY_PID=$!
for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$ADMIN_PORT/ready" >/dev/null 2>&1; then break; fi; sleep 0.1
done
echo "Praxis ready"
run_profile_all_sizes "praxis-simple" "$PROXY_PORT"
kill "$PROXY_PID" 2>/dev/null || true; wait "$PROXY_PID" 2>/dev/null || true; unset PROXY_PID
sleep 1

# ====================== PRAXIS-NATIVE ======================
echo ""
echo "=== praxis-native ==="
PRAXIS_CONFIG="$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-native.yaml" \
    "$REPO_ROOT/target/release/praxis" >"$LOGS_DIR/praxis-native.log" 2>&1 &
PROXY_PID=$!
for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$ADMIN_PORT/ready" >/dev/null 2>&1; then break; fi; sleep 0.1
done
echo "Praxis ready"
run_profile_all_sizes "praxis-native" "$PROXY_PORT"
kill "$PROXY_PID" 2>/dev/null || true; wait "$PROXY_PID" 2>/dev/null || true; unset PROXY_PID
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
for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$EPP_METRICS_PORT/metrics" >/dev/null 2>&1; then break; fi
    if ! kill -0 "$EPP_PID" 2>/dev/null; then echo "error: EPP exited"; tail -10 "$LOGS_DIR/go-epp.log"; exit 1; fi
    sleep 0.2
done
echo "EPP ready"

docker rm -f "$ENVOY_CONTAINER" 2>/dev/null || true
ENVOY_CONFIG_ABS="$REPO_ROOT/benchmarks/comparison/configs/llmd/envoy-go-epp.yaml"
docker run --rm -d --name "$ENVOY_CONTAINER" --network host \
    -v "$ENVOY_CONFIG_ABS:/etc/envoy/envoy.yaml:ro" \
    "$ENVOY_IMAGE" -c /etc/envoy/envoy.yaml --log-level warn >/dev/null 2>&1
for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$ENVOY_ADMIN_PORT/ready" >/dev/null 2>&1; then break; fi; sleep 0.2
done
echo "Envoy ready"
docker logs -f "$ENVOY_CONTAINER" >"$LOGS_DIR/envoy.log" 2>&1 &

run_profile_all_sizes "envoy-go-epp" "$ENVOY_PORT"

rm -rf "$BENCH_TMP" "$EPP_TMPDIR"

# ====================== SUMMARY ======================
echo ""
echo "=== Summary (median of $RUNS runs, ${DURATION}s each, backend=llm-d-inference-sim echo) ==="
echo ""
python3 -c "
import json, os

sizes = ['16k', '64k', '256k']
profiles = ['praxis-simple', 'praxis-native', 'envoy-go-epp']

for sz in sizes:
    print(f'--- Prompt size: {sz} ---')
    print(f'{\"Profile\":>16s}  {\"RPS\":>8s}  {\"p50\":>8s}  {\"p95\":>8s}  {\"p99\":>8s}  {\"Success\":>8s}')
    print('-' * 64)
    for p in profiles:
        path = '$RESULTS_DIR/' + p + '-' + sz + '-median.json'
        if not os.path.exists(path): print(f'{p:>16s}  (no data)'); continue
        with open(path) as f: d = json.load(f)
        rps=d['throughput']; p50=d['latencies']['50th']/1e6; p95=d['latencies']['95th']/1e6
        p99=d['latencies']['99th']/1e6; s=d['success']*100
        print(f'{p:>16s}  {rps:8.0f}  {p50:7.2f}ms {p95:7.2f}ms {p99:7.2f}ms  {s:7.2f}%')

    native_path = '$RESULTS_DIR/praxis-native-' + sz + '-median.json'
    envoy_path = '$RESULTS_DIR/envoy-go-epp-' + sz + '-median.json'
    if os.path.exists(native_path) and os.path.exists(envoy_path):
        n = json.load(open(native_path)); e = json.load(open(envoy_path))
        if e['throughput'] > 0:
            ratio = n['throughput'] / e['throughput']
            p99r = e['latencies']['99th'] / n['latencies']['99th'] if n['latencies']['99th'] > 0 else 0
            print(f'  praxis-native vs envoy-go-epp: {ratio:.1f}x throughput, {p99r:.1f}x lower p99')
    print()
"
echo "Results in: $RESULTS_DIR/"
echo "=== Large-Prompt Benchmark Complete ==="
