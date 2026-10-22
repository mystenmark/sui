// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Transactions whose signatures verified in the epoch, so that a
//! resubmission is not verified again: the reference's
//! `SignatureVerifier::signed_data_cache`. See
//! `IMPLEMENTATION_PLAN_PHASE6.md`, "Verified-signature cache".

use blake2::Blake2b;
use blake2::digest::consts::U32;
use containers::HeapMessageMap;
use messages::Message;
use messages::base::{Digest, Hashed};
use messages::transaction::{GenericSignature, Intent, SenderSignedData, Transaction};

use super::Valid;
use validation::verify::SignerIndices;

/// Entries per generation. An entry survives at least this many later
/// insertions: the reference keeps 100,000.
pub const GENERATION: usize = 100_000;

/// Versioned, and apart from every other use of the hash.
const DOMAIN: &[u8] = b"anchovy verified signatures v1\0";

/// One epoch's: verification reads the epoch number, protocol config and
/// JWKs, so a cache lives no longer than its epoch.
pub(crate) struct SignatureCache {
    /// The epoch's number, part of each key.
    epoch: u64,
    /// Each verified transaction's signer indices, by key.
    current: HeapMessageMap<Hashed, SignerIndices>,
    previous: HeapMessageMap<Hashed, SignerIndices>,
    generation: usize,
    hits: u64,
    misses: u64,
}

impl SignatureCache {
    /// Holds up to `2 * generation` entries, allocated here and never grown.
    pub(crate) fn new(epoch: u64, generation: usize) -> SignatureCache {
        let table = || containers::heap_message_map(generation);
        SignatureCache {
            epoch,
            current: table(),
            previous: table(),
            generation,
            hits: 0,
            misses: 0,
        }
    }

    /// `verify`'s verdict on `transaction`'s signatures, which is `Ok`
    /// without calling it if they verified before.
    ///
    /// The key is computed from the message, whose view cannot be changed
    /// and whose digest was computed from its own bytes, and `verify` is
    /// handed that same view: what is cached is what was verified. Only a
    /// `Valid` transaction gets here: verification does not repeat
    /// validation. `verify` must depend on nothing but the view and the
    /// epoch. In particular it takes no aliases: they would have to be in
    /// the key.
    pub(crate) fn verify(
        &mut self,
        transaction: &Message<Transaction<'static, Valid>>,
        verify: impl FnOnce(&SenderSignedData<'_, Valid>) -> Result<SignerIndices, validation::Error>,
    ) -> Result<SignerIndices, validation::Error> {
        let (key, cached) = self.lookup(transaction);
        if let Some(indices) = cached {
            return Ok(indices);
        }
        let indices = verify(&transaction.get().0)?;
        self.insert(key, indices);
        Ok(indices)
    }

    /// The transaction's key, and its signer indices if it verified before.
    /// For verifying outside `verify`, as a batch does: what verifies then
    /// goes in with `insert`.
    pub(crate) fn lookup(
        &mut self,
        transaction: &Message<Transaction<'static, Valid>>,
    ) -> (Hashed, Option<SignerIndices>) {
        let key = key(self.epoch, &transaction.get().0);
        let cached = self
            .current
            .get(&key)
            .or_else(|| self.previous.get(&key))
            .copied();
        if cached.is_some() {
            self.hits += 1;
        } else {
            self.misses += 1;
        }
        (key, cached)
    }

    /// Lookups answered from the cache, and lookups not.
    pub(crate) fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }

    /// Only a verification inserts, never a hit: resubmitting what is cached
    /// cannot push anything else out.
    pub(crate) fn insert(&mut self, key: Hashed, indices: SignerIndices) {
        if self.current.len() >= self.generation {
            std::mem::swap(&mut self.current, &mut self.previous);
            // Keeps its table: no allocation after construction.
            self.current.clear();
        }
        self.current.insert(key, indices);
    }
}

/// Every byte verification reads, and the epoch. The transaction digest
/// commits to the data's bytes, from which the rest of the data view was
/// parsed. Signatures are length-prefixed, so no two lists of them hash
/// alike.
fn key(epoch: u64, transaction: &SenderSignedData<'_, Valid>) -> Hashed {
    key_of(
        epoch,
        transaction.digest(),
        *transaction.intent(),
        transaction.tx_signatures(),
    )
}

