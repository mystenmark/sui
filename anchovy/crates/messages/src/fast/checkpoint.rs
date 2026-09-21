// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::{Built, Bump, Writer};
use crate::base::{AuthorityPublicKeyBytes, Digest};
use crate::checkpoint::{CheckpointCommitment, CheckpointCommitmentKind};
use crate::effects::GasCostSummary;

/// A signature over a transaction and the alias version it was checked
/// against, as `CheckpointContents::V2` records them.
pub type UserSignature<'a> = (&'a [u8], Option<u64>);

/// Builds `CheckpointContents::V2`: one entry per transaction, in order.
pub struct ContentsBuilder<'a> {
    bump: &'a Bump,
    transactions: containers::Vec<'a, (Digest, Digest, &'a [UserSignature<'a>])>,
    bytes: usize,
}

impl<'a> ContentsBuilder<'a> {
    pub fn new_in(bump: &'a Bump, expected: usize) -> ContentsBuilder<'a> {
        ContentsBuilder {
            bump,
            transactions: containers::Vec::with_capacity_in(expected, bump),
            bytes: 2,
        }
    }

    pub fn push(
        &mut self,
        transaction: Digest,
        effects: Digest,
        signatures: &'a [UserSignature<'a>],
    ) -> &mut Self {
        self.bytes += 66
            + 1
            + signatures
                .iter()
                .map(|(s, _)| 2 + s.len() + 9)
                .sum::<usize>();
        self.transactions.push((transaction, effects, signatures));
        self
    }

    pub fn len(&self) -> usize {
        self.transactions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.transactions.is_empty()
    }

    pub fn finish(self) -> Built<'a> {
        let mut w = Writer::new_in(self.bump, self.bytes);
        w.u8(1);
        w.len_prefix(self.transactions.len());
        for (transaction, effects, signatures) in &self.transactions {
            w.digest(transaction);
            w.digest(effects);
            w.len_prefix(signatures.len());
            for (signature, alias_version) in *signatures {
                w.bytes(signature);
                w.option_u64(*alias_version);
            }
        }
        w.finish("CheckpointContents")
    }
}

/// A `nextEpochCommittee` entry.
pub type CommitteeMember = (AuthorityPublicKeyBytes, u64);

pub struct EndOfEpoch<'a> {
    pub next_epoch_committee: &'a [CommitteeMember],
    pub next_epoch_protocol_version: u64,
    pub epoch_commitments: &'a [(CheckpointCommitmentKind, Digest)],
}

/// Builds a `CheckpointSummary`. Every field is required up front; the
/// optional ones default to absent.
pub struct SummaryBuilder<'a> {
    bump: &'a Bump,
    pub epoch: u64,
    pub sequence_number: u64,
    pub network_total_transactions: u64,
    pub content_digest: Digest,
    pub previous_digest: Option<Digest>,
    pub epoch_rolling_gas_cost_summary: GasCostSummary,
    pub timestamp_ms: u64,
    pub checkpoint_commitments: &'a [(CheckpointCommitmentKind, Digest)],
    pub end_of_epoch_data: Option<EndOfEpoch<'a>>,
    pub version_specific_data: &'a [u8],
}

impl<'a> SummaryBuilder<'a> {
    pub fn new_in(
        bump: &'a Bump,
        epoch: u64,
        sequence_number: u64,
        network_total_transactions: u64,
        content_digest: Digest,
        epoch_rolling_gas_cost_summary: GasCostSummary,
        timestamp_ms: u64,
    ) -> SummaryBuilder<'a> {
        SummaryBuilder {
            bump,
            epoch,
            sequence_number,
            network_total_transactions,
            content_digest,
            previous_digest: None,
            epoch_rolling_gas_cost_summary,
            timestamp_ms,
            checkpoint_commitments: &[],
            end_of_epoch_data: None,
            version_specific_data: &[],
        }
    }

    pub fn finish(self) -> Built<'a> {
        let committee = self.end_of_epoch_data.as_ref().map_or(0, |e| {
            e.next_epoch_committee.len() * 105 + e.epoch_commitments.len() * 34
        });
        let bytes = 128
            + self.checkpoint_commitments.len() * 34
            + committee
            + self.version_specific_data.len();
        let mut w = Writer::new_in(self.bump, bytes);
        w.u64(self.epoch);
        w.u64(self.sequence_number);
        w.u64(self.network_total_transactions);
        w.digest(&self.content_digest);
        w.option_digest(self.previous_digest.as_ref());
        w.u64(self.epoch_rolling_gas_cost_summary.computation_cost);
        w.u64(self.epoch_rolling_gas_cost_summary.storage_cost);
        w.u64(self.epoch_rolling_gas_cost_summary.storage_rebate);
        w.u64(
            self.epoch_rolling_gas_cost_summary
                .non_refundable_storage_fee,
        );
        w.u64(self.timestamp_ms);
        commitments(&mut w, self.checkpoint_commitments);
        match &self.end_of_epoch_data {
            Some(e) => {
                w.u8(1);
                w.len_prefix(e.next_epoch_committee.len());
                for (authority, stake) in e.next_epoch_committee {
                    w.bytes(&authority.bytes);
                    w.u64(*stake);
                }
                w.u64(e.next_epoch_protocol_version);
                commitments(&mut w, e.epoch_commitments);
            }
            None => w.u8(0),
        }
        w.bytes(self.version_specific_data);
        w.finish("CheckpointSummary")
    }
}

fn commitments(w: &mut Writer<'_>, commitments: &[(CheckpointCommitmentKind, Digest)]) {
    w.len_prefix(commitments.len());
    for (kind, digest) in commitments {
        w.u8(match kind {
            CheckpointCommitmentKind::EcmhLiveObjectSetDigest => 0,
            CheckpointCommitmentKind::CheckpointArtifactsDigest => 1,
        });
        w.digest(digest);
    }
}

impl CheckpointCommitment {
    /// The pair the builders take.
    pub fn parts(&self) -> (CheckpointCommitmentKind, Digest) {
        (self.kind(), self.digest)
    }
}
