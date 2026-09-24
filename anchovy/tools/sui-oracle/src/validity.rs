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
//! `check` names the reference function: `tx_data` is
//! `TransactionData::validity_check` on BCS `TransactionData`, `gas_price`
//! the price checks of `SuiGasStatus::new`, `sender_signed` decoding
//! `SenderSignedData` and its `validity_check` (see `validity_signed.rs`). A `case` covers a run of consecutive
//! versions with the same verdict: `ok`, the error's variant name
//! (`UserInputError`'s inner variant when it is one), or `panic` when the
//! reference passed its checks and then panicked in what follows them.

use std::fmt::Write as _;

use std::collections::BTreeSet;

use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use sui_types::accumulator_root::AccumulatorValue;
use sui_types::balance::Balance;
use sui_types::base_types::{ObjectDigest, ObjectID, ObjectRef, SequenceNumber, SuiAddress};
use sui_types::coin_reservation::ParsedObjectRefWithdrawal;
use sui_types::digests::{ChainIdentifier, CheckpointDigest};
use sui_types::error::{SuiError, SuiErrorKind};
use sui_types::gas::SuiGasStatus;
use sui_types::gas_coin::GAS;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{
    AllowedProposers, Argument, CallArg, FundsWithdrawalArg, GasData, GenesisTransaction,
    ObjectArg, Reservation, TransactionData, TransactionDataAPI, TransactionDataV1,
    TransactionExpiration, TransactionKind, TxValidityCheckContext, WithdrawFrom,
    WithdrawalTypeArg,
};

use crate::hex;

pub(crate) const CHAIN_ID: [u8; 32] = [0x11; 32];
const OTHER_CHAIN_ID: [u8; 32] = [0x22; 32];
pub(crate) const EPOCH: u64 = 5;

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

pub(crate) fn chain_identifier(id: [u8; 32]) -> ChainIdentifier {
    ChainIdentifier::from(CheckpointDigest::new(id))
}

fn verdict(result: Result<(), SuiError>) -> String {
    match result {
        Ok(()) => "ok".to_owned(),
        Err(e) => verdict_of(e),
    }
}

