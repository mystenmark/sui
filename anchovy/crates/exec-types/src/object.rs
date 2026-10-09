// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `sui_types::object::Object`, as a `Copy` value of borrowed parts.
//!
//! Its parts live in the transaction's arena, or in the store's message the
//! object was read from. An object read from the store keeps its stored
//! bytes, which its digest hashes. A change makes a new value, sharing the
//! parts it did not change, and without stored bytes until `seal` encodes
//! it; the fields are private so nothing changes under bytes that no longer
//! encode it. This replaces the reference's `Arc<ObjectInner>` and its
//! copy-on-write, which copied an object's contents to change its version.

use containers::Bump;
use messages::base::{
    Digest, ObjectDigest, ObjectId, SequenceNumber, SuiAddress, TransactionDigest,
};
use messages::object::{Data, MoveObject, MoveObjectType, MovePackage, Owner};

use crate::base::ObjectRef;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Object<'a> {
    data: Data<'a>,
    owner: Owner<'a>,
    previous_transaction: TransactionDigest,
    storage_rebate: u64,
    /// The object's encoding, while it is unchanged since read or sealed.
    stored: Option<&'a [u8]>,
}

impl<'a> Object<'a> {
    /// An object as the store holds it.
    pub fn from_view(view: &messages::object::Object<'a>) -> Object<'a> {
        Object {
            data: view.data,
            owner: view.owner,
            previous_transaction: *view.previous_transaction,
            storage_rebate: view.storage_rebate,
            stored: Some(view.bytes),
        }
    }

    pub fn new_move(
        o: MoveObject<'a>,
        owner: Owner<'a>,
        previous_transaction: TransactionDigest,
    ) -> Object<'a> {
        Object {
            data: Data::Move(o),
            owner,
            previous_transaction,
            storage_rebate: 0,
            stored: None,
        }
    }

    pub fn new_from_package(
        package: MovePackage<'a>,
        previous_transaction: TransactionDigest,
    ) -> Object<'a> {
        Object {
            data: Data::Package(package),
            owner: Owner::Immutable,
            previous_transaction,
            storage_rebate: 0,
            stored: None,
        }
    }

    pub fn data(&self) -> &Data<'a> {
        &self.data
    }

    pub fn owner(&self) -> &Owner<'a> {
        &self.owner
    }

    pub fn previous_transaction(&self) -> TransactionDigest {
        self.previous_transaction
    }

    pub fn storage_rebate(&self) -> u64 {
        self.storage_rebate
    }

    /// The encoding, if unchanged since read or sealed.
    pub fn stored_bytes(&self) -> Option<&'a [u8]> {
        self.stored
    }

    /// Encodes the object into `bump`, so that its digest and its stored
    /// form need no further encoding.
    #[must_use]
    pub fn seal(self, bump: &'a Bump) -> Object<'a> {
        if self.stored.is_some() {
            return self;
        }
        let mut w = messages::fast::Writer::new_in(bump, self.encoded_len_hint());
        w.object(
            &self.data,
            &self.owner,
            &self.previous_transaction,
            self.storage_rebate,
        );
        Object {
            stored: Some(w.finish_bytes()),
            ..self
        }
    }

    fn encoded_len_hint(&self) -> usize {
        let data = match &self.data {
            Data::Move(m) => m.contents.len() + 64,
            Data::Package(p) => p.module_map.iter().map(|(_, m)| m.len()).sum::<usize>() + 256,
        };
        data + 128
    }

    /// The digest of the object's encoding.
    ///
    /// # Panics
    /// If the object changed since it was read and was not sealed since.
    pub fn digest(&self) -> ObjectDigest {
        Digest::of(
            "Object",
            self.stored
                .expect("an object's digest is taken once it is sealed"),
        )
    }

    pub fn id(&self) -> ObjectId {
        match &self.data {
            Data::Package(p) => *p.id,
            Data::Move(m) => m.id(),
        }
    }

    pub fn version(&self) -> SequenceNumber {
        match &self.data {
            Data::Package(p) => p.version,
            Data::Move(m) => m.version,
        }
    }

    pub fn compute_object_reference(&self) -> ObjectRef {
        (self.id(), self.version(), self.digest())
    }

    pub fn is_package(&self) -> bool {
        matches!(&self.data, Data::Package(_))
    }

    pub fn try_as_move(&self) -> Option<&MoveObject<'a>> {
        match &self.data {
            Data::Move(m) => Some(m),
            Data::Package(_) => None,
        }
    }

    pub fn try_as_package(&self) -> Option<&MovePackage<'a>> {
        match &self.data {
            Data::Move(_) => None,
            Data::Package(p) => Some(p),
        }
    }

    pub fn type_(&self) -> Option<&MoveObjectType<'a>> {
        self.try_as_move().map(|m| &m.type_)
    }

    pub fn is_immutable(&self) -> bool {
        matches!(self.owner, Owner::Immutable)
    }

    pub fn is_address_owned(&self) -> bool {
        matches!(self.owner, Owner::AddressOwner(_))
    }

    pub fn is_child_object(&self) -> bool {
        matches!(self.owner, Owner::ObjectOwner(_))
    }

    pub fn is_shared(&self) -> bool {
        matches!(self.owner, Owner::Shared { .. })
    }

    pub fn is_consensus(&self) -> bool {
        matches!(
            self.owner,
            Owner::Shared { .. } | Owner::ConsensusAddressOwner { .. } | Owner::Party(_)
        )
    }

    /// `Owner::get_owner_address`, without its error: `None` for an owner
    /// without one.
    pub fn get_single_owner(&self) -> Option<SuiAddress> {
        get_owner_address(&self.owner)
    }

    /// The reference's estimate of the object's size, for gas: its metadata
    /// at fixed sizes and its data roughly as encoded.
    pub fn object_size_for_gas_metering(&self) -> usize {
        const DEFAULT_OWNER_SIZE: usize = 40;
        const TRANSACTION_DIGEST_SIZE: usize = 32;
        const STORAGE_REBATE_SIZE: usize = 8;

        let meta_data_size = DEFAULT_OWNER_SIZE + TRANSACTION_DIGEST_SIZE + STORAGE_REBATE_SIZE;
        let data_size = match &self.data {
            Data::Move(m) => move_object_size_for_gas_metering(m),
            Data::Package(p) => move_package_size(p),
        };
        meta_data_size + data_size
    }

    #[must_use]
    pub fn with_owner(self, owner: Owner<'a>) -> Object<'a> {
        Object {
            owner,
            stored: None,
            ..self
        }
    }

    #[must_use]
    pub fn with_storage_rebate(self, storage_rebate: u64) -> Object<'a> {
        Object {
            storage_rebate,
            stored: None,
            ..self
        }
    }

    #[must_use]
    pub fn with_previous_transaction(self, previous_transaction: TransactionDigest) -> Object<'a> {
        Object {
            previous_transaction,
            stored: None,
            ..self
        }
    }

    #[must_use]
    pub fn with_data(self, data: Data<'a>) -> Object<'a> {
        Object {
            data,
            stored: None,
            ..self
        }
    }
}

