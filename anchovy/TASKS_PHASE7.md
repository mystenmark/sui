# Phase 7 progress: objects, input checks and execution

Plan: `IMPLEMENTATION_PLAN_PHASE7.md`. Branch `mlogan-phase7`, to be merged
into `anchovy-main`.

## Status: steps 1–4 done; step 5 partly (database and genesis flags)

## Done

1. `store`: Tidehunter key spaces, live markers, a genesis marker (the
   database's emptiness lags commits), version-at-or-before reads.
2. `execution`: sui's executor over the store, genesis (framework, Clock,
   accumulator root, gas coins), executed results read back as the
   reference's `ExecutedData` (input and output objects from the effects).
3. `validation::inputs`: the stateful checks in the reference's voting
   order; `sui-oracle --input-vectors` replays the reference's
   `check_transaction_input` with sui-core's loading and liveness steps
   replicated; all cases agree. `InputsChecked` minted in `checks`.
4. `InputChecker` and `TransactionExecutor` processors; `SubmitTransaction`
   answers each transaction with its executed data. A validity or
   signature failure still fails the request; an input failure only its
   transaction's result. Already-executed transactions are answered from
   the store. `--db` and `--genesis-coin` on the binary.

## Remaining

- Step 5: `tools/validator-client` with a funded account; benchmark the
  input checks and execution.
- A rejected transaction's `RawRejectedStatus.error` is empty: it should
  carry BCS of the reference's `SuiError`, which our `ErrorKind`s do not
  map to yet.
- Execution panics on equivocation (an owned input consumed since its
  check), e.g. two transactions of one request spending the same gas coin.
  The worker catches the panic and drops the request's reply, or, built
  with `panic=abort`, the node stops. Locks at voting will prevent it.
- The epoch's start timestamp is 0 until epochs exist.
