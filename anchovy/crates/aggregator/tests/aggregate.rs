// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Certifying checkpoints, checked against sui's types: its committee, its
//! signatures and its certified summary's encoding.

use std::sync::Arc;

use aggregator::{CheckpointAggregator, Committee, CommitteeError};
use sui_types::committee::Committee as SuiCommittee;
use sui_types::crypto::{AuthorityKeyPair, AuthorityQuorumSignInfo, KeypairTraits};
use sui_types::digests::CheckpointContentsDigest;
use sui_types::gas::GasCostSummary;
use sui_types::message_envelope::Message as _;
use sui_types::messages_checkpoint::{
    CertifiedCheckpointSummary, CheckpointSignatureMessage, CheckpointSummary,
    SignedCheckpointSummary,
};
use validator::pipeline::items::{CertifiedCheckpoint, CheckpointSignature};
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore};
use workqueue::{Inbox, Processor};

struct Setup {
    sui: SuiCommittee,
    /// Key pairs by committee position.
    keys: Vec<AuthorityKeyPair>,
    checkpoints: Arc<CheckpointStore>,
    aggregator: CheckpointAggregator,
    /// What the aggregator handed on, and what state sync got.
    committer: Vec<CertifiedCheckpoint>,
    state_sync: Inbox<CertifiedCheckpoint>,
    /// The epoch store the checkpoints are in.
    _epochs: tempfile::TempDir,
}

/// An aggregator whose next checkpoint to certify is `next`.
fn setup(size: usize, next: u64) -> Setup {
    let (sui, keys) = SuiCommittee::new_simple_test_committee_of_size(size);
    let mut keys: Vec<_> = keys
        .into_iter()
        .map(|k| (sui.authority_index(&k.public().into()).unwrap(), k))
        .collect();
    keys.sort_by_key(|(i, _)| *i);
    let keys = keys.into_iter().map(|(_, k)| k).collect();
    let committee = Committee::new(
        sui.epoch,
        sui.voting_rights
            .iter()
            .map(|(name, stake)| (name.0, *stake)),
    )
    .unwrap();
    assert_eq!(committee.quorum_threshold(), sui.quorum_threshold());
    let epochs = tempfile::tempdir().unwrap();
    let (epoch_store, _, builder) = validator::epoch_store::EpochStore::open(epochs.path(), 0);
    let objects = Arc::new(store::Store::open(&epochs.path().join("perpetual")).unwrap());
    let checkpoints = Arc::new(CheckpointStore::new(objects, 0, epoch_store, builder));
    let (to_state_sync, state_sync) = workqueue::queue(16);
    let aggregator = CheckpointAggregator::new(
        Arc::new(committee),
        checkpoints.clone(),
        next,
        to_state_sync,
    );
    Setup {
        sui,
        keys,
        checkpoints,
        aggregator,
        committer: Vec::new(),
        state_sync,
        _epochs: epochs,
    }
}

fn summary(seq: u64, timestamp_ms: u64) -> CheckpointSummary {
    CheckpointSummary {
        epoch: 0,
        sequence_number: seq,
        network_total_transactions: 10 * seq,
        content_digest: CheckpointContentsDigest::new([seq as u8; 32]),
        previous_digest: None,
        epoch_rolling_gas_cost_summary: GasCostSummary::new(1, 2, 3, 4),
        timestamp_ms,
        checkpoint_commitments: vec![],
        end_of_epoch_data: None,
        version_specific_data: vec![1, 2, 3],
    }
}

fn build(checkpoints: &CheckpointStore, summary: &CheckpointSummary) {
    checkpoints.insert_built(BuiltCheckpoint {
        seq: summary.sequence_number,
        summary: bcs::to_bytes(summary).unwrap(),
        digest: summary.digest().into_inner(),
        contents: vec![],
        network_total_transactions: summary.network_total_transactions,
        epoch_rolling_gas: [1, 2, 3, 4],
        last_height: 0,
        timestamp_ms: 0,
        root_state: None,
    });
}

impl Setup {
    fn signed(
        &self,
        summary: &CheckpointSummary,
        signer: usize,
        named: usize,
    ) -> SignedCheckpointSummary {
        let name = self.keys[named].public().into();
        SignedCheckpointSummary::new(summary.epoch, summary.clone(), &self.keys[signer], name)
    }

    /// `signer`'s signature over `summary`, naming `named` as its author.
    fn forged(
        &self,
        summary: &CheckpointSummary,
        signer: usize,
        named: usize,
    ) -> CheckpointSignature {
        message(self.signed(summary, signer, named))
    }

    fn signature(&self, summary: &CheckpointSummary, signer: usize) -> CheckpointSignature {
        self.forged(summary, signer, signer)
    }

