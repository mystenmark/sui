# Phase 29: the rest of the reference's errors

Phase 28 left four gaps in the errors anchovy sends. Each now follows the
reference where that is easy.

## Done
- Reject votes and drops keep their reason. The voter records each reject
  vote's `SuiError`, and the commit stage records each drop's (lock
  conflict, invalid inputs), by position, as the reference's
  `TransactionRejectReasonCache`. `wait_for_effects` answers a rejection
  with it. The reasons expire with the statuses. The immutable-claim check
  gives the reference's errors in its order.
- Failed requests carry a `SuiError` in the status details, as the
  reference's do: `EpochEnded(epoch)`, `ValidatorConsensusLagging`,
  `TimeoutError` (pings), `TooManyTransactionsPendingConsensus` (full
  submission queues), `ValidatorOverloadedRetryAfter` (a full effects
  waiter), `ValidatorHaltedAtEpochEnd`, `Storage`.
- Signature failures carry the reference's texts (simple, multisig,
  zkLogin, passkey). A batch's failure takes the unbatched verifier's
  error. `reasons.rs` compares every signed vector's rejection with the
  reference's, alone and in a batch: 47 match.
- The inputs oracle records each rejection's `SuiError`; all 37 stateful
  rejections match byte for byte.

## Found
- consensus-glue's validator test sent `wait_for_effects` a raw digest,
  which phase 28's decoding rejects; it was not run in phase 28. Fixed.

## Remaining
- Status messages are the error's `Debug`, not the reference's `Display`
  text. Sui's clients decode the details, not the message.
- An epoch-closed drop has no reason (the submitter retries next epoch).
