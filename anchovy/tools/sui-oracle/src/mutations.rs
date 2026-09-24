// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Mutation vectors: valid transactions with one to three random,
//! field-aware changes each, signed, and run through everything static a
//! validator checks on submission (decoding, `validity_check`, signature
//! verification) under a spread of protocol versions on every chain.
//! `sui-oracle --mutation-vectors FILE` writes them in the validity-vector
//! format with check `full`, gzipped.
//!
//! The generator is seeded, so the corpus is the same on every run.

use std::collections::HashSet;
use std::io::Write as _;

use move_core_types::account_address::AccountAddress;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use shared_crypto::intent::{Intent, IntentMessage};
use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use sui_types::base_types::{ObjectDigest, ObjectID, ObjectRef, SequenceNumber, SuiAddress};
use sui_types::coin_reservation::ParsedObjectRefWithdrawal;
use sui_types::crypto::{Signature, SuiKeyPair, get_key_pair_from_rng};
use sui_types::full_checkpoint_content::CheckpointData;
use sui_types::transaction::{
    AllowedProposers, Argument, CallArg, Command, FundsWithdrawalArg, GenesisTransaction,
    ObjectArg, ProgrammableMoveCall, Reservation, SenderSignedData, SharedObjectMutability,
    TransactionData, TransactionDataAPI, TransactionExpiration, TransactionKind,
    TxValidityCheckContext, WithdrawFrom, WithdrawalTypeArg,
};
use sui_types::type_input::{StructInput, TypeInput};

use crate::validity::{CHAIN_ID, EPOCH, Spec, chain_identifier, verdict_of};
use crate::validity_signed::signed;

/// How many mutated transactions to write.
const COUNT: usize = 3000;

/// The protocol versions each mutation runs under: every tenth, and the
/// last few, where most flags change.
fn versions() -> Vec<u64> {
    let max = ProtocolVersion::MAX.as_u64();
    let mut v: Vec<u64> = (1..=max).step_by(10).collect();
    v.extend(max.saturating_sub(8)..=max);
    v.sort_unstable();
    v.dedup();
    v
}

struct Keys {
    sender: SuiKeyPair,
    sponsor: SuiKeyPair,
}

impl Keys {
    fn new() -> Keys {
        let mut rng = StdRng::from_seed([21; 32]);
        Keys {
            sender: SuiKeyPair::Ed25519(get_key_pair_from_rng(&mut rng).1),
            sponsor: SuiKeyPair::Secp256k1(get_key_pair_from_rng(&mut rng).1),
        }
    }
    fn sender(&self) -> SuiAddress {
        SuiAddress::from(&self.sender.public())
    }
    fn sponsor(&self) -> SuiAddress {
        SuiAddress::from(&self.sponsor.public())
    }
}

