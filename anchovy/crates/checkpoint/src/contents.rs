// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A checkpoint's contents and what follows from its effects alone, as the
//! reference's `CheckpointBuilder::create_checkpoints` computes them.

use std::sync::Arc;

use blake2::Blake2b;
use blake2::digest::consts::U32;
use messages::arena::BumpAlloc;
use messages::base::{Digest, ObjectId};
use messages::effects::{
    GasCostSummary, IdOperation, ObjectOut, TransactionEffects, VersionedEffects,
};
use messages::fast::checkpoint::UserSignature;
use messages::fast::{Bump, Writer};
use messages::reader::Reader;
use messages::transaction::{GenericSignature, SenderSignedData, TransactionKind};
use protocol_config::ProtocolConfig;
use validator::pipeline::items::ExecutedEffects;

/// The reference's `ObjectDigest::OBJECT_DIGEST_DELETED` and `_WRAPPED`.
const OBJECT_DIGEST_DELETED: Digest = Digest::new([99; 32]);
const OBJECT_DIGEST_WRAPPED: Digest = Digest::new([88; 32]);

/// What a checkpoint's contents determine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contents {
    /// `CheckpointContents` BCS.
    pub bytes: Arc<[u8]>,
    pub digest: [u8; 32],
    /// Computation, storage, rebate, non-refundable.
    pub gas: [u64; 4],
    /// Present when the protocol commits to the artifacts in the summary.
    pub artifacts_digest: Option<[u8; 32]>,
    pub randomness_rounds: Vec<u64>,
}

/// Builds the contents of a checkpoint whose effects, in checkpoint order,
/// are `effects`, and whose transactions, aligned with them, are
/// `transactions`, with temporaries in `bump`.
///
/// Each user signature is recorded with no alias version: aliases are
/// object state anchovy does not read yet, so every signer signs as itself
/// (see `consensus::vote`).
///
/// # Panics
/// On effects that do not parse or are V1, which this validator's executor
/// never produces, or on `transactions` not aligned with `effects`.
pub fn build(
    config: &ProtocolConfig,
    effects: &[ExecutedEffects],
    transactions: &[SenderSignedData<'_>],
    bump: &Bump,
) -> Contents {
    assert_eq!(
        effects.len(),
        transactions.len(),
        "a transaction per effects"
    );
    let mut gas = [0u64; 4];
    let mut written = containers::Vec::new_in(bump);
    for executed in effects {
        let mut r = Reader::new(&executed.effects);
        let parsed = TransactionEffects::parse(&mut r, &mut BumpAlloc(bump))
            .and_then(|e| r.finish().map(|()| e))
            .expect("executed effects parse");
        let VersionedEffects::V2(v2) = parsed.version else {
            panic!("checkpoints cover effects V2 only");
        };
        add_gas(&mut gas, &v2.gas_used);
        // The reference's `written()`.
        for change in v2.changed_objects {
            let state = match (&change.output_state, change.id_operation) {
                (ObjectOut::NotExist, IdOperation::Deleted) => {
                    Some((v2.lamport_version, OBJECT_DIGEST_DELETED))
                }
                (ObjectOut::NotExist, IdOperation::None) => {
                    Some((v2.lamport_version, OBJECT_DIGEST_WRAPPED))
                }
                (ObjectOut::ObjectWrite(digest, _), _) => Some((v2.lamport_version, **digest)),
                (ObjectOut::PackageWrite(version, digest), _) => Some((*version, **digest)),
                _ => None,
            };
            if let Some((version, digest)) = state {
                written.push((*change.id, version, digest));
            }
        }
    }

    let randomness_rounds = transactions
        .iter()
        .filter_map(|t| match t.data().kind() {
            TransactionKind::RandomnessStateUpdate(update) => Some(update.get().randomness_round),
            _ => None,
        })
        .collect();

    let built = if config.address_aliases() {
        let mut contents = messages::fast::ContentsBuilder::new_in(bump, effects.len());
        // The lengths are asserted equal above.
        for (executed, transaction) in effects.iter().zip(transactions) {
            let listed = listed_signatures(transaction);
            let mut signatures =
                containers::Vec::<UserSignature<'_>>::with_capacity_in(listed.len(), bump);
            signatures.extend(listed.iter().map(|s| (s.0, None)));
            contents.push(
                Digest::new(executed.digest),
                Digest::new(executed.effects_digest),
                signatures.leak(),
            );
        }
        contents.finish()
    } else {
        contents_v1(effects, transactions, bump)
    };

    let artifacts_digest = config
        .include_checkpoint_artifacts_digest_in_summary()
        .then(|| artifacts_digest(&mut written, bump));

    Contents {
        bytes: Arc::from(built.bytes),
        digest: built.digest.bytes,
        gas,
        artifacts_digest,
        randomness_rounds,
    }
}

fn add_gas(total: &mut [u64; 4], gas: &GasCostSummary) {
    total[0] += gas.computation_cost;
    total[1] += gas.storage_cost;
    total[2] += gas.storage_rebate;
    total[3] += gas.non_refundable_storage_fee;
}

/// `CheckpointContents::V1`: the execution digests, then each
/// transaction's signatures.
fn contents_v1<'a>(
    effects: &[ExecutedEffects],
    transactions: &[SenderSignedData<'_>],
    bump: &'a Bump,
) -> messages::fast::Built<'a> {
    let signatures: usize = transactions
        .iter()
        .flat_map(listed_signatures)
        .map(|s| 3 + s.0.len())
        .sum();
    let mut w = Writer::new_in(bump, 10 + effects.len() * 69 + signatures);
    w.u8(0);
    w.len_prefix(effects.len());
    for executed in effects {
        w.digest(&Digest::new(executed.digest));
        w.digest(&Digest::new(executed.effects_digest));
    }
    w.len_prefix(transactions.len());
    for transaction in transactions {
        let listed = listed_signatures(transaction);
        w.len_prefix(listed.len());
        for signature in listed {
            w.bytes(signature.0);
        }
    }
    w.finish("CheckpointContents")
}

