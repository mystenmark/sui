# Phase 28: rejection reasons

Sui's clients classify a validator's rejection by the `SuiError` it sends:
retriable (`ObjectNotFound`, overload) or invalid. Anchovy sent none.

## Done
- `messages::build::error`: the reference's `SuiErrorKind` (the variants
  anchovy sends, each at its reference index) and its whole
  `UserInputError`, encode-only. `validator/tests/sui_error.rs` decodes
  every variant as the reference's.
- Digests display in base58 and type tags as Move does, for the reasons
  that carry them as text.
- `validation::Error` is the reference's error, fields and text included;
  `kind()` derives the old `ErrorKind` for the kind-level vector tests.
- The server sends it as sui does: in a rejected transaction's result, and
  in a failed request's status details (`Status::with_details`, code
  `Internal`), with the reference's `InvalidRequest` and
  `GrpcMessageDeserializeError` for malformed requests; store failures as
  `Storage`.
- `validator/tests/reasons.rs`: every static rejection of the validity,
  mutation, signed-vector and mainnet corpora has the reference's bytes.
- `the_full_composite_workload_runs_alike_through_stalls`: seeds 1-48 under
  derp, 47 pass.

## Found
- Seed 9 of the full mix through stalls panicked in exec-pool's
  `live_generation`: a shared access with no earlier generation found no
  object. Its `StreamLive` key resolved at once, so it could run before its
  object's creator (a transaction with no input objects, hence the stream's
  start version 1) executed here. Fixed: `StreamLive` waits for the stream's
  start, as the reference waits for the object at its initial version
  (`a_first_access_waits_for_its_object_to_be_created`). Seed 9 did not
  replay under derp (neither binary), so the test reproduces it instead.
- Anchovy's `wait_for_effects` takes a raw 32-byte digest, but sui's clients
  send its BCS (33 bytes, length-prefixed): seed 9's log has 141 "a 32-byte
  transaction digest is required" answers. Hidden by sui's quorum; the
  compat test's own request sent raw bytes. Fixed: the request's fields are
  decoded as the reference's, with its errors
  (`waits_are_decoded_as_the_reference_s_client_encodes_them`).

## Remaining
- Stateful rejections (`inputs.rs`) are checked against the reference at
  kind level only; `inputs.vectors` would need full reasons from the oracle.
- Signature-failure texts (`InvalidSignature`) are anchovy's, not
  fastcrypto's; the variant matches.
- Anchovy's own reject votes keep no reason, so `wait_for_effects` answers
  a rejection with none (as sui does when it did not vote to reject).
- Overload and epoch-end failures are still bare statuses (retriable either
  way).
