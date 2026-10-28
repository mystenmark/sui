# Phase 18: randomness

Goal: anchovy agrees with sui validators in a network with the random
beacon enabled: DKG outcome, randomness rounds, randomness deferral and
`DkgFailed` cancellation, the randomness congestion tracker, and
`RandomnessStateUpdate` transactions in checkpoints.

## Done

- Branch `mlogan-phase18` from `anchovy-main` (`c3afaa9cb5`).
- Reference survey: `docs/randomness_reference.md`.
- Design (`docs/randomness_design.md`), agreed: phase 18a observes,
  phase 18b participates; `anemo` approved.
- DKG as an observer (`consensus/dkg.rs`): messages checked in the
  decoder, merge, completion and timeout in the handler, progress kept in
  the epoch store's `Randomness` space; unit tests against real parties.
- Rounds, randomness deferral, `DkgFailed` cancellation, the randomness
  tracker and its debts, the schedule's randomness part and its chunks
  queued after the flush.
- The randomness gate, the signature collector (full signatures and
  partial aggregation), the bytes in the epoch store, the p2p endpoint
  (anemo, `sui.Randomness/SendSignatures`), and fetching rounds peers
  stopped pushing.
- Compat: four beacon scenarios (rounds and a restart, DKG failure,
  randomness congestion, epochs); all 19 pass.

## Remaining

- Phase 18b: anchovy's own DKG messages and confirmations, partial
  signatures, pushing them to peers.

## Left out

- Peer blocking for bad partial signatures; the consensus observer relay.
- Checkpoint state sync beyond a missed round's update: a validator left
  an epoch behind by a quorum without it cannot catch up.
