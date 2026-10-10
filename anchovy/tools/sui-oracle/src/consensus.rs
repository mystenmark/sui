// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `ConsensusTransaction` decoding vectors: a value of every kind built
//! with sui-types, then malformed copies (truncations, bad and
//! non-canonical tags and lengths, invalid bools and option tags, trailing
//! bytes, random byte changes), and nesting at the container depth limit.
//! One per line, gzipped, with whether sui-types decodes it:
//!
//! ```text
//! <label> <ok|error> <hex>
//! ```
//!
//! Labels starting `semantic_` are rejected by sui-types for something
//! other than the encoding (a BLS point, a roaring bitmap, an empty
//! `NonEmpty`, a duration that overflows).
//!
//! The generator is seeded, so the file is the same on every run.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Duration;

use fastcrypto::traits::KeyPair as _;
use fastcrypto_zkp::bn254::zk_login::{JWK, JwkId};
use nonempty::NonEmpty;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use sui_protocol_config::Chain;
use sui_types::base_types::{AuthorityName, ObjectDigest, ObjectID, SequenceNumber, SuiAddress};
use sui_types::committee::Committee;
use sui_types::crypto::{AuthorityKeyPair, SuiKeyPair, get_key_pair_from_rng};
use sui_types::digests::{
    CheckpointArtifactsDigest, CheckpointContentsDigest, CheckpointDigest, Digest,
};
use sui_types::execution::ExecutionTimeObservationKey;
use sui_types::gas::GasCostSummary;
use sui_types::messages_checkpoint::{
    CheckpointCommitment, CheckpointSignatureMessage, CheckpointSummary, ECMHLiveObjectSetDigest,
    EndOfEpochData, SignedCheckpointSummary,
};
use sui_types::messages_consensus::{
    AuthorityCapabilitiesV1, AuthorityCapabilitiesV2, ConsensusTransaction,
    ConsensusTransactionKind, ExecutionTimeObservation, SharedTransactionDenyConfig,
    SharedTransactionDenyConfigV1,
};
use sui_types::supported_protocol_versions::{
    ProtocolVersion, SupportedProtocolVersions, SupportedProtocolVersionsWithHashes,
};
use sui_types::transaction::{
    CertifiedTransaction, PlainTransactionWithClaims, Transaction, TransactionClaim,
    TransactionData,
};
use sui_types::transaction_deny_rules::TransactionDenyRules;
use sui_types::type_input::{StructInput, TypeInput};

use crate::validity::Spec;
use crate::validity_signed::signed;

const EPOCH: u64 = 7;

fn verdict(bytes: &[u8]) -> &'static str {
    if bcs::from_bytes::<ConsensusTransaction>(bytes).is_ok() {
        "ok"
    } else {
        "error"
    }
}

/// The single-byte uleb128 at `at`, rewritten in two bytes.
fn overlong(bytes: &[u8], at: usize) -> Vec<u8> {
    assert!(bytes[at] < 0x80);
    let mut out = bytes[..at].to_vec();
    out.extend([bytes[at] | 0x80, 0]);
    out.extend(&bytes[at + 1..]);
    out
}

fn set(bytes: &[u8], at: usize, value: u8) -> Vec<u8> {
    let mut out = bytes.to_vec();
    out[at] = value;
    out
}

/// Replaces the one occurrence of `from`.
fn patch(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let at = bytes
        .windows(from.len())
        .position(|w| w == from)
        .expect("pattern present");
    assert!(
        bytes[at + 1..].windows(from.len()).all(|w| w != from),
        "pattern unique"
    );
    let mut out = bytes.to_vec();
    out.splice(at..at + from.len(), to.iter().copied());
    out
}

struct Fixture {
    committee: Committee,
    authorities: Vec<AuthorityKeyPair>,
    sender: SuiKeyPair,
}

