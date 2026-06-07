#!/usr/bin/env bash
# Same-backend Track B benchmark.
#
# Runs the profiles available in the Track B branch against one Go
# net/http mock backend:
#
#   - praxis-simple
#   - praxis-go-epp
#   - envoy-go-epp
#
# This intentionally excludes praxis-native because the Track B branch
# does not contain the Track A llmd_endpoint_picker filter.
#
# Usage:
#   ./benchmarks/llm-d/run-same-backend-benchmark.sh [DURATION] [WARMUP] [RUNS]
#
# Defaults match the Track A extended benchmark:
#   DURATION=30, WARMUP=5, RUNS=3

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RESULTS_DIR="$REPO_ROOT/target/criterion/llmd-same-backend"
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

ENVOY_IMAGE="${ENVOY_IMAGE:-envoyproxy/envoy:distroless-v1.33.2}"
ENVOY_CONTAINER="${ENVOY_CONTAINER:-llmd-same-backend-envoy}"
LLM_D_EPP_BIN="${LLM_D_EPP_BIN:-}"
LLM_D_ROUTER_REPO="${LLM_D_ROUTER_REPO:-$REPO_ROOT/../../repos/llm-d-router}"

BACKEND_PID=""
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
    if [ -n "$BACKEND_PID" ]; then kill "$BACKEND_PID" 2>/dev/null || true; fi
    docker rm -f "$ENVOY_CONTAINER" >/dev/null 2>&1 || true
    if [ -n "$BENCH_TMPDIR" ]; then rm -rf "$BENCH_TMPDIR"; fi
    if [ -n "$EPP_TMPDIR" ]; then rm -rf "$EPP_TMPDIR"; fi
    wait 2>/dev/null || true
    exit "$status"
}
trap cleanup EXIT

check_tool() {
    command -v "$1" >/dev/null 2>&1 || { echo "error: $1 not found"; exit 1; }
}

assert_port_free() {
    if ss -tlnH "sport = :$1" 2>/dev/null | grep -q LISTEN; then
        echo "error: port $1 is already in use"
        exit 1
    fi
}

wait_ready() {
    local label="$1" url="$2" pid="$3" timeout="${4:-60}"
    for _ in $(seq 1 "$timeout"); do
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "error: $label (PID $pid) exited during startup"
            return 1
        fi
        if curl -sf "$url" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.2
    done
    echo "error: $label did not become ready"
    return 1
}

wait_http_ready() {
    local label="$1" url="$2" timeout="${3:-60}"
    for _ in $(seq 1 "$timeout"); do
        if curl -sf "$url" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.2
    done
    echo "error: $label did not become ready"
    return 1
}

stop_praxis() {
    if [ -n "$PROXY_PID" ]; then
        kill "$PROXY_PID" 2>/dev/null || true
        wait "$PROXY_PID" 2>/dev/null || true
        PROXY_PID=""
    fi
}

stop_epp() {
    if [ -n "$EPP_PID" ]; then
        kill "$EPP_PID" 2>/dev/null || true
        wait "$EPP_PID" 2>/dev/null || true
        EPP_PID=""
    fi
}

stop_envoy() {
    if [ -n "$DOCKER_LOG_PID" ]; then
        kill "$DOCKER_LOG_PID" 2>/dev/null || true
        wait "$DOCKER_LOG_PID" 2>/dev/null || true
        DOCKER_LOG_PID=""
    fi
    docker rm -f "$ENVOY_CONTAINER" >/dev/null 2>&1 || true
}

preflight_profile() {
    local name="$1" url="$2"
    local response
    response=$(curl -sf -X POST "$url/v1/chat/completions" \
        -H "Content-Type: application/json" \
        -d '{"model":"test-model","messages":[{"role":"user","content":"ping"}],"max_tokens":5}' 2>&1) || {
        echo "error: preflight failed for $name"
        echo "$response"
        return 1
    }

    if ! echo "$response" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d.get('model') == 'test-model'" 2>/dev/null; then
        echo "error: preflight response for $name did not contain model test-model"
        echo "$response"
        return 1
    fi
}

verify_epp_log_contains_model() {
    local log_file="$1"
    sleep 0.5
    if ! grep -q "test-model" "$log_file" 2>/dev/null; then
        echo "error: EPP log does not contain test-model"
        echo "  log: $log_file"
        tail -20 "$log_file" || true
        exit 1
    fi
}

