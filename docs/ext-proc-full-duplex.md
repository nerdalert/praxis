# Full-Duplex ext_proc

## Status

PR-FD02 implements the `ExtProcExchange` transport state machine.
PR-FD03 wires it into `ExtProcFilter`.

## Exchange Core (FD02)

FD02 is the single-owner coalesced compatibility exchange core.
`send(&mut self)` / `receive(&mut self)` supports the first llm-d
path that sends input incrementally and drains output at EOS.

Simultaneous streaming input/output is completed in FD04A, which
adds a single-owner driver method using `tokio::select!` over
outbound readiness and inbound response progress. FD04A must not
use `Arc<Mutex<ExtProcExchange>>`, a worker pool, or a task per
message.

## State Domains

The exchange tracks six state domains:

1. **Shared terminal state** (`terminal: bool`): set on timeout,
   transport error, `ImmediateResponse`, or cancellation.

2. **Request outbound** (`request_send: DirectionSendState`):
   tracks committed messages. Contains `phase: SendPhase` and
   `body_ever_committed: bool`.

3. **Response outbound** (`response_send: DirectionSendState`):
   same for the response direction.

4. **Processor-output state** (`request_output: OutputPhase`,
   `response_output: OutputPhase`): tracks accepted processor
   events. Validates ordering, EOS, and duplicates. Rejected
   events do not advance output phase (transactional validation).

5. **Active processing state** (`active_processing:
   Option<ActiveProcessingState>`): installed when a non-full-duplex
   message commits. Contains `expected: ExpectedResponse`,
   `deadline: tokio::time::Instant`, and `override_consumed: bool`.
   At most one outstanding. Consumed by the exact matching response.

## Response Solicitation Rules

Every response is validated by `validate_response_solicited()`:

- **Non-full-duplex direction**: requires an active processing
  state whose `expected` matches the received response type.
  Wrong-kind, wrong-direction, and unsolicited responses are
  rejected without clearing the active state.

- **Full-duplex direction**: requires committed evidence from
  `committed_for()` — headers committed (phase != NotStarted),
  body ever committed, or trailers committed.

- **ImmediateResponse**: requires `first_sent == true`.

## Timeout Policy

- **Non-full-duplex**: deadline stored in `ActiveProcessingState`
  at send commit. `receive()` uses the stored deadline. Caller
  delay does not extend it.

- **Full-duplex**: NO per-message deadline for ANY message type
  in the direction — headers, body, or trailers. The processor
  may buffer everything before responding.

- **Stream-open**: `open_timeout` covers the gRPC handshake.
  Returns `ExchangeError::OpenTimeout` (distinct from `Timeout`).

- **Override**: one valid override (≥1ms, ≤max, canonical protobuf
  duration) per active processing state replaces the deadline.
  Invalid/repeated/disabled overrides are consumed and ignored.
  Override envelopes never fall through to response classification.

## Body Response Mode Validation

- `FULL_DUPLEX_STREAMED` requires `StreamedBodyResponse` mutation.
- Non-full-duplex rejects `StreamedBodyResponse` mutation.

## Channel Capacity

Capacity 1 (`REQUEST_CHANNEL_CAPACITY`). No measured
performance benefit from capacity 2 was demonstrated.
Capacity 1 provides tighter backpressure.

## Design Constraints

- No async worker pool.
- No task per message.
- One `ExtProcExchange` per HTTP request, stored in per-request
  filter state.
- Bounded channel (capacity 1) feeds tonic.
- No unbounded pending-event queue.
- Transactional send: pure validation → reserve → commit →
  atomic state update. Cancellation before commit leaves state
  unchanged.
- Transactional receive: output phase validated on local copy,
  committed only after all checks pass.