impl Fixture {
    fn new() -> Fixture {
        let (committee, authorities) = Committee::new_simple_test_committee_of_size(4);
        let mut rng = StdRng::from_seed([31; 32]);
        Fixture {
            committee,
            authorities,
            sender: SuiKeyPair::Ed25519(get_key_pair_from_rng(&mut rng).1),
        }
    }

    fn name(&self, i: usize) -> AuthorityName {
        self.authorities[i].public().into()
    }

    fn data(&self, budget: u64) -> TransactionData {
        Spec {
            sender: SuiAddress::from(&self.sender.public()),
            owner: SuiAddress::from(&self.sender.public()),
            budget,
            ..Spec::new()
        }
        .build()
    }

    fn transaction(&self, budget: u64) -> Transaction {
        Transaction::from_data_and_signer(self.data(budget), vec![&self.sender])
    }

    fn certificate(&self, budget: u64) -> CertifiedTransaction {
        CertifiedTransaction::new_from_keypairs_for_testing(
            self.transaction(budget).into_data(),
            &self.authorities[..3],
            &self.committee,
        )
    }

    fn signed_checkpoint(&self, end_of_epoch: bool) -> CheckpointSignatureMessage {
        let summary = CheckpointSummary {
            epoch: EPOCH,
            sequence_number: 1234,
            network_total_transactions: 99_999,
            content_digest: CheckpointContentsDigest::new([3; 32]),
            previous_digest: (!end_of_epoch).then(|| CheckpointDigest::new([4; 32])),
            epoch_rolling_gas_cost_summary: GasCostSummary::new(1, 2, 3, 4),
            timestamp_ms: 1_700_000_000_000,
            checkpoint_commitments: if end_of_epoch {
                vec![
                    CheckpointCommitment::ECMHLiveObjectSetDigest(ECMHLiveObjectSetDigest {
                        digest: Digest::new([5; 32]),
                    }),
                    CheckpointCommitment::CheckpointArtifactsDigest(
                        CheckpointArtifactsDigest::new([6; 32]),
                    ),
                ]
            } else {
                vec![]
            },
            end_of_epoch_data: end_of_epoch.then(|| EndOfEpochData {
                next_epoch_committee: (0..4).map(|i| (self.name(i), 2500)).collect(),
                next_epoch_protocol_version: ProtocolVersion::new(90),
                epoch_commitments: vec![],
            }),
            version_specific_data: if end_of_epoch { vec![] } else { vec![0, 1, 2] },
        };
        CheckpointSignatureMessage {
            summary: SignedCheckpointSummary::new(
                EPOCH,
                summary,
                &self.authorities[1],
                self.name(1),
            ),
        }
    }
}

fn tx(kind: ConsensusTransactionKind) -> Vec<u8> {
    bcs::to_bytes(&ConsensusTransaction {
        tracking_id: [1, 2, 3, 4, 5, 6, 7, 8],
        kind,
    })
    .unwrap()
}

fn object_ref(i: u8) -> sui_types::base_types::ObjectRef {
    (
        ObjectID::new([i; 32]),
        SequenceNumber::from_u64(u64::from(i)),
        ObjectDigest::new([i ^ 0xff; 32]),
    )
}

fn deny_rules() -> TransactionDenyRules {
    TransactionDenyRules {
        object_deny_list: BTreeSet::from([ObjectID::new([0x11; 32]), ObjectID::new([0x22; 32])]),
        package_deny_list: BTreeSet::from([ObjectID::new([0x33; 32])]),
        address_deny_list: BTreeSet::from([SuiAddress::from(ObjectID::new([0x44; 32]))]),
        package_publish_disabled: true,
        package_upgrade_disabled: false,
        shared_object_disabled: true,
        user_transaction_disabled: false,
        gasless_disabled: true,
        receiving_objects_disabled: false,
        zklogin_sig_disabled: true,
        zklogin_disabled_providers: BTreeSet::from(["Google".to_string(), "Ünïcode".to_string()]),
    }
}

