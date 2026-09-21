// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Sui wire types, parsed without copying.
//!
//! [`Message::parse`] takes the buffer a message arrived in and returns a
//! view whose references point into that buffer or into one arena. Parsing
//! checks that the bytes are canonical BCS for the type and nothing more:
//! signatures, identifiers and other semantic rules belong to validation,
//! which messages loaded from trusted storage skip.

pub mod arena;
pub mod base;
pub mod build;
pub mod checkpoint;
pub mod effects;
pub mod error;
pub mod execution_status;
pub mod message;
pub mod object;
pub mod reader;
pub mod signature;
pub mod system_transaction;
pub mod transaction;
pub mod tx_index;
pub mod type_tag;

pub use error::{ParseError, Result};
pub use message::{Message, Wire, WireBuf};

/// Implements [`Wire`] for a view type with an inherent `parse(r, a)`.
/// `guess = N` sets the single-pass arena guess to `N / 16` of the wire size.
macro_rules! impl_wire {
    ($ty:ident) => {
        $crate::impl_wire!($ty, guess = 16);
    };
    ($ty:ident, guess = $sixteenths:expr) => {
        // SAFETY: `shrink` is the identity, so `$ty` is covariant.
        unsafe impl $crate::message::Wire for $ty<'static> {
            type View<'a> = $ty<'a>;

            const ARENA_GUESS_SIXTEENTHS: usize = $sixteenths;

            fn parse<'a, A: $crate::arena::Alloc<'a>>(
                r: &mut $crate::reader::Reader<'a>,
                a: &mut A,
            ) -> $crate::error::Result<$ty<'a>> {
                $ty::parse(r, a)
            }

            fn shrink<'l, 's: 'l>(v: &'l $ty<'s>) -> &'l $ty<'l> {
                v
            }
        }
    };
}
pub(crate) use impl_wire;

#[cfg(test)]
mod message_identity {
    use std::collections::BTreeSet;

    use crate::Message;
    use crate::checkpoint::CheckpointData;
    use crate::transaction::SenderSignedData;

    #[test]
    fn messages_compare_and_hash_by_digest() {
        let mut bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/mainnet-325300367.chk"
        ))
        .unwrap();
        bytes.remove(0);
        let checkpoint = Message::<CheckpointData>::parse(bytes).unwrap();
        let parsed: Vec<Message<SenderSignedData>> = checkpoint
            .get()
            .transactions
            .iter()
            .map(|tx| Message::<SenderSignedData>::parse(tx.transaction.bytes.to_vec()).unwrap())
            .collect();
        let again = Message::<SenderSignedData>::parse(parsed[0].wire_bytes().to_vec()).unwrap();
        assert_eq!(parsed[0], again);
        assert_ne!(parsed[0], parsed[1]);
        let sorted: BTreeSet<&Message<SenderSignedData>> = parsed.iter().collect();
        assert_eq!(sorted.len(), parsed.len());
        let mut hashes: Vec<u64> = parsed
            .iter()
            .map(|m| {
                use std::hash::{Hash, Hasher};
                let mut h = std::hash::DefaultHasher::new();
                m.hash(&mut h);
                h.finish()
            })
            .collect();
        hashes.sort_unstable();
        hashes.dedup();
        assert_eq!(hashes.len(), parsed.len());
    }
}
