// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The store as anchovy's executor reads it during execution: objects as views of the stored
//! messages, kept for the transaction (`'a`). The same answers as `StoreView`, which serves
//! sui's executor.

use containers::Bump;
use exec_types::base::EpochId;
use exec_types::object::Object;
use exec_types::storage::{
    BackingPackageStore, ObjectStore, RuntimeObjectResolver, SuiError, SuiResult,
};
use messages::Kept;
use messages::base::{ObjectId, SequenceNumber};
use messages::object::Owner;

pub struct StoreReads<'a> {
    bump: &'a Bump,
    store: &'a store::Store,
    kept: &'a Kept<messages::object::Object<'static>>,
}

fn storage_error(e: &store::Error) -> SuiError {
    SuiError(format!("Storage: {e:?}"))
}

impl<'a> StoreReads<'a> {
    /// Objects read are kept in `kept`, whose borrow bounds the views handed out, and their
    /// digests go in `bump`.
    pub fn new(
        bump: &'a Bump,
        store: &'a store::Store,
        kept: &'a Kept<messages::object::Object<'static>>,
    ) -> StoreReads<'a> {
        StoreReads { bump, store, kept }
    }

    fn keep(
        &self,
        object: Option<messages::Message<messages::object::Object<'static>>>,
        digest: Option<messages::base::ObjectDigest>,
    ) -> Option<Object<'a>> {
        object.map(|m| Object::from_view_and_digest(self.bump, &self.kept.keep(m), digest))
    }

    pub fn live(&self, id: &ObjectId) -> Result<Option<store::Live>, store::Error> {
        self.store.live(id)
    }

    /// The live version, with the digest the store keeps beside it: `Store::live_object`,
    /// without hashing the object again.
    pub fn live_object(&self, id: &ObjectId) -> Result<Option<Object<'a>>, store::Error> {
        let Some(live) = self.store.live(id)? else {
            return Ok(None);
        };
        Ok(self.keep(self.store.object(id, live.version)?, Some(live.digest)))
    }

    /// The object at `version`, whose digest the caller knows.
    pub fn object_at_with_digest(
        &self,
        id: &ObjectId,
        version: u64,
        digest: messages::base::ObjectDigest,
    ) -> Result<Option<Object<'a>>, store::Error> {
        Ok(self.keep(self.store.object(id, version)?, Some(digest)))
    }

    pub fn object_at(
        &self,
        id: &ObjectId,
        version: u64,
    ) -> Result<Option<Object<'a>>, store::Error> {
        Ok(self.keep(self.store.object(id, version)?, None))
    }
}

impl<'a> ObjectStore<'a> for StoreReads<'a> {
    fn get_object(&self, object_id: &ObjectId) -> Option<Object<'a>> {
        self.live_object(object_id).ok().flatten()
    }

    fn get_object_by_key(
        &self,
        object_id: &ObjectId,
        version: SequenceNumber,
    ) -> Option<Object<'a>> {
        self.object_at(object_id, version).ok().flatten()
    }
}

impl<'a> BackingPackageStore<'a> for StoreReads<'a> {
    fn get_package_object(&self, package_id: &ObjectId) -> SuiResult<Option<Object<'a>>> {
        Ok(self
            .live_object(package_id)
            .map_err(|e| storage_error(&e))?
            .filter(Object::is_package))
    }
}

impl<'a> RuntimeObjectResolver<'a> for StoreReads<'a> {
    fn read_child_object(
        &self,
        parent: &ObjectId,
        child: &ObjectId,
        child_version_upper_bound: SequenceNumber,
    ) -> SuiResult<Option<Object<'a>>> {
        let Some(object) = self.keep(
            self.store
                .object_at_or_before(child, child_version_upper_bound)
                .map_err(|e| storage_error(&e))?,
            None,
        ) else {
            return Ok(None);
        };
        if !matches!(object.owner(), Owner::ObjectOwner(owner) if owner.0 == parent.0) {
            return Err(SuiError(format!(
                "InvalidChildObjectAccess: {child} given parent {parent}, owned by {:?}",
                object.owner()
            )));
        }
        Ok(Some(object))
    }

    /// The object at exactly that version, if that is its live version and
    /// it is owned by `owner`: else `None`, which the executor treats as not
    /// receivable.
    fn get_object_received_at_version(
        &self,
        owner: &ObjectId,
        receiving_object_id: &ObjectId,
        receive_object_at_version: SequenceNumber,
        _epoch_id: EpochId,
    ) -> SuiResult<Option<Object<'a>>> {
        let live = self
            .store
            .live(receiving_object_id)
            .map_err(|e| storage_error(&e))?;
        if live.map(|l| l.version) != Some(receive_object_at_version) {
            return Ok(None);
        }
        let object = self
            .object_at(receiving_object_id, receive_object_at_version)
            .map_err(|e| storage_error(&e))?;
        Ok(object.filter(|o| matches!(o.owner(), Owner::AddressOwner(a) if a.0 == owner.0)))
    }
}
