// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `CommitStage`: chunks, heights, checkpoint sequence numbers and the
//! prologue it derives from hand-built decoded commits, and what it emits
//! as a processor.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest as _};
use consensus::BlockRef;
use messages::Message;
use protocol_config::{Chain, ProtocolVersion};
use sui_types::base_types::{ObjectID, SequenceNumber, SuiAddress, random_object_ref};
use sui_types::crypto::{AccountKeyPair, get_key_pair};
use sui_types::digests::{AdditionalConsensusStateDigest, ConsensusCommitDigest};
use sui_types::messages_consensus::ConsensusDeterminedVersionAssignments;
use sui_types::transaction::{
    Argument, CallArg, Command, ObjectArg, ProgrammableTransaction, SharedObjectMutability,
    Transaction, TransactionData, TransactionKind, VerifiedTransaction,
};
use validator::checks;
use validator::consensus::commit::{CommitStage, Decided, EpochStart};
use validator::consensus::decode::{CommittedTransaction, DecodedCommit};
use validator::consensus::execution_time::{Key, SharedObservations};
use validator::epoch::EpochParams;
use validator::epoch_store::{ConsensusOutput, EpochStore};
use validator::pipeline::items::{
    Chunk, ExecutableTransaction, PendingCheckpoint, SettlementInput, SystemKind,
};
use validator::pipeline::keys::ChunkSettled;
use workqueue::{Inbox, Processor};

const ROOT_VERSION: u64 = 5;

/// The one writer of a fresh epoch store's consensus output, and the
/// directory the store is in.
fn consensus_output() -> (ConsensusOutput, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (_, output, _) = EpochStore::open(dir.path(), 0);
    (output, dir)
}

fn epoch(max_transactions: u64, min_interval_ms: u64) -> Arc<EpochParams> {
    let mut epoch = EpochParams::new(
        Chain::Unknown,
        ProtocolVersion::MAX.as_u64(),
        3,
        messages::base::Digest::new([0; 32]),
        1000,
        4,
    );
    epoch.config.max_transactions_per_checkpoint = Some(max_transactions);
    epoch.config.min_checkpoint_interval_ms = Some(min_interval_ms);
    Arc::new(epoch)
}

struct Outputs {
    chunks: Inbox<Chunk>,
    settlements: Inbox<SettlementInput>,
    checkpoints: Inbox<waiter::Command<ChunkSettled, PendingCheckpoint>>,
    /// The epoch store the stage writes, unless the test keeps it.
    dir: Option<tempfile::TempDir>,
}

fn stage(epoch: &Arc<EpochParams>) -> (CommitStage, Outputs) {
    let (output, dir) = consensus_output();
    let (stage, mut outputs) = stage_over(epoch, output);
    outputs.dir = Some(dir);
    (stage, outputs)
}

/// A stage of a committee of one, over `output`.
fn stage_over(epoch: &Arc<EpochParams>, output: ConsensusOutput) -> (CommitStage, Outputs) {
    let (chunks, chunk_inbox) = workqueue::queue(1024);
    let (settlements, settlement_inbox) = workqueue::queue(1024);
    let (checkpoints, checkpoint_inbox) = workqueue::queue(1024);
    let start = EpochStart {
        previous_checkpoint: 10,
        previous_height: 0,
        accumulator_root_initial_shared_version: ROOT_VERSION,
        authenticator_initial_shared_version: None,
        accumulator_root_version: ROOT_VERSION,
        epoch_start_timestamp_ms: 0,
        epoch_duration_ms: u64::MAX,
    };
    (
        CommitStage::new(
            epoch.clone(),
            start,
            output,
            chunks,
            settlements,
            checkpoints,
            Arc::new(validator::consensus::status::ConsensusTxStatusCache::new()),
            vec![([0; 96], 10_000)],
        ),
        Outputs {
            chunks: chunk_inbox,
            settlements: settlement_inbox,
            checkpoints: checkpoint_inbox,
            dir: None,
        },
    )
}

/// `count` signed transactions, as `Transaction` BCS, each with zero to two
/// shared inputs among three objects.
fn transactions(count: usize, seed: u64) -> Vec<Vec<u8>> {
    let (sender, key): (SuiAddress, AccountKeyPair) = get_key_pair();
    (0..count as u64)
        .map(|i| {
            let n = (seed + i) % 3;
            let inputs = (0..n)
                .map(|j| {
                    CallArg::Object(ObjectArg::SharedObject {
                        id: ObjectID::from_single_byte(0x40 + ((i + j) % 3) as u8),
                        initial_shared_version: SequenceNumber::from_u64(1),
                        mutability: if j == 0 {
                            SharedObjectMutability::Mutable
                        } else {
                            SharedObjectMutability::Immutable
                        },
                    })
                })
                .collect();
            let pt = ProgrammableTransaction {
                inputs,
                commands: vec![],
            };
            let data = TransactionData::new(
                TransactionKind::ProgrammableTransaction(pt),
                sender,
                random_object_ref(),
                1_000_000,
                1000,
            );
            bcs::to_bytes(&Transaction::from_data_and_signer(data, vec![&key])).unwrap()
        })
        .collect()
}

