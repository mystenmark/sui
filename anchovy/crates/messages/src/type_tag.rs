// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::arena::{Alloc, Ref};
use crate::base::AccountAddress;
use crate::error::{ParseError, Result};
use crate::reader::Reader;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TypeTag<'a> {
    Bool,
    U8,
    U64,
    U128,
    Address,
    Signer,
    Vector(Ref<'a, TypeTag<'a>>),
    Struct(Ref<'a, StructTag<'a>>),
    U16,
    U32,
    U256,
}

/// Module and name are not checked against the Move identifier grammar here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StructTag<'a> {
    pub address: &'a AccountAddress,
    pub module: &'a str,
    pub name: &'a str,
    pub type_params: &'a [TypeTag<'a>],
}

/// On the wire `TypeInput` and `StructInput` are `TypeTag` and `StructTag`.
/// The reference keeps them apart only because its `TypeTag` validates
/// identifiers while deserializing, which this crate never does.
pub type TypeInput<'a> = TypeTag<'a>;
pub type StructInput<'a> = StructTag<'a>;

impl<'a> TypeTag<'a> {
    pub const MIN_WIRE_SIZE: usize = 1;

    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<TypeTag<'a>> {
        r.enter()?;
        let tag = match r.variant()? {
            0 => TypeTag::Bool,
            1 => TypeTag::U8,
            2 => TypeTag::U64,
            3 => TypeTag::U128,
            4 => TypeTag::Address,
            5 => TypeTag::Signer,
            6 => {
                let inner = TypeTag::parse(r, a)?;
                TypeTag::Vector(a.value(inner)?)
            }
            7 => {
                let inner = StructTag::parse(r, a)?;
                TypeTag::Struct(a.value(inner)?)
            }
            8 => TypeTag::U16,
            9 => TypeTag::U32,
            10 => TypeTag::U256,
            tag => return Err(ParseError::UnknownVariant { ty: "TypeTag", tag }),
        };
        r.leave();
        Ok(tag)
    }

    pub fn parse_vec<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<&'a [TypeTag<'a>]> {
        let n = r.seq_len(TypeTag::MIN_WIRE_SIZE)?;
        let mut out = a.slice(n)?;
        for _ in 0..n {
            out.push(TypeTag::parse(r, a)?);
        }
        Ok(out.finish())
    }
}

impl<'a> StructTag<'a> {
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<StructTag<'a>> {
        r.enter()?;
        r.count_struct_tag();
        // The address is the one leaf container that type nesting can push
        // past the depth limit, so it is the one leaf that is counted.
        r.enter()?;
        let address = AccountAddress::parse(r)?;
        r.leave();
        let module = r.str()?;
        let name = r.str()?;
        let type_params = TypeTag::parse_vec(r, a)?;
        r.leave();
        Ok(StructTag {
            address,
            module,
            name,
            type_params,
        })
    }
}

crate::impl_wire!(TypeTag);
crate::impl_wire!(StructTag);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;

    // 0x2::coin::Coin<vector<u8>>
    fn coin_of_bytes() -> Vec<u8> {
        let mut v = vec![7];
        let mut addr = [0u8; 32];
        addr[31] = 2;
        v.extend_from_slice(&addr);
        v.extend_from_slice(b"\x04coin\x04Coin\x01\x06\x01");
        v
    }

    #[test]
    fn struct_tag() {
        let m = Message::<TypeTag>::parse(coin_of_bytes()).unwrap();
        let TypeTag::Struct(s) = *m.get() else {
            panic!("not a struct")
        };
        assert_eq!(s.address.0[31], 2);
        assert_eq!((s.module, s.name), ("coin", "Coin"));
        let [TypeTag::Vector(inner)] = s.type_params else {
            panic!("not one vector param")
        };
        assert_eq!(**inner, TypeTag::U8);
        // A StructTag, one type parameter, and the vector's element.
        assert_eq!(m.arena_used(), 56 + 16 + 16);
    }

    #[test]
    fn primitive_needs_no_arena() {
        let m = Message::<TypeTag>::parse(vec![2]).unwrap();
        assert_eq!(*m.get(), TypeTag::U64);
        assert_eq!(m.arena_used(), 0);
        let exact = Message::<TypeTag>::parse_exact(vec![2]).unwrap();
        assert_eq!(exact.arena_size(), 0);
    }

    #[test]
    fn rejects() {
        let err = |b: Vec<u8>| Message::<TypeTag>::parse(b).unwrap_err().0;
        assert_eq!(err(vec![]), ParseError::UnexpectedEof);
        assert_eq!(err(vec![2, 0]), ParseError::TrailingBytes);
        assert_eq!(
            err(vec![11]),
            ParseError::UnknownVariant {
                ty: "TypeTag",
                tag: 11
            }
        );
        let mut bad_utf8 = coin_of_bytes();
        bad_utf8[34] = 0xff;
        assert_eq!(err(bad_utf8), ParseError::InvalidUtf8);
    }

    #[test]
    fn nesting_is_bounded_like_bcs() {
        // Each vector is one enum; the innermost u8 is the 500th container.
        let mut ok = vec![6; 499];
        ok.push(1);
        assert!(Message::<TypeTag>::parse(ok).is_ok());
        let mut deep = vec![6; 500];
        deep.push(1);
        assert_eq!(
            Message::<TypeTag>::parse(deep).unwrap_err().0,
            ParseError::ContainerDepthExceeded
        );
    }
}