fn deny_config(authority: AuthorityName, rules: Option<TransactionDenyRules>) -> Vec<u8> {
    tx(ConsensusTransactionKind::UpdateTransactionDenyConfig(
        Box::new(SharedTransactionDenyConfig::V1(
            SharedTransactionDenyConfigV1 {
                authority,
                generation: 1_700_000_000_123,
                rules,
            },
        )),
    ))
}

fn with_claims(f: &Fixture, claims: Vec<TransactionClaim>) -> Vec<u8> {
    tx(ConsensusTransactionKind::UserTransactionV2(Box::new(
        PlainTransactionWithClaims::new(f.transaction(10_000_000), claims),
    )))
}

fn observation(
    authority: AuthorityName,
    estimates: Vec<(ExecutionTimeObservationKey, Duration)>,
) -> Vec<u8> {
    tx(ConsensusTransactionKind::ExecutionTimeObservation(
        ExecutionTimeObservation::new(authority, 42, estimates),
    ))
}

/// Valid values of every kind, each with whether random byte changes may
/// be applied: not to those carrying BLS signatures, roaring bitmaps or
/// transaction signatures, which sui-types decodes and anchovy does not.
#[allow(deprecated)]
fn valid(f: &Fixture) -> Vec<(String, Vec<u8>, bool)> {
    use ConsensusTransactionKind as K;
    let mut out: Vec<(String, Vec<u8>, bool)> = vec![];
    let mut add =
        |label: &str, bytes: Vec<u8>, fuzz: bool| out.push((label.to_string(), bytes, fuzz));

    add(
        "certified",
        tx(K::CertifiedTransaction(Box::new(f.certificate(10_000_000)))),
        false,
    );
    add(
        "certified_2",
        tx(K::CertifiedTransaction(Box::new(f.certificate(12_345)))),
        false,
    );
    add(
        "checkpoint_signature",
        tx(K::CheckpointSignature(Box::new(f.signed_checkpoint(false)))),
        false,
    );
    add(
        "checkpoint_signature_v2",
        tx(K::CheckpointSignatureV2(Box::new(
            f.signed_checkpoint(false),
        ))),
        false,
    );
    add(
        "checkpoint_signature_v2_end_of_epoch",
        tx(K::CheckpointSignatureV2(Box::new(
            f.signed_checkpoint(true),
        ))),
        false,
    );
    add("end_of_publish", tx(K::EndOfPublish(f.name(0))), true);
    add(
        "capability_v1",
        tx(K::CapabilityNotification(AuthorityCapabilitiesV1 {
            authority: f.name(2),
            generation: 17,
            supported_protocol_versions: SupportedProtocolVersions::new_for_testing(1, 90),
            available_system_packages: vec![object_ref(1), object_ref(2), object_ref(3)],
        })),
        true,
    );
    add(
        "capability_v1_empty",
        tx(K::CapabilityNotification(AuthorityCapabilitiesV1 {
            authority: f.name(2),
            generation: 0,
            supported_protocol_versions: SupportedProtocolVersions::new_for_testing(5, 5),
            available_system_packages: vec![],
        })),
        true,
    );
    add(
        "jwk",
        tx(K::NewJWKFetched(
            f.name(3),
            JwkId {
                iss: "https://accounts.google.com".to_string(),
                kid: "kid-1".to_string(),
            },
            JWK {
                kty: "RSA".to_string(),
                e: "AQAB".to_string(),
                n: "näme".to_string(),
                alg: "RS256".to_string(),
            },
        )),
        true,
    );
    add(
        "randomness_state_update",
        tx(K::RandomnessStateUpdate(9, vec![1, 2, 3, 4])),
        true,
    );
    add(
        "dkg_message",
        tx(K::RandomnessDkgMessage(
            f.name(0),
            (0..200).map(|i| i as u8).collect(),
        )),
        true,
    );
    add(
        "dkg_message_empty",
        tx(K::RandomnessDkgMessage(f.name(0), vec![])),
        true,
    );
    add(
        "dkg_confirmation",
        tx(K::RandomnessDkgConfirmation(f.name(1), vec![7; 33])),
        true,
    );
    add(
        "capability_v2",
        tx(K::CapabilityNotificationV2(AuthorityCapabilitiesV2 {
            authority: f.name(1),
            generation: 1_700_000_000_000,
            supported_protocol_versions:
                SupportedProtocolVersionsWithHashes::from_supported_versions(
                    SupportedProtocolVersions::new_for_testing(80, 82),
                    Chain::Unknown,
                ),
            available_system_packages: vec![object_ref(9)],
        })),
        true,
    );
    add(
        "user_transaction",
        tx(K::UserTransaction(Box::new(f.transaction(10_000_000)))),
        false,
    );
    add(
        "execution_time_observation",
        observation(
            f.name(2),
            vec![
                (
                    ExecutionTimeObservationKey::MoveEntryPoint {
                        package: ObjectID::new([0xab; 32]),
                        module: "coin".to_string(),
                        function: "split".to_string(),
                        type_arguments: vec![
                            TypeInput::U64,
                            TypeInput::Vector(Box::new(TypeInput::Struct(Box::new(StructInput {
                                address: ObjectID::new([2; 32]).into(),
                                module: "sui".to_string(),
                                name: "SUI".to_string(),
                                type_params: vec![TypeInput::Bool],
                            })))),
                        ],
                    },
                    Duration::from_micros(1500),
                ),
                (
                    ExecutionTimeObservationKey::TransferObjects,
                    Duration::from_secs(3),
                ),
                (ExecutionTimeObservationKey::SplitCoins, Duration::ZERO),
                (
                    ExecutionTimeObservationKey::MergeCoins,
                    Duration::new(u64::MAX, 999_999_999),
                ),
                (
                    ExecutionTimeObservationKey::Publish,
                    Duration::from_nanos(1),
                ),
                (
                    ExecutionTimeObservationKey::MakeMoveVec,
                    Duration::from_millis(2),
                ),
                (
                    ExecutionTimeObservationKey::Upgrade,
                    Duration::from_millis(9),
                ),
            ],
        ),
        true,
    );
    add(
        "execution_time_observation_empty",
        observation(f.name(2), vec![]),
        true,
    );
    add(
        "checkpoint_signature_v2_b",
        tx(K::CheckpointSignatureV2(Box::new(
            f.signed_checkpoint(false),
        ))),
        false,
    );
    add(
        "user_transaction_v2_no_claims",
        with_claims(f, vec![]),
        false,
    );
    add(
        "user_transaction_v2_immutable",
        with_claims(
            f,
            vec![TransactionClaim::ImmutableInputObjects(vec![
                ObjectID::new([5; 32]),
                ObjectID::new([6; 32]),
            ])],
        ),
        false,
    );
    add(
        "user_transaction_v2_aliases_v2",
        with_claims(
            f,
            vec![TransactionClaim::AddressAliasesV2(
                NonEmpty::from_vec(vec![(0, Some(SequenceNumber::from_u64(3))), (1, None)])
                    .unwrap(),
            )],
        ),
        false,
    );
    add(
        "user_transaction_v2_aliases_v1",
        with_claims(
            f,
            vec![TransactionClaim::AddressAliases(NonEmpty::new((
                SuiAddress::from(ObjectID::new([8; 32])),
                None,
            )))],
        ),
        false,
    );
    add(
        "user_transaction_v2_all_claims",
        with_claims(
            f,
            vec![
                TransactionClaim::ImmutableInputObjects(vec![]),
                TransactionClaim::AddressAliasesV2(NonEmpty::new((
                    5,
                    Some(SequenceNumber::from_u64(u64::MAX)),
                ))),
                TransactionClaim::AddressAliases(NonEmpty::new((
                    SuiAddress::ZERO,
                    Some(SequenceNumber::from_u64(1)),
                ))),
            ],
        ),
        false,
    );
    add("deny_config_withdraw", deny_config(f.name(0), None), true);
    add(
        "deny_config_rules",
        deny_config(f.name(0), Some(deny_rules())),
        true,
    );
    add(
        "deny_config_default_rules",
        deny_config(f.name(3), Some(TransactionDenyRules::default())),
        true,
    );
    out
}

