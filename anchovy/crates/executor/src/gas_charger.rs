// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The adapter's `gas_charger`. Smashing and the final charge go through the temporary store,
//! and come with it; only gas model 15 on exists, so the legacy charging is left out.

// Smashing payments construct and read what is unused until `GasCharger::new` is ported.
#![allow(dead_code)]

use containers::{Bump, IndexMap};
use exec_types::base::ObjectRef;
use messages::base::{ObjectId, SuiAddress, TransactionDigest};
use messages::effects::GasCostSummary;
use sui_protocol_config::ProtocolConfig;

use crate::error::{ExecutionError, ExecutionErrorKind};
use crate::gas::{SuiGasStatus, SuiGasStatusAPI};
use crate::gas_model::tables::GasStatus;
use exec_types::invariant_violation;

/// Encapsulates the gas metering state (`SuiGasStatus`) and the payment source metadata,
/// whether it is from a smashed list (coin objects or address-balance withdrawals) or
/// un-metered. In other words, this serves the point of interaction between the on-chain data
/// (coins and address balances) and the gas meter.
#[derive(Debug)]
pub struct GasCharger<'a> {
    tx_digest: TransactionDigest,
    gas_model_version: u64,
    payment: PaymentMetadata<'a>,
    gas_status: SuiGasStatus,
}

/// Internal representation of how a transaction's gas is being paid.
/// `Unmetered` for no payment (dev inspect and system transactions).
/// `Gasless` for metered-but-free transactions (gas is metered but not charged).
/// `Smash` when one or more user-provided payment methods have been combined into a single
/// source.
// One per transaction, so the variants' sizes don't matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum PaymentMetadata<'a> {
    Unmetered,
    Gasless,
    /// Contains the list of payments (coins and address balances) and additional metadata
    Smash(SmashMetadata<'a>),
}

/// State produced by smashing multiple gas payment sources into one.
/// Tracks the combined balance (`total_smashed`), the target location where the
/// smashed value lives, and the original payment methods for bookkeeping.
/// Note that the target location (`gas_charge_location`) may differ from the first payment
/// method in the list if it has been overridden during execution.
#[derive(Debug)]
struct SmashMetadata<'a> {
    /// The location to charge gas from at the end of execution. Starts with the primary
    /// payment method but may be overridden.
    gas_charge_location: PaymentLocation,
    /// The total balance of all smashed payment methods.
    total_smashed: u64,
    /// The "primary" payment method that serves as the recipient of the `total_smashed`. Also,
    /// provides the initial location of the `gas_charge_location` before any overrides.
    smash_target: PaymentMethod,
    /// The original payment methods to be smashed into the `smash_target`. It does not include
    /// the `smash_target` itself. Keyed by location to guarantee uniqueness.
    smashed_payments: IndexMap<'a, PaymentLocation, PaymentMethod>,
}

/// Public wrapper that describes how gas will be paid before smashing occurs.
/// Constructed via `PaymentKind::unmetered()` or `PaymentKind::smash(methods)` and
/// consumed by `GasCharger::new`.
#[derive(Debug)]
pub struct PaymentKind<'a>(PaymentKind_<'a>);

/// Inner representation for `PaymentKind`. Kept private so construction is forced through
/// the validation in `PaymentKind::smash`.
#[derive(Debug)]
enum PaymentKind_<'a> {
    Unmetered,
    Gasless,
    /// A non-empty map of gas coins or address balance withdrawals, keyed by location.
    /// The first entry is the smash target; all others are smashed into it.
    Smash(IndexMap<'a, PaymentLocation, PaymentMethod>),
}

/// A single source of SUI used to pay for gas: either a coin object or a withdrawal
/// reservation from an address balance.
#[derive(Debug, Clone, Copy)]
pub enum PaymentMethod {
    Coin(ObjectRef),
    AddressBalance(SuiAddress, /* withdrawal reservation */ u64),
}

/// Identifies where a gas payment lives, independent of its value (`ObjectRef` or reservation).
/// Used often as a key, e.g. during smashing and during gas final charging.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum PaymentLocation {
    Coin(ObjectId),
    AddressBalance(SuiAddress),
}

