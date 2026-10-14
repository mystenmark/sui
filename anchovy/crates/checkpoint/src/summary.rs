// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A checkpoint's summary, which needs the previous checkpoint, and this
//! validator's signature over it.

use messages::base::Digest;
use messages::checkpoint::CheckpointCommitmentKind;
use messages::effects::GasCostSummary;
use messages::fast::{Bump, SummaryBuilder};
use validator::pipeline::items::CheckpointContentsBuilt;
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

/// The summary of `built` after `previous` (`None` only for checkpoint 0),
/// as the reference's `CheckpointSummary::new`. `bump` is scratch, reset
/// here.
///
/// The last checkpoint of an epoch is built without its end-of-epoch data:
/// that needs the advance-epoch transaction, the next committee and
/// protocol version and the epoch's root state digest, which wait on
/// reconfiguration.
pub fn build(
    built: &CheckpointContentsBuilt,
    previous: Option<&BuiltCheckpoint>,
    bump: &mut Bump,
) -> Summary {
    let epoch = built.epoch.epoch;
    let network_total_transactions =
        previous.map_or(0, |p| p.network_total_transactions) + built.transaction_count;
    // Rolling gas restarts with each epoch's first checkpoint.
    let epoch_rolling_gas = match previous {
        Some(p) if summary_epoch(&p.summary) == epoch => {
            std::array::from_fn(|i| p.epoch_rolling_gas[i] + built.gas[i])
        }
        _ => built.gas,
    };

    let version_specific_data = match built.epoch.config.checkpoint_summary_version_specific_data {
        None | Some(0) => Vec::new(),
        Some(1) => version_specific_data_v1(&built.randomness_rounds),
        Some(v) => panic!("unknown checkpoint summary version-specific data version {v}"),
    };
    let commitments: Vec<(CheckpointCommitmentKind, Digest)> = built
        .artifacts_digest
        .iter()
        .map(|d| {
            (
                CheckpointCommitmentKind::CheckpointArtifactsDigest,
                Digest::new(*d),
            )
        })
        .collect();

    bump.reset();
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
fn version_specific_data_v1(randomness_rounds: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + 8 * randomness_rounds.len());
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
pub fn sign(key: &ProtocolKey, summary: &[u8], epoch: u64) -> [u8; 48] {
    let mut message = Vec::with_capacity(3 + summary.len() + 8);
    message.extend_from_slice(&CHECKPOINT_SUMMARY_INTENT);
    message.extend_from_slice(summary);
    message.extend_from_slice(&epoch.to_le_bytes());
    key.sign(&message)
}

/// The `ConsensusTransaction` BCS submitting `key`'s signature over
/// `summary`: a `CheckpointSignatureV2` of a `SignedCheckpointSummary`.
pub fn signature_transaction(key: &ProtocolKey, summary: &[u8], epoch: u64) -> Vec<u8> {
    let signature = sign(key, summary, epoch);
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
        assert_eq!(version_specific_data_v1(&[]), vec![0, 0]);
        let rounds: Vec<u64> = (0..130).collect();
        let bytes = version_specific_data_v1(&rounds);
        assert_eq!(&bytes[..3], &[0, 0x82, 0x01]);
        assert_eq!(bytes.len(), 3 + 8 * 130);
    }
}