fn key_of(
    epoch: u64,
    digest: &Digest,
    intent: Intent,
    signatures: &[GenericSignature<'_>],
) -> Hashed {
    use blake2::Digest as _;
    let mut hasher = Blake2b::<U32>::new();
    hasher.update(DOMAIN);
    hasher.update(epoch.to_le_bytes());
    hasher.update(digest.bytes);
    hasher.update([intent.scope, intent.version, intent.app_id]);
    hasher.update((signatures.len() as u64).to_le_bytes());
    for signature in signatures {
        hasher.update((signature.0.len() as u64).to_le_bytes());
        hasher.update(signature.0);
    }
    // Hashed just now.
    Hashed::computed(Digest::new(hasher.finalize().into()))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use std::marker::PhantomData;

    use messages::checkpoint::CheckpointData;
    use messages::transaction::DigestReady;

    use super::*;

    fn checkpoint() -> Message<CheckpointData<'static>> {
        let mut bytes =
            include_bytes!("../../../messages/tests/data/mainnet-325300367.chk").to_vec();
        bytes.remove(0);
        Message::parse(bytes).map_err(|(e, _)| e).unwrap()
    }

    /// The checkpoint's transactions, as the verifier gets them. (Not
    /// validated: the cache does not look.)
    fn transactions() -> Vec<Message<Transaction<'static, Valid>>> {
        let parsed: Vec<Message<Transaction<'static, DigestReady>>> = checkpoint()
            .get()
            .transactions
            .iter()
            .map(|t| {
                Message::parse(t.transaction.bytes().to_vec())
                    .map_err(|(e, _)| e)
                    .unwrap()
            })
            .collect();
        Message::relabel_all(parsed, &super::super::Witness(PhantomData))
    }

    /// Verifies through `cache`; whether the verification ran.
    fn verified(
        cache: &mut SignatureCache,
        transaction: &Message<Transaction<'static, Valid>>,
    ) -> bool {
        let ran = Cell::new(false);
        cache
            .verify(transaction, |_| {
                ran.set(true);
                Ok(SignerIndices::default())
            })
            .unwrap();
        ran.get()
    }

    #[test]
    fn an_entry_outlives_a_generation_of_insertions_and_hits_evict_nothing() {
        let t = transactions();
        assert!(t.len() >= 5);
        let mut cache = SignatureCache::new(5, 2);
        // 0 and 1, then 2 rotates them into the previous generation.
        for transaction in &t[..3] {
            assert!(verified(&mut cache, transaction));
        }
        // Hits, however many, insert nothing.
        for _ in 0..10 {
            assert!(!verified(&mut cache, &t[0]));
            assert!(!verified(&mut cache, &t[1]));
        }
        // 0 has seen two insertions since (2 and 3); the next rotation, at 4,
        // ends it.
        assert!(verified(&mut cache, &t[3]));
        assert!(!verified(&mut cache, &t[0]));
        assert!(verified(&mut cache, &t[4]));
        assert!(verified(&mut cache, &t[0]), "evicted, so verified again");
        assert!(cache.current.len() + cache.previous.len() <= 4);
    }

    /// Inserting many times the capacity, rotating each generation over and
    /// over, never grows either table.
    #[test]
    fn the_tables_never_grow() {
        let mut cache = SignatureCache::new(5, 4);
        let capacities = (cache.current.capacity(), cache.previous.capacity());
        for n in 0..100_u64 {
            let key = Hashed::of("key", &n.to_le_bytes());
            cache.insert(key, SignerIndices::default());
        }
        let after = (cache.current.capacity(), cache.previous.capacity());
        // The tables may have swapped places.
        assert!(after == capacities || after == (capacities.1, capacities.0));
        assert!(cache.current.len() <= 4 && cache.previous.len() <= 4);
    }

    #[test]
    fn a_failure_is_not_cached() {
        let transactions = transactions();
        let mut cache = SignatureCache::new(5, 8);
        for _ in 0..2 {
            let refused = cache.verify(&transactions[0], |_| {
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
        let original = &transactions[0].get().0;
        let [a, b] = [&b"abc"[..], b"de"].map(GenericSignature);
        let key_with = |epoch: u64, signatures: &[GenericSignature<'_>]| {
            key_of(epoch, original.digest(), *original.intent(), signatures)
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