/// Malformed copies that apply to any value.
fn generic(label: &str, bytes: &[u8], fuzz: bool, rng: &mut StdRng) -> Vec<(String, Vec<u8>)> {
    let mut out = vec![];
    for cut in [0, 7, 8, 9, bytes.len() / 2, bytes.len() - 1] {
        out.push((format!("{label}_truncated_{cut}"), bytes[..cut].to_vec()));
    }
    let mut trailing = bytes.to_vec();
    trailing.push(0);
    out.push((format!("{label}_trailing"), trailing));
    out.push((format!("{label}_tag_overlong"), overlong(bytes, 8)));
    out.push((format!("{label}_tag_14"), set(bytes, 8, 14)));
    out.push((format!("{label}_tag_huge"), {
        let mut b = bytes[..8].to_vec();
        b.extend([0xff, 0xff, 0xff, 0xff, 0x0f]);
        b.extend(&bytes[9..]);
        b
    }));
    if fuzz {
        for i in 0..12 {
            let mut b = bytes.to_vec();
            let at = rng.gen_range(8..b.len());
            b[at] = match i % 3 {
                0 => rng.r#gen(),
                1 => b[at].wrapping_add(1),
                _ => 0x80,
            };
            out.push((format!("{label}_byte_{at}_{i}"), b));
        }
    }
    out
}

