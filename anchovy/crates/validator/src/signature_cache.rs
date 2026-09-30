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
use messages::Message;
use messages::base::Digest;
use messages::transaction::{DigestReady, SenderSignedData, Transaction};

use crate::epoch::EpochState;

/// Entries per generation. An entry survives at least this many later
/// insertions: the reference keeps 100,000.
pub const GENERATION: usize = 100_000;

/// Versioned, and apart from every other use of the hash.
const DOMAIN: &[u8] = b"anchovy verified signatures v1\0";

pub(crate) struct SignatureCache {
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
    pub(crate) fn new(epoch: Arc<EpochState>, generation: usize) -> SignatureCache {
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

    /// `verify`'s verdict on `transaction`'s signatures in `epoch`, which is
    /// `Ok` without calling it if they verified before in that epoch.
    ///
    /// The key is computed from the message, whose view cannot be changed
    /// and whose digest was computed from its own bytes, and `verify` is
    /// handed that same view: what is cached is what was verified. `verify`
    /// must depend on nothing but the view and `epoch`. In particular it
    /// takes no aliases: they would have to be in the key.
    pub(crate) fn verify(
        &mut self,
        epoch: &Arc<EpochState>,
        transaction: &Message<Transaction<'static, DigestReady>>,
        verify: impl FnOnce(&SenderSignedData<'_, DigestReady>) -> Result<(), validation::Error>,
    ) -> Result<(), validation::Error> {
        if !Arc::ptr_eq(&self.epoch, epoch) {
            self.epoch = epoch.clone();
            self.current.clear();
            self.previous.clear();
        }
        let signed = &transaction.get().0;
        let key = key(self.epoch.epoch, signed);
        if self.current.contains(&key) || self.previous.contains(&key) {
            self.hits += 1;
            return Ok(());
        }
        self.misses += 1;
        verify(signed)?;
        self.insert(key);
        Ok(())
    }

    /// Lookups answered from the cache, and lookups not.
    pub(crate) fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }

    /// Only a verification inserts, never a hit: resubmitting what is cached
    /// cannot push anything else out.
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
/// commits to the data's bytes, from which the rest of the data view was
/// parsed. Signatures are length-prefixed, so no two lists of them hash
/// alike.
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

    /// The checkpoint's transactions, as the verifier gets them.
    fn transactions() -> Vec<Message<Transaction<'static, DigestReady>>> {
        checkpoint()
            .get()
            .transactions
            .iter()
            .map(|t| {
                Message::parse(t.transaction.bytes.to_vec())
                    .map_err(|(e, _)| e)
                    .unwrap()
            })
            .collect()
    }

    /// Verifies through `cache`; whether the verification ran.
    fn verified(
        cache: &mut SignatureCache,
        epoch: &Arc<EpochState>,
        transaction: &Message<Transaction<'static, DigestReady>>,
    ) -> bool {
        let ran = Cell::new(false);
        cache
            .verify(epoch, transaction, |_| {
                ran.set(true);
                Ok(())
            })
            .unwrap();
        ran.get()
    }

    #[test]
    fn another_epoch_empties_the_cache() {
        let transactions = transactions();
        let transaction = &transactions[0];
        let first = epoch(5);
        let mut cache = SignatureCache::new(first.clone(), 8);
        assert!(verified(&mut cache, &first, transaction));
        assert!(!verified(&mut cache, &first, transaction));
        // Another epoch, even with the same number: verified again.
        let second = epoch(5);
        assert!(verified(&mut cache, &second, transaction));
        // And the first epoch's entries are gone.
        assert!(verified(&mut cache, &first, transaction));
    }

    #[test]
    fn an_entry_outlives_a_generation_of_insertions_and_hits_evict_nothing() {
        let t = transactions();
        assert!(t.len() >= 5);
        let epoch = epoch(5);
        let mut cache = SignatureCache::new(epoch.clone(), 2);
        // 0 and 1, then 2 rotates them into the previous generation.
        for transaction in &t[..3] {
            assert!(verified(&mut cache, &epoch, transaction));
        }
        // Hits, however many, insert nothing.
        for _ in 0..10 {
            assert!(!verified(&mut cache, &epoch, &t[0]));
            assert!(!verified(&mut cache, &epoch, &t[1]));
        }
        // 0 has seen two insertions since (2 and 3); the next rotation, at 4,
        // ends it.
        assert!(verified(&mut cache, &epoch, &t[3]));
        assert!(!verified(&mut cache, &epoch, &t[0]));
        assert!(verified(&mut cache, &epoch, &t[4]));
        assert!(
            verified(&mut cache, &epoch, &t[0]),
            "evicted, so verified again"
        );
        assert!(cache.current.len() + cache.previous.len() <= 4);
    }

    /// Inserting many times the capacity, rotating each generation over and
    /// over, never grows either table.
    #[test]
    fn the_tables_never_grow() {
        let t = transactions();
        let epoch = epoch(5);
        let mut cache = SignatureCache::new(epoch.clone(), 4);
        let capacities = (cache.current.capacity(), cache.previous.capacity());
        for round in 0..20_u64 {
            // Another epoch number for each round: new keys, same transactions.
            let epoch = Arc::new(EpochState::new(
                Chain::Unknown,
                ProtocolVersion::MAX.as_u64(),
                round,
                Digest::new([0x11; 32]),
                1000,
                4,
                [],
            ));
            for transaction in &t {
                verified(&mut cache, &epoch, transaction);
            }
        }
        let after = (cache.current.capacity(), cache.previous.capacity());
        // The tables may have swapped places.
        assert!(after == capacities || after == (capacities.1, capacities.0));
        assert!(cache.current.len() <= 4 && cache.previous.len() <= 4);
    }

    #[test]
    fn a_failure_is_not_cached() {
        let transactions = transactions();
        let epoch = epoch(5);
        let mut cache = SignatureCache::new(epoch.clone(), 8);
        for _ in 0..2 {
            let refused = cache.verify(&epoch, &transactions[0], |_| {
                Err(validation::Error::new(
                    validation::ErrorKind::InvalidSignature,
                    "refused",
                ))
            });
            assert!(refused.is_err());
        }
        assert_eq!(cache.stats(), (0, 2));
    }

    #[test]
    fn the_key_is_every_signature_byte_in_order() {
        let transactions = transactions();
        let original = transactions[0].get().0;
        let [a, b] = [&b"abc"[..], b"de"].map(GenericSignature);
        let key_with = |epoch: u64, signatures: &[GenericSignature<'_>]| {
            key(
                epoch,
                &SenderSignedData {
                    tx_signatures: signatures,
                    ..original
                },
            )
        };
        let base = key_with(5, &[a, b]);
        assert_ne!(base, key_with(5, &[b, a]), "order");
        assert_ne!(base, key_with(5, &[a]), "count");
        // The same bytes split differently.
        let [c, d] = [&b"ab"[..], b"cde"].map(GenericSignature);
        assert_ne!(base, key_with(5, &[c, d]), "boundaries");
        assert_ne!(base, key_with(6, &[a, b]), "epoch");
        assert_eq!(base, key_with(5, &[a, b]));
    }
}
