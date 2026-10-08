// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The store as sui's executor reads it: objects decoded into sui's types
//! on demand.

use messages::base::ObjectId as Id;
use sui_types::base_types::{EpochId, ObjectID, ObjectRef, SequenceNumber};
use sui_types::error::{SuiError, SuiErrorKind, SuiResult};
use sui_types::object::{Object, Owner};
use sui_types::storage::{
    BackingPackageStore, ObjectStore, PackageObject, ParentSync, RuntimeObjectResolver,
};

use crate::{Error, Result};

pub struct StoreView<'a> {
    store: &'a store::Store,
}

fn id(object: &ObjectID) -> Id {
    Id(object.into_bytes())
}

fn decode(
    object: Option<messages::Message<messages::object::Object<'static>>>,
) -> Result<Option<Object>> {
    Ok(match object {
        Some(object) => Some(bcs::from_bytes(object.get().bytes)?),
        None => None,
    })
}

/// The executor's traits answer in `SuiError`s.
fn sui(e: &Error) -> SuiError {
    SuiErrorKind::Storage(format!("{e:?}")).into()
}

impl<'a> StoreView<'a> {
    pub fn new(store: &'a store::Store) -> StoreView<'a> {
        StoreView { store }
    }

    pub fn live(&self, object: &ObjectID) -> Result<Option<store::Live>> {
        Ok(self.store.live(&id(object))?)
    }

    pub fn live_version(&self, object: &ObjectID) -> Result<Option<SequenceNumber>> {
        Ok(self
            .live(object)?
            .map(|l| SequenceNumber::from_u64(l.version)))
    }

    pub fn live_object(&self, object: &ObjectID) -> Result<Option<Object>> {
        decode(self.store.live_object(&id(object))?)
    }

    pub fn object_at(&self, object: &ObjectID, version: u64) -> Result<Option<Object>> {
        decode(self.store.object(&id(object), version)?)
    }
}

impl ObjectStore for StoreView<'_> {
    fn get_object(&self, object_id: &ObjectID) -> Option<Object> {
        self.live_object(object_id).ok().flatten()
    }

    fn get_object_by_key(&self, object_id: &ObjectID, version: SequenceNumber) -> Option<Object> {
        self.object_at(object_id, version.value()).ok().flatten()
    }
}

impl BackingPackageStore for StoreView<'_> {
    fn get_package_object(&self, package_id: &ObjectID) -> SuiResult<Option<PackageObject>> {
        Ok(self
            .live_object(package_id)
            .map_err(|e| sui(&e))?
            .filter(|o| o.is_package())
            .map(PackageObject::new))
    }
}

impl RuntimeObjectResolver for StoreView<'_> {
    fn read_child_object(
        &self,
        parent: &ObjectID,
        child: &ObjectID,
        child_version_upper_bound: SequenceNumber,
    ) -> SuiResult<Option<Object>> {
        let Some(object) = decode(
            self.store
                .object_at_or_before(&id(child), child_version_upper_bound.value())
                .map_err(|e| sui(&e.into()))?,
        )
        .map_err(|e| sui(&e))?
        else {
            return Ok(None);
        };
        if object.owner != Owner::ObjectOwner((*parent).into()) {
            return Err(SuiErrorKind::InvalidChildObjectAccess {
                object: *child,
                given_parent: *parent,
                actual_owner: object.owner.clone(),
            }
            .into());
        }
        Ok(Some(object))
    }

    /// The object at exactly that version, if that is its live version and
    /// it is owned by `owner`: else `None`, which the executor treats as not
    /// receivable.
    fn get_object_received_at_version(
        &self,
        owner: &ObjectID,
        receiving_object_id: &ObjectID,
        receive_object_at_version: SequenceNumber,
        _epoch_id: EpochId,
    ) -> SuiResult<Option<Object>> {
        let live = self
            .live_version(receiving_object_id)
            .map_err(|e| sui(&e))?;
        if live != Some(receive_object_at_version) {
            return Ok(None);
        }
        let object = self
            .object_at(receiving_object_id, receive_object_at_version.value())
            .map_err(|e| sui(&e))?;
        Ok(object.filter(|o| o.owner == Owner::AddressOwner((*owner).into())))
    }
}

impl ParentSync for StoreView<'_> {
    /// Older protocol versions only.
    fn get_latest_parent_entry_ref_deprecated(&self, object_id: ObjectID) -> Option<ObjectRef> {
        self.live_object(&object_id)
            .ok()
            .flatten()
            .map(|o| o.compute_object_reference())
    }
}
