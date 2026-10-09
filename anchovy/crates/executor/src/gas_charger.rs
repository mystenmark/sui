// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The adapter's `gas_charger`. Its payment types first; `GasCharger` comes with the temporary
//! store it charges through.

use exec_types::base::ObjectRef;
use messages::base::{ObjectId, SuiAddress};

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