    fn feed(&mut self, signatures: Vec<CheckpointSignature>) {
        let certified = self.aggregator.process(signatures);
        assert!(certified.as_ref().is_none_or(|c| !c.is_empty()));
        self.committer.extend(certified.unwrap_or_default());
    }

    /// The checkpoints certified since last taken, as the committer and
    /// state sync both got them.
    fn take_certified(&mut self) -> Vec<CertifiedCheckpoint> {
        let synced: Vec<_> = std::iter::from_fn(|| self.state_sync.try_pop()).collect();
        let certified = std::mem::take(&mut self.committer);
        assert_eq!(
            synced
                .iter()
                .map(|c| (c.seq, &c.summary))
                .collect::<Vec<_>>(),
            certified
                .iter()
                .map(|c| (c.seq, &c.summary))
                .collect::<Vec<_>>()
        );
        certified
    }

    /// sui's certificate over `summary` from `signers`.
    fn sui_certificate(&self, summary: &CheckpointSummary, signers: &[usize]) -> Vec<u8> {
        let infos = signers
            .iter()
            .map(|&i| self.signed(summary, i, i).auth_sig().clone())
            .collect();
        let quorum =
            AuthorityQuorumSignInfo::<true>::new_from_auth_sign_infos(infos, &self.sui).unwrap();
        bcs::to_bytes(&CertifiedCheckpointSummary::new_from_data_and_sig(
            summary.clone(),
            quorum,
        ))
        .unwrap()
    }
}

fn message(summary: SignedCheckpointSummary) -> CheckpointSignature {
    CheckpointSignature {
        seq: summary.data().sequence_number,
        message: bcs::to_bytes(&CheckpointSignatureMessage { summary })
            .unwrap()
            .into(),
    }
}

#[test]
fn a_quorum_certifies_with_sui_s_bytes() {
    let mut s = setup(4, 7);
    let local = summary(7, 1000);
    build(&s.checkpoints, &local);
    s.feed(vec![s.signature(&local, 2), s.signature(&local, 0)]);
    assert!(s.take_certified().is_empty());

    s.feed(vec![s.signature(&local, 3), s.signature(&local, 1)]);
    let certified = s.take_certified();
    assert_eq!(certified.len(), 1);
    assert_eq!(certified[0].seq, 7);
    assert_eq!(
        &*certified[0].summary,
        &s.sui_certificate(&local, &[0, 2, 3])[..]
    );

    let decoded: CertifiedCheckpointSummary = bcs::from_bytes(&certified[0].summary).unwrap();
    decoded.verify_authority_signatures(&s.sui).unwrap();
    assert_eq!(decoded.data(), &local);

    // Signatures after the certificate are dropped.
    s.feed(vec![s.signature(&local, 1)]);
    assert!(s.take_certified().is_empty());
}

#[test]
fn a_committee_of_a_hundred_certifies_at_two_thirds() {
    let mut s = setup(100, 1);
    let local = summary(1, 5);
    build(&s.checkpoints, &local);
    let threshold = s.sui.quorum_threshold();
    let mut stake = 0;
    let mut signers = vec![];
    for i in 0..100 {
        s.feed(vec![s.signature(&local, i)]);
        stake += s.sui.voting_rights[i].1;
        signers.push(i);
        if stake >= threshold {
            break;
        }
        assert!(s.take_certified().is_empty(), "certified at {stake}");
    }
    let certified = s.take_certified();
    assert_eq!(certified.len(), 1);
    assert_eq!(
        &*certified[0].summary,
        &s.sui_certificate(&local, &signers)[..]
    );
}

