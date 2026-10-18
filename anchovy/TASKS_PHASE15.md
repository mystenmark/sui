# Phase 15 progress: consensus integration

Goal: run consensus-core (Mysticeti) under the node, through a crate that
only contains glue (`consensus-glue`); the commit handler and voter stay in
`validator::consensus`.

## Status: implemented; one gap needs state sync

## Steps

1. Committees and keys. Done: epoch records keep each member's consensus
   key (the worker key), network key, address and hostname; the node holds
   its protocol, worker and network keys (`ValidatorKeys`).
2. Survey of sui's wiring: `docs/consensus_integration_reference.md`.
3. Design: `docs/consensus_integration_design.md`. Agreed.
4. Implementation, in the design's order of work:
   1. The node's consensus trait, the last handled commit, a oneshot on
      each commit answered once handled, the epoch directory's `store/` and
      `consensus/`. Done.
   2. `consensus-glue`: committee and keys, protocol config, verifier,
      commits, submission, runner. Done. The authority starts in the
      background: the pipeline's workers start after consensus, and a
      starting authority may wait on their votes.
   3. One validator on real consensus: executes, certifies, resumes after a
      crash, moves through epochs. Done.
   4. A genesis of several validators; four validators: agree, and one
      crashed catches up. Done.
   5. Backpressure: commit answers withheld while too many chunks or
      transactions are in flight, released by the committer. Done
      (unit-tested; the glue's tests never reach the limits).

## Open

- Four validators across epochs (test ignored): a validator still short of
  an epoch's last certified checkpoint when a quorum has moved on and
  stopped that epoch's consensus cannot finish the epoch. sui catches it up
  through state sync; the pipeline takes no certified checkpoints from state
  sync yet. Passes when the validators keep pace.
- consensus-core's `prost::DecodeError::new` deprecation warning: anchovy's
  lockfile has prost 0.14.4 (from sui-types), sui's 0.14.1.
