// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! What sui's decoding of a `ConsensusTransaction` checks beyond the wire
//! format, which parsing leaves to validation. A block holding a transaction
//! that fails these is invalid, as one sui cannot decode.

use containers::Bump;
use fastcrypto::bls12381::min_sig::{BLS12381AggregateSignature, BLS12381Signature};
use fastcrypto::traits::ToFromBytes;
use messages::checkpoint::AuthorityQuorumSignInfo;
use messages::consensus::{ConsensusTransaction, ConsensusTransactionKind, TransactionClaim};

use crate::sender_signed::deserialization_checks;
use crate::{Error, ErrorKind};

/// `MAX_VALIDATOR_COUNT`, which bounds a quorum signature's signers.
const MAX_VALIDATOR_COUNT: u64 = 150;

fn malformed(what: &str) -> Error {
    Error::new(ErrorKind::TransactionDeserializationError, what)
}

/// Passes if sui decodes the transaction: its embedded transactions pass
/// their deserialization checks, its BLS signatures are curve points, its
/// signer bitmaps decode, its non-empty lists are not empty and its
/// durations do not overflow.
pub fn decode_checks(transaction: &ConsensusTransaction<'_>, bump: &Bump) -> Result<(), Error> {
    match transaction.kind() {
        ConsensusTransactionKind::CertifiedTransaction(certified) => {
            deserialization_checks(&certified.data, bump)?;
            quorum_signature(&certified.auth_signature)
        }
        ConsensusTransactionKind::CheckpointSignature(message)
        | ConsensusTransactionKind::CheckpointSignatureV2(message) => {
            BLS12381Signature::from_bytes(message.summary.auth_signature.signature)
                .map(|_| ())
                .map_err(|_| malformed("checkpoint signature is not a curve point"))
        }
        ConsensusTransactionKind::UserTransaction(transaction) => {
            deserialization_checks(&transaction.0, bump).map(|_| ())
        }
        ConsensusTransactionKind::UserTransactionV2(user) => {
            deserialization_checks(&user.transaction().0, bump)?;
            let empty_aliases = user.claims().iter().any(|claim| match claim {
                TransactionClaim::AddressAliases(a) => a.is_empty(),
                TransactionClaim::AddressAliasesV2(a) => a.is_empty(),
                TransactionClaim::ImmutableInputObjects(_) => false,
            });
            if empty_aliases {
                return Err(malformed("empty address alias claim"));
            }
            Ok(())
        }
        ConsensusTransactionKind::ExecutionTimeObservation(observation) => {
            // serde's `Duration` carries whole seconds out of `nanos`.
            let overflows = observation.estimates.iter().any(|(_, d)| {
                d.secs
                    .checked_add(u64::from(d.nanos / 1_000_000_000))
                    .is_none()
            });
            if overflows {
                return Err(malformed("duration overflows"));
            }
            Ok(())
        }
        ConsensusTransactionKind::EndOfPublish(_)
        | ConsensusTransactionKind::CapabilityNotification(_)
        | ConsensusTransactionKind::NewJwkFetched(_)
        | ConsensusTransactionKind::RandomnessStateUpdate { .. }
        | ConsensusTransactionKind::RandomnessDkgMessage(_)
        | ConsensusTransactionKind::RandomnessDkgConfirmation(_)
        | ConsensusTransactionKind::CapabilityNotificationV2(_)
        | ConsensusTransactionKind::UpdateTransactionDenyConfig(_) => Ok(()),
    }
}

/// `AuthorityStrongQuorumSignInfo`'s decoding: an aggregate signature that is
/// a curve point, and a roaring bitmap of at most `MAX_VALIDATOR_COUNT`
/// signers (`deserialize_sui_bitmap`).
fn quorum_signature(info: &AuthorityQuorumSignInfo<'_>) -> Result<(), Error> {
    BLS12381AggregateSignature::from_bytes(info.signature)
        .map_err(|_| malformed("quorum signature is not a curve point"))?;
    let signers = roaring::RoaringBitmap::deserialize_from(info.signers_map)
        .map_err(|_| malformed("signer bitmap does not decode"))?;
    if signers.len() > MAX_VALIDATOR_COUNT {
        return Err(malformed("too many signers"));
    }
    Ok(())
}