#[test]
fn checkpoints_certify_in_order() {
    let mut s = setup(4, 1);
    let (one, two) = (summary(1, 10), summary(2, 20));
    build(&s.checkpoints, &one);
    build(&s.checkpoints, &two);
    s.feed(vec![
        s.signature(&two, 0),
        s.signature(&one, 0),
        s.signature(&two, 1),
        s.signature(&one, 1),
        s.signature(&two, 2),
    ]);
    // Two has a quorum, but waits for one.
    assert!(s.take_certified().is_empty());
    s.feed(vec![s.signature(&one, 3)]);
    let certified = s.take_certified();
    assert_eq!(certified.iter().map(|c| c.seq).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(
        &*certified[1].summary,
        &s.sui_certificate(&two, &[0, 1, 2])[..]
    );
    // Signatures for certified checkpoints are dropped.
    s.feed(vec![s.signature(&one, 2), s.signature(&two, 3)]);
    assert!(s.take_certified().is_empty());
}

#[test]
fn a_repeated_signature_counts_once() {
    let mut s = setup(4, 3);
    let local = summary(3, 0);
    build(&s.checkpoints, &local);
    s.feed(vec![
        s.signature(&local, 1),
        s.signature(&local, 1),
        s.signature(&local, 2),
        s.signature(&local, 1),
    ]);
    assert!(s.take_certified().is_empty());
    s.feed(vec![s.signature(&local, 0)]);
    assert_eq!(s.take_certified().len(), 1);
}

#[test]
fn an_invalid_signature_is_evicted_at_quorum() {
    let mut s = setup(4, 3);
    let local = summary(3, 0);
    build(&s.checkpoints, &local);
    // Signed by 3 in 0's name: stake enough for a quorum, but the aggregate
    // fails and 0's vote is evicted.
    s.feed(vec![
        s.forged(&local, 3, 0),
        s.signature(&local, 1),
        s.signature(&local, 2),
    ]);
    assert!(s.take_certified().is_empty());
    s.feed(vec![s.signature(&local, 3)]);
    let certified = s.take_certified();
    assert_eq!(
        &*certified[0].summary,
        &s.sui_certificate(&local, &[1, 2, 3])[..]
    );
}

#[test]
fn a_forgery_does_not_block_the_genuine_signature() {
    let mut s = setup(4, 3);
    let local = summary(3, 0);
    build(&s.checkpoints, &local);
    s.feed(vec![
        s.forged(&local, 3, 0),
        s.signature(&local, 0),
        s.signature(&local, 1),
        s.signature(&local, 2),
    ]);
    let certified = s.take_certified();
    assert_eq!(
        &*certified[0].summary,
        &s.sui_certificate(&local, &[0, 1, 2])[..]
    );
}

#[test]
fn a_signature_over_bad_bytes_is_dropped() {
    let mut s = setup(4, 3);
    let local = summary(3, 0);
    build(&s.checkpoints, &local);
    let mut bad = s.signature(&local, 0);
    let mut bytes = bad.message.to_vec();
    // Not a curve point.
    let at = bytes.len() - 48;
    bytes[at..].fill(0xff);
    bad.message = bytes.into();
    let truncated = CheckpointSignature {
        seq: 3,
        message: Arc::from(&[1u8, 2, 3][..]),
    };
    s.feed(vec![
        bad,
        truncated,
        s.signature(&local, 1),
        s.signature(&local, 2),
    ]);
    assert!(s.take_certified().is_empty());
    s.feed(vec![s.signature(&local, 0)]);
    assert_eq!(s.take_certified().len(), 1);
}

#[test]
fn outsiders_and_other_epochs_are_dropped() {
    let mut s = setup(4, 3);
    let local = summary(3, 0);
    build(&s.checkpoints, &local);
    let (_, outsiders) = SuiCommittee::new_simple_test_committee_of_size(1);
    let outsider = message(SignedCheckpointSummary::new(
        0,
        local.clone(),
        &outsiders[0],
        outsiders[0].public().into(),
    ));
    let mut later = local.clone();
    later.epoch = 1;
    let other_epoch = message(SignedCheckpointSummary::new(
        1,
        later,
        &s.keys[0],
        s.keys[0].public().into(),
    ));
    s.feed(vec![
        outsider,
        other_epoch,
        s.signature(&local, 1),
        s.signature(&local, 2),
    ]);
    assert!(s.take_certified().is_empty());
}

#[test]
fn signatures_over_another_summary_never_count() {
    let mut s = setup(4, 3);
    let local = summary(3, 0);
    let fork = summary(3, 1);
    build(&s.checkpoints, &local);
    s.feed(vec![
        s.signature(&fork, 0),
        s.signature(&fork, 1),
        s.signature(&fork, 2),
        s.signature(&local, 3),
    ]);
    assert!(s.take_certified().is_empty());

    // One forked signer does not stop the rest certifying.
    let mut s = setup(4, 3);
    build(&s.checkpoints, &local);
    s.feed(vec![
        s.signature(&fork, 0),
        s.signature(&local, 1),
        s.signature(&local, 2),
        s.signature(&local, 3),
    ]);
    let certified = s.take_certified();
    assert_eq!(
        &*certified[0].summary,
        &s.sui_certificate(&local, &[1, 2, 3])[..]
    );
}

#[test]
fn a_committee_rejects_bad_and_repeated_keys() {
    assert_eq!(
        Committee::new(0, [([0u8; 96], 1)]).err(),
        Some(CommitteeError::InvalidKey)
    );
    let (sui, _) = SuiCommittee::new_simple_test_committee_of_size(1);
    let name = sui.voting_rights[0].0.0;
    assert_eq!(
        Committee::new(0, [(name, 1), (name, 2)]).err(),
        Some(CommitteeError::Duplicate)
    );
}