/// A resolved gas payment: the location that will receive the final charge or refund,
/// paired with the total SUI available after smashing. Produced by
/// `GasCharger::gas_payment_amount` and consumed by PTB execution to set up the
/// runtime gas coin.
#[derive(Debug, Clone, Copy)]
pub struct GasPayment {
    /// The location of the gas payment (coin or address balance), which also serves as the
    /// target for smashed gas payments.
    pub location: PaymentLocation,
    /// The total amount available for gas payment after smashing
    pub amount: u64,
}

impl PaymentMethod {
    pub fn location(&self) -> PaymentLocation {
        match self {
            PaymentMethod::Coin(obj_ref) => PaymentLocation::Coin(obj_ref.0),
            PaymentMethod::AddressBalance(addr, _) => PaymentLocation::AddressBalance(*addr),
        }
    }
}

impl<'a> GasCharger<'a> {
    pub fn new_unmetered(tx_digest: TransactionDigest, protocol_config: &ProtocolConfig) -> Self {
        Self {
            tx_digest,
            gas_model_version: protocol_config.gas_model_version(),
            payment: PaymentMetadata::Unmetered,
            gas_status: SuiGasStatus::new_unmetered(),
        }
    }

    // TODO: there is only one caller to this function that should not exist otherwise.
    //       Explore way to remove it.
    pub(crate) fn used_coins(&self) -> impl Iterator<Item = &'_ ObjectRef> {
        let metadata = match &self.payment {
            PaymentMetadata::Unmetered | PaymentMetadata::Gasless => None,
            PaymentMetadata::Smash(metadata) => Some(metadata),
        };
        metadata.into_iter().flat_map(SmashMetadata::used_coins)
    }

    // Override the gas payment location for smashing
    pub fn override_gas_charge_location(
        &mut self,
        location: PaymentLocation,
    ) -> Result<(), ExecutionError<'a>> {
        if let PaymentMetadata::Smash(metadata) = &mut self.payment {
            metadata.gas_charge_location = location;
            Ok(())
        } else {
            invariant_violation!("Can only override gas charge location in the smash-gas case")
        }
    }

    /// Return the amount available at the given input payment location.
    /// For unmetered, this is None.
    /// For smashed gas payments, this is the payment location and the total amount smashed.
    /// This information feels a bit brittle but should be used only by PTB execution.
    /// This might also differ from the final charge location, if override_gas_charge_location
    /// is used.
    pub fn gas_payment_amount(&self) -> Option<GasPayment> {
        match &self.payment {
            PaymentMetadata::Unmetered | PaymentMetadata::Gasless => None,
            PaymentMetadata::Smash(metadata) => Some(GasPayment {
                location: metadata.smash_target.location(),
                amount: metadata.total_smashed,
            }),
        }
    }

    /// The coin that receives the final gas charge, or `None` when gas is paid from an address
    /// balance (or there is no payment, e.g. unmetered/gasless).
    pub fn gas_coin(&self) -> Option<ObjectId> {
        self.gas_payment_amount().and_then(|gp| match gp.location {
            PaymentLocation::Coin(coin_id) => Some(coin_id),
            PaymentLocation::AddressBalance(_) => None,
        })
    }

    pub(crate) fn gas_payment_location(&self) -> Option<PaymentLocation> {
        match &self.payment {
            PaymentMetadata::Unmetered | PaymentMetadata::Gasless => None,
            PaymentMetadata::Smash(metadata) => Some(metadata.gas_charge_location),
        }
    }

    pub fn gas_budget(&self) -> u64 {
        self.gas_status.gas_budget()
    }

    pub fn unmetered_storage_rebate(&self) -> u64 {
        self.gas_status.unmetered_storage_rebate()
    }

    pub fn no_charges(&self) -> bool {
        self.gas_status.gas_used() == 0
            && self.gas_status.storage_rebate() == 0
            && self.gas_status.storage_gas_units() == 0
    }

    pub fn is_unmetered(&self) -> bool {
        self.gas_status.is_unmetered()
    }

    pub fn set_computation_to_budget(&mut self) {
        self.gas_status.adjust_computation_on_out_of_gas();
    }

    pub fn move_gas_status(&self) -> &GasStatus {
        self.gas_status.move_gas_status()
    }

    pub fn move_gas_status_mut(&mut self) -> &mut GasStatus {
        self.gas_status.move_gas_status_mut()
    }

    pub fn into_gas_status(self) -> SuiGasStatus {
        self.gas_status
    }

    pub fn summary(&self) -> GasCostSummary {
        self.gas_status.summary()
    }

    //
    // Gas charging operations
    //

    pub fn track_storage_mutation(
        &mut self,
        object_id: ObjectId,
        new_size: usize,
        storage_rebate: u64,
    ) -> Option<u64> {
        self.gas_status
            .track_storage_mutation(&object_id, new_size, storage_rebate)
    }

    pub fn reset_storage_cost_and_rebate(&mut self) {
        self.gas_status.reset_storage_cost_and_rebate();
    }

    pub fn charge_publish_package(&mut self, size: usize) -> Result<(), ExecutionError<'a>> {
        self.gas_status.charge_publish_package(size)
    }

    pub fn charge_coin_transfers(
        &mut self,
        protocol_config: &ProtocolConfig,
        num_non_gas_coin_owners: u64,
    ) -> Result<(), ExecutionError<'a>> {
        // times two for the global pause and per-address settings
        // this "overcharges" slightly since it does not check the global pause for each owner
        // but rather each coin type.
        let bytes_read_per_owner =
            sui_types::deny_list_v2::CONFIG_SETTING_DYNAMIC_FIELD_SIZE_FOR_GAS;
        // associate the cost with dynamic field access so that it will increase if/when this
        // cost increases
        let cost_per_byte =
            protocol_config.dynamic_field_borrow_child_object_type_cost_per_byte() as usize;
        let cost_per_owner = bytes_read_per_owner * cost_per_byte;
        let owner_cost = cost_per_owner * (num_non_gas_coin_owners as usize);
        self.gas_status.charge_storage_read(owner_cost)
    }

    pub fn round_computation<T>(
        &mut self,
        result: Result<T, ExecutionError<'a>>,
    ) -> Result<T, ExecutionError<'a>> {
        debug_assert!(self.gas_status.storage_rebate() == 0);
        debug_assert!(self.gas_status.storage_gas_units() == 0);

        if matches!(&self.payment, PaymentMetadata::Unmetered) {
            return result;
        }
        let is_move_abort = matches!(
            result.as_ref().err().map(|e| e.kind()),
            Some(ExecutionErrorKind::MoveAbort(..))
        );
        let round_res = self.gas_status.bucketize_computation(Some(is_move_abort));
        match result {
            Ok(v) => round_res.map(|_| v),
            Err(e) => Err(e),
        }
    }
}

