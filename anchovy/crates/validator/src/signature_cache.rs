// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Transactions whose signatures verified in the current epoch, so that a
//! resubmission is not verified again: the reference's
//! `SignatureVerifier::signed_data_cache`. See
//! `IMPLEMENTATION_PLAN_PHASE6.md`, "Verified-signature cache".

use std::sync::Arc;

use blake2::Blake2b;
use blake2::digest::consts::U32;
use containers::DigestHasher;
use hashbrown::HashSet;
use messages::base::Digest;
use messages::transaction::{DigestReady, SenderSignedData};

use crate::epoch::EpochState;

/// Entries per generation: two generations hold the reference's 100,000.
pub const GENERATION: usize = 50_000;

/// Versioned, and apart from every other use of the hash.
const DOMAIN: &[u8] = b"anchovy verified signatures v1\0";

pub struct SignatureCache {
    /// What the entries were verified under. Verification reads the epoch
    /// number, protocol config and JWKs from it.
    epoch: Arc<EpochState>,
    current: HashSet<Digest, DigestHasher>,
    previous: HashSet<Digest, DigestHasher>,
    generation: usize,
    hits: u64,
    misses: u64,
}

impl SignatureCache {
    /// Holds up to `2 * generation` entries, allocated here and never grown.
    pub fn new(epoch: Arc<EpochState>, generation: usize) -> SignatureCache {
        let table = || HashSet::with_capacity_and_hasher(generation, DigestHasher::new());
        SignatureCache {
            epoch,
            current: table(),
            previous: table(),
            generation,
            hits: 0,
            misses: 0,
        }
    }

    /// `verify`'s verdict on `transaction`'s signatures in `epoch`, which
    /// is `Ok` without calling it if they verified before. `verify` must
    /// verify exactly `transaction`, with no aliases: aliases would change
    /// the verdict, and the key does not include them.
    pub fn verify(
        &mut self,
        epoch: &Arc<EpochState>,
        transaction: &SenderSignedData<'_, DigestReady>,
        verify: impl FnOnce() -> Result<(), validation::Error>,
    ) -> Result<(), validation::Error> {
        if !Arc::ptr_eq(&self.epoch, epoch) {
            self.epoch = epoch.clone();
            self.current.clear();
            self.previous.clear();
        }
        let key = key(self.epoch.epoch, transaction);
        if self.contains(key) {
            self.hits += 1;
            return Ok(());
        }
        self.misses += 1;
        verify()?;
        self.insert(key);
        Ok(())
    }

    /// Lookups answered from the cache, and lookups not.
    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }

    fn contains(&mut self, key: Digest) -> bool {
        if self.current.contains(&key) {
            return true;
        }
        // Still in use: keep it past the next rotation.
        if self.previous.remove(&key) {
            self.insert(key);
            return true;
        }
        false
    }

    fn insert(&mut self, key: Digest) {
        if self.current.len() >= self.generation {
            std::mem::swap(&mut self.current, &mut self.previous);
            // Keeps its table: no allocation after construction.
            self.current.clear();
        }
        self.current.insert(key);
    }
}

