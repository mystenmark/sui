# Phase 26: derp failures and sweeps

## Done
- Composite seed 6 (stalls), `MissingNative` in the settlement executor:
  on the synced path a settlement waited only for the root version, which
  its chunk's transactions read too, so it could run before a publish in
  its chunk that its type argument names. Synced settlements now wait for
  their package keys (`resolver::package_keys`), as the reference's
  transaction manager does. It predates phase 25: the Move settlement
  loaded the same packages. The seed replays identically and passes.
- The phase 25 note that seed 6's replay passed was wrong: replayed with
  the same manifest, it fails every time.
- sweep-v1 seed 13 (phase 24): macOS Security framework exception in the
  sui node's JWK fetch (`CSSMERR_CSSM_FUNCTION_FAILED`), not anchovy.

## Remaining
- Sweeps: composite and composite-through-stalls, seeds 1-24 each.
- #28520 (consensus-pool owned-version validation) for anchovy admission.
