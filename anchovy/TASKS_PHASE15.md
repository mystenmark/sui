# Phase 15 progress: consensus integration

Goal: run consensus-core (Mysticeti) under the node, through a crate that
only contains glue (`consensus-glue`); the commit handler and voter stay in
`validator::consensus`.

## Status: committees and keys done; design with the user

## Steps

1. Committees and keys. Done: epoch records keep each member's consensus
   key (the worker key), network key, address and hostname; the node holds
   its protocol, worker and network keys (`ValidatorKeys`).
2. Survey of sui's wiring: `docs/consensus_integration_reference.md`.
3. Design: `docs/consensus_integration_design.md`. To be agreed.
4. Implementation, in the design's order of work.