/// A signed transaction splitting its gas coin, as `Transaction` BCS.
fn split_transaction() -> Vec<u8> {
    let (sender, key): (SuiAddress, AccountKeyPair) = get_key_pair();
    let pt = ProgrammableTransaction {
        inputs: vec![CallArg::Pure(bcs::to_bytes(&1u64).unwrap())],
        commands: vec![Command::SplitCoins(
            Argument::GasCoin,
            vec![Argument::Input(0)],
        )],
    };
    let data = TransactionData::new(
        TransactionKind::ProgrammableTransaction(pt),
        sender,
        random_object_ref(),
        1_000_000,
        1000,
    );
    bcs::to_bytes(&Transaction::from_data_and_signer(data, vec![&key])).unwrap()
}

fn commit(index: u32, round: u32, timestamp_ms: u64, transactions: &[Vec<u8>]) -> DecodedCommit {
    let block = BlockRef {
        round,
        author: 1,
        digest: [round as u8; 32],
    };
    DecodedCommit {
        commit_index: index,
        leader: block,
        timestamp_ms,
        digest: [index as u8; 32],
        transactions: transactions
            .iter()
            .enumerate()
            .map(|(i, bytes)| {
                CommittedTransaction::new(
                    block,
                    i as u16,
                    checks::sequenced_by_consensus(
                        Message::parse(bytes.clone()).map_err(|(e, _)| e).unwrap(),
                    ),
                    &[],
                )
            })
            .collect(),
        observations: Vec::new(),
        jwks: Vec::new(),
        rejected: Vec::new(),
        cache_hits: 0,
        cache_misses: transactions.len(),
        handled: None,
    }
}

fn digest_of(transaction: &ExecutableTransaction) -> [u8; 32] {
    match transaction {
        ExecutableTransaction::User(t) => t.get().0.digest().bytes,
        ExecutableTransaction::System(t) => t.digest,
    }
}

/// Everything a commit's decisions carry, as comparable text.
fn summary(decided: &Decided) -> String {
    let chunks: Vec<_> = decided
        .chunks
        .iter()
        .map(|c| {
            let entries: Vec<_> = c
                .entries
                .iter()
                .map(|e| {
                    let bytes = match &e.transaction {
                        ExecutableTransaction::System(t) => Some(t.bytes.clone()),
                        ExecutableTransaction::User(_) => None,
                    };
                    (digest_of(&e.transaction), bytes, &e.shared, e.origin)
                })
                .collect();
            (c.round, c.height, c.checkpoint_seq, entries)
        })
        .collect();
    let settlements: Vec<_> = decided
        .settlements
        .iter()
        .map(|s| (s.round, s.height, s.checkpoint_seq, &s.digests, s.root))
        .collect();
    let checkpoints: Vec<_> = decided
        .checkpoints
        .iter()
        .map(|c| (c.seq, &c.heights, c.timestamp_ms, c.close.is_some()))
        .collect();
    format!("{chunks:?} {settlements:?} {checkpoints:?}")
}

/// The reference's prologue additional state digest: its
/// `AdditionalConsensusState` (the window of commit timestamps) folded with
/// an `IndirectStateObserver` that observed nothing.
fn reference_state_digest(timestamps: &VecDeque<u64>) -> AdditionalConsensusStateDigest {
    // A struct of one field, in a struct of one field: BCS adds no framing.
    let d1 = AdditionalConsensusStateDigest::new(
        Blake2b::<U32>::digest(bcs::to_bytes(timestamps).unwrap()).into(),
    );
    let d2 = AdditionalConsensusStateDigest::new(Blake2b::<U32>::digest([]).into());
    let mut fold = Blake2b::<U32>::new();
    fold.update(bcs::to_bytes(&d1).unwrap());
    fold.update(bcs::to_bytes(&d2).unwrap());
    AdditionalConsensusStateDigest::new(fold.finalize().into())
}

