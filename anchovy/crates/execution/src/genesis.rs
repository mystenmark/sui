// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A minimal genesis, for one node: the framework packages, gas coins for
//! the given addresses, and the system objects execution needs, made by
//! their framework `create` functions as the real genesis makes them. No
//! system state, validators or staking.

use move_core_types::ident_str;
use sui_types::SUI_FRAMEWORK_ADDRESS;
use sui_types::base_types::{ObjectID, SequenceNumber, SuiAddress};
use sui_types::digests::TransactionDigest;
use sui_types::gas_coin::GasCoin;
use sui_types::in_memory_storage::InMemoryStorage;
use sui_types::object::{MoveObject, Object, Owner};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::CheckedInputObjects;

use crate::{Error, Execution, Result};

/// Every genesis object, in no particular order.
pub fn objects(execution: &Execution, allocations: &[(SuiAddress, u64)]) -> Result<Vec<Object>> {
    let genesis = TransactionDigest::genesis_marker();
    let mut objects: Vec<Object> = sui_framework::BuiltInFramework::genesis_objects().collect();
    objects.extend(system_objects(execution, objects.clone())?);
    for (i, (owner, amount)) in allocations.iter().enumerate() {
        // Derived, not random: the same allocations make the same genesis.
        let id = ObjectID::derive_id(genesis, i as u64);
        let coin = MoveObject::new_gas_coin(SequenceNumber::from_u64(1), id, *amount);
        objects.push(Object::new_move(coin, Owner::AddressOwner(*owner), genesis));
    }
    Ok(objects)
}

/// The system objects execution reads, at the epoch's protocol version.
fn system_objects(execution: &Execution, packages: Vec<Object>) -> Result<Vec<Object>> {
    let config = execution.protocol_config();
    let mut builder = ProgrammableTransactionBuilder::new();
    let create = |builder: &mut ProgrammableTransactionBuilder, module| {
        builder
            .move_call(
                SUI_FRAMEWORK_ADDRESS.into(),
                module,
                ident_str!("create").to_owned(),
                vec![],
                vec![],
            )
            .map_err(|e| Error::Decode(bcs::Error::Custom(e.to_string())))
    };
    create(&mut builder, ident_str!("clock").to_owned())?;
    if config.create_root_accumulator_object() {
        create(&mut builder, ident_str!("accumulator").to_owned())?;
    }
    let store = InMemoryStorage::new(packages);
    let output = execution
        .executor
        .update_genesis_state(
            &store,
            config,
            execution.metrics.clone(),
            execution.epoch,
            execution.epoch_start_timestamp_ms,
            &TransactionDigest::genesis_marker(),
            CheckedInputObjects::new_for_genesis(vec![]),
            builder.finish(),
        )
        .map_err(|e| Error::Sui(e.into()))?;
    Ok(output.written.into_values().collect())
}

/// Writes genesis, with gas coins for `allocations` (address, amount), into
/// `store` unless it has one; whether it wrote it.
pub fn init(
    execution: &Execution,
    store: &store::Store,
    allocations: &[([u8; 32], u64)],
) -> Result<bool> {
    if store.has_genesis()? {
        return Ok(false);
    }
    let allocations: Vec<(SuiAddress, u64)> = allocations
        .iter()
        .map(|(address, amount)| (SuiAddress::from_bytes(address).expect("32 bytes"), *amount))
        .collect();
    commit(store, &objects(execution, &allocations)?)?;
    Ok(true)
}

/// Writes genesis into an empty store.
pub fn commit(store: &store::Store, objects: &[Object]) -> Result<()> {
    let written = objects
        .iter()
        .map(crate::written)
        .collect::<Result<Vec<_>>>()?;
    Ok(store.commit_genesis(written)?)
}

/// The balance of a gas coin, for tests and tools.
pub fn balance(object: &Object) -> Option<u64> {
    GasCoin::try_from(object).ok().map(|c| c.value())
}
