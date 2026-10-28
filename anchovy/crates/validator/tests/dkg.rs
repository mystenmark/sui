// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The DKG observer against real parties: it merges, completes, and finds
//! the parties' group key at the commits the reference would.

use std::sync::Arc;

use fastcrypto::bls12381::min_sig::BLS12381KeyPair;
use fastcrypto::groups::bls12381::{G2Element, Scalar};
use fastcrypto::serde_helpers::ToFromByteArray;
use fastcrypto::traits::{KeyPair as _, ToFromBytes};
use fastcrypto_tbls::dkg_v1::Party;
use fastcrypto_tbls::ecies_v1;
use rand::SeedableRng;
use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use sui_types::messages_consensus::{VersionedDkgConfirmation, VersionedDkgMessage};
use validator::consensus::dkg::{
    Dkg, DkgChanges, DkgPayload, DkgSetup, DkgStatus, DkgTraffic, decode_status, encode_status,
};

struct Network {
    config: ProtocolConfig,
    setup: Arc<DkgSetup>,
    parties: Vec<Party<G2Element, G2Element>>,
}

fn network(stakes: &[u64]) -> Network {
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    let keys: Vec<BLS12381KeyPair> = stakes
        .iter()
        .map(|_| BLS12381KeyPair::generate(&mut rng))
        .collect();
    let mut committee: Vec<([u8; 96], u64)> = keys
        .iter()
        .zip(stakes)
        .map(|(key, stake)| (key.public().as_bytes().try_into().unwrap(), *stake))
        .collect();
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_by_key(|&i| committee[i].0);
    committee.sort_unstable();
    let config = ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown);
    let setup = Arc::new(DkgSetup::new(&committee, &config, &[3; 32], 1));
    let parties = order
        .iter()
        .map(|&i| {
            let secret =
                Scalar::from_byte_array(keys[i].copy().private().as_bytes().try_into().unwrap())
                    .unwrap();
            Party::new(
                ecies_v1::PrivateKey::<G2Element>::from(secret),
                setup.nodes().clone(),
                setup.threshold(),
                setup.random_oracle().clone(),
                &mut rng,
            )
            .unwrap()
        })
        .collect();
    Network {
        config,
        setup,
        parties,
    }
}

fn message(network: &Network, author: u32, bytes: &[u8]) -> DkgTraffic {
    DkgTraffic {
        author,
        payload: DkgPayload::Message(network.setup.message(author, bytes).map(Box::new)),
    }
}

fn confirmation(network: &Network, author: u32, bytes: &[u8]) -> DkgTraffic {
    DkgTraffic {
        author,
        payload: DkgPayload::Confirmation(network.setup.confirmation(author, bytes)),
    }
}

/// Each party's message, encoded as consensus carries it.
fn messages(network: &Network) -> Vec<Vec<u8>> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(8);
    network
        .parties
        .iter()
        .map(|p| {
            bcs::to_bytes(&VersionedDkgMessage::V1(
                p.create_message(&mut rng).unwrap(),
            ))
            .unwrap()
        })
        .collect()
}

/// Each party's confirmation and the group key, merging the messages of
/// `dealers`.
fn confirm(network: &Network, encoded: &[Vec<u8>], dealers: &[usize]) -> (Vec<Vec<u8>>, DkgStatus) {
    let mut rng = rand::rngs::StdRng::seed_from_u64(9);
    let mut confirmations = Vec::new();
    let mut used_by = Vec::new();
    for party in &network.parties {
        let processed: Vec<_> = dealers
            .iter()
            .map(|&d| {
                let VersionedDkgMessage::V1(m) = bcs::from_bytes(&encoded[d]).unwrap() else {
                    unreachable!()
                };
                party.process_message(m, &mut rng).unwrap()
            })
            .collect();
        let (confirmation, used) = party.merge(&processed).unwrap();
        confirmations.push(bcs::to_bytes(&VersionedDkgConfirmation::V1(confirmation)).unwrap());
        used_by.push(used);
    }
    let decoded: Vec<_> = confirmations
        .iter()
        .map(|c| {
            let VersionedDkgConfirmation::V1(c) = bcs::from_bytes(c).unwrap() else {
                unreachable!()
            };
            c
        })
        .collect();
    let output = network.parties[0]
        .complete(&used_by[0], &decoded, &mut rng)
        .unwrap();
    (confirmations, DkgStatus::Successful(output.vss_pk))
}