/// The prologue digest of a commit splitting a coin, after a commit carrying
/// an observation of splits if `observed`, with the stage restarted between
/// them if `restart`, as consensus replays the window of earlier commits.
fn prologue_after(observed: bool, restart: bool, split: &[u8]) -> [u8; 32] {
    let epoch = epoch(10_000, 0);
    let dir = tempfile::tempdir().unwrap();
    let (_, output, _) = EpochStore::open(dir.path(), 0);
    let committed = output.committed_checkpoints();
    let (mut stage, _outputs) = stage_over(&epoch, output);
    let mut first = commit(1, 2, 1_000, &[]);
    if observed {
        first.observations = vec![SharedObservations {
            author: 0,
            authority: [0; 96],
            generation: 1,
            estimates: vec![(Key::SplitCoins, Duration::from_millis(50))],
        }];
    }
    stage.process(first);
    // The first commit's checkpoint commits: its output is written.
    committed.committed_through(u64::MAX);
    if restart {
        drop((stage, committed));
        let (_, output, _) = EpochStore::open(dir.path(), 0);
        stage = stage_over(&epoch, output).0;
        assert!(stage.decide(commit(1, 2, 1_000, &[])).is_none(), "replayed");
    }
    let decided = stage
        .decide(commit(2, 4, 2_000, &[split.to_vec()]))
        .unwrap();
    digest_of(&decided.chunks[0].entries[0].transaction)
}

#[test]
fn execution_time_observations_outlive_a_restart() {
    let split = split_transaction();
    let observed = prologue_after(true, false, &split);
    assert_ne!(observed, prologue_after(false, false, &split));
    assert_eq!(observed, prologue_after(true, true, &split));
}

#[test]
fn the_prologue_is_sui_s_byte_for_byte() {
    let epoch = epoch(10_000, 0);
    let (mut stage, _outputs) = stage(&epoch);
    let mut window = VecDeque::new();
    // Past the window of 10 commits, the oldest timestamps drop out.
    for i in 1..=13u32 {
        let timestamp = 1_000 * u64::from(i) + u64::from(i * i);
        let round = 2 * i;
        let decided = stage.decide(commit(i, round, timestamp, &[])).unwrap();
        if window.len() == 10 {
            window.pop_front();
        }
        window.push_back(timestamp);

        let ExecutableTransaction::System(prologue) = &decided.chunks[0].entries[0].transaction
        else {
            panic!("the prologue comes first");
        };
        assert_eq!(prologue.kind, SystemKind::Prologue { round });
        let expected = VerifiedTransaction::new_consensus_commit_prologue_v4(
            3,
            u64::from(round),
            timestamp,
            ConsensusCommitDigest::new([i as u8; 32]),
            ConsensusDeterminedVersionAssignments::CancelledTransactionsV2(vec![]),
            reference_state_digest(&window),
        );
        assert_eq!(prologue.digest, expected.digest().into_inner());
        assert_eq!(prologue.bytes, bcs::to_bytes(expected.inner()).unwrap());
    }
}

#[test]
fn chunks_split_at_the_limit_and_checkpoints_follow_the_previous_epoch() {
    let epoch = epoch(3, 0);
    let (mut stage, _outputs) = stage(&epoch);
    let users = transactions(7, 0);
    let decided = stage.decide(commit(1, 4, 100, &users)).unwrap();

    // The prologue and 7 transactions: chunks of 3, 3 and 2.
    let sizes: Vec<_> = decided.chunks.iter().map(|c| c.entries.len()).collect();
    assert_eq!(sizes, vec![3, 3, 2]);
    let heights: Vec<_> = decided.chunks.iter().map(|c| c.height).collect();
    assert_eq!(heights, vec![1, 2, 3]);
    // A full chunk leaves no room in its checkpoint: one checkpoint each,
    // after the previous epoch's last, 10.
    let seqs: Vec<_> = decided.chunks.iter().map(|c| c.checkpoint_seq).collect();
    assert_eq!(seqs, vec![11, 12, 13]);
    let checkpoints: Vec<_> = decided
        .checkpoints
        .iter()
        .map(|c| (c.seq, c.heights.clone(), c.timestamp_ms))
        .collect();
    assert_eq!(
        checkpoints,
        vec![(11, vec![1], 100), (12, vec![2], 100), (13, vec![3], 100)]
    );

    // Settlement: the chunk's digests in order, and the root read at the
    // generation its place in the commit gives.
    assert_eq!(decided.settlements.len(), decided.chunks.len());
    for (position, chunk) in decided.chunks.iter().enumerate() {
        let settlement = &decided.settlements[position];
        let digests: Vec<_> = chunk
            .entries
            .iter()
            .map(|e| digest_of(&e.transaction))
            .collect();
        assert_eq!(settlement.digests, digests);
        assert_eq!(
            (settlement.height, settlement.checkpoint_seq),
            (chunk.height, chunk.checkpoint_seq)
        );
        assert_eq!(
            settlement.root.id,
            messages::base::ObjectId::from_u16(0xacc)
        );
        assert_eq!(settlement.root.initial_shared_version, ROOT_VERSION);
        assert_eq!(settlement.root.ordinal.round, 4);
        assert_eq!(settlement.root.ordinal.writes_before, position as u32);
    }
    // The user transactions keep where consensus put them.
    let origins: Vec<_> = decided
        .chunks
        .iter()
        .flat_map(|c| &c.entries)
        .map(|e| e.origin.map(|(_, index)| index))
        .collect();
    assert_eq!(origins[0], None);
    assert_eq!(origins.len(), 8);

    // Heights and sequence numbers carry on into the next commit.
    let decided = stage.decide(commit(2, 6, 200, &[])).unwrap();
    assert_eq!(
        (decided.chunks[0].height, decided.chunks[0].checkpoint_seq),
        (4, 14)
    );
    assert_eq!(decided.checkpoints[0].heights, vec![4]);
}