fn object(rng: &mut StdRng) -> ObjectRef {
    (
        ObjectID::new(rng.r#gen()),
        SequenceNumber::from_u64(rng.gen_range(0..10)),
        ObjectDigest::new(rng.r#gen()),
    )
}

/// A value near an edge more often than not.
fn interesting_u64(rng: &mut StdRng) -> u64 {
    *[
        0,
        1,
        2,
        999,
        1000,
        1001,
        EPOCH - 1,
        EPOCH,
        EPOCH + 1,
        100_000,
        50_000_000_000,
        u64::MAX - 1,
        u64::MAX,
        rng.r#gen(),
        rng.gen_range(0..1_000_000_000),
    ]
    .choose(rng)
    .unwrap()
}

fn identifier(rng: &mut StdRng) -> String {
    [
        "m",
        "coin",
        "Coin",
        "_x",
        "_",
        "",
        "1m",
        "m-n",
        "é",
        "balance",
        "send_funds",
        "SUI",
    ]
    .choose(rng)
    .unwrap()
    .to_string()
}

fn type_input(rng: &mut StdRng, depth: u32) -> TypeInput {
    let leaf = [
        TypeInput::Bool,
        TypeInput::U8,
        TypeInput::U16,
        TypeInput::U32,
        TypeInput::U64,
        TypeInput::U128,
        TypeInput::U256,
        TypeInput::Address,
        TypeInput::Signer,
    ];
    match rng.gen_range(0..if depth > 20 { 1 } else { 4 }) {
        0 => leaf.choose(rng).unwrap().clone(),
        1 => TypeInput::Vector(Box::new(type_input(rng, depth + 1))),
        _ => TypeInput::Struct(Box::new(StructInput {
            address: if rng.gen_bool(0.5) {
                AccountAddress::from_hex_literal("0x2").unwrap()
            } else {
                AccountAddress::new(rng.r#gen())
            },
            module: identifier(rng),
            name: identifier(rng),
            type_params: (0..rng.gen_range(0..3))
                .map(|_| type_input(rng, depth + 1))
                .collect(),
        })),
    }
}

fn argument(rng: &mut StdRng, inputs: usize, command: usize) -> Argument {
    let small = |rng: &mut StdRng, n: usize| rng.gen_range(0..(n + 2)) as u16;
    match rng.gen_range(0..5) {
        0 => Argument::GasCoin,
        1 | 2 => Argument::Input(small(rng, inputs)),
        3 => Argument::Result(small(rng, command)),
        _ => Argument::NestedResult(small(rng, command), rng.gen_range(0..3)),
    }
}

fn arguments(rng: &mut StdRng, inputs: usize, command: usize) -> Vec<Argument> {
    // Near the argument limit only now and then: such commands are large.
    let n = if rng.gen_bool(0.03) {
        *[511, 512, 513].choose(rng).unwrap()
    } else {
        *[0, 1, 1, 2, 3].choose(rng).unwrap()
    };
    (0..n).map(|_| argument(rng, inputs, command)).collect()
}

fn call_arg(rng: &mut StdRng) -> CallArg {
    match rng.gen_range(0..7) {
        0 => CallArg::Pure(vec![
            7;
            if rng.gen_bool(0.05) {
                *[16_383, 16_384].choose(rng).unwrap()
            } else {
                *[0, 1, 8, 32, 33].choose(rng).unwrap()
            }
        ]),
        1 => CallArg::Object(ObjectArg::ImmOrOwnedObject(object(rng))),
        2 => CallArg::Object(ObjectArg::SharedObject {
            id: if rng.gen_bool(0.3) {
                ObjectID::from_single_byte(8)
            } else {
                ObjectID::new(rng.r#gen())
            },
            initial_shared_version: SequenceNumber::from_u64(1),
            mutability: *[
                SharedObjectMutability::Immutable,
                SharedObjectMutability::Mutable,
                SharedObjectMutability::NonExclusiveWrite,
            ]
            .choose(rng)
            .unwrap(),
        }),
        3 => CallArg::Object(ObjectArg::Receiving(object(rng))),
        4 => CallArg::Object(ObjectArg::ImmOrOwnedObject(
            ParsedObjectRefWithdrawal::new(
                ObjectID::new(rng.r#gen()),
                EPOCH - rng.gen_range(0..3),
                rng.gen_range(0..3),
            )
            .encode(SequenceNumber::from_u64(1), chain_identifier(CHAIN_ID)),
        )),
        _ => CallArg::FundsWithdrawal(FundsWithdrawalArg {
            reservation: Reservation::MaxAmountU64(rng.gen_range(0..3)),
            type_arg: WithdrawalTypeArg::Balance("0x2::sui::SUI".parse().unwrap()),
            withdraw_from: match rng.gen_range(0..3) {
                0 => WithdrawFrom::Sender,
                1 => WithdrawFrom::Sponsor,
                _ => WithdrawFrom::SenderAllowance {
                    funder: SuiAddress::from(ObjectID::new(rng.r#gen())),
                    allowance: ObjectID::new(rng.r#gen()),
                },
            },
        }),
    }
}

fn command(rng: &mut StdRng, inputs: usize, index: usize) -> Command {
    let arg = |rng: &mut StdRng| argument(rng, inputs, index);
    match rng.gen_range(0..7) {
        0 => Command::MoveCall(Box::new(ProgrammableMoveCall {
            package: if rng.gen_bool(0.5) {
                ObjectID::from_single_byte(2)
            } else {
                ObjectID::new(rng.r#gen())
            },
            module: identifier(rng),
            function: identifier(rng),
            type_arguments: (0..*[0, 1, 2, 15, 16, 17].choose(rng).unwrap())
                .map(|_| type_input(rng, 1))
                .collect(),
            arguments: arguments(rng, inputs, index),
        })),
        1 => Command::TransferObjects(arguments(rng, inputs, index), arg(rng)),
        2 => Command::SplitCoins(arg(rng), arguments(rng, inputs, index)),
        3 => Command::MergeCoins(arg(rng), arguments(rng, inputs, index)),
        4 => Command::MakeMoveVec(
            rng.gen_bool(0.5).then(|| type_input(rng, 1)),
            arguments(rng, inputs, index),
        ),
        5 => Command::Publish(
            vec![vec![0]; *[0, 1, 2].choose(rng).unwrap()],
            (0..rng.gen_range(0..4))
                .map(|_| ObjectID::new(rng.r#gen()))
                .collect(),
        ),
        _ => Command::Upgrade(
            vec![vec![0]; *[0, 1].choose(rng).unwrap()],
            vec![],
            ObjectID::new(rng.r#gen()),
            arg(rng),
        ),
    }
}

fn expiration(rng: &mut StdRng) -> TransactionExpiration {
    let epoch = |rng: &mut StdRng| {
        rng.gen_bool(0.8).then(|| {
            *[EPOCH - 1, EPOCH, EPOCH + 1, EPOCH + 2, u64::MAX]
                .choose(rng)
                .unwrap()
        })
    };
    let chain = |rng: &mut StdRng| {
        chain_identifier(if rng.gen_bool(0.8) {
            CHAIN_ID
        } else {
            [0x22; 32]
        })
    };
    let ts = |rng: &mut StdRng| rng.gen_bool(0.1).then_some(1);
    match rng.gen_range(0..4) {
        0 => TransactionExpiration::None,
        1 => TransactionExpiration::Epoch(interesting_u64(rng)),
        2 => TransactionExpiration::ValidDuring {
            min_epoch: epoch(rng),
            max_epoch: epoch(rng),
            min_timestamp: ts(rng),
            max_timestamp: ts(rng),
            chain: chain(rng),
            nonce: rng.r#gen(),
        },
        _ => TransactionExpiration::Validity {
            min_epoch: epoch(rng),
            max_epoch: epoch(rng),
            min_timestamp: ts(rng),
            max_timestamp: ts(rng),
            chain: chain(rng),
            nonce: rng.r#gen(),
            allowed_proposers: rng.gen_bool(0.7).then(|| AllowedProposers {
                epoch: *[EPOCH, EPOCH + 1].choose(rng).unwrap(),
                proposers: nonempty::NonEmpty::from_vec(
                    (0..rng.gen_range(1..6))
                        .map(|_| rng.gen_range(0..6))
                        .collect(),
                )
                .unwrap(),
            }),
        },
    }
}

/// One random change to `tx`.
fn mutate(rng: &mut StdRng, tx: &mut TransactionData, keys: &Keys) {
    let TransactionData::V1(v1) = tx;
    match rng.gen_range(0..12) {
        0 => v1.gas_data.price = interesting_u64(rng),
        1 => v1.gas_data.budget = interesting_u64(rng),
        2 => {
            let payment = &mut v1.gas_data.payment;
            match rng.gen_range(0..5) {
                0 => payment.clear(),
                1 => {
                    if let Some(first) = payment.first().copied() {
                        payment.push(first);
                    }
                }
                2 => payment.push(object(rng)),
                3 => {
                    let n = *[255, 256, 257].choose(rng).unwrap();
                    *payment = (0..n).map(|_| object(rng)).collect();
                }
                _ => payment.push(
                    ParsedObjectRefWithdrawal::new(
                        ObjectID::new(rng.r#gen()),
                        EPOCH,
                        rng.gen_range(0..3),
                    )
                    .encode(SequenceNumber::from_u64(1), chain_identifier(CHAIN_ID)),
                ),
            }
        }
        3 => {
            v1.gas_data.owner = *[
                keys.sender(),
                keys.sponsor(),
                SuiAddress::from(ObjectID::new(rng.r#gen())),
            ]
            .choose(rng)
            .unwrap();
        }
        4 => v1.sender = *[keys.sender(), keys.sponsor()].choose(rng).unwrap(),
        5 => v1.expiration = expiration(rng),
        6 if rng.gen_bool(0.2) => {
            v1.kind = TransactionKind::Genesis(GenesisTransaction { objects: vec![] });
        }
        _ => {
            let TransactionKind::ProgrammableTransaction(pt) = &mut v1.kind else {
                return;
            };
            let (inputs, commands) = (pt.inputs.len(), pt.commands.len());
            match rng.gen_range(0..8) {
                0 if inputs > 0 => {
                    pt.inputs.remove(rng.gen_range(0..inputs));
                }
                1 if inputs > 0 => {
                    let i = pt.inputs[rng.gen_range(0..inputs)].clone();
                    pt.inputs.push(i);
                }
                2 => pt.inputs.push(call_arg(rng)),
                3 if commands > 0 => {
                    pt.commands.remove(rng.gen_range(0..commands));
                }
                4 if commands > 1 => {
                    let (a, b) = (rng.gen_range(0..commands), rng.gen_range(0..commands));
                    pt.commands.swap(a, b);
                }
                5 if commands > 0 => {
                    let c = pt.commands[rng.gen_range(0..commands)].clone();
                    pt.commands.push(c);
                }
                6 => {
                    let at = rng.gen_range(0..=commands);
                    pt.commands.insert(at, command(rng, inputs, at));
                }
                // Shapes one random change rarely makes: several publishes;
                // randomness used, then followed by another command.
                7 if rng.gen_bool(0.3) => {
                    for _ in 0..rng.gen_range(2..4) {
                        pt.commands.push(Command::Publish(vec![vec![0]], vec![]));
                    }
                }
                7 if rng.gen_bool(0.5) => {
                    let random = pt.inputs.len() as u16;
                    pt.inputs.push(CallArg::Object(ObjectArg::SharedObject {
                        id: ObjectID::from_single_byte(8),
                        initial_shared_version: SequenceNumber::from_u64(1),
                        mutability: SharedObjectMutability::Immutable,
                    }));
                    pt.commands
                        .push(Command::MoveCall(Box::new(ProgrammableMoveCall {
                            package: ObjectID::from_single_byte(2),
                            module: "random".to_owned(),
                            function: "new_generator".to_owned(),
                            type_arguments: vec![],
                            arguments: vec![Argument::Input(random)],
                        })));
                    let after = command(rng, pt.inputs.len(), pt.commands.len());
                    pt.commands.push(after);
                }
                _ => {
                    let n = *[1023, 1024, 1025].choose(rng).unwrap();
                    // A small command, so the transaction stays small.
                    let c = Command::SplitCoins(Argument::GasCoin, vec![Argument::Input(0)]);
                    pt.commands = vec![c; n];
                }
            }
        }
    }
}

/// Valid starting points: the crafted transactions' shapes, and real
/// mainnet transactions re-sent from our keys.
fn seeds(keys: &Keys) -> Vec<TransactionData> {
    let mut seeds = vec![];
    let mut own = |mut spec: Spec| {
        spec.sender = keys.sender();
        spec.owner = keys.sender();
        seeds.push(spec.build());
    };
    own(Spec::new());
    own(Spec::transfer_object());
    own(Spec::address_balance());
    own(Spec::with_inputs(vec![CallArg::Pure(vec![1; 8])]));
    let mut sponsored = Spec::new();
    sponsored.sender = keys.sender();
    sponsored.owner = keys.sponsor();
    seeds.push(sponsored.build());

    let mut bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../crates/messages/tests/data/mainnet-325300367.chk"
    ))
    .unwrap();
    bytes.remove(0);
    let checkpoint: CheckpointData = bcs::from_bytes(&bytes).unwrap();
    for tx in &checkpoint.transactions {
        let mut data = tx.transaction.data().transaction_data().clone();
        if !matches!(data.kind(), TransactionKind::ProgrammableTransaction(_)) {
            continue;
        }
        let TransactionData::V1(v1) = &mut data;
        v1.sender = keys.sender();
        v1.gas_data.owner = keys.sender();
        // Mainnet's chain and epoch are not the vectors'.
        v1.expiration = TransactionExpiration::None;
        seeds.push(data);
    }
    seeds
}

/// Signed by whichever of our keys are its signers, the signatures then
/// broken some of the time.
fn sign(rng: &mut StdRng, data: &TransactionData, keys: &Keys) -> Vec<u8> {
    let msg = IntentMessage::new(Intent::sui_transaction(), data);
    let mut sigs = vec![];
    for (address, key) in [
        (keys.sender(), &keys.sender),
        (keys.sponsor(), &keys.sponsor),
    ] {
        if data.sender() == address || data.gas_owner() == address {
            sigs.push(Signature::new_secure(&msg, key).as_ref().to_vec());
        }
    }
    if rng.gen_bool(0.2) && !sigs.is_empty() {
        let i = rng.gen_range(0..sigs.len());
        match rng.gen_range(0..6) {
            0 => {
                let at = rng.gen_range(0..sigs[i].len());
                sigs[i][at] ^= 1 << rng.gen_range(0..8);
            }
            1 => {
                sigs.remove(i);
            }
            2 => {
                let s = sigs[i].clone();
                sigs.push(s);
            }
            3 => sigs.reverse(),
            4 => {
                let wrong = if rng.gen_bool(0.5) {
                    &keys.sender
                } else {
                    &keys.sponsor
                };
                sigs[i] = Signature::new_secure(&msg, wrong).as_ref().to_vec();
            }
            _ => {
                let len = *[0, 1, 97, 98, 140, 300].choose(rng).unwrap();
                let mut garbage: Vec<u8> = (0..len).map(|_| rng.r#gen()).collect();
                if let Some(flag) = garbage.first_mut() {
                    *flag = rng.gen_range(0..8);
                }
                sigs[i] = garbage;
            }
        }
    }
    let sigs: Vec<&[u8]> = sigs.iter().map(Vec::as_slice).collect();
    signed([0, 0, 0], data, &sigs)
}

/// The reference, as a validator runs it on submission, or `panic`.
fn verdict(bytes: &[u8], ctx: &TxValidityCheckContext<'_>, chain: Chain) -> String {
    let run = || {
        let Ok(tx) = bcs::from_bytes::<SenderSignedData>(bytes) else {
            return "TransactionDeserializationError".to_owned();
        };
        let size = match tx.validity_check(ctx) {
            Ok(size) => size,
            Err(e) => return verdict_of(e),
        };
        match crate::validity_verify::verify(&tx, ctx.config, chain, ctx.epoch).as_str() {
            "ok" => format!("ok:{size}"),
            error => error.to_owned(),
        }
    };
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(run);
    std::panic::set_hook(hook);
    result.unwrap_or_else(|_| "panic".to_owned())
}

pub fn vectors() -> Vec<u8> {
    let keys = Keys::new();
    let seeds = seeds(&keys);
    let mut rng = StdRng::from_seed([33; 32]);
    let chains = [Chain::Unknown, Chain::Mainnet, Chain::Testnet];
    let versions = versions();
    let configs: Vec<(Chain, Vec<ProtocolConfig>)> = chains
        .iter()
        .map(|&c| {
            (
                c,
                versions
                    .iter()
                    .map(|&v| ProtocolConfig::get_for_version(ProtocolVersion::new(v), c))
                    .collect(),
            )
        })
        .collect();

    let mut out = format!("jwks {}\n", crate::validity_verify::jwks_json());
    let mut seen = HashSet::new();
    let mut id = 0;
    while id < COUNT {
        let mut tx = seeds.choose(&mut rng).unwrap().clone();
        for _ in 0..rng.gen_range(1..=3) {
            mutate(&mut rng, &mut tx, &keys);
        }
        let bytes = sign(&mut rng, &tx, &keys);
        if !seen.insert(bytes.clone()) {
            continue;
        }
        out.push_str(&format!(
            "tx {id} full mutation_{id} {}\n",
            crate::hex(&bytes)
        ));
        for (chain, configs) in &configs {
            let verdicts: Vec<String> = configs
                .iter()
                .map(|config| {
                    let ctx = TxValidityCheckContext {
                        config,
                        epoch: EPOCH,
                        chain_identifier: chain_identifier(CHAIN_ID),
                        reference_gas_price: 1000,
                        committee_size: 4,
                    };
                    verdict(&bytes, &ctx, *chain)
                })
                .collect();
            // Runs of equal verdicts over versions with no gap between them:
            // a range claims every version in it.
            let mut first = 0;
            while first < verdicts.len() {
                let mut last = first;
                while last + 1 < verdicts.len()
                    && verdicts[last + 1] == verdicts[first]
                    && versions[last + 1] == versions[last] + 1
                {
                    last += 1;
                }
                out.push_str(&format!(
                    "case {id} {chain:?} {}-{} {EPOCH} {} 1000 4 {}\n",
                    versions[first],
                    versions[last],
                    crate::hex(&CHAIN_ID),
                    verdicts[first]
                ));
                first = last + 1;
            }
        }
        id += 1;
    }
    let mut gz = flate2::write::GzEncoder::new(vec![], flate2::Compression::best());
    gz.write_all(out.as_bytes()).unwrap();
    gz.finish().unwrap()
}