/// Malformed copies aimed at particular fields.
#[allow(deprecated)]
fn targeted(f: &Fixture) -> Vec<(String, Vec<u8>)> {
    use ConsensusTransactionKind as K;
    let mut out = vec![];
    let name_at = 9;

    let dkg = tx(K::RandomnessDkgMessage(f.name(0), vec![9; 5]));
    let len_at = name_at + 97;
    out.push(("dkg_length_overlong".to_string(), overlong(&dkg, len_at)));
    out.push(("dkg_length_long".to_string(), set(&dkg, len_at, 6)));
    out.push(("dkg_length_short".to_string(), set(&dkg, len_at, 4)));
    out.push((
        "authority_length_overlong".to_string(),
        overlong(&dkg, name_at),
    ));
    let mut short_name = dkg.clone();
    short_name[name_at] = 95;
    short_name.remove(name_at + 1);
    out.push(("authority_length_95".to_string(), short_name));

    let config = deny_config(f.name(0), Some(deny_rules()));
    let option_at = 10 + 97 + 8;
    out.push(("deny_option_2".to_string(), set(&config, option_at, 2)));
    let lists = bcs::to_bytes(&deny_rules().object_deny_list).unwrap().len()
        + bcs::to_bytes(&deny_rules().package_deny_list)
            .unwrap()
            .len()
        + bcs::to_bytes(&deny_rules().address_deny_list)
            .unwrap()
            .len();
    for k in 0..7 {
        out.push((
            format!("deny_bool_{k}_2"),
            set(&config, option_at + 1 + lists + k, 2),
        ));
    }
    out.push((
        "deny_set_unsorted".to_string(),
        patch(
            &patch(&config, &[0x11; 32], &[0x99; 32]),
            &[0x22; 32],
            &[0x11; 32],
        ),
    ));
    out.push((
        "deny_set_repeated".to_string(),
        patch(&config, &[0x22; 32], &[0x11; 32]),
    ));
    out.push((
        "deny_provider_bad_utf8".to_string(),
        patch(&config, b"Google", b"Goo\xffle"),
    ));
    let withdraw = deny_config(f.name(0), None);
    out.push(("deny_version_tag_1".to_string(), set(&withdraw, 9, 1)));

    let jwk = tx(K::NewJWKFetched(
        f.name(3),
        JwkId {
            iss: "iss".to_string(),
            kid: "kid".to_string(),
        },
        JWK {
            kty: "RSA".to_string(),
            e: "AQAB".to_string(),
            n: "abc".to_string(),
            alg: "RS256".to_string(),
        },
    ));
    out.push(("jwk_bad_utf8".to_string(), patch(&jwk, b"abc", b"a\xc3c")));

    let caps = tx(K::CapabilityNotificationV2(AuthorityCapabilitiesV2 {
        authority: f.name(1),
        generation: 5,
        supported_protocol_versions: SupportedProtocolVersionsWithHashes {
            versions: vec![(ProtocolVersion::new(80), Digest::new([0x5d; 32]))],
        },
        available_system_packages: vec![object_ref(9)],
    }));
    let mut digest_31 = patch(&caps, &[32, 0x5d], &[31, 0x5d]);
    let at = digest_31.windows(32).position(|w| w == [0x5d; 32]).unwrap();
    digest_31.remove(at + 1);
    out.push(("capability_v2_digest_31".to_string(), digest_31));
    out.push((
        "capability_v2_object_digest_33".to_string(),
        patch(&caps, &[32, 0xf6], &[33, 0xf6]),
    ));

    let claims = with_claims(
        f,
        vec![TransactionClaim::AddressAliasesV2(NonEmpty::new((
            5,
            Some(SequenceNumber::from_u64(3)),
        )))],
    );
    let option_at = claims.len() - 9;
    out.push(("claim_option_2".to_string(), set(&claims, option_at, 2)));
    out.push(("claim_tag_3".to_string(), set(&claims, option_at - 3, 3)));
    out.push((
        "claim_count_overlong".to_string(),
        overlong(&claims, option_at - 4),
    ));
    let no_claims = with_claims(f, vec![]);
    let mut empty_v2 = no_claims[..no_claims.len() - 1].to_vec();
    empty_v2.extend([1, 2, 0]);
    out.push(("semantic_claim_aliases_v2_empty".to_string(), empty_v2));
    let mut empty_v1 = no_claims[..no_claims.len() - 1].to_vec();
    empty_v1.extend([1, 0, 0]);
    out.push(("semantic_claim_aliases_v1_empty".to_string(), empty_v1));
    let mut immutable_empty = no_claims[..no_claims.len() - 1].to_vec();
    immutable_empty.extend([1, 1, 0]);
    out.push(("claim_immutable_empty".to_string(), immutable_empty));

    let duration = |secs: u64, nanos: u32| {
        let mut b = observation(
            f.name(2),
            vec![(ExecutionTimeObservationKey::SplitCoins, Duration::ZERO)],
        );
        let n = b.len();
        b[n - 12..n - 4].copy_from_slice(&secs.to_le_bytes());
        b[n - 4..].copy_from_slice(&nanos.to_le_bytes());
        b
    };
    out.push(("duration_nanos_1e9".to_string(), duration(5, 1_000_000_000)));
    out.push(("duration_nanos_max".to_string(), duration(0, u32::MAX)));
    out.push((
        "semantic_duration_overflow".to_string(),
        duration(u64::MAX, 1_000_000_000),
    ));
    let key_at = observation(f.name(2), vec![]).len();
    let one = observation(
        f.name(2),
        vec![(ExecutionTimeObservationKey::Upgrade, Duration::ZERO)],
    );
    out.push(("observation_key_7".to_string(), set(&one, key_at, 7)));

    let certified = tx(K::CertifiedTransaction(Box::new(f.certificate(10_000_000))));
    let sig_info = bcs::to_bytes(f.certificate(10_000_000).auth_sig()).unwrap();
    let base = certified.len() - sig_info.len();
    let mut zero_sig = certified.clone();
    zero_sig[base + 8..base + 56].fill(0);
    out.push(("semantic_certified_signature_zero".to_string(), zero_sig));
    let mut bad_bitmap = certified[..base + 56].to_vec();
    bad_bitmap.extend([3, 1, 2, 3]);
    out.push(("semantic_certified_bitmap_garbage".to_string(), bad_bitmap));
    let signed = tx(K::CheckpointSignatureV2(Box::new(
        f.signed_checkpoint(false),
    )));
    let mut zero_sig = signed.clone();
    let n = zero_sig.len();
    zero_sig[n - 48..].fill(0);
    out.push(("semantic_checkpoint_signature_zero".to_string(), zero_sig));
    out
}

