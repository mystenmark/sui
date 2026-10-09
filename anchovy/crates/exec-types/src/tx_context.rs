// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `sui_types::base_types::TxContext`. The digest is fixed-size here: the
//! reference holds it as bytes only so the test-scenario natives, which
//! anchovy leaves out, can replace it with any.

use messages::base::{SuiAddress, TransactionDigest};
use move_core_types::account_address::AccountAddress;
use sui_protocol_config::ProtocolConfig;

use crate::base::{EpochId, derive_id};
use crate::error::{ExecutionError, ExecutionErrorKind};
use messages::base::ObjectId;

/// `MoveLegacyTxContext`'s encoding: sender, digest (as a byte vector of
/// 32), epoch, epoch timestamp and ids created.
pub const LEGACY_CONTEXT_LEN: usize = 32 + 1 + 32 + 8 + 8 + 8;

/// `MoveLegacyTxContext`, as read back from Move: the digest as Move holds
/// it, of any length.
#[derive(Clone, Copy, Debug)]
pub struct MoveLegacyTxContext<'b> {
    sender: AccountAddress,
    digest: &'b [u8],
    #[allow(dead_code)]
    epoch: EpochId,
    #[allow(dead_code)]
    epoch_timestamp_ms: u64,
    ids_created: u64,
}

impl<'b> MoveLegacyTxContext<'b> {
    /// Its BCS, as the VM serializes the `TxContext` struct.
    pub fn from_bcs(bytes: &'b [u8]) -> Option<MoveLegacyTxContext<'b>> {
        let mut r = messages::reader::Reader::new(bytes);
        let sender = AccountAddress::new(r.record::<messages::base::AccountAddress>().ok()?.0);
        let digest = r.byte_vec().ok()?;
        let epoch = r.u64().ok()?;
        let epoch_timestamp_ms = r.u64().ok()?;
        let ids_created = r.u64().ok()?;
        r.finish().ok()?;
        Some(MoveLegacyTxContext {
            sender,
            digest,
            epoch,
            epoch_timestamp_ms,
            ids_created,
        })
    }
}

// Information about the transaction context.
// This struct is not related to Move and can evolve as needed/required.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxContext {
    /// Sender of the transaction
    sender: AccountAddress,
    /// Digest of the current transaction
    digest: TransactionDigest,
    /// The current epoch number
    epoch: EpochId,
    /// Timestamp that the epoch started at
    epoch_timestamp_ms: u64,
    /// Number of `ObjectID`'s generated during execution of the current transaction
    ids_created: u64,
    // Reference gas price
    rgp: u64,
    // gas price passed to transaction as input
    gas_price: u64,
    // gas budget passed to transaction as input
    gas_budget: u64,
    // address of the sponsor if any
    sponsor: Option<AccountAddress>,
    // whether the `TxContext` is native or not
    is_native: bool,
}

impl TxContext {
    pub fn new_from_components(
        sender: &SuiAddress,
        digest: &TransactionDigest,
        epoch_id: &EpochId,
        epoch_timestamp_ms: u64,
        rgp: u64,
        gas_price: u64,
        gas_budget: u64,
        sponsor: Option<SuiAddress>,
        protocol_config: &ProtocolConfig,
    ) -> Self {
        Self {
            sender: AccountAddress::new(sender.0),
            digest: *digest,
            epoch: *epoch_id,
            epoch_timestamp_ms,
            ids_created: 0,
            rgp,
            gas_price,
            gas_budget,
            sponsor: sponsor.map(|s| AccountAddress::new(s.0)),
            is_native: protocol_config.move_native_context(),
        }
    }

    pub fn epoch(&self) -> EpochId {
        self.epoch
    }

    pub fn sender(&self) -> SuiAddress {
        SuiAddress(self.sender.into_bytes())
    }

    pub fn epoch_timestamp_ms(&self) -> u64 {
        self.epoch_timestamp_ms
    }

    /// Return the transaction digest, to include in new objects
    pub fn digest(&self) -> TransactionDigest {
        self.digest
    }

    pub fn sponsor(&self) -> Option<SuiAddress> {
        self.sponsor.map(|s| SuiAddress(s.into_bytes()))
    }

    pub fn rgp(&self) -> u64 {
        self.rgp
    }

    pub fn gas_price(&self) -> u64 {
        self.gas_price
    }

    pub fn gas_budget(&self) -> u64 {
        self.gas_budget
    }

    pub fn ids_created(&self) -> u64 {
        self.ids_created
    }

    /// Derive a globally unique object ID by hashing self.digest | self.ids_created
    pub fn fresh_id(&mut self) -> ObjectId {
        let id = derive_id(&self.digest, self.ids_created);

        self.ids_created += 1;
        id
    }

    /// The `MoveLegacyTxContext` Move sees: with a native context, only the
    /// digest is real.
    pub fn to_bcs_legacy_context(&self) -> [u8; LEGACY_CONTEXT_LEN] {
        let (sender, epoch, epoch_timestamp_ms, ids_created) = if self.is_native {
            (AccountAddress::ZERO, 0, 0, 0)
        } else {
            (
                self.sender,
                self.epoch,
                self.epoch_timestamp_ms,
                self.ids_created,
            )
        };
        let mut out = [0u8; LEGACY_CONTEXT_LEN];
        out[..32].copy_from_slice(sender.as_ref());
        out[32] = 32;
        out[33..65].copy_from_slice(&self.digest.bytes);
        out[65..73].copy_from_slice(&epoch.to_le_bytes());
        out[73..81].copy_from_slice(&epoch_timestamp_ms.to_le_bytes());
        out[81..89].copy_from_slice(&ids_created.to_le_bytes());
        out
    }

    /// Updates state of the context instance. It's intended to use
    /// when mutable context is passed over some boundary via
    /// serialize/deserialize and this is the reason why this method
    /// consumes the other context..
    pub fn update_state(
        &mut self,
        other: MoveLegacyTxContext<'_>,
    ) -> Result<(), ExecutionError<'static>> {
        if !self.is_native {
            if self.sender != other.sender
                || self.digest.bytes[..] != *other.digest
                || other.ids_created < self.ids_created
            {
                return Err(ExecutionError::new_with_source(
                    ExecutionErrorKind::InvariantViolation,
                    "Immutable fields for TxContext changed",
                ));
            }
            self.ids_created = other.ids_created;
        }
        Ok(())
    }
}