#[test]
fn commits_share_a_checkpoint_until_the_interval_passes() {
    let epoch = epoch(100, 200);
    let (mut stage, _outputs) = stage(&epoch);
    let mut cut = Vec::new();
    for (i, timestamp) in [(1, 100), (2, 150), (3, 250), (4, 300)] {
        let users = transactions(2, i.into());
        let decided = stage.decide(commit(i, 2 * i, timestamp, &users)).unwrap();
        assert_eq!(decided.chunks.len(), 1);
        assert_eq!(
            decided.chunks[0].checkpoint_seq,
            if i < 4 { 11 } else { 12 }
        );
        cut.extend(
            decided
                .checkpoints
                .into_iter()
                .map(|c| (c.seq, c.heights, c.timestamp_ms)),
        );
    }
    assert_eq!(cut, vec![(11, vec![1, 2, 3], 250)]);
}

/// The same commits give the same decisions, whatever else the stage saw:
/// here commits consensus replayed and transactions included twice.
#[test]
fn stages_decide_alike() {
    let epoch = epoch(4, 100);
    let batches: Vec<_> = (0..4).map(|i| transactions(5, i)).collect();
    let commits = |batch: usize| -> Vec<Vec<u8>> {
        // Each commit repeats a transaction of the commit before.
        let mut transactions = batches[batch].clone();
        if batch > 0 {
            transactions.push(batches[batch - 1][0].clone());
        }
        transactions
    };
    let timestamps = [0, 60, 120, 400];
    let nth = |i: usize| commit(i as u32 + 1, 3 * i as u32 + 1, timestamps[i], &commits(i));

    let (mut first, _a) = stage(&epoch);
    let once: Vec<_> = (0..4)
        .map(|i| summary(&first.decide(nth(i)).unwrap()))
        .collect();

    let (mut second, _b) = stage(&epoch);
    let mut replayed = Vec::new();
    for i in 0..4 {
        replayed.push(summary(&second.decide(nth(i)).unwrap()));
        assert!(second.decide(nth(i)).is_none());
    }
    assert_eq!(once, replayed);
}

#[test]
fn the_stage_emits_chunks_settlements_and_checkpoint_waits() {
    let epoch = epoch(4, 0);
    let (mut stage, outputs) = stage(&epoch);
    let users = transactions(5, 0);
    assert_eq!(stage.process(commit(1, 2, 50, &users)), Some(()));

    let mut chunks = Vec::new();
    while let Some(chunk) = outputs.chunks.try_pop() {
        chunks.push((chunk.height, chunk.checkpoint_seq, chunk.entries.len()));
    }
    assert_eq!(chunks, vec![(1, 11, 4), (2, 12, 2)]);
    let mut settled = Vec::new();
    while let Some(input) = outputs.settlements.try_pop() {
        let SettlementInput::Chunk(settlement) = input else {
            panic!("chunks to settle");
        };
        settled.push((settlement.height, settlement.digests.len()));
    }
    assert_eq!(settled, vec![(1, 4), (2, 2)]);

    // The pending checkpoints wait for their chunks' `ChunkSettled`.
    let mut waiter = waiter::WaiterProcessor::new(|_: &ChunkSettled| false);
    let command = outputs.checkpoints.try_pop().unwrap();
    assert!(outputs.checkpoints.try_pop().is_none());
    assert!(waiter.process(command).is_none());
    assert!(
        waiter
            .process(waiter::Command::notify(vec![ChunkSettled(2)]))
            .unwrap()
            .iter()
            .map(|c| (c.seq, c.heights.clone()))
            .eq([(12, vec![2])])
    );
    let ready = waiter
        .process(waiter::Command::notify(vec![ChunkSettled(1)]))
        .unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!((ready[0].seq, ready[0].timestamp_ms), (11, 50));

    // A commit handled before emits nothing.
    assert_eq!(stage.process(commit(1, 2, 50, &users)), None);
    assert!(outputs.chunks.try_pop().is_none());
}
