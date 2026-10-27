// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A checkpoint's summary, which needs the previous checkpoint, and this
//! validator's signature over it.

use messages::base::{AuthorityPublicKeyBytes, Digest};
use messages::checkpoint::CheckpointCommitmentKind;
use messages::effects::GasCostSummary;
use messages::fast::checkpoint::{CommitteeMember, EndOfEpoch};
use messages::fast::{Bump, SummaryBuilder};
use validator::epoch::EpochParams;
use validator::pipeline::items::{CheckpointContentsBuilt, LastOfEpoch};
use validator::pipeline::tables::BuiltCheckpoint;

use crate::key::ProtocolKey;

/// `IntentScope::CheckpointSummary`, `IntentVersion::V0`, `AppId::Sui`.
const CHECKPOINT_SUMMARY_INTENT: [u8; 3] = [2, 0, 0];

/// `ConsensusTransactionKind::CheckpointSignatureV2`'s variant index.
const CHECKPOINT_SIGNATURE_V2: u8 = 11;

/// A built summary, and the totals the next checkpoint continues from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// `CheckpointSummary` BCS.
    pub bytes: Vec<u8>,
    pub digest: [u8; 32],
    pub network_total_transactions: u64,
    pub epoch_rolling_gas: [u64; 4],
}

/// Epoch `epoch`'s gas totals through `built` (computation, storage, rebate,
/// non-refundable), after `previous`: restarted with each epoch's first
/// checkpoint.
pub fn rolling_gas(
    built: &CheckpointContentsBuilt,
    previous: Option<&BuiltCheckpoint>,
    epoch: u64,
) -> [u64; 4] {
    match previous {
        Some(p) if summary_epoch(&p.summary) == epoch => {
            std::array::from_fn(|i| p.epoch_rolling_gas[i] + built.gas[i])
        }
        _ => built.gas,
    }
}

/// The summary of `built`, in epoch `params`, after `previous` (`None` only
/// for checkpoint 0), as the reference's `CheckpointSummary::new`, with its
/// end-of-epoch data if it is the epoch's last, with temporaries in `bump`.
pub fn build(
    built: &CheckpointContentsBuilt,
    previous: Option<&BuiltCheckpoint>,
    params: &EpochParams,
    bump: &Bump,
) -> Summary {
    let epoch = params.epoch;
    let network_total_transactions =
        previous.map_or(0, |p| p.network_total_transactions) + built.transaction_count;
    let epoch_rolling_gas = rolling_gas(built, previous, epoch);

    let version_specific_data = match params
        .config
        .checkpoint_summary_version_specific_data_as_option()
    {
        None | Some(0) => containers::Vec::new_in(bump),
        Some(1) => version_specific_data_v1(&built.randomness_rounds, bump),
        Some(v) => panic!("unknown checkpoint summary version-specific data version {v}"),
    };
    let mut commitments = containers::Vec::new_in(bump);
    commitments.extend(built.artifacts_digest.iter().map(|d| {
        (
            CheckpointCommitmentKind::CheckpointArtifactsDigest,
            Digest::new(*d),
        )
    }));

    let mut summary = SummaryBuilder::new_in(
        bump,
        epoch,
        built.pending.seq,
        network_total_transactions,
        Digest::new(built.contents_digest),
        GasCostSummary {
            computation_cost: epoch_rolling_gas[0],
            storage_cost: epoch_rolling_gas[1],
            storage_rebate: epoch_rolling_gas[2],
            non_refundable_storage_fee: epoch_rolling_gas[3],
        },
        built.pending.timestamp_ms,
    );
    summary.previous_digest = previous.map(|p| Digest::new(p.digest));
    summary.checkpoint_commitments = &commitments;
    summary.version_specific_data = &version_specific_data;
    let closed = match &built.last {
        Some(LastOfEpoch::Closed(end)) => Some(end),
        Some(LastOfEpoch::Effects(_)) => panic!("an epoch's last checkpoint summarized unclosed"),
        None => None,
    };
    let next_committee: Vec<CommitteeMember> = closed.map_or_else(Vec::new, |end| {
        end.next_committee
            .iter()
            .map(|(key, power)| (AuthorityPublicKeyBytes::new(*key), *power))
            .collect()
    });
    let epoch_commitments: Vec<(CheckpointCommitmentKind, Digest)> = closed
        .and_then(|end| end.root_state_digest)
        .map(|d| {
            (
                CheckpointCommitmentKind::EcmhLiveObjectSetDigest,
                Digest::new(d),
            )
        })
        .into_iter()
        .collect();
    summary.end_of_epoch_data = closed.map(|end| EndOfEpoch {
        next_epoch_committee: &next_committee,
        next_epoch_protocol_version: end.next_protocol_version,
        epoch_commitments: &epoch_commitments,
    });
    let finished = summary.finish();
    Summary {
        bytes: finished.bytes.to_vec(),
        digest: finished.digest.bytes,
        network_total_transactions,
        epoch_rolling_gas,
    }
}

/// A summary's epoch: its first field.
fn summary_epoch(summary: &[u8]) -> u64 {
    u64::from_le_bytes(
        summary[..8]
            .try_into()
            .expect("a summary starts with its epoch"),
    )
}

/// `CheckpointVersionSpecificData::V1 { randomness_rounds }` BCS.
fn version_specific_data_v1<'b>(
    randomness_rounds: &[u64],
    bump: &'b Bump,
) -> containers::Vec<'b, u8> {
    let mut out = containers::Vec::with_capacity_in(6 + 8 * randomness_rounds.len(), bump);
    out.push(0);
    let mut n = randomness_rounds.len();
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    for round in randomness_rounds {
        out.extend_from_slice(&round.to_le_bytes());
    }
    out
}

/// `key`'s signature over `summary` in `epoch`, as the reference's
/// `AuthoritySignInfo::new`: the intent message's BCS, then the epoch.
pub fn sign(key: &ProtocolKey, summary: &[u8], epoch: u64, bump: &Bump) -> [u8; 48] {
    let mut message = containers::Vec::with_capacity_in(3 + summary.len() + 8, bump);
    message.extend_from_slice(&CHECKPOINT_SUMMARY_INTENT);
    message.extend_from_slice(summary);
    message.extend_from_slice(&epoch.to_le_bytes());
    key.sign(&message)
}

/// The `ConsensusTransaction` BCS submitting `key`'s signature over
/// `summary`: a `CheckpointSignatureV2` of a `SignedCheckpointSummary`.
pub fn signature_transaction(
    key: &ProtocolKey,
    summary: &[u8],
    epoch: u64,
    bump: &Bump,
) -> Vec<u8> {
    let signature = sign(key, summary, epoch, bump);
    let mut out = Vec::with_capacity(8 + 1 + summary.len() + 8 + 97 + 48);
    // The tracking id is local bookkeeping; the reference hashes the
    // signature into it.
    out.extend_from_slice(&signature[..8]);
    out.push(CHECKPOINT_SIGNATURE_V2);
    out.extend_from_slice(summary);
    out.extend_from_slice(&epoch.to_le_bytes());
    out.push(96);
    out.extend_from_slice(key.public());
    out.extend_from_slice(&signature);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_specific_data_lengths() {
        let bump = Bump::default();
        assert_eq!(*version_specific_data_v1(&[], &bump), [0, 0]);
        let rounds: Vec<u64> = (0..130).collect();
        let bytes = version_specific_data_v1(&rounds, &bump);
        assert_eq!(&bytes[..3], &[0, 0x82, 0x01]);
        assert_eq!(bytes.len(), 3 + 8 * 130);
    }
}