/// An error's variant name: `UserInputError`'s inner one when it is one.
pub(crate) fn verdict_of(e: SuiError) -> String {
    match e.as_inner() {
        SuiErrorKind::UserInputError { error } => error.as_ref().to_owned(),
        kind => kind.as_ref().to_owned(),
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

pub(crate) fn valid_during(min: Option<u64>, max: Option<u64>) -> TransactionExpiration {
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

pub(crate) fn sender() -> SuiAddress {
    SuiAddress::from(ObjectID::new([0xa1; 32]))
}

pub(crate) fn recipient() -> SuiAddress {
    SuiAddress::from(ObjectID::new([0xb2; 32]))
}

pub(crate) fn object(n: u8) -> ObjectRef {
    (
        ObjectID::new([n; 32]),
        SequenceNumber::from_u64(7),
        ObjectDigest::new([0xd4; 32]),
    )
}

/// A coin reservation of `amount` from `id`'s balance for `epoch`.
fn reservation(id: ObjectID, epoch: u64, amount: u64) -> ObjectRef {
    ParsedObjectRefWithdrawal::new(id, epoch, amount)
        .encode(SequenceNumber::from_u64(1), chain_identifier(CHAIN_ID))
}

fn sui_balance(owner: SuiAddress) -> ObjectID {
    *AccumulatorValue::get_field_id(owner, &Balance::type_tag(GAS::type_tag()))
        .unwrap()
        .inner()
}

pub(crate) struct Spec {
    pub(crate) kind: TransactionKind,
    pub(crate) payment: Vec<ObjectRef>,
    pub(crate) owner: SuiAddress,
    pub(crate) price: u64,
    pub(crate) budget: u64,
    pub(crate) expiration: TransactionExpiration,
}

impl Spec {
    /// A transfer of the gas coin, one gas object, price 1000.
    pub(crate) fn new() -> Spec {
        let mut builder = ProgrammableTransactionBuilder::new();
        builder.transfer_arg(recipient(), Argument::GasCoin);
        Spec {
            kind: TransactionKind::ProgrammableTransaction(builder.finish()),
            payment: vec![object(0xc3)],
            owner: sender(),
            price: 1000,
            budget: 10_000_000,
            expiration: TransactionExpiration::None,
        }
    }

    /// A transfer of an owned object, so the gas coin is not an argument.
    pub(crate) fn transfer_object() -> Spec {
        let mut builder = ProgrammableTransactionBuilder::new();
        let owned = builder
            .obj(ObjectArg::ImmOrOwnedObject(object(0xe5)))
            .unwrap();
        builder.transfer_arg(recipient(), owned);
        Spec {
            kind: TransactionKind::ProgrammableTransaction(builder.finish()),
            ..Spec::new()
        }
    }

    /// Address-balance gas: no gas objects, a two-epoch window.
    pub(crate) fn address_balance() -> Spec {
        Spec {
            payment: vec![],
            expiration: valid_during(Some(EPOCH), Some(EPOCH + 1)),
            ..Spec::transfer_object()
        }
    }

    pub(crate) fn with_inputs(inputs: Vec<CallArg>) -> Spec {
        let mut builder = ProgrammableTransactionBuilder::new();
        for input in inputs {
            builder.input(input).unwrap();
        }
        let owned = builder
            .obj(ObjectArg::ImmOrOwnedObject(object(0xe5)))
            .unwrap();
        builder.transfer_arg(recipient(), owned);
        Spec {
            kind: TransactionKind::ProgrammableTransaction(builder.finish()),
            ..Spec::new()
        }
    }

    pub(crate) fn build(self) -> TransactionData {
        TransactionData::V1(TransactionDataV1 {
            kind: self.kind,
            sender: sender(),
            gas_data: GasData {
                payment: self.payment,
                owner: self.owner,
                price: self.price,
                budget: self.budget,
            },
            expiration: self.expiration,
        })
    }
}

pub(crate) fn withdrawal(amount: u64, from: WithdrawFrom) -> CallArg {
    CallArg::FundsWithdrawal(FundsWithdrawalArg {
        reservation: Reservation::MaxAmountU64(amount),
        type_arg: WithdrawalTypeArg::Balance(GAS::type_tag()),
        withdraw_from: from,
    })
}

/// Just under, at and over every value `limit` takes across versions.
pub(crate) fn boundaries(
    configs: &[&ProtocolConfig],
    limit: impl Fn(&ProtocolConfig) -> Option<u64>,
) -> Vec<u64> {
    let mut out = BTreeSet::new();
    for v in configs.iter().filter_map(|c| limit(c)) {
        out.extend([v.saturating_sub(1), v, v.saturating_add(1)]);
    }
    out.into_iter().collect()
}

fn gas_cases(configs: &[&ProtocolConfig]) -> Vec<(String, TransactionData)> {
    let mut cases = vec![];
    let mut counts = boundaries(configs, |c| {
        c.max_gas_payment_objects_as_option().map(u64::from)
    });
    counts.insert(0, 0);
    for n in counts {
        let payment = (0..n).map(|i| {
            let mut id = [0xc3; 32];
            id[..8].copy_from_slice(&i.to_le_bytes());
            (
                ObjectID::new(id),
                SequenceNumber::from_u64(7),
                ObjectDigest::new([0xd4; 32]),
            )
        });
        cases.push((
            format!("gas_objects_{n}"),
            Spec {
                payment: payment.collect(),
                ..Spec::new()
            }
            .build(),
        ));
    }

    let mut prices = boundaries(configs, |c| c.max_gas_price_as_option());
    prices.extend([0, 1, 999, 1000, 1001, u64::MAX]);
    prices.sort_unstable();
    prices.dedup();
    for price in prices {
        cases.push((
            format!("price_{price}"),
            Spec {
                price,
                ..Spec::new()
            }
            .build(),
        ));
    }

    let mut budgets = boundaries(configs, |c| c.max_tx_gas_as_option());
    budgets.extend(boundaries(configs, |c| {
        c.base_tx_cost_fixed_as_option().map(|b| b * 1000)
    }));
    budgets.extend(boundaries(configs, |c| c.base_tx_cost_fixed_as_option()));
    budgets.extend([0, u64::MAX]);
    budgets.sort_unstable();
    budgets.dedup();
    for budget in budgets {
        cases.push((
            format!("budget_{budget}"),
            Spec {
                budget,
                ..Spec::new()
            }
            .build(),
        ));
    }
    cases
}

fn address_balance_cases() -> Vec<(String, TransactionData)> {
    let mut cases = vec![
        ("ab_basic".to_owned(), Spec::address_balance().build()),
        (
            "ab_gas_coin_arg".to_owned(),
            Spec {
                payment: vec![],
                expiration: valid_during(Some(EPOCH), Some(EPOCH + 1)),
                ..Spec::new()
            }
            .build(),
        ),
        (
            "ab_price_999".to_owned(),
            Spec {
                price: 999,
                ..Spec::address_balance()
            }
            .build(),
        ),
        (
            "ab_gasless".to_owned(),
            Spec {
                price: 0,
                budget: 0,
                ..Spec::address_balance()
            }
            .build(),
        ),
        (
            "ab_gasless_budget".to_owned(),
            Spec {
                price: 0,
                ..Spec::address_balance()
            }
            .build(),
        ),
        (
            "ab_budget_0".to_owned(),
            Spec {
                budget: 0,
                ..Spec::address_balance()
            }
            .build(),
        ),
    ];
    for (label, expiration) in [
        ("none", TransactionExpiration::None),
        ("epoch", TransactionExpiration::Epoch(EPOCH)),
        ("one_epoch", valid_during(Some(EPOCH), Some(EPOCH))),
        (
            "three_epochs",
            valid_during(Some(EPOCH - 1), Some(EPOCH + 1)),
        ),
    ] {
        cases.push((
            format!("ab_expiration_{label}"),
            Spec {
                expiration,
                ..Spec::address_balance()
            }
            .build(),
        ));
    }
    cases
}

fn withdrawal_cases() -> Vec<(String, TransactionData)> {
    let allowance = WithdrawFrom::SenderAllowance {
        funder: recipient(),
        allowance: ObjectID::new([0x77; 32]),
    };
    let mut cases = vec![];
    for (label, inputs) in [
        ("sender", vec![withdrawal(5, WithdrawFrom::Sender)]),
        ("sender_zero", vec![withdrawal(0, WithdrawFrom::Sender)]),
        ("sponsor", vec![withdrawal(5, WithdrawFrom::Sponsor)]),
        ("allowance", vec![withdrawal(5, allowance)]),
        (
            "ten",
            (0..10)
                .map(|i| withdrawal(i + 1, WithdrawFrom::Sender))
                .collect(),
        ),
        (
            "eleven",
            (0..11)
                .map(|i| withdrawal(i + 1, WithdrawFrom::Sender))
                .collect(),
        ),
    ] {
        cases.push((
            format!("withdraw_{label}"),
            Spec::with_inputs(inputs.clone()).build(),
        ));
        let spec = Spec::with_inputs(inputs);
        cases.push((
            format!("withdraw_{label}_ab_gas"),
            Spec {
                payment: vec![],
                expiration: valid_during(Some(EPOCH), Some(EPOCH + 1)),
                ..spec
            }
            .build(),
        ));
    }

    let mine = sui_balance(sender());
    for (label, epoch, amount) in [
        ("current", EPOCH, 5),
        ("previous", EPOCH - 1, 5),
        ("stale", EPOCH - 2, 5),
        ("future", EPOCH + 1, 5),
        ("zero", EPOCH, 0),
    ] {
        let input = CallArg::Object(ObjectArg::ImmOrOwnedObject(reservation(
            mine, epoch, amount,
        )));
        cases.push((
            format!("reservation_input_{label}"),
            Spec::with_inputs(vec![input]).build(),
        ));
        cases.push((
            format!("reservation_gas_{label}"),
            Spec {
                payment: vec![reservation(mine, epoch, amount)],
                ..Spec::transfer_object()
            }
            .build(),
        ));
    }
    let theirs = sui_balance(recipient());
    cases.push((
        "reservation_gas_not_mine".to_owned(),
        Spec {
            payment: vec![reservation(theirs, EPOCH, 5)],
            ..Spec::transfer_object()
        }
        .build(),
    ));
    cases.push((
        "reservation_gas_sponsored".to_owned(),
        Spec {
            payment: vec![reservation(mine, EPOCH, 5)],
            owner: recipient(),
            ..Spec::transfer_object()
        }
        .build(),
    ));
    cases
}

fn sponsorship_cases() -> Vec<(String, TransactionData)> {
    let genesis = || TransactionKind::Genesis(GenesisTransaction { objects: vec![] });
    vec![
        (
            "sponsored".to_owned(),
            Spec {
                owner: recipient(),
                ..Spec::new()
            }
            .build(),
        ),
        (
            "system".to_owned(),
            Spec {
                kind: genesis(),
                ..Spec::new()
            }
            .build(),
        ),
        (
            "system_sponsored".to_owned(),
            Spec {
                kind: genesis(),
                owner: recipient(),
                ..Spec::new()
            }
            .build(),
        ),
        (
            "system_no_gas".to_owned(),
            Spec {
                kind: genesis(),
                payment: vec![],
                ..Spec::new()
            }
            .build(),
        ),
    ]
}

/// `SuiGasStatus::new`'s price checks, run on each transaction's gas price.
/// `None` if the checks passed and the budget setup after them panicked: it
/// asserts a non-zero price (which a zero reference gas price lets through)
/// and overflows on huge prices where no price cap applies.
fn gas_status(
    tx: &TransactionData,
    ctx: &TxValidityCheckContext<'_>,
) -> Option<Result<(), SuiError>> {
    let gas = tx.gas_data();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        SuiGasStatus::new(gas.budget, gas.price, ctx.reference_gas_price, ctx.config).map(|_| ())
    }))
    .ok()
}

