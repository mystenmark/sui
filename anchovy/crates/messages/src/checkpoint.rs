// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::arena::Alloc;
use crate::base::{
    AuthorityPublicKeyBytes, CheckpointContentsDigest, CheckpointDigest, Digest, TransactionDigest,
    TransactionEffectsDigest, U64Le,
};
use crate::effects::{GasCostSummary, TransactionEffects, TransactionEvents};
use crate::error::{ParseError, Result};
use crate::object::Object;
use crate::reader::{Reader, WireRecord};
use crate::transaction::{GenericSignature, SenderSignedData};

/// `CheckpointCommitment` as it sits on the wire: both variants are a
/// variant index and a digest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct CheckpointCommitment {
    // Checked wherever a reference is made.
    kind: u8,
    pub digest: Digest,
}

// SAFETY: `repr(C)` over wire records: alignment 1, no padding. An unchecked
// `kind` is not an invalid value, and `kind()` does not trust it.
unsafe impl WireRecord for CheckpointCommitment {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckpointCommitmentKind {
    EcmhLiveObjectSetDigest,
    CheckpointArtifactsDigest,
}

impl CheckpointCommitment {
    pub fn kind(&self) -> CheckpointCommitmentKind {
        match self.kind {
            0 => CheckpointCommitmentKind::EcmhLiveObjectSetDigest,
            _ => CheckpointCommitmentKind::CheckpointArtifactsDigest,
        }
    }

    fn parse_vec<'a>(r: &mut Reader<'a>) -> Result<&'a [CheckpointCommitment]> {
        let commitments: &[CheckpointCommitment] = r.record_vec()?;
        for c in commitments {
            if c.kind > 1 {
                return Err(ParseError::UnknownVariant {
                    ty: "CheckpointCommitment",
                    tag: u32::from(c.kind),
                });
            }
            c.digest.check()?;
        }
        Ok(commitments)
    }
}

/// A `nextEpochCommittee` entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct CommitteeMember {
    pub authority: AuthorityPublicKeyBytes,
    pub stake: U64Le,
}

// SAFETY: `repr(C)` over wire records: alignment 1, no padding.
unsafe impl WireRecord for CommitteeMember {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EndOfEpochData<'a> {
    pub next_epoch_committee: &'a [CommitteeMember],
    pub next_epoch_protocol_version: u64,
    pub epoch_commitments: &'a [CheckpointCommitment],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CheckpointSummary<'a> {
    /// The exact encoding, which is what gets hashed and signed.
    pub bytes: &'a [u8],
    /// Computed once, from `bytes`, while parsing.
    pub digest: CheckpointDigest,
    pub epoch: u64,
    pub sequence_number: u64,
    pub network_total_transactions: u64,
    pub content_digest: &'a CheckpointContentsDigest,
    pub previous_digest: Option<&'a CheckpointDigest>,
    pub epoch_rolling_gas_cost_summary: GasCostSummary,
    pub timestamp_ms: u64,
    pub checkpoint_commitments: &'a [CheckpointCommitment],
    pub end_of_epoch_data: Option<EndOfEpochData<'a>>,
    pub version_specific_data: &'a [u8],
}

impl<'a> CheckpointSummary<'a> {
    /// Allocates nothing; the arena parameter only tells the passes apart.
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, _: &mut A) -> Result<CheckpointSummary<'a>> {
        let start = r.pos();
        let epoch = r.u64()?;
        let sequence_number = r.u64()?;
        let network_total_transactions = r.u64()?;
        let content_digest = CheckpointContentsDigest::parse(r)?;
        let previous_digest = if r.option()? {
            Some(CheckpointDigest::parse(r)?)
        } else {
            None
        };
        let epoch_rolling_gas_cost_summary = GasCostSummary::parse(r)?;
        let timestamp_ms = r.u64()?;
        let checkpoint_commitments = CheckpointCommitment::parse_vec(r)?;
        let end_of_epoch_data = if r.option()? {
            let next_epoch_committee: &[CommitteeMember] = r.record_vec()?;
            for m in next_epoch_committee {
                m.authority.check()?;
            }
            Some(EndOfEpochData {
                next_epoch_committee,
                next_epoch_protocol_version: r.u64()?,
                epoch_commitments: CheckpointCommitment::parse_vec(r)?,
            })
        } else {
            None
        };
        let version_specific_data = r.byte_vec()?;
        let bytes = r.span(start);
        Ok(CheckpointSummary {
            bytes,
            digest: if A::BUILD {
                Digest::of("CheckpointSummary", bytes)
            } else {
                Digest::ZERO
            },
            epoch,
            sequence_number,
            network_total_transactions,
            content_digest,
            previous_digest,
            epoch_rolling_gas_cost_summary,
            timestamp_ms,
            checkpoint_commitments,
            end_of_epoch_data,
            version_specific_data,
        })
    }
}

