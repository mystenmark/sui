// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Checkpoint signatures, routed to the summary waiter by the block voter
//! from valid blocks, and by the commit decoder from commits.

use std::sync::Arc;

use consensus::{Block, BlockRef, CommittedSubDag};
use messages::base::Digest;
use protocol_config::{Chain, ProtocolVersion};
use sui_types::committee::Committee;
use sui_types::crypto::{AuthorityPublicKeyBytes, KeypairTraits};
use sui_types::digests::CheckpointContentsDigest;
use sui_types::gas::GasCostSummary;
use sui_types::messages_checkpoint::{
    CheckpointSignatureMessage, CheckpointSummary, SignedCheckpointSummary,
};
use sui_types::messages_consensus::ConsensusTransaction;
use validator::consensus::cache::ConsensusTxCache;
use validator::consensus::decode::CommitDecoder;
use validator::consensus::vote::BlockVoter;
use validator::epoch::{EpochParams, EpochState};
use validator::pipeline::items::CheckpointSignature;
use validator::pipeline::keys::LocalSummary;
use waiter::{Command, WaiterProcessor};
use workqueue::{Inbox, Processor};

fn epoch() -> EpochParams {
    EpochParams::new(
        Chain::Unknown,
        ProtocolVersion::MAX.as_u64(),
        0,
        Digest::new([0; 32]),
        1000,
        4,
    )
}

/// A `CheckpointSignatureMessage` over checkpoint `seq`, and the
/// `ConsensusTransaction` carrying it.
fn signature(seq: u64) -> (Vec<u8>, Vec<u8>) {
    let (_, keys) = Committee::new_simple_test_committee_of_size(1);
    let summary = CheckpointSummary {
        epoch: 0,
        sequence_number: seq,
        network_total_transactions: seq,
        content_digest: CheckpointContentsDigest::new([3; 32]),
        previous_digest: None,
        epoch_rolling_gas_cost_summary: GasCostSummary::default(),
        timestamp_ms: 9,
        checkpoint_commitments: vec![],
        end_of_epoch_data: None,
        version_specific_data: vec![],
    };
    let signed = SignedCheckpointSummary::new(0, summary, &keys[0], keys[0].public().into());
    let message = CheckpointSignatureMessage { summary: signed };
    let bytes = bcs::to_bytes(&message).unwrap();
    let transaction = bcs::to_bytes(&ConsensusTransaction::new_checkpoint_signature_message_v2(
        message,
    ))
    .unwrap();
    (bytes, transaction)
}

fn commit(transactions: Vec<Vec<u8>>, rejected: Vec<u16>) -> CommittedSubDag {
    let reference = BlockRef {
        round: 1,
        author: 0,
        digest: [1; 32],
    };
    CommittedSubDag {
        commit_index: 1,
        digest: [1; 32],
        leader: reference,
        timestamp_ms: 0,
        blocks: vec![Block {
            reference,
            transactions,
        }],
        rejected: vec![(reference, rejected)],
    }
}

/// The signatures the decoder pushed, with the keys they wait for.
fn routed(
    inbox: &Inbox<Command<LocalSummary, CheckpointSignature>>,
) -> Vec<(LocalSummary, CheckpointSignature)> {
    let mut keys = Vec::new();
    let mut ready = Vec::new();
    let mut waiter = WaiterProcessor::new(|key: &LocalSummary| {
        keys.push(*key);
        true
    });
    while let Some(command) = inbox.try_pop() {
        assert!(matches!(command, Command::Wait(_)));
        ready.extend(waiter.process(command).unwrap_or_default());
    }
    drop(waiter);
    assert_eq!(keys.len(), ready.len());
    keys.into_iter().map(|key| (key, ready.remove(0))).collect()
}

#[test]
fn committed_signatures_wait_for_their_local_summary() {
    let (queue, inbox) = workqueue::queue(4);
    let mut decoder = CommitDecoder::new(Arc::new(epoch()), Arc::new(ConsensusTxCache::new()))
        .with_signatures(queue);
    let (seven, seven_tx) = signature(7);
    let (eight, eight_tx) = signature(8);
    let end_of_publish = bcs::to_bytes(&ConsensusTransaction::new_end_of_publish(
        AuthorityPublicKeyBytes::ZERO,
    ))
    .unwrap();
    let output = decoder.decode(commit(vec![seven_tx, end_of_publish, eight_tx], vec![]));
    assert!(output.transactions.is_empty());
    assert_eq!((output.cache_hits, output.cache_misses), (0, 0));

    let routed = routed(&inbox);
    assert_eq!(routed.len(), 2);
    assert_eq!(routed[0].0, LocalSummary(7));
    assert_eq!(routed[0].1.seq, 7);
    assert_eq!(&*routed[0].1.message, &seven[..]);
    assert_eq!(routed[1].0, LocalSummary(8));
    assert_eq!(&*routed[1].1.message, &eight[..]);
}

#[test]
fn rejected_signatures_are_not_routed() {
    let (queue, inbox) = workqueue::queue(4);
    let mut decoder = CommitDecoder::new(Arc::new(epoch()), Arc::new(ConsensusTxCache::new()))
        .with_signatures(queue);
    let (_, transaction) = signature(7);
    decoder.decode(commit(vec![transaction], vec![0]));
    assert!(inbox.try_pop().is_none());
}

#[test]
fn unrouted_signatures_are_left_out() {
    let mut decoder = CommitDecoder::new(Arc::new(epoch()), Arc::new(ConsensusTxCache::new()));
    let (_, transaction) = signature(7);
    let output = decoder.decode(commit(vec![transaction], vec![]));
    assert!(output.transactions.is_empty());
}

/// A voter over an empty store: blocks of signatures read nothing.
fn voter(
    queue: workqueue::Queue<Command<LocalSummary, CheckpointSignature>>,
) -> (BlockVoter, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(store::Store::open(dir.path()).unwrap());
    let epoch = Arc::new(EpochState::new(
        Chain::Unknown,
        ProtocolVersion::MAX.as_u64(),
        0,
        Digest::new([0; 32]),
        1000,
        4,
        [],
    ));
    let voter = BlockVoter::new(epoch.clone(), store, Arc::new(ConsensusTxCache::new()))
        .with_signatures(queue);
    (voter, dir)
}

fn block(transactions: Vec<Vec<u8>>) -> Block {
    Block {
        reference: BlockRef {
            round: 1,
            author: 1,
            digest: [2; 32],
        },
        transactions,
    }
}

#[test]
fn signatures_in_a_voted_block_wait_for_their_local_summary() {
    let (queue, inbox) = workqueue::queue(4);
    let (mut voter, _dir) = voter(queue);
    let (seven, seven_tx) = signature(7);
    let (_, eight_tx) = signature(8);
    assert_eq!(voter.vote(block(vec![seven_tx, eight_tx])), Ok(vec![]));

    let routed = routed(&inbox);
    assert_eq!(routed.len(), 2);
    assert_eq!(routed[0].0, LocalSummary(7));
    assert_eq!(&*routed[0].1.message, &seven[..]);
    assert_eq!(routed[1].0, LocalSummary(8));
}

#[test]
fn signatures_in_an_invalid_block_are_not_routed() {
    let (queue, inbox) = workqueue::queue(4);
    let (mut voter, _dir) = voter(queue);
    let (_, transaction) = signature(7);
    assert!(voter.vote(block(vec![transaction, vec![0xff]])).is_err());
    assert!(inbox.try_pop().is_none());
}
