# Phase 19: toward a production network

From the production assessment after phase 18b, in the user's order (safe
mode skipped for now):

1. Remote panics and the version over-claim.
2. A node binary: sui's `validator.yaml`, its key files and `genesis.blob`
   (`sui-config` approved).
3. Certified-checkpoint state sync, with a StateSync server.
4. Mainnet replay of the executor.
5. Resubmitting this validator's checkpoint signatures until sequenced.

## Done

- [x] Pings, as the reference answers them: an empty submission is
  acknowledged with its block's ping position, which every committed block
  finalizes; `wait_for_effects` answers executed with the zero effects
  digest, within ten seconds.
- [x] The read RPCs (object, transaction, checkpoint, system state) answer
  `Unimplemented` rather than panicking. In the reference only diagnostic
  tools (sui-tool) call them.
- [x] Only the protocol versions the executor runs are announced and
  started: today only the highest (138), the first with gas model 15.
- [x] Found by the two-sided compat test: after a restart, rounds completed
  just before it were never pushed again, stalling a peer that needed this
  member's signature. Recent rounds' signatures are now pushed again.

- [x] Step 2: `anchovy-node` runs from sui's `validator.yaml`: its keys,
  `genesis.blob` (installed into a fresh database; a database holding
  another chain is refused), `db_path`, the RPC and p2p addresses, and
  consensus parameters. The chain is named by the genesis digest.
  Certified checkpoints are dropped until state sync exists.

## In progress

- [ ] Step 3: certified-checkpoint state sync, with a StateSync server.

## Left out / later

- The read RPCs themselves.
- Gas model 14 and older (the executor's legacy path): needed before
  mainnet or testnet reach version 138 to join them.
- Safe mode.
