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
- Seed 9 of the full mix through stalls panics in exec-pool's
  `live_generation`: `StreamId { 0xa611f9…, initial_shared_version: 1 }`,
  "which voting found live", has no live object. Not yet diagnosed.

## Remaining
- Diagnose and fix seed 9.
- Stateful rejections (`inputs.rs`) are checked against the reference at
  kind level only; `inputs.vectors` would need full reasons from the oracle.
- Signature-failure texts (`InvalidSignature`) are anchovy's, not
  fastcrypto's; the variant matches.
- Anchovy's own reject votes keep no reason, so `wait_for_effects` answers
  a rejection with none (as sui does when it did not vote to reject).
- Overload and epoch-end failures are still bare statuses (retriable either
  way).