/// Transaction kinds with a type argument nested `depth` vectors deep.
fn deep(f: &Fixture, wrapper: &str, depth: usize) -> Vec<u8> {
    use sui_types::transaction::{Argument, Command, ProgrammableTransaction, TransactionKind};
    let mut ty = vec![6; depth];
    ty.push(1);
    let mut out = vec![1, 2, 3, 4, 5, 6, 7, 8];
    if wrapper == "observation" {
        out.push(10);
        out.extend(bcs::to_bytes(&f.name(0)).unwrap());
        out.extend(42u64.to_le_bytes());
        out.push(1);
        out.push(0);
        out.extend([0xab; 32]);
        out.extend(bcs::to_bytes("m").unwrap());
        out.extend(bcs::to_bytes("f").unwrap());
        out.push(1);
        out.extend(&ty);
        out.extend(bcs::to_bytes(&Duration::from_secs(1)).unwrap());
        return out;
    }
    // A shallow transaction whose type argument is then deepened in place.
    let marker = TypeInput::Struct(Box::new(StructInput {
        address: ObjectID::new([0xee; 32]).into(),
        module: "deep".to_string(),
        name: "Marker".to_string(),
        type_params: vec![],
    }));
    let kind = TransactionKind::ProgrammableTransaction(ProgrammableTransaction {
        inputs: vec![],
        commands: vec![Command::MakeMoveVec(
            Some(marker.clone()),
            vec![Argument::GasCoin],
        )],
    });
    let shallow = signed(
        [0, 0, 0],
        &Spec {
            kind,
            ..Spec::new()
        }
        .build(),
        &[&[0; 97]],
    );
    let transaction = patch(&shallow, &bcs::to_bytes(&marker).unwrap(), &ty);
    match wrapper {
        "user" => {
            out.push(9);
            out.extend(&transaction);
        }
        "user_v2" => {
            out.push(12);
            out.extend(&transaction);
            out.push(0);
        }
        "certified" => {
            out.push(0);
            out.extend(&transaction);
            out.extend(bcs::to_bytes(f.certificate(10_000_000).auth_sig()).unwrap());
        }
        _ => unreachable!(),
    }
    out
}