#[test]
fn the_observer_completes_with_the_parties_key() {
    let network = network(&[2500, 2500, 2500, 2500]);
    let encoded = messages(&network);
    let mut dkg = Dkg::new(network.setup.clone());
    // Reduced, each member weighs 125 of 500, and t' is 167: two messages
    // merge, and 2t' − 1 takes three confirmations.
    let changes = dkg.handle(&network.config, vec![message(&network, 0, &encoded[0])], 1);
    assert!(changes.used.is_none());
    let changes = dkg.handle(
        &network.config,
        vec![
            message(&network, 1, &encoded[1]),
            message(&network, 2, &encoded[2]),
        ],
        2,
    );
    assert_eq!(changes.used, Some(vec![0, 1, 2]));
    // A message after the merge does not count.
    let changes = dkg.handle(&network.config, vec![message(&network, 3, &encoded[3])], 3);
    assert_eq!(changes.messages, vec![(3, None)]);

    let (confirmations, key) = confirm(&network, &encoded, &[0, 1, 2]);
    let all: Vec<_> = (0..4)
        .map(|i| confirmation(&network, i, &confirmations[i as usize]))
        .collect();
    let mut all = all.into_iter();
    let changes = dkg.handle(&network.config, all.by_ref().take(2).collect(), 4);
    assert!(changes.status.is_none());
    let changes = dkg.handle(&network.config, all.collect(), 5);
    assert_eq!(changes.status, Some(key.clone()));
    assert_eq!(dkg.status(), &key);
    assert_eq!(decode_status(&encode_status(&key)), key);
}

#[test]
fn the_first_message_of_an_author_wins_even_if_invalid() {
    let network = network(&[2500, 2500, 2500, 2500]);
    let encoded = messages(&network);
    let mut dkg = Dkg::new(network.setup.clone());
    let changes = dkg.handle(
        &network.config,
        vec![
            message(&network, 0, b"garbage"),
            message(&network, 0, &encoded[0]),
            // Another author's message, claiming author 0's party.
            message(&network, 1, &encoded[0]),
        ],
        1,
    );
    assert_eq!(changes.messages, vec![(0, None), (1, None)]);
}

#[test]
fn dkg_fails_past_the_timeout_round() {
    let network = network(&[2500, 2500, 2500, 2500]);
    let mut dkg = Dkg::new(network.setup.clone());
    let timeout = u64::from(network.config.random_beacon_dkg_timeout_round());
    assert!(
        dkg.handle(&network.config, vec![], timeout)
            .status
            .is_none()
    );
    let changes = dkg.handle(&network.config, vec![], timeout + 1);
    assert_eq!(changes.status, Some(DkgStatus::Failed));
}

#[test]
fn a_restored_observer_completes_as_the_original() {
    let network = network(&[2500, 2500, 2500, 2500]);
    let encoded = messages(&network);
    let mut original = Dkg::new(network.setup.clone());
    let traffic = |range: std::ops::Range<u32>| -> Vec<_> {
        range
            .map(|i| message(&network, i, &encoded[i as usize]))
            .collect()
    };
    let written: Vec<DkgChanges> = vec![original.handle(&network.config, traffic(0..3), 1)];
    let mut restored = Dkg::new(network.setup.clone());
    restored.restore(written);
    let (confirmations, key) = confirm(&network, &encoded, &[0, 1, 2]);
    let confirm_all = || -> Vec<_> {
        (0..4)
            .map(|i| confirmation(&network, i, &confirmations[i as usize]))
            .collect()
    };
    assert_eq!(
        restored.handle(&network.config, confirm_all(), 2).status,
        Some(key.clone())
    );
    assert_eq!(
        original.handle(&network.config, confirm_all(), 2).status,
        Some(key)
    );
}