impl SmashMetadata<'_> {
    /// Iterates over all payment methods: the smash target followed by the smashed payments.
    fn payment_methods(&self) -> impl Iterator<Item = &'_ PaymentMethod> {
        std::iter::once(&self.smash_target).chain(self.smashed_payments.values())
    }

    fn used_coins(&self) -> impl Iterator<Item = &'_ ObjectRef> {
        self.payment_methods().filter_map(|method| match method {
            PaymentMethod::Coin(obj_ref) => Some(obj_ref),
            PaymentMethod::AddressBalance(_, _) => None,
        })
    }
}

impl<'a> PaymentKind<'a> {
    pub fn unmetered() -> Self {
        Self(PaymentKind_::Unmetered)
    }

    /// Metered-but-free.
    pub fn gasless() -> Self {
        Self(PaymentKind_::Gasless)
    }

    /// `None` on an invalid payment set: empty, a duplicate gas coin, or an overflowing
    /// address-balance reservation sum.
    pub fn smash(bump: &'a Bump, payment_methods: &[PaymentMethod]) -> Option<Self> {
        if payment_methods.is_empty() {
            return None;
        }
        let mut unique_methods = IndexMap::with_capacity_in(payment_methods.len(), bump);
        for &payment_method in payment_methods {
            match (
                unique_methods.entry(payment_method.location()),
                payment_method,
            ) {
                (containers::Entry::Vacant(entry), payment_method) => {
                    entry.insert(payment_method);
                }
                (
                    containers::Entry::Occupied(mut occupied),
                    PaymentMethod::AddressBalance(other, additional),
                ) => {
                    let PaymentMethod::AddressBalance(addr, amount) = occupied.get_mut() else {
                        unreachable!("Payment method does not match location")
                    };
                    assert_eq!(*addr, other, "Payment method does not match location");
                    *amount = amount.checked_add(additional)?;
                }
                // Duplicate gas coin; input checks should have rejected it.
                (containers::Entry::Occupied(_), _) => return None,
            }
        }
        Some(Self(PaymentKind_::Smash(unique_methods)))
    }
}