pub fn vectors() -> Vec<u8> {
    let f = Fixture::new();
    let mut rng = StdRng::from_seed([47; 32]);
    let mut cases: Vec<(String, Vec<u8>)> = vec![];
    for (label, bytes, fuzz) in valid(&f) {
        assert_eq!(verdict(&bytes), "ok", "{label}");
        let malformed = generic(&label, &bytes, fuzz, &mut rng);
        cases.push((label, bytes));
        cases.extend(malformed);
    }
    cases.extend(targeted(&f));
    for wrapper in ["user", "user_v2", "certified", "observation"] {
        let deepest = (1..600)
            .take_while(|d| verdict(&deep(&f, wrapper, *d)) == "ok")
            .last()
            .unwrap();
        for (at, depth) in [
            ("below", deepest - 1),
            ("at", deepest),
            ("over", deepest + 1),
        ] {
            cases.push((
                format!("depth_{wrapper}_{at}_{depth}"),
                deep(&f, wrapper, depth),
            ));
        }
    }
    let mut out = String::new();
    for (label, bytes) in cases {
        writeln!(out, "{label} {} {}", verdict(&bytes), crate::hex(&bytes)).unwrap();
    }
    let mut gz = flate2::write::GzEncoder::new(vec![], flate2::Compression::best());
    gz.write_all(out.as_bytes()).unwrap();
    gz.finish().unwrap()
}
