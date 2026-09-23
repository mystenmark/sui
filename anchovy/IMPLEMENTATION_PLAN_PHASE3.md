# Phase 3: validator gRPC server skeleton

Requirements are in `PRD.md`, Phase 3: a binary that serves the validator
gRPC API with tokio and tonic, most handlers `todo!()`.

## Overview

- `messages::grpc`: wire types for the requests the validator API encodes
  in BCS (`ObjectInfoRequest`, `TransactionInfoRequest`,
  `CheckpointRequest`, `CheckpointRequestV2`, `SystemStateRequest`), with a
  parser and a writer each.
- `crates/validator`: the `sui.validator.Validator` service (routes, codecs,
  handler trait) as a library, and an `anchovy-validator` binary that serves
  it.

## Goals

1. A client built from sui's `sui-network` reaches every route of our
   server by the same path, and our server decodes its requests.
2. Requests are decoded into this repo's wire types, not sui-types: BCS
   requests with `messages`' parser, protobuf requests with `prost` structs
   declared here with the reference's field tags.
3. `ValidatorHealth` and the ping forms of `SubmitTransaction` and
   `WaitForEffects` answer; every other handler is `todo!()`.
4. TLS as validators speak it: a self-signed certificate over the
   validator's Ed25519 network key, server name `sui`, so a stock sui
   client that pins the key connects.

## Non-goals

- Validity checks on submitted transactions (Phase 4). Decoding only
  builds the in-memory representation, per the PRD clarification.
- Any real handler behaviour beyond pings and health: no consensus, no
  storage, no execution.
- Response types beyond what the implemented handlers return. The
  `todo!()` handlers return pre-encoded bytes, so their response types can
  arrive with the handlers.
- Anemo (P2P) services, metrics, config files.

## The reference API

`crates/sui-network/build.rs` declares service `Validator` in package
`sui.validator` with eight unary methods. Three use protobuf (`tonic_prost`)
whose fields carry BCS blobs; five use `BcsCodec`, which is plain BCS in the
gRPC frame (no compression).

| Route                  | Codec    | Request                   | Skeleton        |
|------------------------|----------|---------------------------|-----------------|
| `SubmitTransaction`    | protobuf | `RawSubmitTxRequest`      | ping answers    |
| `WaitForEffects`       | protobuf | `RawWaitForEffectsRequest`| ping answers    |
| `ObjectInfo`           | BCS      | `ObjectInfoRequest`       | `todo!()`       |
| `TransactionInfo`      | BCS      | `TransactionInfoRequest`  | `todo!()`       |
| `Checkpoint`           | BCS      | `CheckpointRequest`       | `todo!()`       |
| `CheckpointV2`         | BCS      | `CheckpointRequestV2`     | `todo!()`       |
| `GetSystemStateObject` | BCS      | `SystemStateRequest`      | `todo!()`       |
| `ValidatorHealth`      | protobuf | `RawValidatorHealthRequest` | answers       |

## Design

### `messages::grpc`

The five BCS requests are small and fixed-shape (an id or digest, options,
bools), so they are plain `Copy` structs parsed from a `Reader` with no
arena, not `Message` views. Each has a `write` into `fast::Writer` for
tests and for a future client. Test vectors come from `tools/sui-oracle`,
which gains a mode that BCS-encodes sample requests with sui-types.

### Service definition

`tonic-build`'s manual service builder in `build.rs`, as the reference
does, so routing, path constants and the handler trait are tonic's own
generated code rather than hand-written tower plumbing. Codec paths point
at this crate:

- `BcsCodec<Req>`: decodes the frame with `messages::grpc`'s parser;
  `invalid_argument` on a parse error. Its responses are `Encoded`, bytes
  a handler already wrote, copied into the frame.
- `tonic_prost::ProstCodec` for the three protobuf routes, over `prost`
  structs copied field-for-field (tags, `bytes = "bytes"`) from
  `sui_types::messages_grpc`. A submitted transaction stays `Bytes` in the
  handler; parsing it into a `Message` is Phase 4's first step.

### Handlers

One struct, `Validator`, implements the generated trait. Pings return
empty results; health returns zeros for fields the skeleton cannot know.
`todo!()` panics unwind into the connection task (the release profile does
not set `panic = "abort"`), which resets that stream; the server keeps
serving.

### Binary

`anchovy-validator --listen <addr> --network-key <path>`: tokio
multi-threaded runtime, tonic server with the service and TLS, graceful
shutdown on ctrl-c. Argument parsing with `clap` (derive).

### TLS

`sui-tls` makes a self-signed certificate for the network key with
`rcgen`, server name `sui`, and clients verify the certificate's public
key rather than a chain. We do the same with `rustls`, `tokio-rustls` and
`rcgen` (all in sui's lockfile), and `fastcrypto` for the key.

## Testing

- `messages::grpc`: each request parses the oracle's vectors and writes
  them back byte-for-byte; truncated and trailing-byte inputs fail.
- In-crate: a tonic client over our own codecs calls every route on an
  in-process server; pings and health answer, `todo!()` routes fail
  without taking down the server.
- Conformance: `tools/validator-client`, outside the workspace like
  `sui-oracle`, links `sui-network` and `sui-tls` and runs the reference
  client against a running `anchovy-validator`: TLS handshake with the
  pinned key, ping and health answered, each `todo!()` route reached (seen
  as a reset stream, not `unimplemented`).

## Steps

1. `messages::grpc` request types and oracle vectors.
2. `crates/validator`: `build.rs`, codecs, prost structs, handler
   skeleton, in-process test.
3. `anchovy-validator` binary, plaintext.
4. TLS.
5. `tools/validator-client` conformance run.