/// The aggregate BLS signature is not checked to be a curve point, and the
/// signers bitmap is not decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AuthorityQuorumSignInfo<'a> {
    pub epoch: u64,
    pub signature: &'a [u8; 48],
    /// A serialized roaring bitmap of committee indices.
    pub signers_map: &'a [u8],
}

/// `Envelope<CheckpointSummary, AuthorityQuorumSignInfo>`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CertifiedCheckpointSummary<'a> {
    /// The exact encoding, as it is stored and sent.
    pub bytes: &'a [u8],
    pub data: CheckpointSummary<'a>,
    pub auth_signature: AuthorityQuorumSignInfo<'a>,
}

impl<'a> CertifiedCheckpointSummary<'a> {
    pub fn parse<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
    ) -> Result<CertifiedCheckpointSummary<'a>> {
        let start = r.pos();
        let data = CheckpointSummary::parse(r, a)?;
        let auth_signature = AuthorityQuorumSignInfo {
            epoch: r.u64()?,
            signature: r.array()?,
            signers_map: r.byte_vec()?,
        };
        Ok(CertifiedCheckpointSummary {
            bytes: r.span(start),
            data,
            auth_signature,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct ExecutionDigests {
    pub transaction: TransactionDigest,
    pub effects: TransactionEffectsDigest,
}

// SAFETY: `repr(C)` over wire records: alignment 1, no padding.
unsafe impl WireRecord for ExecutionDigests {}

impl ExecutionDigests {
    fn check(&self) -> Result<()> {
        self.transaction.check()?;
        self.effects.check()
    }
}

/// `Vec<Vec<GenericSignature>>`.
fn parse_signature_lists<'a, A: Alloc<'a>>(
    r: &mut Reader<'a>,
    a: &mut A,
) -> Result<&'a [&'a [GenericSignature<'a>]]> {
    let n = r.seq_len(1)?;
    let mut out = a.slice(n)?;
    for _ in 0..n {
        out.push(GenericSignature::parse_vec(r, a)?);
    }
    Ok(out.finish())
}

/// A `CheckpointContentsV2` entry. Each signature comes with the version of
/// the address alias object it was checked against, if any.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CheckpointTransactionContents<'a> {
    pub digest: &'a ExecutionDigests,
    pub user_signatures: &'a [(GenericSignature<'a>, Option<u64>)],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VersionedCheckpointContents<'a> {
    /// `user_signatures` is not checked to be as long as `transactions`.
    V1 {
        transactions: &'a [ExecutionDigests],
        user_signatures: &'a [&'a [GenericSignature<'a>]],
    },
    V2(&'a [CheckpointTransactionContents<'a>]),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CheckpointContents<'a> {
    /// The exact encoding, which is what gets hashed.
    pub bytes: &'a [u8],
    pub version: VersionedCheckpointContents<'a>,
}