make_targets() {
    local port="$1"
    local tfile="$BENCH_TMPDIR/targets-${port}.txt"
    cat > "$tfile" << TARGETS
POST http://127.0.0.1:${port}/v1/chat/completions
Content-Type: application/json
@${BENCH_TMPDIR}/body.json
TARGETS
    echo "$tfile"
}

run_iteration() {
    local profile="$1" port="$2" run_num="$3"
    local targets_file bin_file json_file text_file
    targets_file=$(make_targets "$port")

    if [ "$WARMUP" -gt 0 ]; then
        vegeta attack -targets "$targets_file" -rate 0 -max-workers 8 \
            -duration "${WARMUP}s" >/dev/null 2>&1 || true
    fi

    bin_file="$RESULTS_DIR/${profile}-run${run_num}.bin"
    json_file="$RESULTS_DIR/${profile}-run${run_num}.json"
    text_file="$RESULTS_DIR/${profile}-run${run_num}.txt"

    vegeta attack -targets "$targets_file" -rate 0 -max-workers 16 \
        -duration "${DURATION}s" > "$bin_file"
    vegeta report --type=json < "$bin_file" > "$json_file"
    vegeta report --type=text < "$bin_file" > "$text_file"

    python3 - "$json_file" << 'PY'
import json
import sys

path = sys.argv[1]
with open(path) as f:
    d = json.load(f)

lat = d.get("latencies", {})
codes = d.get("status_codes", {})
errors = d.get("errors", [])
success = d.get("success", 0)

print(
    f"  RPS: {d.get('throughput', 0):.0f}  "
    f"p50: {lat.get('50th', 0) / 1e6:.2f}ms  "
    f"p95: {lat.get('95th', 0) / 1e6:.2f}ms  "
    f"p99: {lat.get('99th', 0) / 1e6:.2f}ms  "
    f"success: {success * 100:.4f}%"
)
print(f"  status_codes: {codes}")

if success < 1.0:
    print(f"  FAIL: success rate {success * 100:.4f}% is not 100%")
    if errors:
        print(f"  errors: {errors}")
    sys.exit(1)
if set(codes.keys()) != {"200"}:
    print(f"  FAIL: non-200 status codes present: {codes}")
    sys.exit(1)
PY
}

compute_median() {
    local profile="$1"
    python3 - "$RESULTS_DIR" "$profile" "$RUNS" "$DURATION" "$WARMUP" << 'PY'
import json
import statistics
import sys

results_dir, profile, runs_s, duration_s, warmup_s = sys.argv[1:]
runs = []
for i in range(1, int(runs_s) + 1):
    with open(f"{results_dir}/{profile}-run{i}.json") as f:
        runs.append(json.load(f))

def median(vals):
    return statistics.median(vals)

lat_keys = ["mean", "50th", "90th", "95th", "99th", "max"]
status_codes = {}
for run in runs:
    for code, count in run.get("status_codes", {}).items():
        status_codes[code] = status_codes.get(code, 0) + count

result = {
    "profile": profile,
    "runs": len(runs),
    "duration_secs": int(duration_s),
    "warmup_secs": int(warmup_s),
    "throughput": median([r.get("throughput", 0) for r in runs]),
    "success": median([r.get("success", 0) for r in runs]),
    "latencies": {
        key: median([r.get("latencies", {}).get(key, 0) for r in runs])
        for key in lat_keys
    },
    "status_codes_total": status_codes,
}

with open(f"{results_dir}/{profile}-median.json", "w") as f:
    json.dump(result, f, indent=2)

lat = result["latencies"]
print(
    f"  Median: {result['throughput']:.0f} req/s, "
    f"p50={lat['50th'] / 1e6:.2f}ms, "
    f"p95={lat['95th'] / 1e6:.2f}ms, "
    f"p99={lat['99th'] / 1e6:.2f}ms, "
    f"success={result['success'] * 100:.4f}%"
)
PY
}

