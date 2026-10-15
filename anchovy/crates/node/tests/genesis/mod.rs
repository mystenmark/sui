// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A genesis as sui's genesis builder makes it: the framework, the system
//! state with one validator and its staking pool, and gas coins for a test
//! account.

use fastcrypto::traits::KeyPair as _;
use rand::SeedableRng;
use rand::rngs::StdRng;
use sui_config::genesis::{
    GenesisCeremonyParameters, TokenAllocation, TokenDistributionScheduleBuilder,
};
use sui_genesis_builder::Builder;
use sui_genesis_builder::validator_info::ValidatorInfo;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::crypto::{
    AccountKeyPair, AuthorityKeyPair, NetworkKeyPair, generate_proof_of_possession,
    get_key_pair_from_rng,
};
use sui_types::object::{Object, Owner};
use sui_types::supported_protocol_versions::ProtocolVersion;
use validator::pipeline::tables::BuiltCheckpoint;

pub struct Genesis {
    pub objects: Vec<Object>,
    pub checkpoint: BuiltCheckpoint,
    /// The genesis checkpoint's digest.
    pub chain_identifier: [u8; 32],
    /// The test account's gas coins, by id.
    #[allow(dead_code, reason = "not every test crate spends them")]
    pub coins: Vec<ObjectID>,
}

/// The validator's protocol key pair, the same for every genesis.
pub fn protocol_key_pair() -> AuthorityKeyPair {
    get_key_pair_from_rng(&mut StdRng::from_seed([7; 32])).1
}

/// A genesis whose one validator signs with `protocol_key_pair`, whose first
/// epoch lasts `epoch_duration_ms`, and giving `account` `coins` gas coins of
/// `amount` MIST.
pub fn build(account: SuiAddress, coins: usize, amount: u64, epoch_duration_ms: u64) -> Genesis {
    let mut rng = StdRng::from_seed([8; 32]);
    let protocol = protocol_key_pair();
    let validator_account: AccountKeyPair = get_key_pair_from_rng(&mut rng).1;
    let network: NetworkKeyPair = get_key_pair_from_rng(&mut rng).1;
    let worker: NetworkKeyPair = get_key_pair_from_rng(&mut rng).1;
    let validator_address = SuiAddress::from(validator_account.public());
    let tcp = |port: u16| format!("/ip4/127.0.0.1/tcp/{port}/http").parse().unwrap();
    let udp = |port: u16| format!("/ip4/127.0.0.1/udp/{port}").parse().unwrap();
    let info = ValidatorInfo {
        name: "anchovy".to_owned(),
        account_address: validator_address,
        protocol_key: protocol.public().into(),
        worker_key: worker.public().clone(),
        network_key: network.public().clone(),
        gas_price: 1000,
        commission_rate: 0,
        network_address: tcp(8080),
        p2p_address: udp(8084),
        narwhal_primary_address: udp(8081),
        narwhal_worker_address: udp(8082),
        description: String::new(),
        image_url: String::new(),
        project_url: String::new(),
    };
    let proof_of_possession = generate_proof_of_possession(&protocol, validator_address);

    let mut tokens = TokenDistributionScheduleBuilder::new();
    tokens.default_allocation_for_validators([validator_address]);
    for _ in 0..coins {
        tokens.add_allocation(TokenAllocation {
            recipient_address: account,
            amount_mist: amount,
            staked_with_validator: None,
        });
    }
    let genesis = Builder::new()
        .with_parameters(GenesisCeremonyParameters {
            chain_start_timestamp_ms: 0,
            protocol_version: ProtocolVersion::MAX,
            epoch_duration_ms,
            ..GenesisCeremonyParameters::new()
        })
        .with_token_distribution_schedule(tokens.build())
        .add_validator(info, proof_of_possession)
        .add_validator_signature(&protocol)
        .build();

    let summary = genesis.checkpoint();
    let gas = &summary.epoch_rolling_gas_cost_summary;
    let checkpoint = BuiltCheckpoint {
        seq: 0,
        summary: bcs::to_bytes(summary.data()).unwrap(),
        digest: summary.digest().into_inner(),
        contents: bcs::to_bytes(genesis.checkpoint_contents()).unwrap(),
        network_total_transactions: summary.network_total_transactions,
        epoch_rolling_gas: [
            gas.computation_cost,
            gas.storage_cost,
            gas.storage_rebate,
            gas.non_refundable_storage_fee,
        ],
        last_height: 0,
        timestamp_ms: summary.timestamp_ms,
        root_state: None,
    };
    let mut coins: Vec<ObjectID> = genesis
        .objects()
        .iter()
        .filter(|o| o.owner == Owner::AddressOwner(account) && o.is_gas_coin())
        .map(|o| o.id())
        .collect();
    coins.sort();
    Genesis {
        objects: genesis.objects().to_vec(),
        chain_identifier: checkpoint.digest,
        checkpoint,
        coins,
    }
}
