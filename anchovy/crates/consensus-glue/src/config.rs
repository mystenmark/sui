// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

use consensus_config::{ChainType, ConsensusProtocolConfig, Parameters};
use mysten_network::Multiaddr;
use sui_protocol_config::{Chain, ProtocolConfig};

/// As sui's `to_consensus_protocol_config`.
pub(crate) fn protocol_config(config: &ProtocolConfig) -> ConsensusProtocolConfig {
    let chain = match config.chain() {
        Chain::Mainnet => ChainType::Mainnet,
        Chain::Testnet => ChainType::Testnet,
        Chain::Unknown => ChainType::Unknown,
    };
    ConsensusProtocolConfig::new(
        config.version.as_u64(),
        chain,
        config.max_transaction_size_bytes(),
        config.max_transactions_in_block_bytes(),
        config.max_num_transactions_in_block(),
        config.gc_depth(),
        config.consensus_slim_block_propagation(),
        /* transaction_voting_enabled */ true,
        config.mysticeti_num_leaders_per_round(),
        config.consensus_bad_nodes_stake_threshold(),
        /* enable_v3 */ false,
        /* leader_schedule_window_size */ 300,
        /* leader_schedule_update_interval */ 12,
    )
}

/// The defaults, with the epoch's database and, if given, the address to
/// listen on instead of the committee's.
pub(crate) fn parameters(db_path: PathBuf, listen_address: Option<Multiaddr>) -> Parameters {
    Parameters {
        db_path,
        listen_address_override: listen_address,
        ..Parameters::default()
    }
}
