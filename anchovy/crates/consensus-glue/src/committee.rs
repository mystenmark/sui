// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The epoch's committee and the validator's keys, as consensus-core takes
//! them.

use consensus_config::{
    Authority, AuthorityName, Committee, NetworkKeyPair, NetworkPublicKey, ProtocolKeyPair,
    ProtocolPublicKey,
};
use execution::system_state::Member;
use fastcrypto::ed25519::{Ed25519KeyPair, Ed25519PrivateKey, Ed25519PublicKey};
use fastcrypto::traits::ToFromBytes;

/// The members are sorted by protocol key, as sui sorts them, so an
/// authority's index is its index in the checkpoint committee.
pub(crate) fn committee(epoch: u64, members: &[Member]) -> Committee {
    Committee::new(epoch, members.iter().map(authority).collect())
}

/// # Panics
/// If the member's keys or address are malformed: they were read from the
/// system state, which sui validated.
fn authority(member: &Member) -> Authority {
    let public = |bytes: &[u8; 32]| {
        Ed25519PublicKey::from_bytes(bytes).expect("a member's key is an Ed25519 key")
    };
    Authority {
        stake: member.stake,
        address: member
            .address
            .parse()
            .expect("a member's address is a multiaddr"),
        hostname: member.hostname.clone(),
        authority_name: AuthorityName::new(member.name.to_vec()),
        protocol_key: ProtocolPublicKey::new(public(&member.consensus_key)),
        network_key: NetworkPublicKey::new(public(&member.network_key)),
    }
}

pub(crate) fn protocol_key_pair(secret: &[u8; 32]) -> ProtocolKeyPair {
    ProtocolKeyPair::new(key_pair(secret))
}

pub(crate) fn network_key_pair(secret: &[u8; 32]) -> NetworkKeyPair {
    NetworkKeyPair::new(key_pair(secret))
}

fn key_pair(secret: &[u8; 32]) -> Ed25519KeyPair {
    Ed25519PrivateKey::from_bytes(secret)
        .expect("any 32 bytes are an Ed25519 secret")
        .into()
}
