// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Following the certified chain: summaries verified against sui's
//! certificates, chained from the committed checkpoint, and waited on to be
//! built here.

use std::sync::Arc;

use aggregator::Committee;
use fastcrypto::traits::AggregateAuthenticator as _;
use sui_types::committee::Committee as SuiCommittee;
use sui_types::crypto::{
    AggregateAuthoritySignature, AuthorityKeyPair, AuthorityQuorumSignInfo, KeypairTraits,
};
use sui_types::digests::CheckpointContentsDigest;
use sui_types::gas::GasCostSummary;
use sui_types::message_envelope::Message;
use sui_types::messages_checkpoint::{
    CertifiedCheckpointSummary, CheckpointContents, CheckpointSummary, EndOfEpochData,
    SignedCheckpointSummary,
};
use sui_types::supported_protocol_versions::ProtocolVersion;
use synced::{SyncedScheduler, WINDOW};
use tokio::sync::oneshot;
use validator::epoch_store::EpochStore;
use validator::pipeline::chunk_lock::{ChunkLock, Cut};
use validator::pipeline::items::{BuiltCommand, BuiltWaitItem};
use validator::pipeline::keys::CheckpointBuilt;
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore};
use validator::state_sync::{SyncRequest, SyncedInput, Wanted};
use waiter::Command;
use workqueue::{Inbox, Processor};

/// A committee of four in epoch 0.
struct Signers {
    sui: SuiCommittee,
    /// Key pairs by committee position.
    keys: Vec<AuthorityKeyPair>,
}

impl Signers {
    fn new() -> Signers {
        let (sui, keys) = SuiCommittee::new_simple_test_committee_of_size(4);
        let mut keys: Vec<_> = keys
            .into_iter()
            .map(|k| (sui.authority_index(&k.public().into()).unwrap(), k))
            .collect();
        keys.sort_by_key(|(i, _)| *i);
        let keys = keys.into_iter().map(|(_, k)| k).collect();
        Signers { sui, keys }
    }

    fn committee(&self) -> Committee {
        Committee::new(
            self.sui.epoch,
            self.sui
                .voting_rights
                .iter()
                .map(|(name, stake)| (name.0, *stake)),
        )
        .unwrap()
    }

    /// `signers`' certificate over `summary`, as sui encodes it, whatever
    /// their stake.
    fn certify(&self, summary: &CheckpointSummary, signers: &[usize]) -> Vec<u8> {
        let signatures: Vec<_> = signers
            .iter()
            .map(|&i| {
                let name = self.keys[i].public().into();
                SignedCheckpointSummary::new(summary.epoch, summary.clone(), &self.keys[i], name)
                    .auth_sig()
                    .signature
                    .clone()
            })
            .collect();
        let quorum = AuthorityQuorumSignInfo::<true> {
            epoch: summary.epoch,
            signature: AggregateAuthoritySignature::aggregate(&signatures).unwrap(),
            signers_map: signers.iter().map(|&i| i as u32).collect(),
        };
        bcs::to_bytes(&CertifiedCheckpointSummary::new_from_data_and_sig(
            summary.clone(),
            quorum,
        ))
        .unwrap()
    }
}

fn summary(seq: u64, previous: Option<&CheckpointSummary>) -> CheckpointSummary {
    CheckpointSummary {
        epoch: 0,
        sequence_number: seq,
        network_total_transactions: seq,
        content_digest: CheckpointContentsDigest::new([seq as u8; 32]),
        previous_digest: previous.map(Message::digest),
        epoch_rolling_gas_cost_summary: GasCostSummary::default(),
        timestamp_ms: seq,
        checkpoint_commitments: vec![],
        end_of_epoch_data: None,
        version_specific_data: vec![],
    }
}

