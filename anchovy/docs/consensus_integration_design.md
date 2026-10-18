# Consensus integration

Runs consensus-core (Mysticeti) under the node, as sui's `ConsensusManager`
does (`docs/consensus_integration_reference.md`). The pipeline's interfaces
already have consensus-core's shape: the voter answers blocks
(`VoteRequest`), the commit stage takes commits (`CommitRequest`),
submissions go through `consensus::TransactionClient`, and a restart says
where consensus replays from (`ConsensusOutput::replay_from`).

## The boundary

Async code reaches the pipeline one way: it pushes an entry onto one of
the pipeline's queues, the entry carrying a oneshot, and awaits the
oneshot. Nothing else crosses: no watches, flags or callbacks for async
code to read. Acknowledgements, progress and flow control travel in the
replies.

## A crate of glue: `consensus-glue`

Only the translation between consensus-core and the pipeline, and
consensus-core's lifecycle. The commit handler, the voter and the decoder
stay where they are, in `validator::consensus`; the `consensus` crate keeps
the pipeline's own copies of consensus-core's types, so nothing else links
consensus-core. `consensus-glue` depends on consensus-core, consensus-config,
`consensus` and the node's interfaces.

| Part | Does |
|---|---|
| `committee` | `consensus_config::Committee` from the epoch record's members (already sorted by protocol key, as sui's); the worker and network secrets as `ProtocolKeyPair` and `NetworkKeyPair` |
| `config` | `ConsensusProtocolConfig` from sui's `ProtocolConfig`, as `cm:161-182`; `Parameters` defaults, with `db_path` and an optional listen address |
| `verifier` | consensus-core's `TransactionVerifier` over the voter's queue |
| `commits` | consensus-core's commits into the pipeline's, and the handled commit into the commit consumer monitor |
| `submit` | the pipeline's transaction client lanes into consensus-core's client |
| `runner` | starts and stops a `ConsensusAuthority` per epoch |

### Verifier

`verify_and_vote_batch(block, transactions)` makes a `consensus::Block`,
pushes a `VoteRequest` with its oneshot to the voter, and waits for the
verdict: rejected indices, or an invalid block
(`ValidationError::InvalidTransaction`). consensus-core's verifier is
synchronous, so this one blocks on the oneshot rather than awaiting it. It
is called from blocking threads and, for its own blocks, inline in async
code, where tokio's `blocking_recv` panics; so it blocks with
`futures::executor::block_on`, which only parks the thread. A closed queue
(the epoch stopping) answers that the block is invalid; consensus is
stopping too. `verify_batch` is the same, its rejections dropped: voting is
always on, as in sui.

### Commits

A task on consensus's runtime takes each `CommittedSubDag` from
consensus-core's unbounded channel and pushes its copy (commit index and
digest, leader, timestamp, blocks with their transactions, rejections) to
the pipeline's commit queue, with a oneshot. The commit stage answers it
once the commit is handled (prior commits included) and the pipeline can
take the next. The task awaits the answer, then sets the commit consumer
monitor to the commit and takes the next: one commit in flight, as sui's
handler handles commits one at a time.

### Submission

The pipeline's `TransactionClient` is a pair of lanes (normal, high). A task
takes from them, high first, submits to consensus-core's client, and
answers the submission with consensus-core's block, indices and status, in
the pipeline's types. A failed submission drops its answer: the submitter
sees consensus gone, as when it stops.

### Lifecycle

`Node::run` takes consensus as a trait instead of a closure, with `start`
(per epoch, after the pipeline: the epoch's state and record, the
pipeline's `ConsensusOutput`) returning the epoch's client, and `stop`,
before the node reconfigures, as sui shuts consensus down first.

`ConsensusOutput` gains the last commit the pipeline handled (the commit
record's index, 0 at the epoch's start): `CommitConsumerArgs::new(replay_from
- 1, last_processed)`. The window before it is already in `replay_from`.
These are values the pipeline hands consensus when it starts, not state it
exposes.

consensus-core is async, the node's epoch start is not, and it may run
inside the node's own runtime. The runner owns a tokio runtime for
consensus, and starts and stops the authority on it from a short-lived
thread, so it never blocks one runtime inside another. The boot counter
starts at 0 and goes up once an authority has handled a commit, as
`cm:368-387`.

### Where consensus keeps its data

Consensus's database is epoch state: each epoch's directory holds the epoch
store (`store/`) and consensus's database (`consensus/`), and both go when
the directory is deleted.

## Backpressure (a decision)

With a real consensus, commits can outrun execution. As sui's handler
awaits its backpressure manager before each commit: the commit stage
withholds a commit's answer while the object store holds more than a
threshold of uncommitted transactions, unless every certified checkpoint
has committed (else checkpoint building could deadlock). The withheld
answer is sent when the condition clears, by the checkpoint committer as
it commits. So the glue, awaiting the answer, sends no further commit;
consensus-core's channel absorbs the wait, and only its commit sync slows,
as in sui. The pipeline's internal queues stay bounded, a full one a
sizing bug: the threshold must keep in-flight work under their
capacities.

## Tests

1. **One validator on real consensus:** a node with consensus-core over a
   committee of one; transactions submitted through the RPC handle execute
   and their checkpoints certify; a crash mid-epoch resumes with consensus
   replaying from its database; epochs follow one another.
2. **Four validators in one process,** on localhost: a genesis with four
   (the builder takes several), four nodes. Transactions submitted to one
   execute on all; checkpoints certify with signatures from a quorum, so
   their summaries agree byte for byte; roots agree at each epoch's end; one
   validator crashed and restarted catches up.

The pipeline tests that play consensus by hand stay: deterministic, and
fast.

## Order of work

1. The node's consensus trait (`start`, `stop`); `ConsensusOutput`'s last
   handled commit; a oneshot on each `CommitRequest`, answered once
   handled; the epoch directory's `store/` and `consensus/`.
2. `consensus-glue`: committee and keys, configs, verifier, commits,
   submission, runner.
3. One validator on real consensus.
4. A genesis of several validators; four validators.
5. Backpressure: commit answers withheld, and released by the committer.
