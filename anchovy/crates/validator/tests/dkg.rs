// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The DKG observer against real parties: it merges, completes, and finds
//! the parties' group key at the commits the reference would; and the
//! signature collector with a party's shares.

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

/// Each party's DKG output, its shares included, merging the messages of
/// `dealers` and completing with every party's confirmation.
fn outputs(
    network: &Network,
    encoded: &[Vec<u8>],
    dealers: &[usize],
) -> Vec<fastcrypto_tbls::dkg_v1::Output<G2Element, G2Element>> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(10);
    let merged: Vec<_> = network
        .parties
        .iter()
        .map(|party| {
            let processed: Vec<_> = dealers
                .iter()
                .map(|&d| {
                    let VersionedDkgMessage::V1(m) = bcs::from_bytes(&encoded[d]).unwrap() else {
                        unreachable!()
                    };
                    party.process_message(m, &mut rng).unwrap()
                })
                .collect();
            party.merge(&processed).unwrap()
        })
        .collect();
    let confirmations: Vec<_> = merged.iter().map(|(c, _)| c.clone()).collect();
    network
        .parties
        .iter()
        .zip(&merged)
        .map(|(party, (_, used))| party.complete(used, &confirmations, &mut rng).unwrap())
        .collect()
}

/// The signature collector signs a reserved round with this member's
/// shares, aggregates them with a peer's partial signatures into the
/// group's signature, and then pushes that instead of its partials.
#[test]
fn the_collector_signs_and_aggregates_with_its_shares() {
    use fastcrypto_tbls::tbls::ThresholdBls;
    use fastcrypto_tbls::types::ThresholdBls12381MinSig;
    use validator::pipeline::items::GateInput;
    use validator::randomness::{
        BeaconInput, SendSignaturesRequest, SignatureCollector, signature_message,
    };
    use workqueue::Processor;

    let network = network(&[2500, 2500, 2500, 2500]);
    let encoded = messages(&network);
    let outputs = outputs(&network, &encoded, &[0, 1, 2]);
    let key = outputs[0].vss_pk.clone();
    let dir = tempfile::tempdir().unwrap();
    let (store, _, _) = validator::epoch_store::EpochStore::open(dir.path(), 1);
    let (gate, gate_inbox) = workqueue::queue(16);
    let mut collector = SignatureCollector::new(1, network.setup.clone(), Some(0), store, gate);
    let mut bump = containers::Bump::default();
    let mut process = |collector: &mut SignatureCollector, input| {
        collector.process(input, &mut bump);
    };
    let outgoing = |collector: &mut SignatureCollector| {
        let (answer, receiver) = tokio::sync::oneshot::channel();
        collector.process(
            BeaconInput::Outgoing(answer),
            &mut containers::Bump::default(),
        );
        receiver.blocking_recv().unwrap()
    };

    process(&mut collector, BeaconInput::GroupKey(key.clone()));
    process(
        &mut collector,
        BeaconInput::Shares(outputs[0].shares.clone().unwrap()),
    );
    process(&mut collector, BeaconInput::Round(0));
    // Its own shares weigh 125 of the 167 needed.
    assert!(gate_inbox.try_pop().is_none());
    let pushed = outgoing(&mut collector);
    assert_eq!(pushed.len(), 1);
    assert_eq!((pushed[0].round, pushed[0].sig), (0, None));
    assert_eq!(pushed[0].partial_sigs.len(), 125);

    let message = signature_message(0);
    let peer = ThresholdBls12381MinSig::partial_sign_batch(
        outputs[1].shares.as_ref().unwrap().iter(),
        &message,
    );
    let (taken, _) = tokio::sync::oneshot::channel();
    process(
        &mut collector,
        BeaconInput::Signatures {
            party: 1,
            request: SendSignaturesRequest {
                epoch: 1,
                round: 0,
                partial_sigs: peer.iter().map(|p| bcs::to_bytes(p).unwrap()).collect(),
                sig: None,
            },
            taken,
        },
    );
    let Some(GateInput::Bytes { round: 0, bytes }) = gate_inbox.try_pop() else {
        panic!("round 0's bytes");
    };
    let sig: fastcrypto::groups::bls12381::G1Element = bcs::from_bytes(&bytes).unwrap();
    ThresholdBls12381MinSig::verify(&key.c0(), &message, &sig).unwrap();
    let pushed = outgoing(&mut collector);
    assert_eq!(pushed[0].sig, Some(sig));
    assert!(pushed[0].partial_sigs.is_empty());
}