/// `summary`'s committed record, under `certificate`.
fn record(summary: &CheckpointSummary, certificate: Vec<u8>) -> store::CheckpointRecord {
    BuiltCheckpoint {
        seq: summary.sequence_number,
        summary: bcs::to_bytes(summary).unwrap(),
        digest: summary.digest().into_inner(),
        contents: bcs::to_bytes(&CheckpointContents::new_with_digests_only_for_tests(
            Vec::new(),
        ))
        .unwrap(),
        network_total_transactions: 0,
        epoch_rolling_gas: [0; 4],
        last_height: 0,
        timestamp_ms: 0,
        root_state: None,
    }
    .record(certificate)
}

struct Setup {
    signers: Signers,
    checkpoints: Arc<CheckpointStore>,
    scheduler: SyncedScheduler,
    built: Inbox<BuiltCommand>,
    /// The chain from genesis: each summary's previous digest is the one
    /// before's.
    chain: Vec<CheckpointSummary>,
    _dir: tempfile::TempDir,
}

impl Setup {
    /// Genesis committed, and checkpoints 1 to `length` in the chain, the
    /// last ending the epoch if `ends`; every one of them cut.
    fn new(length: u64, ends: bool) -> Setup {
        Setup::cut_at(
            length,
            ends,
            Cut {
                seq: length,
                height: length,
            },
        )
    }

    /// `new`, the commit stage having cut the checkpoints through `cut`.
    fn cut_at(length: u64, ends: bool, cut: Cut) -> Setup {
        let signers = Signers::new();
        let mut chain = vec![summary(0, None)];
        for seq in 1..=length {
            let next = summary(seq, chain.last());
            chain.push(next);
        }
        if ends {
            chain.last_mut().unwrap().end_of_epoch_data = Some(EndOfEpochData {
                next_epoch_committee: signers.sui.voting_rights.clone(),
                next_epoch_protocol_version: ProtocolVersion::new(1),
                epoch_commitments: vec![],
            });
        }
        let dir = tempfile::tempdir().unwrap();
        let (epoch_store, _, builder) = EpochStore::open(dir.path(), 0);
        let store = Arc::new(store::Store::open(&dir.path().join("perpetual")).unwrap());
        store.commit_genesis_checkpoint(
            record(&chain[0], signers.certify(&chain[0], &[0, 1, 2])),
            vec![],
        );
        let checkpoints = Arc::new(CheckpointStore::new(store.clone(), 0, epoch_store, builder));
        let (built, inbox) = workqueue::queue(16);
        let lock = Arc::new(ChunkLock::new(cut.height, cut));
        let scheduler = SyncedScheduler::new(
            Arc::new(signers.committee()),
            store,
            checkpoints.clone(),
            lock.clone(),
            synced::Outputs {
                built,
                inputs: workqueue::queue(16).0,
                effects: workqueue::queue(16).0,
                hasher: workqueue::queue(16).0,
                committer: workqueue::queue(16).0,
                beacon: workqueue::queue(16).0,
                gate: workqueue::queue(16).0,
            },
        );
        Setup {
            signers,
            checkpoints,
            scheduler,
            built: inbox,
            chain,
            _dir: dir,
        }
    }

    /// Checkpoint `seq` of the chain, certified by a quorum.
    fn certified(&self, seq: usize) -> Vec<u8> {
        self.signers.certify(&self.chain[seq], &[0, 1, 2])
    }

    /// Hands the scheduler `summaries`: what it then waits on to be built,
    /// by sequence number, and what it wants next.
    fn sync(&mut self, summaries: Vec<Vec<u8>>) -> (Vec<u64>, Wanted) {
        let (reply, wanted) = oneshot::channel();
        self.scheduler.process(
            SyncedInput::Request(SyncRequest {
                summaries,
                contents: Vec::new(),
                reply,
            }),
            &mut workqueue::Bump::default(),
        );
        let mut waiting = Vec::new();
        while let Some(command) = self.built.try_pop() {
            let Command::Wait(batch) = command else {
                panic!("the scheduler only waits");
            };
            for (item, keys) in batch.into_parts() {
                let BuiltWaitItem::Synced { certified, .. } = item else {
                    panic!("the scheduler waits synced checkpoints");
                };
                assert_eq!(keys, vec![CheckpointBuilt(certified.seq)]);
                waiting.push(certified.seq);
            }
        }
        (waiting, wanted.blocking_recv().unwrap())
    }
}

