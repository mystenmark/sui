// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Readying a verified transaction for consensus: the checks voting makes,
//! and the `ConsensusTransaction` its submitter sends.

use std::sync::Arc;

use containers::Bump;
use messages::object::Owner;
use validation::inputs::{InputKind, input_objects};
use validation::sender_signed;
use workqueue::Processor;

use crate::checks::{self, Verified, VerifiedTransaction};
use crate::epoch::EpochState;
use crate::processors::{Outcome, Request, executed};

/// Checks a request's inputs as voting checks them, and answers each
/// transaction: executed already (from the store, not submitted again, as
/// the reference answers it), rejected with its input checks' error, or
/// `Outcome::Submit` with the `ConsensusTransaction` its caller, the RPC
/// handler, submits to consensus.
///
/// Inward:
/// - `SignatureVerifier`: `Request<Verified>`.
///
/// Outward: the request's reply.
///
/// Reads: the object store (input checks, immutable-input claims).
pub struct SubmitChecker {
    epoch: Arc<EpochState>,
    pub store: Arc<store::Store>,
    bump: Bump,
}

/// Enough for a transaction's parsed signatures.
const ARENA_BYTES: usize = 16 * 1024;

impl SubmitChecker {
    pub fn new(epoch: Arc<EpochState>, store: Arc<store::Store>) -> SubmitChecker {
        SubmitChecker {
            epoch,
            store,
            bump: Bump::with_capacity(ARENA_BYTES),
        }
    }

    /// The `ConsensusTransaction` BCS the reference's `submit_transaction`
    /// sends for `transaction`: a `UserTransactionV2` with its claims.
    fn consensus_transaction(
        &mut self,
        epoch: &EpochState,
        transaction: &VerifiedTransaction,
    ) -> Result<Vec<u8>, validation::Error> {
        let signed = &transaction.get().0;
        let data = signed.data();
        let mut claims = 0u8;
        let mut encoded = Vec::new();

        // The owned inputs that are immutable, in input order.
        let mut immutable = Vec::new();
        for input in input_objects(data)? {
            let InputKind::ImmOrOwned(r) = input else {
                continue;
            };
            if let Some(object) = self.store.live_object(&r.id).expect("the store reads")
                && object.get().owner == Owner::Immutable
            {
                immutable.push(r.id);
            }
        }
        if !immutable.is_empty() {
            claims += 1;
            encoded.push(1);
            uleb128(&mut encoded, immutable.len());
            for id in &immutable {
                encoded.extend_from_slice(&id.0);
            }
        }

        // No signer has an alias: aliases are object state anchovy does not
        // read yet, so each alias version is `None`.
        let config = &epoch.params.config;
        if config.address_aliases() {
            claims += 1;
            if config.fix_checkpoint_signature_mapping() {
                self.bump.reset();
                let (signatures, _) = sender_signed::deserialization_checks(signed, &self.bump)?;
                let indices = validation::verify::signer_signature_indices(
                    signed,
                    signatures,
                    &epoch.verifier,
                    &[],
                    &self.bump,
                )?;
                encoded.push(2);
                uleb128(&mut encoded, indices.as_slice().len());
                for index in indices.as_slice() {
                    encoded.extend_from_slice(&[*index, 0]);
                }
            } else {
                let sponsor =
                    (data.gas_data().owner != data.sender()).then_some(data.gas_data().owner);
                let signers: Vec<_> = [Some(data.sender()), sponsor]
                    .into_iter()
                    .flatten()
                    .collect();
                encoded.push(0);
                uleb128(&mut encoded, signers.len());
                for signer in signers {
                    encoded.extend_from_slice(&signer.0);
                    encoded.push(0);
                }
            }
        }

        // The reference's tracking id hashes the digest and its authority's
        // name; it only labels the message, so the digest's first bytes do.
        let bytes = signed.bytes();
        let mut out = Vec::with_capacity(8 + 1 + bytes.len() + 1 + encoded.len());
        out.extend_from_slice(&signed.digest().bytes[..8]);
        out.push(USER_TRANSACTION_V2);
        out.extend_from_slice(bytes);
        out.push(claims);
        out.extend_from_slice(&encoded);
        Ok(out)
    }
}

/// `ConsensusTransactionKind::UserTransactionV2`'s variant index.
const USER_TRANSACTION_V2: u8 = 12;

fn uleb128(out: &mut Vec<u8>, mut n: usize) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

impl Processor for SubmitChecker {
    type Input = Request<Verified>;
    type Output = ();

    fn process(&mut self, request: Request<Verified>) -> Option<()> {
        let Request {
            transactions,
            then: _,
            reply,
        } = request;
        let epoch = self.epoch.clone();
        let mut outcomes = Vec::with_capacity(transactions.len());
        for transaction in &transactions {
            let digest = transaction.get().0.digest().bytes;
            if let Some(outcome) = executed(&self.store, &digest) {
                outcomes.push(outcome);
                continue;
            }
            let submission = checks::inputs_pass(&epoch, &self.store, transaction)
                .and_then(|()| self.consensus_transaction(&epoch, transaction));
            outcomes.push(match submission {
                Ok(bytes) => Outcome::Submit(bytes),
                Err(e) => Outcome::Rejected(e),
            });
        }
        let _ = reply.send(Ok(outcomes));
        None
    }
}
