// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A genesis as sui's genesis builder makes it: the framework, the system
//! state with its validators and their staking pools, and gas coins for a test
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
    #[allow(dead_code, reason = "not every test crate runs the pipeline")]
    pub checkpoint: BuiltCheckpoint,
    /// The genesis checkpoint's digest.
    #[allow(dead_code, reason = "not every test crate runs the pipeline")]
    pub chain_identifier: [u8; 32],
    /// The test account's gas coins, by id.
    #[allow(dead_code, reason = "not every test crate spends them")]
    pub coins: Vec<ObjectID>,
}

/// Validator 0's protocol key pair, the same for every genesis.
#[allow(dead_code, reason = "not every test crate signs as the validator")]
pub fn protocol_key_pair() -> AuthorityKeyPair {
    protocol_key_pair_of(0)
}

/// Validator 0's keys, as the node holds them.
#[allow(dead_code, reason = "not every test crate runs a node")]
pub fn validator_keys() -> node::epoch::ValidatorKeys {
    validator_keys_of(0)
}

/// Each validator's keys come from seeds of their own, validator 0's
/// those of a genesis of one.
fn seed(validator: u8, key: u8) -> StdRng {
    StdRng::from_seed([validator * 16 + key; 32])
}

pub fn protocol_key_pair_of(validator: u8) -> AuthorityKeyPair {
    get_key_pair_from_rng(&mut seed(validator, 7)).1
}

/// Consensus's key for the validator's blocks.
fn worker_key_pair_of(validator: u8) -> NetworkKeyPair {
    get_key_pair_from_rng(&mut seed(validator, 9)).1
}

fn network_key_pair_of(validator: u8) -> NetworkKeyPair {
    get_key_pair_from_rng(&mut seed(validator, 10)).1
}

#[allow(dead_code, reason = "not every test crate runs a node")]
pub fn validator_keys_of(validator: u8) -> node::epoch::ValidatorKeys {
    use fastcrypto::traits::ToFromBytes;
    let secret =
        |pair: NetworkKeyPair| -> [u8; 32] { pair.private().as_bytes().try_into().unwrap() };
    node::epoch::ValidatorKeys {
        protocol: std::sync::Arc::new(checkpoint::ProtocolKey::new(protocol_key_pair_of(
            validator,
        ))),
        worker: secret(worker_key_pair_of(validator)),
        network: validator::tls::NetworkKey::from_bytes(secret(network_key_pair_of(validator))),
    }
}

/// A genesis whose one validator signs with `protocol_key_pair`, whose first
/// epoch lasts `epoch_duration_ms`, and giving `account` `coins` gas coins of
/// `amount` MIST.
#[allow(dead_code, reason = "not every test crate has one validator")]
pub fn build(account: SuiAddress, coins: usize, amount: u64, epoch_duration_ms: u64) -> Genesis {
    build_with(&[8081], account, coins, amount, epoch_duration_ms)
}

/// A genesis of a validator for each of `ports`, of equal stake, validator
/// `i` with the keys of `validator_keys_of(i)` and its addresses, consensus's
/// among them, at port `ports[i]` of localhost: validators' addresses differ.
pub fn build_with(
    ports: &[u16],
    account: SuiAddress,
    coins: usize,
    amount: u64,
    epoch_duration_ms: u64,
) -> Genesis {
    let mut rng = StdRng::from_seed([8; 32]);
    let tcp = |port: u16| format!("/ip4/127.0.0.1/tcp/{port}/http").parse().unwrap();
    let udp = |port: u16| format!("/ip4/127.0.0.1/udp/{port}").parse().unwrap();
    let mut validators = Vec::new();
    for (i, port) in (0..).zip(ports) {
        let protocol = protocol_key_pair_of(i);
        let validator_account: AccountKeyPair = get_key_pair_from_rng(&mut rng).1;
        let (network, worker) = (network_key_pair_of(i), worker_key_pair_of(i));
        let validator_address = SuiAddress::from(validator_account.public());
        let info = ValidatorInfo {
            name: format!("anchovy-{i}"),
            account_address: validator_address,
            protocol_key: protocol.public().into(),
            worker_key: worker.public().clone(),
            network_key: network.public().clone(),
            gas_price: 1000,
            commission_rate: 0,
            network_address: tcp(*port),
            p2p_address: udp(*port),
            narwhal_primary_address: udp(*port),
            narwhal_worker_address: udp(*port),
            description: String::new(),
            image_url: String::new(),
            project_url: String::new(),
        };
        let proof_of_possession = generate_proof_of_possession(&protocol, validator_address);
        validators.push((info, proof_of_possession, protocol));
    }

    let mut tokens = TokenDistributionScheduleBuilder::new();
    tokens.default_allocation_for_validators(
        validators.iter().map(|(info, ..)| info.account_address),
    );
    for _ in 0..coins {
        tokens.add_allocation(TokenAllocation {
            recipient_address: account,
            amount_mist: amount,
            staked_with_validator: None,
        });
    }
    let mut builder = Builder::new()
        .with_parameters(GenesisCeremonyParameters {
            chain_start_timestamp_ms: 0,
            protocol_version: ProtocolVersion::MAX,
            epoch_duration_ms,
            ..GenesisCeremonyParameters::new()
        })
        .with_token_distribution_schedule(tokens.build());
    for (info, proof_of_possession, _) in &validators {
        builder = builder.add_validator(info.clone(), proof_of_possession.clone());
    }
    for (_, _, protocol) in &validators {
        builder = builder.add_validator_signature(protocol);
    }
    from_sui(&builder.build(), account)
}

/// Anchovy's genesis from sui's, `account`'s gas coins by id.
pub fn from_sui(genesis: &sui_config::genesis::Genesis, account: SuiAddress) -> Genesis {
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