fn wanted(next: Option<u64>, room: u64) -> Wanted {
    Wanted {
        next,
        room,
        contents: Vec::new(),
    }
}

#[test]
fn the_chain_from_the_committed_checkpoint_waits_to_be_built() {
    let mut s = Setup::new(3, false);
    assert_eq!(s.sync(vec![]), (vec![], wanted(Some(1), WINDOW)));

    let summaries = (1..=3).map(|seq| s.certified(seq)).collect();
    assert_eq!(
        s.sync(summaries),
        (vec![1, 2, 3], wanted(Some(4), WINDOW - 3))
    );

    // Fetched again, from another peer.
    assert_eq!(
        s.sync(vec![s.certified(2), s.certified(3)]),
        (vec![], wanted(Some(4), WINDOW - 3))
    );

    // Committing makes room.
    s.checkpoints.committed(2);
    assert_eq!(s.sync(vec![]), (vec![], wanted(Some(4), WINDOW - 1)));
}

#[test]
fn a_summary_off_the_chain_ends_its_request() {
    let mut s = Setup::new(3, false);
    assert_eq!(
        s.sync(vec![s.certified(2), s.certified(1)]),
        (vec![], wanted(Some(1), WINDOW))
    );

    let mut unchained = s.chain[2].clone();
    unchained.previous_digest = Some(s.chain[0].digest());
    let unchained = s.signers.certify(&unchained, &[0, 1, 2]);
    assert_eq!(
        s.sync(vec![s.certified(1), unchained, s.certified(2)]),
        (vec![1], wanted(Some(2), WINDOW - 1))
    );
}

#[test]
fn a_summary_without_a_quorum_or_of_another_epoch_ends_its_request() {
    let mut s = Setup::new(2, false);
    let short = s.signers.certify(&s.chain[1], &[0, 1]);
    assert_eq!(s.sync(vec![short]), (vec![], wanted(Some(1), WINDOW)));

    let mut later = s.chain[1].clone();
    later.epoch = 1;
    let later = s.signers.certify(&later, &[0, 1, 2]);
    assert_eq!(s.sync(vec![later]), (vec![], wanted(Some(1), WINDOW)));

    assert_eq!(
        s.sync(vec![vec![1, 2, 3], s.certified(1)]),
        (vec![], wanted(Some(1), WINDOW))
    );
    assert_eq!(
        s.sync(vec![s.certified(1)]),
        (vec![1], wanted(Some(2), WINDOW - 1))
    );
}

#[test]
fn the_epoch_s_last_checkpoint_ends_the_chain() {
    let mut s = Setup::new(2, true);
    let summaries = vec![s.certified(1), s.certified(2), s.certified(2)];
    assert_eq!(s.sync(summaries), (vec![1, 2], wanted(None, WINDOW - 2)));
    assert_eq!(s.sync(vec![]), (vec![], wanted(None, WINDOW - 2)));
}

/// Past the cut, a checkpoint waits for no build: its contents are asked
/// for, a few at a time.
#[test]
fn checkpoints_past_the_cut_ask_for_their_contents() {
    let mut s = Setup::cut_at(7, false, Cut { seq: 1, height: 3 });
    let summaries = (1..=7).map(|seq| s.certified(seq)).collect();
    let (waiting, wanted) = s.sync(summaries);
    assert_eq!(waiting, vec![1]);
    assert_eq!(wanted.next, Some(8));
    let asked: Vec<_> = wanted.contents.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(asked, vec![2, 3, 4, 5]);
    assert_eq!(
        wanted.contents[0].1.bytes,
        s.chain[2].content_digest.into_inner()
    );
}
