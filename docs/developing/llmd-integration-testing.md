# llm-d Integration Testing

Integration testing guide for Praxis as the gateway in an llm-d
inference serving setup. Covers issue
[#295](https://github.com/praxis-proxy/praxis/issues/295).

## Architecture

```text
HTTP client -> Praxis (ext_proc + endpoint_selector)
                  |
                  +-- gRPC ext_proc Process stream -->  llm-d EPP/scheduler
                  |                                         |
                  +-- selected endpoint -----------------> vLLM pod(s)
```

Praxis opens a full-duplex `ext_proc` Process stream to the llm-d
EPP for every HTTP request. The EPP runs its scheduling logic
(endpoint discovery, cache-aware selection, traffic splitting) and
returns the selected `host:port` as a trusted header mutation. The
`endpoint_selector` filter reads that mutation and routes the request
to the selected backend.

## Integration Test Tiers

### Tier 1: In-Process Integration Tests (merged in #747)

13 integration tests in `tests/integration/tests/suite/ext_proc.rs`
use an in-process tonic `ExternalProcessor` mock and the documented
example config. They run in standard CI without Docker, KIND, or
external infrastructure.

```bash
cargo test -p praxis-tests-integration --test suite -- ext_proc
```

**What they prove:**
- Deferred routing after request body EOS
- Destination header stripping
- Client spoofing protection
- Required-mode failure rejection (missing, invalid, ambiguous)
- Processor failure returns configured `status_on_error`
- `ImmediateResponse` reaches client without backend contact
- Bodyless request lifecycle
- Repeated requests with one Process stream each
- Ordered mutation precedence across the pipeline boundary

### Tier 2: Real llm-d Cluster

Proves end-to-end behavior with actual vLLM model-serving pods and
the full llm-d scheduler. Requires a pre-existing Kubernetes cluster
with:

- llm-d CRD controllers (`InferencePool`, `InferenceModel`)
- vLLM pods serving a real or test model
- llm-d EPP/scheduler with the full plugin chain
- Praxis deployed as the gateway

**What it proves (when available):**
- EPP endpoint selection under varying load
- Cache-aware routing with shared system prompts, when scheduler
  evidence is observable
- `InferenceModel` traffic splitting across pools, when distribution
  evidence is observable
- Disaggregated prefill/decode (version-gated), when routing
  evidence is observable
- Praxis appears as a functional gateway in the llm-d topology

**Harness:**
Run the environment-specific integration test harness from the repository or
automation workspace that owns the llm-d deployment artifacts. The
harness should send traffic through Praxis, inspect scheduler/EPP
evidence, and report PASS/FAIL/SKIP for each issue #295 requirement.

```bash
PRAXIS_URL=http://<praxis-gateway> \
NAMESPACE=<llmd-namespace> \
./run-real-cluster-integration-tests.sh
```

To include the destructive EPP outage/recovery test, add
`ALLOW_EPP_OUTAGE_TEST=1`. Use only against dedicated test
clusters, not shared environments.

See the harness README for the full environment-variable reference,
evidence-regex configuration, and `FAIL_ON_SKIP` strict mode.

Cache-aware routing, traffic splitting, and disaggregated
prefill/decode are marked PASS only when the harness observes
explicit scheduler/EPP evidence for the decision path. Plain HTTP
200 responses are treated as smoke evidence and become SKIP if the
environment does not expose a verifiable signal.

## Issue #295 Requirement Mapping

| # | Requirement | Tier 1 | Tier 2 (Real) |
|---|-------------|--------|---------------|
| 1 | vLLM model-serving pods | Mock processor | Real vLLM |
| 2 | llm-d inference scheduler | Mock processor | Full scheduler |
| 3 | EPP selection under load/cache | Not tested | Tested when evidence observable |
| 4 | Prefix cache-aware routing | Not tested | PASS with evidence, otherwise SKIP |
| 5 | `InferenceModel` traffic splitting | Not tested | PASS with evidence, otherwise SKIP |
| 6 | Disaggregated prefill/decode | Not tested | PASS with evidence, otherwise SKIP |
| 7 | Praxis as functional gateway | Proven (#747) | Proven |
| 8 | Automated regression CI | PR CI (#747) | Manual/scheduled dispatch |

## CI Strategy

- **Required PR CI**: Tier 1 integration tests run in the standard
  test matrix. No GPU or external cluster needed.
- **Scheduled/manual dispatch**: Tier 2 requires a real cluster
  with GPU capacity. Run as a manually dispatched or scheduled
  workflow when infrastructure is available.
- **Concurrency and cleanup**: Use a concurrency group and per-test
  timeouts. Restore mutated resources (e.g. scaled deployments) on
  every exit path. Namespace deletion is appropriate only for
  disposable/dedicated test namespaces, not shared environments.

## AI/Product Boundary

Generic `ext_proc` and `endpoint_selector` behavior lives in
`praxis-proxy/praxis`. AI-specific model routing, pool membership
checks, and inference-protocol features belong in
`praxis-proxy/ai`. If Tier 2 integration testing reveals AI-specific
follow-up work, document it as future `praxis-proxy/ai` scope.
