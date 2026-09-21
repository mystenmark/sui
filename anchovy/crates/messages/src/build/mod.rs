// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Owned mirrors of the wire types, for building messages.
//!
//! `bcs::to_bytes` of a value here is its wire encoding, and the serde shape
//! of every type reproduces sui's format snapshot. `bcs::from_bytes` accepts
//! what the view parsers accept: well-formed BCS and nothing semantic.
//!
//! Every view converts to its mirror with `From<&View>`, except the
//! signature views, which can hold a `Passkey` the mirrors have no variant
//! for and so convert with `TryFrom<&View>`.

pub mod base;
pub mod checkpoint;
pub mod effects;
pub mod execution_status;
pub mod object;
pub mod signature;
pub mod system_transaction;
pub mod transaction;
pub mod type_tag;

/// `[u8; N]` as a byte string that must be exactly `N` long.
pub(crate) mod fixed_bytes {
    use std::fmt;

    use serde::de::{Deserializer, Error, Visitor};
    use serde::ser::Serializer;

    pub fn serialize<S: Serializer, const N: usize>(
        bytes: &[u8; N],
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(bytes)
    }

    struct FixedBytes<const N: usize>;

    impl<const N: usize> Visitor<'_> for FixedBytes<N> {
        type Value = [u8; N];

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{N} bytes")
        }

        fn visit_bytes<E: Error>(self, v: &[u8]) -> Result<[u8; N], E> {
            v.try_into().map_err(|_| E::invalid_length(v.len(), &self))
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        d: D,
    ) -> Result<[u8; N], D::Error> {
        d.deserialize_bytes(FixedBytes::<N>)
    }
}

/// `[u8; N]` as a tuple, for the sizes above 32 that serde has no impl for.
pub(crate) mod byte_array {
    use std::fmt;

    use serde::de::{Deserializer, Error, SeqAccess, Visitor};
    use serde::ser::{SerializeTuple, Serializer};

    pub fn serialize<S: Serializer, const N: usize>(
        bytes: &[u8; N],
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let mut tuple = s.serialize_tuple(N)?;
        for b in bytes {
            tuple.serialize_element(b)?;
        }
        tuple.end()
    }

    struct ByteArray<const N: usize>;

    impl<'de, const N: usize> Visitor<'de> for ByteArray<N> {
        type Value = [u8; N];

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "a tuple of {N} bytes")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<[u8; N], A::Error> {
            let mut out = [0; N];
            for (i, b) in out.iter_mut().enumerate() {
                *b = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::invalid_length(i, &self))?;
            }
            Ok(out)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        d: D,
    ) -> Result<[u8; N], D::Error> {
        d.deserialize_tuple(N, ByteArray::<N>)
    }
}

/// A value as a sequence that must hold exactly one element.
pub(crate) mod one_element {
    use serde::de::{Deserialize, Deserializer, Error};
    use serde::ser::{Serialize, Serializer};

    pub fn serialize<T: Serialize, S: Serializer>(value: &T, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq([value])
    }

    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        d: D,
    ) -> Result<T, D::Error> {
        let mut elements = Vec::<T>::deserialize(d)?;
        match (elements.pop(), elements.is_empty()) {
            (Some(value), true) => Ok(value),
            _ => Err(D::Error::custom("expected exactly one element")),
        }
    }
}

/// A map whose values are byte strings.
pub(crate) mod bytes_map {
    use std::collections::BTreeMap;

    use serde::de::{Deserialize, Deserializer};
    use serde::ser::Serializer;
    use serde_bytes::{ByteBuf, Bytes};

    pub fn serialize<S: Serializer>(
        map: &BTreeMap<String, Vec<u8>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.collect_map(map.iter().map(|(k, v)| (k, Bytes::new(v))))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<String, Vec<u8>>, D::Error> {
        let map = BTreeMap::<String, ByteBuf>::deserialize(d)?;
        Ok(map.into_iter().map(|(k, v)| (k, v.into_vec())).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::base::{AuthorityPublicKeyBytes, Digest, SuiAddress};
    use super::object::MovePackage;
    use super::transaction::{
        GasData, Intent, IntentMessage, SenderSignedData, SenderSignedTransaction, TransactionData,
        TransactionDataV1, TransactionExpiration, TransactionKind,
    };
    use crate::Message;
    use crate::error::ParseError;
    use crate::transaction as view;

    fn length_prefixed(len: u8) -> Vec<u8> {
        let mut bytes = vec![len];
        bytes.resize(1 + usize::from(len), 0);
        bytes
    }

    #[test]
    fn digest_is_32_bytes() {
        assert_eq!(
            bcs::to_bytes(&Digest([0; 32])).unwrap(),
            length_prefixed(32)
        );
        assert!(bcs::from_bytes::<Digest>(&length_prefixed(32)).is_ok());
        assert!(bcs::from_bytes::<Digest>(&length_prefixed(31)).is_err());
        assert!(bcs::from_bytes::<Digest>(&length_prefixed(33)).is_err());
    }

    #[test]
    fn authority_public_key_is_96_bytes() {
        assert_eq!(
            bcs::to_bytes(&AuthorityPublicKeyBytes([0; 96])).unwrap(),
            length_prefixed(96)
        );
        assert!(bcs::from_bytes::<AuthorityPublicKeyBytes>(&length_prefixed(96)).is_ok());
        assert!(bcs::from_bytes::<AuthorityPublicKeyBytes>(&length_prefixed(95)).is_err());
        assert!(bcs::from_bytes::<AuthorityPublicKeyBytes>(&length_prefixed(97)).is_err());
    }

    #[test]
    fn sender_signed_data_is_one_transaction() {
        let one = SenderSignedData(SenderSignedTransaction {
            intent_message: IntentMessage {
                intent: Intent {
                    scope: 0,
                    version: 0,
                    app_id: 0,
                },
                value: TransactionData::V1(TransactionDataV1 {
                    kind: TransactionKind::EndOfEpochTransaction(Vec::new()),
                    sender: SuiAddress([0; 32]),
                    gas_data: GasData {
                        payment: Vec::new(),
                        owner: SuiAddress([0; 32]),
                        price: 0,
                        budget: 0,
                    },
                    expiration: TransactionExpiration::None,
                }),
            },
            tx_signatures: Vec::new(),
        });
        let bytes = bcs::to_bytes(&one).unwrap();
        assert_eq!(bytes[0], 1);
        assert_eq!(bcs::from_bytes::<SenderSignedData>(&bytes).unwrap(), one);

        let none = vec![0];
        let mut two = vec![2];
        two.extend_from_slice(&bytes[1..]);
        two.extend_from_slice(&bytes[1..]);
        for bytes in [none, two] {
            assert!(bcs::from_bytes::<SenderSignedData>(&bytes).is_err());
            let (e, _) = Message::<view::SenderSignedData<'static>>::parse(bytes).unwrap_err();
            assert_eq!(e, ParseError::NotOneTransaction);
        }
    }

    #[test]
    fn module_map_must_be_sorted() {
        let mut header = vec![0; 32 + 8];
        header.push(2);
        let mut sorted = header.clone();
        sorted.extend_from_slice(b"\x01a\x00\x01b\x00\x00\x00");
        let mut unsorted = header;
        unsorted.extend_from_slice(b"\x01b\x00\x01a\x00\x00\x00");

        let package: MovePackage = bcs::from_bytes(&sorted).unwrap();
        assert_eq!(bcs::to_bytes(&package).unwrap(), sorted);
        assert!(bcs::from_bytes::<MovePackage>(&unsorted).is_err());
    }
}
