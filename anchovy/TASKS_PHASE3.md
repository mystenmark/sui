# Phase 3 progress: validator gRPC server skeleton

Plan: `IMPLEMENTATION_PLAN_PHASE3.md`. Branch `mlogan-phase3`, to be merged
into `anchovy-main`.

## Status: steps 1 and 2 done, step 3 next

## Done

1. `messages::grpc`: `ObjectInfoRequest`, `TransactionInfoRequest`,
   `CheckpointRequest`, `CheckpointRequestV2`, `SystemStateRequest` as plain
   `Copy` values with `parse` (whole buffer, trailing bytes rejected) and
   `write`. `sui-oracle --grpc-requests` writes 23 reference encodings to
   `tests/data/grpc_requests.txt`; each round-trips byte-for-byte and every
   truncation and a trailing byte fail.
2. `crates/validator`: `build.rs` declares the service with
   `tonic-build`'s manual builder, same package, routes and codec per route
   as `sui-network`. `codec::BcsCodec` parses BCS requests with
   `messages::grpc` (`invalid_argument` on failure); BCS responses are
   `Encoded` bytes. `proto` copies the reference's prost structs field for
   field. `Validator` answers health with all fields absent; everything
   else is `todo!()`. In-process test: every route reached through the
   generated client, server survives the panics, malformed BCS rejected
   before the handler.

## Findings

- Pings cannot be answered without consensus (the reference returns a
  real consensus position, then waits for its commit), so they are
  `todo!()` like the rest; plan updated.
- A `todo!()` panic resets only its h2 stream; the connection and server
  keep going (tonic spawns a task per stream).

## Remaining

3. `anchovy-validator` binary, plaintext.
4. TLS matching `sui-tls`.
5. `tools/validator-client` conformance run.
