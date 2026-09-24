// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Validity-check vectors: transactions built with sui-types, each run
//! through the reference's check under many contexts, every protocol
//! version and every chain. Format, one record per line:
//!
//! ```text
//! tx <id> <check> <label> <bcs hex>
//! case <id> <chain> <first version>-<last version> <epoch> <chain id hex> <rgp> <committee size> <verdict>
//! ```
//!
//! `check` names the reference function (`tx_data` is
//! `TransactionData::validity_check`). A `case` covers a run of consecutive
//! versions with the same verdict: `ok`, or the error's variant name
//! (`UserInputError`'s inner variant when it is one).

use std::fmt::Write as _;

use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use sui_types::base_types::{ObjectDigest, ObjectID, SequenceNumber, SuiAddress};
use sui_types::digests::{ChainIdentifier, CheckpointDigest};
use sui_types::error::{SuiError, SuiErrorKind};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{
    AllowedProposers, Argument, GasData, TransactionData, TransactionDataAPI, TransactionDataV1,
    TransactionExpiration, TransactionKind, TxValidityCheckContext,
};

use crate::hex;

const CHAIN_ID: [u8; 32] = [0x11; 32];
const OTHER_CHAIN_ID: [u8; 32] = [0x22; 32];
const EPOCH: u64 = 5;

struct Context {
    epoch: u64,
    chain_id: [u8; 32],
    rgp: u64,
    committee_size: u32,
}

const CONTEXTS: [Context; 3] = [
    Context {
        epoch: EPOCH,
        chain_id: CHAIN_ID,
        rgp: 1000,
        committee_size: 4,
    },
    Context {
        epoch: EPOCH,
        chain_id: CHAIN_ID,
        rgp: 1000,
        committee_size: 2,
    },
    Context {
        epoch: EPOCH,
        chain_id: CHAIN_ID,
        rgp: 0,
        committee_size: 100,
    },
];

fn chain_identifier(id: [u8; 32]) -> ChainIdentifier {
    ChainIdentifier::from(CheckpointDigest::new(id))
}

fn verdict(result: Result<(), SuiError>) -> String {
    match result {
        Ok(()) => "ok".to_owned(),
        Err(e) => match e.as_inner() {
            SuiErrorKind::UserInputError { error } => error.as_ref().to_owned(),
            kind => kind.as_ref().to_owned(),
        },
    }
}

/// A transfer of the gas coin, paid with one gas object at price 1000.
fn base(expiration: TransactionExpiration, price: u64) -> TransactionData {
    let sender = SuiAddress::from(ObjectID::new([0xa1; 32]));
    let mut builder = ProgrammableTransactionBuilder::new();
    builder.transfer_arg(
        SuiAddress::from(ObjectID::new([0xb2; 32])),
        Argument::GasCoin,
    );
    TransactionData::V1(TransactionDataV1 {
        kind: TransactionKind::ProgrammableTransaction(builder.finish()),
        sender,
        gas_data: GasData {
            payment: vec![(
                ObjectID::new([0xc3; 32]),
                SequenceNumber::from_u64(7),
                ObjectDigest::new([0xd4; 32]),
            )],
            owner: sender,
            price,
            budget: 10_000_000,
        },
        expiration,
    })
}

fn valid_during(min: Option<u64>, max: Option<u64>) -> TransactionExpiration {
    TransactionExpiration::ValidDuring {
        min_epoch: min,
        max_epoch: max,
        min_timestamp: None,
        max_timestamp: None,
        chain: chain_identifier(CHAIN_ID),
        nonce: 9,
    }
}

fn validity(epoch: u64, proposers: &[u32]) -> TransactionExpiration {
    TransactionExpiration::Validity {
        min_epoch: Some(EPOCH),
        max_epoch: Some(EPOCH),
        min_timestamp: None,
        max_timestamp: None,
        chain: chain_identifier(CHAIN_ID),
        nonce: 9,
        allowed_proposers: nonempty::NonEmpty::from_slice(proposers)
            .map(|proposers| AllowedProposers { epoch, proposers }),
    }
}