/// Every byte verification reads, and the epoch. The transaction digest
/// commits to the data's bytes: a `DigestReady` transaction's digest was
/// computed from its own bytes. Signatures are length-prefixed, so no two
/// lists of them hash alike.
fn key(epoch: u64, transaction: &SenderSignedData<'_, DigestReady>) -> Digest {
    use blake2::Digest as _;
    let intent = transaction.intent;
    let mut hasher = Blake2b::<U32>::new();
    hasher.update(DOMAIN);
    hasher.update(epoch.to_le_bytes());
    hasher.update(transaction.digest().bytes);
    hasher.update([intent.scope, intent.version, intent.app_id]);
    hasher.update((transaction.tx_signatures.len() as u64).to_le_bytes());
    for signature in transaction.tx_signatures {
        hasher.update((signature.0.len() as u64).to_le_bytes());
        hasher.update(signature.0);
    }
    Digest::new(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use messages::Message;
    use messages::checkpoint::CheckpointData;
    use messages::transaction::GenericSignature;
    use protocol_config::{Chain, ProtocolVersion};

    use super::*;

    fn epoch(number: u64) -> Arc<EpochState> {
        Arc::new(EpochState::new(
            Chain::Unknown,
            ProtocolVersion::MAX.as_u64(),
            number,
            Digest::new([0x11; 32]),
            1000,
            4,
            [],
        ))
    }

    fn checkpoint() -> Message<CheckpointData<'static>> {
        let mut bytes = include_bytes!("../../messages/tests/data/mainnet-325300367.chk").to_vec();
        bytes.remove(0);
        Message::parse(bytes).map_err(|(e, _)| e).unwrap()
    }

    /// Verifies through `cache`, counting calls of the verification.
    fn verify(
        cache: &mut SignatureCache,
        epoch: &Arc<EpochState>,
        transaction: &SenderSignedData<'_, DigestReady>,
        calls: &Cell<u32>,
    ) {
        cache
            .verify(epoch, transaction, || {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn another_epoch_empties_the_cache() {
        let checkpoint = checkpoint();
        let transaction = &checkpoint.get().transactions[0].transaction;
        let (first, calls) = (epoch(5), Cell::new(0));
        let mut cache = SignatureCache::new(first.clone(), 8);
        verify(&mut cache, &first, transaction, &calls);
        verify(&mut cache, &first, transaction, &calls);
        assert_eq!(calls.get(), 1);
        // Another epoch, even with the same number: verified again.
        let second = epoch(5);
        verify(&mut cache, &second, transaction, &calls);
        assert_eq!(calls.get(), 2);
        // And the first epoch's entries are gone.
        verify(&mut cache, &first, transaction, &calls);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn two_generations_bound_the_cache_and_keep_what_is_used() {
        let checkpoint = checkpoint();
        let transactions: Vec<_> = checkpoint
            .get()
            .transactions
            .iter()
            .map(|t| &t.transaction)
            .collect();
        assert!(transactions.len() >= 6);
        let (epoch, calls) = (epoch(5), Cell::new(0));
        let mut cache = SignatureCache::new(epoch.clone(), 2);
        for t in &transactions[..4] {
            verify(&mut cache, &epoch, t, &calls);
        }
        assert_eq!(calls.get(), 4);
        // Two generations of two: {0, 1} previous, {2, 3} current. A hit on
        // 0 moves it forward, which rotates: {2, 3} previous, {0} current,
        // and 1 is gone.
        verify(&mut cache, &epoch, transactions[0], &calls);
        assert_eq!(calls.get(), 4);
        verify(&mut cache, &epoch, transactions[4], &calls);
        assert_eq!(calls.get(), 5);
        assert!(cache.current.len() + cache.previous.len() <= 4);
        verify(&mut cache, &epoch, transactions[0], &calls);
        assert_eq!(calls.get(), 5, "used, so kept");
        verify(&mut cache, &epoch, transactions[1], &calls);
        assert_eq!(calls.get(), 6, "evicted, so verified again");
    }

    #[test]
    fn the_key_is_every_signature_byte_in_order() {
        let checkpoint = checkpoint();
        let original = checkpoint.get().transactions[0].transaction;
        let [a, b] = [&b"abc"[..], b"de"].map(GenericSignature);
        let key_with = |signatures: &[GenericSignature<'_>]| {
            key(
                5,
                &SenderSignedData {
                    tx_signatures: signatures,
                    ..original
                },
            )
        };
        let base = key_with(&[a, b]);
        assert_ne!(base, key_with(&[b, a]), "order");
        assert_ne!(base, key_with(&[a]), "count");
        // The same bytes split differently.
        let [c, d] = [&b"ab"[..], b"cde"].map(GenericSignature);
        assert_ne!(base, key_with(&[c, d]), "boundaries");
        assert_ne!(
            base,
            key(
                6,
                &SenderSignedData {
                    tx_signatures: &[a, b],
                    ..original
                }
            ),
            "epoch"
        );
        assert_eq!(base, key_with(&[a, b]));
    }
}
