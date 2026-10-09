// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The parts of `sui_types::storage` execution reads through. Objects come
//! back as views borrowed for the transaction (`'a`): the implementation
//! keeps what it read alive that long.

use std::fmt;

use messages::base::{ObjectId, SequenceNumber, SuiAddress};
use messages::type_tag::TypeTag;

use crate::base::EpochId;
use crate::object::Object;

/// `sui_types::error::SuiError`, as execution sees it: a storage failure,
/// only ever reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuiError(pub String);

impl fmt::Display for SuiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SuiError {}

pub type SuiResult<T = ()> = Result<T, SuiError>;

#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum ObjectFundsSufficiency {
    Sufficient,
    Insufficient,
    Overflow,
    LoadError(String),
}

pub trait BackingPackageStore<'a> {
    /// The package with id `package_id`, if there is one: its object, whose
    /// data is a package.
    fn get_package_object(&self, package_id: &ObjectId) -> SuiResult<Option<Object<'a>>>;
}

/// An abstraction of the (possibly distributed) store for objects. This
/// API only allows for the retrieval of objects, not any state changes
pub trait RuntimeObjectResolver<'a>: BackingPackageStore<'a> {
    /// `child` must have an `ObjectOwner` ownership equal to `owner`.
    fn read_child_object(
        &self,
        parent: &ObjectId,
        child: &ObjectId,
        child_version_upper_bound: SequenceNumber,
    ) -> SuiResult<Option<Object<'a>>>;

    /// `receiving_object_id` must have an `AddressOwner` ownership equal to `owner`.
    /// `get_object_received_at_version` must be the exact version at which the object will be received,
    /// and it cannot have been previously received at that version. NB: An object not existing at
    /// that version, and not having valid access to the object will be treated exactly the same
    /// and `Ok(None)` must be returned.
    fn get_object_received_at_version(
        &self,
        owner: &ObjectId,
        receiving_object_id: &ObjectId,
        receive_object_at_version: SequenceNumber,
        epoch_id: EpochId,
    ) -> SuiResult<Option<Object<'a>>>;

    /// Get's the package at the given version. Returns `Some(package)` only if the `package_id` is
    /// a `MovePackage` with the given `package_version`. Returns `None` in all other cases.
    ///
    /// Since the has the _possibility_ of doing unsequenced reads of object IDs it is important
    /// here that:
    /// * If the package object does not exist; or
    /// * If the package object exists but is not a Move package; or
    /// * If the package object exists and is a Move package, but the version is not the supplied version.
    ///
    /// All return the same error.
    ///
    /// To be extra careful, we simply return `None` in all cases unless the object is a package
    /// with the exact version supplied, and let the caller decide how to handle it.
    fn get_package_at_version(
        &self,
        package_id: &ObjectId,
        package_version: SequenceNumber,
    ) -> Option<Object<'a>> {
        let object = self.get_package_object(package_id).ok().flatten()?;
        let move_pkg = object.try_as_package()?;
        if move_pkg.version == package_version {
            Some(object)
        } else {
            None
        }
    }
}

/// `sui_types::storage::ObjectStore`: objects by id, live or at a version.
pub trait ObjectStore<'a> {
    fn get_object(&self, object_id: &ObjectId) -> Option<Object<'a>>;

    fn get_object_by_key(&self, object_id: &ObjectId, version: SequenceNumber)
    -> Option<Object<'a>>;

    /// Load an implicitly read system object at the given version.
    /// Returns None if the store no longer has that version.
    fn load_implicitly_read_system_object(
        &self,
        object_id: &ObjectId,
        version: SequenceNumber,
    ) -> Option<Object<'a>> {
        self.get_object_by_key(object_id, version)
    }
}

/// `sui_types::storage::BackingStore`, without `ParentSync`, which only older protocol versions
/// read.
pub trait BackingStore<'a>: RuntimeObjectResolver<'a> + ObjectStore<'a> {}

impl<'a, T> BackingStore<'a> for T where T: RuntimeObjectResolver<'a> + ObjectStore<'a> {}

/// Resolves the balance available for object-funds withdrawals during execution.
pub trait ObjectFundsResolver {
    fn object_available_balance(&self, owner: SuiAddress, type_: &TypeTag<'_>) -> SuiResult<u128>;
}