impl<'a> CheckpointContents<'a> {
    /// Hashed on demand: the reference does not treat the contents as a
    /// message, only the summary that commits to them.
    pub fn digest(&self) -> CheckpointContentsDigest {
        Digest::of("CheckpointContents", self.bytes)
    }

    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<CheckpointContents<'a>> {
        let start = r.pos();
        let version = match r.variant()? {
            0 => {
                let transactions: &[ExecutionDigests] = r.record_vec()?;
                for t in transactions {
                    t.check()?;
                }
                VersionedCheckpointContents::V1 {
                    transactions,
                    user_signatures: parse_signature_lists(r, a)?,
                }
            }
            1 => {
                let n = r.seq_len(size_of::<ExecutionDigests>() + 1)?;
                let mut transactions = a.slice(n)?;
                for _ in 0..n {
                    let digest: &ExecutionDigests = r.record()?;
                    digest.check()?;

                    // A signature's length and an option tag.
                    let n = r.seq_len(1 + 1)?;
                    let mut user_signatures = a.slice(n)?;
                    for _ in 0..n {
                        user_signatures.push((GenericSignature(r.byte_vec()?), r.option_u64()?));
                    }
                    transactions.push(CheckpointTransactionContents {
                        digest,
                        user_signatures: user_signatures.finish(),
                    });
                }
                VersionedCheckpointContents::V2(transactions.finish())
            }
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "CheckpointContents",
                    tag,
                });
            }
        };
        Ok(CheckpointContents {
            bytes: r.span(start),
            version,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExecutionData<'a> {
    pub transaction: SenderSignedData<'a>,
    pub effects: TransactionEffects<'a>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FullCheckpointContents<'a> {
    pub transactions: &'a [ExecutionData<'a>],
    /// Not checked to be as long as `transactions`.
    pub user_signatures: &'a [&'a [GenericSignature<'a>]],
}

impl<'a> FullCheckpointContents<'a> {
    pub fn parse<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
    ) -> Result<FullCheckpointContents<'a>> {
        r.enter()?;
        let n = r.seq_len(SenderSignedData::MIN_WIRE_SIZE + TransactionEffects::MIN_WIRE_SIZE)?;
        let mut transactions = a.slice(n)?;
        for _ in 0..n {
            r.enter()?;
            transactions.push(ExecutionData {
                transaction: SenderSignedData::parse_envelope(r, a)?,
                effects: TransactionEffects::parse(r, a)?,
            });
            r.leave();
        }
        let transactions = transactions.finish();
        let user_signatures = parse_signature_lists(r, a)?;
        r.leave();
        Ok(FullCheckpointContents {
            transactions,
            user_signatures,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CheckpointTransaction<'a> {
    pub transaction: SenderSignedData<'a>,
    pub effects: TransactionEffects<'a>,
    pub events: Option<TransactionEvents<'a>>,
    pub input_objects: &'a [Object<'a>],
    pub output_objects: &'a [Object<'a>],
}

impl<'a> CheckpointTransaction<'a> {
    /// A transaction, its effects, an option tag and two lengths.
    pub const MIN_WIRE_SIZE: usize =
        SenderSignedData::MIN_WIRE_SIZE + TransactionEffects::MIN_WIRE_SIZE + 3;

    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<CheckpointTransaction<'a>> {
        r.enter()?;
        let transaction = SenderSignedData::parse_envelope(r, a)?;
        let effects = TransactionEffects::parse(r, a)?;
        let events = if r.option()? {
            Some(TransactionEvents::parse(r, a)?)
        } else {
            None
        };
        let input_objects = Object::parse_vec(r, a)?;
        let output_objects = Object::parse_vec(r, a)?;
        r.leave();
        Ok(CheckpointTransaction {
            transaction,
            effects,
            events,
            input_objects,
            output_objects,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CheckpointData<'a> {
    pub checkpoint_summary: CertifiedCheckpointSummary<'a>,
    pub checkpoint_contents: CheckpointContents<'a>,
    pub transactions: &'a [CheckpointTransaction<'a>],
}

impl<'a> CheckpointData<'a> {
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<CheckpointData<'a>> {
        r.enter()?;
        let checkpoint_summary = CertifiedCheckpointSummary::parse(r, a)?;
        let checkpoint_contents = CheckpointContents::parse(r, a)?;
        let n = r.seq_len(CheckpointTransaction::MIN_WIRE_SIZE)?;
        let mut transactions = a.slice(n)?;
        for _ in 0..n {
            transactions.push(CheckpointTransaction::parse(r, a)?);
        }
        r.leave();
        Ok(CheckpointData {
            checkpoint_summary,
            checkpoint_contents,
            transactions: transactions.finish(),
        })
    }
}

crate::impl_wire!(CheckpointSummary, guess = 0);
crate::impl_wire!(CertifiedCheckpointSummary, guess = 0);
// Mainnet p99 of arena over wire size: 0.34 and 0.76. Full contents are
// not served on mainnet; the guess is a blend of transactions and effects.
crate::impl_wire!(CheckpointContents, guess = 6);
crate::impl_wire!(FullCheckpointContents, guess = 24);
crate::impl_wire!(CheckpointData, guess = 13);

crate::base::assert_wire_layout!(
    CheckpointCommitment = 34,
    CommitteeMember = 105,
    ExecutionDigests = 66,
);
