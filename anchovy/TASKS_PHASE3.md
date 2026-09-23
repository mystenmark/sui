# Phase 3 progress: validator gRPC server skeleton

Plan: `IMPLEMENTATION_PLAN_PHASE3.md`. Branch `mlogan-phase3`, to be merged
into `anchovy-main`.

## Status: complete

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
3. `anchovy-validator --listen <addr> --network-key <file>`: reads sui's
   key file format (base64 of flag 0 and the 32-byte secret), serves until
   ctrl-c, exits 0. `validator::serve` is the shared entry point.
4. TLS (`tls.rs`), done with step 3 rather than after a plaintext binary:
   `sui-tls`'s server config (ring, TLS 1.3 only, ALPN `h2`, no client
   auth) and certificate (`rcgen` defaults, SAN `sui`, self-signed over
   the network key). The key goes to ring as a fixed-prefix PKCS#8 v1, so
   no `fastcrypto`, `ed25519` or `pkcs8` crates. Handshakes run in their
   own tasks with a 10 s timeout and feed tonic through a channel.
   Tests: a client configured as sui's (TLS 1.3, pinned key) is served; a
   client pinning another key is refused; key files parse, and the wrong
   scheme or length is rejected.
5. `tools/validator-client` (outside the workspace, lockfile pruned from
   sui's): generates a key with sui-types, writes it with sui's
   `encode_base64`, starts `anchovy-validator`, and connects exactly as
   `NetworkAuthorityClient::connect` does (`sui_tls` client config pinning
   the key, `mysten_network::client::connect`). Health answers; each of
   the seven other routes, sent with sui-types' own requests and codecs,
   reaches its handler (the server's `todo!()` message for that handler
   appears on stderr); the server keeps serving; a client pinning another
   key is refused. Run: `cargo run --manifest-path
   tools/validator-client/Cargo.toml -- target/debug/anchovy-validator`.

## Findings

- Pings cannot be answered without consensus (the reference returns a
  real consensus position, then waits for its commit), so they are
  `todo!()` like the rest; plan updated.
- A `todo!()` panic resets only its h2 stream; the connection and server
  keep going (tonic spawns a task per stream).
- `mysten_network::client::connect` is https-only: an `/http` multiaddr
  becomes an `http://` URI that its connector refuses, so the conformance
  tool addresses the server as `/ip4/.../tcp/.../https`.
