# Phase 17 progress: protocol upgrades and congestion control

Goal: anchovy agrees with sui validators across a protocol upgrade and
through congested commits, in the mixed network of `crates/compat`.

## Steps

1. Reference surveys:
   - protocol upgrades: `docs/protocol_upgrade_reference.md`;
   - congestion control: `docs/congestion_control_reference.md`.
2. Designs, to agree before implementing.
3. Implementation, each part with a compat scenario that forces it.

## Status

Design agreed (`docs/upgrades_and_congestion_design.md`); anchovy now uses
sui's `sui-protocol-config`, its copy removed.

1. The end of the epoch: done. Certificates close at the end time; the
   epoch's last commit is the first with no deferred transaction left, or
   past the close deadline (unit tests; a compat scenario comes with
   congestion, which makes deferrals easy to force).
2. Protocol upgrades: done. Capabilities announced at each epoch start,
   recorded per member, the next version chosen at the last commit and
   written with its packages. Compat: an upgrade that needs anchovy's
   capabilities, and a split vote.
   - Departure from the design: the commit handler, which holds the
     capability set, chooses the version; `EpochClose` carries it.
   - Anchovy's executor runs only the highest version's gas model (15):
     the upgrade scenarios give the version below it by a config override.
     An epoch at an older gas model would fail in anchovy's executor.
3. Congestion control: done. The tracker (budget from the commit interval
   observer, now in the commit handler; start cost; charging), debts in the
   epoch store with the reference's decay, deferral and cancellation, a
   cancelled transaction scheduled in place with marker versions
   (`Scheduled::Cancelled`, `Input::Cancelled`) and listed in the prologue.
   Compat: congested writes deferred, cancelled, and across an epoch end.
   Found by compat, beyond the references:
   - a commit that loads deferred transactions is never the epoch's last
     (the reference removes loaded keys after the close check; now in the
     congestion reference's R2.7);
   - the estimator must start from the observations the previous epoch
     stored (TASKS_PHASE16's known divergence): validators share
     observations for congested objects, so congestion reaches it.

## Left out

- Randomness: randomness-using transactions skip congestion control and
  are never deferred for randomness; DKG-failure cancellation is absent.
  A network with randomness must not run anchovy yet.
- Framework (package) upgrades are built by the reference's rule but
  untested: compat can only make version upgrades.
- Epochs at gas model 14 or older: anchovy's executor runs only gas
  model 15.
