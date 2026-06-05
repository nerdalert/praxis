#!/usr/bin/env bash
# GuideLLM smoke test against a running llm-d benchmark profile.
#
# Usage:
#   ./benchmarks/llm-d/run-guidellm-smoke.sh [PROFILE] [TARGET_URL] [MAX_SECONDS]
#
# Positional arguments:
#   PROFILE      profile name for output labeling (default: praxis-native)
#   TARGET_URL   profile base URL without /v1 (default: http://127.0.0.1:18090)
#   MAX_SECONDS  benchmark duration (default: 30)
#
# Environment:
#   GUIDELLM_BIN   path to guidellm binary (default: searches PATH and common venv locations)
#   MODEL_NAME     model to benchmark (default: test-model)
#   CONCURRENCY    concurrent requests (default: 4)
#
# Prerequisites:
#   - guidellm (pip install guidellm, or set GUIDELLM_BIN)
#   - A running profile endpoint at TARGET_URL
#   - The profile must serve /health, /v1/models, and /v1/chat/completions
#
# This script does NOT start Praxis, Envoy, or the backend.
# Start them first using the other benchmark scripts, then
# run GuideLLM against the running endpoint.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

PROFILE="${1:-praxis-native}"
TARGET_URL="${2:-http://127.0.0.1:18090}"
MAX_SECONDS="${3:-30}"
MODEL_NAME="${MODEL_NAME:-test-model}"
CONCURRENCY="${CONCURRENCY:-4}"
DATA_FILE="$REPO_ROOT/benchmarks/llm-d/data/guidellm-prompts.json"
RESULTS_DIR="$REPO_ROOT/target/criterion/llmd-guidellm/$PROFILE"

# Resolve GuideLLM binary.
GUIDELLM_BIN="${GUIDELLM_BIN:-}"
if [ -z "$GUIDELLM_BIN" ]; then
    if command -v guidellm &>/dev/null; then
        GUIDELLM_BIN="guidellm"
    elif [ -x /tmp/guidellm-venv/bin/guidellm ]; then
        GUIDELLM_BIN="/tmp/guidellm-venv/bin/guidellm"
    else
        echo "error: guidellm not found."
        echo "  Install: pip install guidellm"
        echo "  Or set GUIDELLM_BIN=/path/to/guidellm"
        exit 1
    fi
fi

echo "=== GuideLLM Smoke: $PROFILE ==="
echo "Target: $TARGET_URL"
echo "Model: $MODEL_NAME"
echo "Concurrency: $CONCURRENCY"
echo "Duration: ${MAX_SECONDS}s"
echo ""

# Verify endpoint.
echo "Verifying endpoint..."
HEALTH=$(curl -sf -o /dev/null -w "%{http_code}" "$TARGET_URL/health" 2>/dev/null) || HEALTH="000"
MODELS=$(curl -sf -o /dev/null -w "%{http_code}" "$TARGET_URL/v1/models" 2>/dev/null) || MODELS="000"
CHAT=$(curl -sf -o /dev/null -w "%{http_code}" -X POST "$TARGET_URL/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -d "{\"model\":\"$MODEL_NAME\",\"messages\":[{\"role\":\"user\",\"content\":\"ping\"}],\"max_tokens\":5}" 2>/dev/null) || CHAT="000"

echo "  /health: $HEALTH"
echo "  /v1/models: $MODELS"
echo "  /v1/chat/completions: $CHAT"

if [ "$HEALTH" != "200" ] || [ "$MODELS" != "200" ] || [ "$CHAT" != "200" ]; then
    echo "error: endpoint not fully functional. Start the profile first."
    exit 1
fi
echo ""

mkdir -p "$RESULTS_DIR"

echo "Running GuideLLM..."
"$GUIDELLM_BIN" benchmark run \
    --target="$TARGET_URL" \
    --model="$MODEL_NAME" \
    --data="$DATA_FILE" \
    --profile=concurrent \
    --rate="$CONCURRENCY" \
    --max-seconds="$MAX_SECONDS" \
    --outputs="$RESULTS_DIR/results.json" \
    2>&1

echo ""
echo "=== GuideLLM Smoke Complete ==="
echo "Results: $RESULTS_DIR/results.json"
