// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use messages::base::ObjectId;
use messages::object::MoveObjectType;
use move_binary_format::errors::{PartialVMError, PartialVMResult};
use move_core_types::runtime_value as R;
use move_core_types::vm_status::StatusCode;
use move_vm_runtime::execution::values::Value;

/// Whether `a` and `b` are the same layout. Runtime layouts do not implement `PartialEq`; values
/// serialize identically under equal layouts.
// Each `zip` follows a check that the lengths are equal.
#[allow(clippy::disallowed_methods)]
pub fn runtime_layouts_equal(a: &R::MoveTypeLayout, b: &R::MoveTypeLayout) -> bool {
    use R::MoveTypeLayout as L;
    #[allow(clippy::disallowed_methods)]
    fn all_equal(a: &[R::MoveTypeLayout], b: &[R::MoveTypeLayout]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(a, b)| runtime_layouts_equal(a, b))
    }
    match (a, b) {
        (L::Bool, L::Bool)
        | (L::U8, L::U8)
        | (L::U16, L::U16)
        | (L::U32, L::U32)
        | (L::U64, L::U64)
        | (L::U128, L::U128)
        | (L::U256, L::U256)
        | (L::Address, L::Address)
        | (L::Signer, L::Signer) => true,
        (L::Vector(a), L::Vector(b)) => runtime_layouts_equal(a, b),
        (L::Struct(a), L::Struct(b)) => all_equal(&a.0, &b.0),
        (L::Enum(a), L::Enum(b)) => {
            a.0.len() == b.0.len() && a.0.iter().zip(b.0.iter()).all(|(a, b)| all_equal(a, b))
        }
        (
            L::Bool
            | L::U8
            | L::U16
            | L::U32
            | L::U64
            | L::U128
            | L::U256
            | L::Address
            | L::Signer
            | L::Vector(_)
            | L::Struct(_)
            | L::Enum(_),
            _,
        ) => false,
    }
}

/// A changed child's final value, serialized with the layout it was loaded with to compare it
/// with its stored bytes. The write-out reuses the bytes when its layout is equal.
#[derive(Debug)]
pub struct SerializedChild {
    pub layout: R::MoveTypeLayout,
    pub bytes: std::vec::Vec<u8>,
}

/// This type is used to track if an object has changed since it was read from storage: by its
/// owner ID, type and BCS bytes. The reference keeps a copy of the deserialized value instead
/// and compares values; BCS is canonical, so equal values of one layout have equal bytes and the
/// verdicts agree. The bytes are borrowed from the stored object, and the layout, which the
/// natives build for the load anyway, is moved here to serialize the final value with.
pub struct ObjectFingerprint<'a>(Option<ObjectFingerprint_<'a>>);

enum ObjectFingerprint_<'a> {
    /// The object did not exist (as a child object) in storage at the start of the transaction.
    Empty,
    // The object was loaded as a child object from storage.
    Preexisting {
        owner: ObjectId,
        ty: MoveObjectType<'a>,
        bytes: &'a [u8],
        layout: R::MoveTypeLayout,
    },
}

impl<'a> ObjectFingerprint<'a> {
    /// Creates a new object fingerprint for a child object not found in storage.
    pub fn none() -> Self {
        Self(Some(ObjectFingerprint_::Empty))
    }

    /// Creates a new object fingerprint for a child found in storage.
    pub fn preexisting(
        preexisting_owner: &ObjectId,
        preexisting_type: &MoveObjectType<'a>,
        preexisting_bytes: &'a [u8],
        layout: R::MoveTypeLayout,
    ) -> Self {
        Self(Some(ObjectFingerprint_::Preexisting {
            owner: *preexisting_owner,
            ty: *preexisting_type,
            bytes: preexisting_bytes,
            layout,
        }))
    }

    /// Checks if the object has changed since it was read from storage, and if its value was
    /// serialized to tell, and differs, the serialization.
    /// Gives an invariant violation if the fingerprint is disabled, or if the final value cannot
    /// be serialized with the layout it was read with although owner and type are the same.
    pub fn object_has_changed(
        self,
        final_owner: &ObjectId,
        final_type: &MoveObjectType<'_>,
        final_value: &Option<Value>,
    ) -> PartialVMResult<(bool, Option<SerializedChild>)> {
        use ObjectFingerprint_ as F;
        let Some(inner) = self.0 else {
            return Err(
                PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR).with_message(
                    "Object fingerprint not enabled, yet we were asked for the changes".to_string(),
                ),
            );
        };
        Ok(match (inner, final_value) {
            (F::Empty, None) => (false, None),
            (F::Empty, Some(_)) | (F::Preexisting { .. }, None) => (true, None),
            (
                F::Preexisting {
                    owner: preexisting_owner,
                    ty: preexisting_type,
                    bytes: preexisting_bytes,
                    layout,
                },
                Some(final_value),
            ) => {
                // owner changed or value changed.
                // For the value, we must first check if the types are the same before comparing the
                // values
                if preexisting_owner != *final_owner || preexisting_type != *final_type {
                    return Ok((true, None));
                }
                let Some(final_bytes) = final_value.typed_serialize(&layout) else {
                    return Err(
                        PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                            .with_message(
                                "Child object value does not serialize with its layout".to_string(),
                            ),
                    );
                };
                if final_bytes == preexisting_bytes {
                    (false, None)
                } else {
                    let serialized = SerializedChild {
                        layout,
                        bytes: final_bytes,
                    };
                    (true, Some(serialized))
                }
            }
        })
    }
}