start_go_mock_backend() {
    BENCH_TMPDIR=$(mktemp -d)
    cat > "$BENCH_TMPDIR/body.json" << 'BODY'
{"model":"test-model","messages":[{"role":"user","content":"Hello, how are you?"}],"max_tokens":50}
BODY

    cat > "$BENCH_TMPDIR/mock.go" << 'GOSRC'
package main

import (
	"io"
	"net/http"
	"os"
)

const resp = `{"id":"chatcmpl-bench","object":"chat.completion","model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"Hello from mock."},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}`

func main() {
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.Copy(io.Discard, r.Body)
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusOK)
		_, _ = io.WriteString(w, resp)
	})
	if err := http.ListenAndServe("127.0.0.1:"+os.Args[1], nil); err != nil {
		os.Exit(1)
	}
}
GOSRC

    go build -o "$BENCH_TMPDIR/mock" "$BENCH_TMPDIR/mock.go"
    "$BENCH_TMPDIR/mock" "$BACKEND_PORT" &
    BACKEND_PID=$!
    wait_ready "Go mock backend" "http://127.0.0.1:$BACKEND_PORT/" "$BACKEND_PID" 30
    echo "Go mock backend ready (PID $BACKEND_PID)"
}

prepare_epp_config() {
    EPP_TMPDIR=$(mktemp -d)
    local endpoints_abs="$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-endpoints.yaml"
    sed "s|PLACEHOLDER_ENDPOINTS_PATH|$endpoints_abs|" \
        "$REPO_ROOT/benchmarks/comparison/configs/llmd/epp-config.yaml" \
        > "$EPP_TMPDIR/epp-config.yaml"
}

start_epp() {
    local log_file="$1"
    prepare_epp_config
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
        >"$log_file" 2>&1 &
    EPP_PID=$!
    wait_ready "Go EPP" "http://127.0.0.1:$EPP_METRICS_PORT/metrics" "$EPP_PID" 60
    echo "Go EPP ready (PID $EPP_PID)"
}

start_praxis() {
    local profile="$1" config="$2"
    PRAXIS_CONFIG="$config" "$PRAXIS_BIN" >"$LOGS_DIR/${profile}.log" 2>&1 &
    PROXY_PID=$!
    wait_ready "Praxis $profile" "http://127.0.0.1:$ADMIN_PORT/ready" "$PROXY_PID" 60
    echo "Praxis $profile ready (PID $PROXY_PID)"
}

run_praxis_profile() {
    local profile="$1" config="$2"
    echo ""
    echo "--- $profile (${RUNS} runs x ${DURATION}s) ---"
    start_praxis "$profile" "$config"
    preflight_profile "$profile" "http://127.0.0.1:$PROXY_PORT"
    for run in $(seq 1 "$RUNS"); do
        echo "Run $run:"
        run_iteration "$profile" "$PROXY_PORT" "$run"
    done
    compute_median "$profile"
    stop_praxis
    sleep 1
}

run_praxis_go_epp_profile() {
    local profile="praxis-go-epp"
    local epp_log="$LOGS_DIR/${profile}-epp.log"
    echo ""
    echo "--- $profile (${RUNS} runs x ${DURATION}s) ---"
    start_epp "$epp_log"
    start_praxis "$profile" "$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-go-epp.yaml"
    preflight_profile "$profile" "http://127.0.0.1:$PROXY_PORT"
    verify_epp_log_contains_model "$epp_log"
    for run in $(seq 1 "$RUNS"); do
        echo "Run $run:"
        run_iteration "$profile" "$PROXY_PORT" "$run"
    done
    compute_median "$profile"
    stop_praxis
    stop_epp
    if [ -n "$EPP_TMPDIR" ]; then rm -rf "$EPP_TMPDIR"; EPP_TMPDIR=""; fi
    sleep 1
}

