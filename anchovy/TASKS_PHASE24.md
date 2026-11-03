# Phase 24: hunting races with derp, and resubmitting system messages

The user's list after phase 23:
1. Hunt the rare coin-reservation panic of phase 22 with derp.
2. Resubmit this validator's checkpoint signatures until sequenced.
3. Wider derp sweeps over the compat suite; the recipe as a script.
4. Crash-recovery testing under derp.

## Done

- [x] `the_composite_workload_runs_alike_through_stalls`: anchovy starts
  each epoch late (unpolled 20–90 s once sui's validators are past it) and
  its consensus is held now and then, so it catches up on certified
  checkpoints. Under derp almost every seed failed, two ways:
  - **Funds decided without the balance.** A chunk whose transactions ran
    from certified checkpoints settled from their stored effects before its
    resolver sent its reservations; the ledger fulfilled reservations below
    σ uncounted (meant for replays), and a transaction the quorum found
    insufficient was fulfilled. Now the resolver sends every chunk's
    reservations, even none, and a settlement waits until the version
    before it has all of them; nothing is fulfilled unchecked.
  - **The phase 22 panic** ("a certified coin reservation names a field"):
    a reservation fulfilled before the previous chunk settled read the
    fields before that settlement wrote them. The rewrite now waits for the
    root at the transaction's version, as execution does for the root.
- [x] `scripts/derp-sweep.sh TEST FIRST COUNT [JOBS] [OUT]`.
- [x] System messages (checkpoint signatures, DKG messages and
  confirmations, capabilities) are submitted again each time their block is
  garbage collected, until sequenced, as the reference's
  `submit_to_consensus`; before, each was submitted once.

## Remaining

- [ ] Verify the fixes over many stalled seeds; the plain workload too.
- [ ] Sweep the rest of the compat suite.
- [ ] Crash recovery under derp.
- [ ] After the above (the user's request): built-in logging in the
  processor framework (`workqueue`, `waiter`). Every processor logs when an
  item is enqueued, dispatched and finished; every waiter logs when a wait
  is registered and when it is satisfied. Items log through a trait of
  their own, a short summary (a transaction's digest, not its full `Debug`),
  rather than `Debug`: `trait Brief: Debug` with a default `brief` that
  formats with `Debug`, so most types opt in with an empty impl and big
  ones override it (a blanket impl over `Debug` would need specialization).