fn expiration_cases() -> Vec<(String, TransactionData)> {
    let mut cases = vec![
        ("none".to_owned(), base(TransactionExpiration::None, 1000)),
        (
            "epoch_4".to_owned(),
            base(TransactionExpiration::Epoch(4), 1000),
        ),
        (
            "epoch_5".to_owned(),
            base(TransactionExpiration::Epoch(5), 1000),
        ),
        (
            "epoch_6".to_owned(),
            base(TransactionExpiration::Epoch(6), 1000),
        ),
    ];
    for (min, max) in [
        (Some(5), Some(5)),
        (Some(4), Some(5)),
        (Some(5), Some(6)),
        (Some(5), Some(7)),
        (Some(4), Some(4)),
        (Some(6), Some(6)),
        (Some(6), Some(5)),
        (None, Some(5)),
        (Some(5), None),
        (None, None),
        (Some(u64::MAX), Some(u64::MAX)),
    ] {
        cases.push((
            format!("valid_during_{min:?}_{max:?}"),
            base(valid_during(min, max), 1000),
        ));
    }
    for (label, min_ts, max_ts) in [("min_ts", Some(1), None), ("max_ts", None, Some(1))] {
        let TransactionExpiration::ValidDuring {
            min_epoch,
            max_epoch,
            chain,
            nonce,
            ..
        } = valid_during(Some(5), Some(5))
        else {
            unreachable!()
        };
        let expiration = TransactionExpiration::ValidDuring {
            min_epoch,
            max_epoch,
            min_timestamp: min_ts,
            max_timestamp: max_ts,
            chain,
            nonce,
        };
        cases.push((format!("valid_during_{label}"), base(expiration, 1000)));
    }
    let mut other_chain = valid_during(Some(5), Some(5));
    if let TransactionExpiration::ValidDuring { chain: c, .. } = &mut other_chain {
        *c = chain_identifier(OTHER_CHAIN_ID);
    }
    cases.push((
        "valid_during_other_chain".to_owned(),
        base(other_chain, 1000),
    ));

    for (label, epoch, proposers, price) in [
        ("no_proposers", EPOCH, &[][..], 1000),
        ("one", EPOCH, &[0][..], 1000),
        ("three", EPOCH, &[0, 1, 2][..], 1000),
        ("four_unpaid", EPOCH, &[0, 1, 2, 3][..], 1000),
        ("four_paid", EPOCH, &[0, 1, 2, 3][..], 4000),
        ("unsorted", EPOCH, &[1, 0][..], 1000),
        ("duplicate", EPOCH, &[0, 0][..], 1000),
        ("out_of_range", EPOCH, &[0, 4][..], 1000),
        ("other_epoch", EPOCH + 1, &[0, 1, 2, 3, 9][..], 1000),
    ] {
        cases.push((
            format!("validity_{label}"),
            base(validity(epoch, proposers), price),
        ));
    }
    cases
}

/// Every case, run under every context, chain and protocol version.
pub fn vectors() -> String {
    let chains = [Chain::Unknown, Chain::Mainnet, Chain::Testnet];
    let max = ProtocolVersion::MAX.as_u64();
    let configs: Vec<Vec<ProtocolConfig>> = chains
        .iter()
        .map(|&chain| {
            (1..=max)
                .map(|v| ProtocolConfig::get_for_version(ProtocolVersion::new(v), chain))
                .collect()
        })
        .collect();

    let mut out = String::new();
    for (id, (label, tx)) in expiration_cases().into_iter().enumerate() {
        writeln!(
            out,
            "tx {id} tx_data {label} {}",
            hex(&bcs::to_bytes(&tx).unwrap())
        )
        .unwrap();
        for context in &CONTEXTS {
            for (chain, configs) in chains.iter().zip(&configs) {
                let verdicts: Vec<String> = configs
                    .iter()
                    .map(|config| {
                        let ctx = TxValidityCheckContext {
                            config,
                            epoch: context.epoch,
                            chain_identifier: chain_identifier(context.chain_id),
                            reference_gas_price: context.rgp,
                            committee_size: context.committee_size,
                        };
                        verdict(tx.validity_check(&ctx))
                    })
                    .collect();
                let mut first = 0;
                while first < verdicts.len() {
                    let mut last = first;
                    while last + 1 < verdicts.len() && verdicts[last + 1] == verdicts[first] {
                        last += 1;
                    }
                    writeln!(
                        out,
                        "case {id} {chain:?} {}-{} {} {} {} {} {}",
                        first + 1,
                        last + 1,
                        context.epoch,
                        hex(&context.chain_id),
                        context.rgp,
                        context.committee_size,
                        verdicts[first]
                    )
                    .unwrap();
                    first = last + 1;
                }
            }
        }
    }
    out
}