/// `Owner::get_owner_address`, without its error.
pub fn get_owner_address(owner: &Owner<'_>) -> Option<SuiAddress> {
    match owner {
        Owner::AddressOwner(address)
        | Owner::ObjectOwner(address)
        | Owner::ConsensusAddressOwner { owner: address, .. } => Some(**address),
        Owner::Shared { .. } | Owner::Immutable | Owner::Party(_) => None,
    }
}

/// `MoveObject::object_size_for_gas_metering`.
pub fn move_object_size_for_gas_metering(m: &MoveObject<'_>) -> usize {
    // + 1 for 'has_public_transfer'
    // + 8 for `version`
    m.contents.len() + m.type_.bcs_size() + 1 + 8
}

/// `MovePackage::size`, which is its `object_size_for_gas_metering`.
pub fn move_package_size(p: &MovePackage<'_>) -> usize {
    let module_map_size = p
        .module_map
        .iter()
        .map(|(name, module)| name.len() + module.len())
        .sum::<usize>();
    let type_origin_table_size = p
        .type_origin_table
        .iter()
        .map(|t| t.module_name.len() + t.datatype_name.len() + ObjectId::LENGTH)
        .sum::<usize>();

    let linkage_table_size =
        p.linkage_table.len() * (ObjectId::LENGTH + (ObjectId::LENGTH + 8/* SequenceNumber */));

    8 /* SequenceNumber */ + module_map_size + type_origin_table_size + linkage_table_size
}

/// `OBJECT_START_VERSION`: the version objects and packages are created at.
pub const OBJECT_START_VERSION: SequenceNumber = 1;

/// `MovePackage::original_package_id`: the id a package was first published at, which every
/// version's modules carry as their address.
///
/// # Panics
/// If an upgraded package's first module does not deserialize, as the reference does.
pub fn original_package_id(package: &MovePackage<'_>) -> ObjectId {
    if package.version == OBJECT_START_VERSION {
        // for a non-upgraded package, original ID is just the package ID
        return *package.id;
    }
    // The reference takes the first module in name order and this the first in encoded order,
    // but every module of a package carries the same address.
    let (_, bytes) = package.module_map.first().expect("Empty module map");
    let module = move_binary_format::CompiledModule::deserialize_with_defaults(bytes)
        .expect("A Move package contains a module that cannot be deserialized");
    ObjectId(module.address().into_bytes())
}
