// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Transactions the consensus handler holds back to a later commit, as the
//! reference's: keyed by `DeferralKey`, kept with the consensus output, and
//! merged into the commit of the round they wait for.

use ::consensus::{BlockRef, TransactionIndex};
use messages::Message;

use crate::checks::{self, VerifiedTransaction};

/// The reference's `DeferralKey`, ordered as it orders them: randomness
/// first, then by the round waited for and the round deferred from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeferralKey {
    /// Until randomness is available.
    Randomness { deferred_from_round: u64 },
    /// Until the commit of `future_round` or later.
    ConsensusRound {
        future_round: u64,
        deferred_from_round: u64,
    },
}

/// A key's length in the epoch store.
pub const KEY_LEN: usize = 17;

impl DeferralKey {
    /// To the next round's commit, from `deferred_from_round`: the round of
    /// the commit that first deferred the transaction.
    pub fn next_round(round: u64, deferred_from_round: u64) -> DeferralKey {
        DeferralKey::ConsensusRound {
            future_round: round + 1,
            deferred_from_round,
        }
    }

    pub fn deferred_from_round(&self) -> u64 {
        match *self {
            DeferralKey::Randomness {
                deferred_from_round,
            }
            | DeferralKey::ConsensusRound {
                deferred_from_round,
                ..
            } => deferred_from_round,
        }
    }

    /// The reference's `transaction_deferral_within_limit`.
    pub fn within_limit(&self, max_rounds: u64) -> bool {
        match *self {
            DeferralKey::ConsensusRound {
                future_round,
                deferred_from_round,
            } => future_round - deferred_from_round <= max_rounds,
            DeferralKey::Randomness { .. } => true,
        }
    }

    /// The keys of transactions due at the commit of `round`: the
    /// reference's `range_for_up_to_consensus_round`.
    pub fn due_by(round: u64) -> std::ops::Range<DeferralKey> {
        let key = |future_round| DeferralKey::ConsensusRound {
            future_round,
            deferred_from_round: 0,
        };
        key(0)..key(round + 1)
    }

    /// Every randomness key: the reference's `full_range_for_randomness`.
    pub fn randomness() -> std::ops::Range<DeferralKey> {
        DeferralKey::Randomness {
            deferred_from_round: 0,
        }..DeferralKey::ConsensusRound {
            future_round: 0,
            deferred_from_round: 0,
        }
    }

    /// Bytes ordered as the keys are.
    pub fn encode(&self) -> [u8; KEY_LEN] {
        let (variant, first, second) = match *self {
            DeferralKey::Randomness {
                deferred_from_round,
            } => (0, deferred_from_round, 0),
            DeferralKey::ConsensusRound {
                future_round,
                deferred_from_round,
            } => (1, future_round, deferred_from_round),
        };
        let mut key = [0; KEY_LEN];
        key[0] = variant;
        key[1..9].copy_from_slice(&first.to_be_bytes());
        key[9..].copy_from_slice(&second.to_be_bytes());
        key
    }

    /// # Panics
    /// Unless `key` is as `encode` made it.
    pub fn decode(key: &[u8; KEY_LEN]) -> DeferralKey {
        let word = |i: usize| u64::from_be_bytes(key[i..i + 8].try_into().expect("8 bytes"));
        match key[0] {
            0 => DeferralKey::Randomness {
                deferred_from_round: word(1),
            },
            1 => DeferralKey::ConsensusRound {
                future_round: word(1),
                deferred_from_round: word(9),
            },
            variant => panic!("deferral key variant {variant}"),
        }
    }
}

/// A deferred transaction, and where consensus committed it.
#[derive(Debug)]
pub struct Deferred {
    pub origin: (BlockRef, TransactionIndex),
    pub transaction: VerifiedTransaction,
}

impl Deferred {
    pub fn digest(&self) -> [u8; 32] {
        self.transaction.get().0.digest().bytes
    }
}

/// `deferred` as the epoch store keeps them: each one's origin, then its
/// transaction's length and bytes.
pub fn encode_deferred(deferred: &[Deferred]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for Deferred {
        origin: (block, index),
        transaction,
    } in deferred
    {
        bytes.extend_from_slice(&block.round.to_le_bytes());
        bytes.extend_from_slice(&block.author.to_le_bytes());
        bytes.extend_from_slice(&block.digest);
        bytes.extend_from_slice(&index.to_le_bytes());
        let wire = transaction.get().0.bytes();
        bytes.extend_from_slice(
            &u32::try_from(wire.len())
                .expect("a small transaction")
                .to_le_bytes(),
        );
        bytes.extend_from_slice(wire);
    }
    bytes
}

/// # Panics
/// Unless `bytes` are as `encode_deferred` made them.
pub fn decode_deferred(mut bytes: &[u8]) -> impl Iterator<Item = Deferred> + '_ {
    fn take<'a>(bytes: &mut &'a [u8], n: usize) -> &'a [u8] {
        let (head, tail) = bytes.split_at(n);
        *bytes = tail;
        head
    }
    std::iter::from_fn(move || {
        if bytes.is_empty() {
            return None;
        }
        let round = u32::from_le_bytes(take(&mut bytes, 4).try_into().expect("4 bytes"));
        let author = u32::from_le_bytes(take(&mut bytes, 4).try_into().expect("4 bytes"));
        let digest = take(&mut bytes, 32).try_into().expect("32 bytes");
        let index =
            TransactionIndex::from_le_bytes(take(&mut bytes, 2).try_into().expect("2 bytes"));
        let len = u32::from_le_bytes(take(&mut bytes, 4).try_into().expect("4 bytes")) as usize;
        let transaction = Message::parse(take(&mut bytes, len).to_vec())
            .map_err(|(e, _)| e)
            .expect("a deferred transaction parses");
        Some(Deferred {
            origin: (
                BlockRef {
                    round,
                    author,
                    digest,
                },
                index,
            ),
            transaction: checks::sequenced_by_consensus(transaction),
        })
    })
}