run_envoy_go_epp_profile() {
    local profile="envoy-go-epp"
    local epp_log="$LOGS_DIR/${profile}-epp.log"
    echo ""
    echo "--- $profile (${RUNS} runs x ${DURATION}s) ---"
    start_epp "$epp_log"

    docker rm -f "$ENVOY_CONTAINER" >/dev/null 2>&1 || true
    docker run --rm -d \
        --name "$ENVOY_CONTAINER" \
        --network host \
        -v "$REPO_ROOT/benchmarks/comparison/configs/llmd/envoy-go-epp.yaml:/etc/envoy/envoy.yaml:ro" \
        "$ENVOY_IMAGE" \
        -c /etc/envoy/envoy.yaml --log-level warn \
        >"$LOGS_DIR/${profile}-envoy-start.log" 2>&1

    wait_http_ready "Envoy" "http://127.0.0.1:$ENVOY_ADMIN_PORT/ready" 60
    docker logs -f "$ENVOY_CONTAINER" >"$LOGS_DIR/${profile}-envoy.log" 2>&1 &
    DOCKER_LOG_PID=$!
    echo "Envoy ready (container $ENVOY_CONTAINER)"

    preflight_profile "$profile" "http://127.0.0.1:$ENVOY_PORT"
    verify_epp_log_contains_model "$epp_log"
    for run in $(seq 1 "$RUNS"); do
        echo "Run $run:"
        run_iteration "$profile" "$ENVOY_PORT" "$run"
    done
    compute_median "$profile"
    stop_envoy
    stop_epp
    if [ -n "$EPP_TMPDIR" ]; then rm -rf "$EPP_TMPDIR"; EPP_TMPDIR=""; fi
    sleep 1
}

write_summary() {
    python3 - "$RESULTS_DIR" << 'PY' > "$RESULTS_DIR/summary.md"
import json
import sys

results_dir = sys.argv[1]
profiles = ["praxis-simple", "praxis-go-epp", "envoy-go-epp"]

print("# llm-d Same-Backend Benchmark")
print()
print("Backend: Go net/http mock shared by all profiles.")
print("Methodology: 3 runs x 30s, 5s warmup, Vegeta rate 0, max-workers 16.")
print()
print("| Profile | RPS | p50 | p95 | p99 | Success |")
print("|---|---:|---:|---:|---:|---:|")

for profile in profiles:
    with open(f"{results_dir}/{profile}-median.json") as f:
        d = json.load(f)
    lat = d["latencies"]
    print(
        f"| `{profile}` | {d['throughput']:.0f} | "
        f"{lat['50th'] / 1e6:.2f}ms | "
        f"{lat['95th'] / 1e6:.2f}ms | "
        f"{lat['99th'] / 1e6:.2f}ms | "
        f"{d['success'] * 100:.2f}% |"
    )
PY
    cat "$RESULTS_DIR/summary.md"
}

check_tool vegeta
check_tool python3
check_tool go
check_tool docker
check_tool ss

for port in "$BACKEND_PORT" "$PROXY_PORT" "$ENVOY_PORT" "$ADMIN_PORT" "$EPP_GRPC_PORT" "$EPP_HEALTH_PORT" "$EPP_METRICS_PORT" "$ENVOY_ADMIN_PORT"; do
    assert_port_free "$port"
done

mkdir -p "$RESULTS_DIR" "$LOGS_DIR"

PRAXIS_BIN="$REPO_ROOT/target/release/praxis"
if [ ! -x "$PRAXIS_BIN" ]; then
    cargo build --release -p praxis --features ext-proc
fi

if ! "$PRAXIS_BIN" -t -c "$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-go-epp.yaml" >/dev/null 2>&1; then
    echo "error: Praxis binary does not support llmd_external_epp"
    echo "  Rebuild with: cargo build --release -p praxis --features ext-proc"
    exit 1
fi

if [ -z "$LLM_D_EPP_BIN" ]; then
    if [ -x /tmp/epp ]; then
        LLM_D_EPP_BIN="/tmp/epp"
    elif [ -d "$LLM_D_ROUTER_REPO" ]; then
        (cd "$LLM_D_ROUTER_REPO" && go build -o /tmp/epp ./cmd/epp)
        LLM_D_EPP_BIN="/tmp/epp"
    else
        echo "error: no EPP binary. Set LLM_D_EPP_BIN or LLM_D_ROUTER_REPO"
        exit 1
    fi
fi

echo "=== llm-d Same-Backend Benchmark ==="
echo "Duration: ${DURATION}s  Warmup: ${WARMUP}s  Runs: ${RUNS}"
echo "Results: $RESULTS_DIR"
echo "Envoy image: $ENVOY_IMAGE"
echo "EPP binary: $LLM_D_EPP_BIN"
echo ""

start_go_mock_backend
run_praxis_profile "praxis-simple" "$REPO_ROOT/benchmarks/comparison/configs/llmd/praxis-simple.yaml"
run_praxis_go_epp_profile
run_envoy_go_epp_profile
write_summary

echo ""
echo "=== Same-Backend Benchmark Complete ==="
