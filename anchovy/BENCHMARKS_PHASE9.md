# Phase 9 benchmarks: voting

`cargo bench -p validator --bench vote` (`TXS=5000`): blocks of 100 signed
transfers, each from its own sender and paying with its own gas coin, voted
on against a funded genesis. Cold means the signatures are not yet verified
(a peer's transactions); warm means they verified before, so the signature
cache answers.

| step | cold µs/tx | warm µs/tx | allocs/tx |
|---|---|---|---|
| first working voter | 31.4 | 5.4 | 36 |
| each Ed25519 key parsed once; signer indices kept with verified signatures | 25.9 | 2.7 | 35 |
| owned inputs read their live marker once, digests from it | 25.7 | 2.1 | 25 |
| the user transaction takes over the consensus transaction's buffers | 25.1 | 2.0 | 23 |
| a block's Ed25519 signatures verified in one batch | 16.8 | 1.9 | 23 |
| the batch takes keys unparsed, its RNG seeded once | 13.6 | 1.9 | 23 |

## What was slow

- **Key decompression, three times over.** Deriving a signer's address,
  verifying its signature, and the alias claim's signer indices each
  decompressed the Ed25519 key, at about 4.4 µs each. The key is now parsed
  once, and the indices come back with verification and stay in the
  signature cache. The batch decompresses keys itself, so the batching path
  doesn't parse them at all: an Ed25519 key's encoding is its bytes as
  given.
- **Signature verification.** It's now batched per block with
  ed25519-consensus, which accepts exactly the batches whose signatures
  each verify (ZIP 215), the same library fastcrypto verifies singly with.
  A failed batch falls back to each transaction's signatures alone, so a
  block holding a bad signature costs at most about twice the unbatched
  work. fastcrypto's batch drew its random coefficients with a system call
  per draw; the batch now seeds a generator once.
- **Owned inputs.** Each one cost three store reads and two hashes of the
  object: the object at its version, its digest, then the live object and
  its digest again. The live marker carries the live version and digest,
  so it's read once and serves both.
- **Parsing the transaction twice.** The consensus transaction's view
  parsed it, then voting copied it out and parsed it again. It now takes
  over the consensus transaction's buffers.

## What is left

- **Cold:** about 92% is the batch itself, mostly key decompression and the
  multi-scalar multiplication. That's the signatures' intrinsic cost, so the
  remaining lever is parallelism: blocks voted on in parallel, or a block's
  batch split across threads.
- **Warm:** the already-executed lookup (a tidehunter read, much of it the
  read's own timers), the transaction digest, the signature cache key, and
  the input reads.
