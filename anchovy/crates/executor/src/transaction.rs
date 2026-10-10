// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The `sui_types::transaction` functions execution calls, over the transaction's views.

use containers::{BTreeMap, Bump};
use messages::transaction::{CallArg, FundsWithdrawalArg, GasData, TransactionKind};
use messages::type_tag::TypeTag;
use sui_protocol_config::ProtocolConfig;

pub fn is_gas_paid_from_address_balance(
    gas_data: &GasData<'_>,
    transaction_kind: &TransactionKind<'_>,
) -> bool {
    gas_data.payment.is_empty()
        && matches!(
            transaction_kind,
            TransactionKind::ProgrammableTransaction(_)
        )
}

pub fn is_gasless_transaction(
    gas_data: &GasData<'_>,
    transaction_kind: &TransactionKind<'_>,
) -> bool {
    is_gas_paid_from_address_balance(gas_data, transaction_kind) && gas_data.price == 0
}

/// `TransactionKind::get_funds_withdrawals`.
pub fn get_funds_withdrawals<'b, 'a>(
    transaction_kind: &'b TransactionKind<'a>,
) -> impl Iterator<Item = &'a FundsWithdrawalArg<'a>> + 'b {
    let inputs = match transaction_kind {
        TransactionKind::ProgrammableTransaction(pt) => pt.inputs,
        _ => &[],
    };
    inputs.iter().filter_map(|input| {
        if let CallArg::FundsWithdrawal(withdraw) = input {
            Some(withdraw.get())
        } else {
            None
        }
    })
}

/// `get_gasless_allowed_token_types`, parsed into `bump` on each call: the reference caches the
/// parse per protocol version, which a transaction's arena cannot outlive.
///
/// # Panics
/// On a type in the protocol config that does not parse, as the reference does.
pub fn get_gasless_allowed_token_types<'a>(
    bump: &'a Bump,
    config: &ProtocolConfig,
) -> BTreeMap<'a, TypeTag<'a>, u64> {
    let mut map = BTreeMap::new_in(bump);
    for (s, min_amount) in config.gasless_allowed_token_types().iter() {
        let tag: move_core_types::language_storage::TypeTag = s
            .parse()
            .unwrap_or_else(|e| panic!("invalid gasless token type {s:?}: {e}"));
        map.insert(exec_types::type_tags::type_tag_in(bump, &tag), *min_amount);
    }
    map
}