/// The signatures the contents list for `transaction`: none for the epoch's
/// last, whose signatures the reference's builder leaves out
/// (`create_and_execute_advance_epoch_tx`), else its own.
fn listed_signatures<'a, 's>(transaction: &'s SenderSignedData<'a>) -> &'s [GenericSignature<'a>] {
    match transaction.data().kind() {
        TransactionKind::EndOfEpochTransaction(_) | TransactionKind::ChangeEpoch(_) => &[],
        _ => transaction.tx_signatures(),
    }
}

type Blake2b256 = Blake2b<U32>;

/// The reference's `CheckpointArtifacts::from(effects).digest()`: the
/// latest state of each object written, by id, committed to by a Merkle
/// tree, the one artifact's digest then hashed as a list. `written` is
/// in checkpoint order and is sorted here.
pub fn artifacts_digest(written: &mut [(ObjectId, u64, Digest)], bump: &Bump) -> [u8; 32] {
    use blake2::Digest as _;
    // Stable, so an object's states stay in checkpoint order: the last of
    // each run is its latest.
    written.sort_by_key(|w| w.0);
    let mut leaves = containers::Vec::<[u8; 32]>::with_capacity_in(written.len(), bump);
    for (i, (id, version, digest)) in written.iter().enumerate() {
        if written.get(i + 1).is_some_and(|next| next.0 == *id) {
            debug_assert!(written[i + 1].1 > *version, "object versions increase");
            continue;
        }
        // The leaf is the BCS of `(ObjectID, SequenceNumber, ObjectDigest)`.
        let mut hasher = Blake2b256::new();
        hasher.update([0u8]);
        hasher.update(id.0);
        hasher.update(version.to_le_bytes());
        hasher.update([32u8]);
        hasher.update(digest.bytes);
        leaves.push(hasher.finalize().into());
    }
    let root = merkle_root(&mut leaves);
    // BCS of a one-element `Vec<Digest>`.
    let mut hasher = Blake2b256::new();
    hasher.update([1u8, 32]);
    hasher.update(root);
    hasher.finalize().into()
}

/// fastcrypto's `MerkleTree::<Blake2b256>` root over leaf hashes `level`:
/// inner nodes hash a 1 prefix and both children, an odd level is padded
/// with the all-zero empty node, and the empty tree's root is that node.
/// The root over `level`, which is overwritten; a level of odd length is
/// padded with a zero node.
fn merkle_root(level: &mut [[u8; 32]]) -> [u8; 32] {
    use blake2::Digest as _;
    let mut len = level.len();
    while len > 1 {
        for i in 0..len.div_ceil(2) {
            let right = if 2 * i + 1 < len {
                level[2 * i + 1]
            } else {
                [0; 32]
            };
            let mut hasher = Blake2b256::new();
            hasher.update([1u8]);
            hasher.update(level[2 * i]);
            hasher.update(right);
            level[i] = hasher.finalize().into();
        }
        len = len.div_ceil(2);
    }
    level.first().copied().unwrap_or([0; 32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merkle_root_of_none_one_and_odd() {
        use blake2::Digest as _;

        assert_eq!(merkle_root(&mut []), [0; 32]);
        assert_eq!(merkle_root(&mut [[7; 32]]), [7; 32]);

        let inner = |l: [u8; 32], r: [u8; 32]| -> [u8; 32] {
            let mut h = Blake2b256::new();
            h.update([1u8]);
            h.update(l);
            h.update(r);
            h.finalize().into()
        };
        let expected = inner(inner([1; 32], [2; 32]), inner([3; 32], [0; 32]));
        assert_eq!(merkle_root(&mut [[1; 32], [2; 32], [3; 32]]), expected);
    }

    #[test]
    fn artifacts_keep_the_latest_state_of_each_object() {
        let a = ObjectId([1; 32]);
        let b = ObjectId([2; 32]);
        let mut repeated = vec![
            (b, 3, Digest::new([5; 32])),
            (a, 3, Digest::new([6; 32])),
            (b, 4, Digest::new([7; 32])),
        ];
        let mut latest = vec![(a, 3, Digest::new([6; 32])), (b, 4, Digest::new([7; 32]))];
        assert_eq!(
            artifacts_digest(&mut repeated, &Bump::default()),
            artifacts_digest(&mut latest, &Bump::default())
        );
    }
}