/// Every case, run under every context, chain and protocol version.
pub fn vectors() -> String {
    // Expected panics are recorded as verdicts, not printed.
    std::panic::set_hook(Box::new(|_| {}));
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

    let all: Vec<&ProtocolConfig> = configs.iter().flatten().collect();
    // What each vector runs: the transaction data, or signed bytes.
    enum Subject {
        TxData(Box<TransactionData>),
        Signed(Vec<u8>),
    }
    let mut cases: Vec<(&str, String, Subject)> = vec![];
    let tx_data = expiration_cases()
        .into_iter()
        .chain(gas_cases(&all))
        .chain(address_balance_cases())
        .chain(withdrawal_cases())
        .chain(sponsorship_cases())
        .chain(crate::validity_kind::kind_cases(&all))
        .chain(crate::validity_kind::gasless_cases());
    for (label, tx) in tx_data {
        cases.push(("tx_data", label, Subject::TxData(Box::new(tx))));
    }
    for (label, tx) in gas_cases(&all) {
        if label.starts_with("price_") {
            cases.push(("gas_price", label, Subject::TxData(Box::new(tx))));
        }
    }
    for (label, bytes) in crate::validity_signed::cases() {
        cases.push(("sender_signed", label, Subject::Signed(bytes)));
    }

    let mut out = String::new();
    for (id, (check, label, subject)) in cases.into_iter().enumerate() {
        let bytes = match &subject {
            Subject::TxData(tx) => bcs::to_bytes(tx).unwrap(),
            Subject::Signed(bytes) => bytes.clone(),
        };
        writeln!(out, "tx {id} {check} {label} {}", hex(&bytes)).unwrap();
        for context in &CONTEXTS {
            for (chain, configs) in chains.iter().enumerate().map(|(i, c)| (c, &configs[i])) {
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
                        match (check, &subject) {
                            ("tx_data", Subject::TxData(tx)) => verdict(tx.validity_check(&ctx)),
                            ("gas_price", Subject::TxData(tx)) => {
                                gas_status(tx, &ctx).map_or_else(|| "panic".to_owned(), verdict)
                            }
                            ("sender_signed", Subject::Signed(bytes)) => {
                                crate::validity_signed::verdict(bytes, &ctx)
                            }
                            _ => unreachable!(),
                        }
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
