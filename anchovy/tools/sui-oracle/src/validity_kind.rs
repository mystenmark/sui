// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Validity vectors for `TransactionKind::validity_check` (programmable
//! transaction limits, inputs, commands, type arguments, identifiers,
//! argument indices, randomness; flags for system kinds) and for the
//! gasless rules.

use move_core_types::account_address::AccountAddress;
use sui_protocol_config::ProtocolConfig;
use sui_types::base_types::{ObjectID, SequenceNumber};
use sui_types::crypto::RandomnessRound;
use sui_types::digests::{AdditionalConsensusStateDigest, ConsensusCommitDigest};
use sui_types::messages_consensus::{
    ConsensusCommitPrologueV2, ConsensusCommitPrologueV3, ConsensusCommitPrologueV4,
    ConsensusDeterminedVersionAssignments,
};
use sui_types::transaction::{
    Argument, AuthenticatorStateUpdate, CallArg, Command, EndOfEpochTransactionKind, ObjectArg,
    ProgrammableMoveCall, ProgrammableTransaction, RandomnessStateUpdate, SharedObjectMutability,
    TransactionData, TransactionKind, WithdrawFrom,
};
use sui_types::type_input::{StructInput, TypeInput};

use crate::validity::{
    CHAIN_ID, EPOCH, Spec, boundaries, chain_identifier, object, valid_during, withdrawal,
};

fn ptb(inputs: Vec<CallArg>, commands: Vec<Command>) -> TransactionKind {
    TransactionKind::ProgrammableTransaction(ProgrammableTransaction { inputs, commands })
}

fn tx(kind: TransactionKind) -> TransactionData {
    Spec {
        kind,
        ..Spec::new()
    }
    .build()
}

fn pure_u64() -> CallArg {
    CallArg::Pure(bcs::to_bytes(&1u64).unwrap())
}

fn split() -> Command {
    Command::SplitCoins(Argument::GasCoin, vec![Argument::Input(0)])
}

fn owned(n: u8) -> CallArg {
    CallArg::Object(ObjectArg::ImmOrOwnedObject(object(n)))
}

fn shared(id: ObjectID, mutability: SharedObjectMutability) -> CallArg {
    CallArg::Object(ObjectArg::SharedObject {
        id,
        initial_shared_version: SequenceNumber::from_u64(1),
        mutability,
    })
}

fn package(n: u64) -> ObjectID {
    let mut id = [0x5a; 32];
    id[..8].copy_from_slice(&n.to_le_bytes());
    ObjectID::new(id)
}

fn call(
    module: &str,
    function: &str,
    type_arguments: Vec<TypeInput>,
    arguments: Vec<Argument>,
) -> Command {
    Command::MoveCall(Box::new(ProgrammableMoveCall {
        package: package(0),
        module: module.to_owned(),
        function: function.to_owned(),
        type_arguments,
        arguments,
    }))
}

fn framework_call(module: &str, function: &str, type_arguments: Vec<TypeInput>) -> Command {
    Command::MoveCall(Box::new(ProgrammableMoveCall {
        package: ObjectID::from_single_byte(2),
        module: module.to_owned(),
        function: function.to_owned(),
        type_arguments,
        arguments: vec![Argument::Input(0)],
    }))
}

fn struct_type(
    address: AccountAddress,
    module: &str,
    name: &str,
    params: Vec<TypeInput>,
) -> TypeInput {
    TypeInput::Struct(Box::new(StructInput {
        address,
        module: module.to_owned(),
        name: name.to_owned(),
        type_params: params,
    }))
}

fn nested_vector(depth: u64) -> TypeInput {
    (0..depth).fold(TypeInput::U8, |t, _| TypeInput::Vector(Box::new(t)))
}

fn publish(modules: usize, deps: u64) -> Command {
    Command::Publish(vec![vec![0]; modules], (0..deps).map(package).collect())
}

pub(crate) fn kind_cases(configs: &[&ProtocolConfig]) -> Vec<(String, TransactionData)> {
    let mut cases = vec![];
    let mut add = |label: String, kind: TransactionKind| cases.push((label, tx(kind)));

    for n in boundaries(configs, |c| {
        c.max_programmable_tx_commands_as_option().map(u64::from)
    }) {
        add(
            format!("commands_{n}"),
            ptb(vec![pure_u64()], (0..n).map(|_| split()).collect()),
        );
    }

    let transfer_two = || {
        Command::TransferObjects(
            vec![Argument::Input(0), Argument::Input(1)],
            Argument::Input(2),
        )
    };
    let address = CallArg::Pure(bcs::to_bytes(&[0xb2u8; 32]).unwrap());
    add(
        "duplicate_owned".to_owned(),
        ptb(
            vec![owned(0x31), owned(0x31), address.clone()],
            vec![transfer_two()],
        ),
    );
    add(
        "duplicate_owned_shared".to_owned(),
        ptb(
            vec![
                owned(0x31),
                shared(ObjectID::new([0x31; 32]), SharedObjectMutability::Mutable),
                address.clone(),
            ],
            vec![transfer_two()],
        ),
    );
    add(
        "distinct_owned".to_owned(),
        ptb(
            vec![owned(0x31), owned(0x32), address.clone()],
            vec![transfer_two()],
        ),
    );

    // Input objects counted through packages: a publish's dependencies.
    for n in boundaries(configs, |c| c.max_input_objects_as_option()) {
        add(
            format!("input_objects_{n}"),
            ptb(vec![], vec![publish(1, n)]),
        );
    }

    for n in boundaries(configs, |c| {
        c.max_pure_argument_size_as_option().map(u64::from)
    }) {
        add(
            format!("pure_{n}"),
            ptb(vec![CallArg::Pure(vec![7; n as usize])], vec![split()]),
        );
    }

    add(
        "receiving".to_owned(),
        ptb(
            vec![
                CallArg::Object(ObjectArg::Receiving(object(0x41))),
                pure_u64(),
            ],
            vec![Command::SplitCoins(
                Argument::GasCoin,
                vec![Argument::Input(1)],
            )],
        ),
    );
    for (label, mutability) in [
        ("immutable", SharedObjectMutability::Immutable),
        ("mutable", SharedObjectMutability::Mutable),
        ("non_exclusive", SharedObjectMutability::NonExclusiveWrite),
    ] {
        add(
            format!("shared_{label}"),
            ptb(
                vec![shared(ObjectID::new([0x42; 32]), mutability), pure_u64()],
                vec![Command::SplitCoins(
                    Argument::GasCoin,
                    vec![Argument::Input(1)],
                )],
            ),
        );
    }

    // `Balance<vector^k<u8>>` has k + 2 type nodes.
    for n in boundaries(configs, |c| c.max_accumulator_type_nodes_as_option()) {
        let depth = n.saturating_sub(2);
        let withdraw = CallArg::FundsWithdrawal(sui_types::transaction::FundsWithdrawalArg {
            reservation: sui_types::transaction::Reservation::MaxAmountU64(5),
            type_arg: sui_types::transaction::WithdrawalTypeArg::Balance(
                nested_vector(depth).to_type_tag().unwrap(),
            ),
            withdraw_from: WithdrawFrom::Sender,
        });
        add(
            format!("withdraw_nodes_{n}"),
            ptb(vec![withdraw, pure_u64()], vec![split_input(1)]),
        );
    }

    for n in boundaries(configs, |c| c.max_publish_or_upgrade_per_ptb_as_option()) {
        add(
            format!("publishes_{n}"),
            ptb(vec![], (0..n).map(|_| publish(1, 0)).collect()),
        );
    }
    for n in boundaries(configs, |c| {
        c.max_modules_in_publish_as_option().map(u64::from)
    }) {
        add(
            format!("modules_{n}"),
            ptb(vec![], vec![publish(n as usize, 0)]),
        );
    }
    for n in boundaries(configs, |c| {
        c.max_package_dependencies_as_option().map(u64::from)
    }) {
        add(
            format!("dependencies_{n}"),
            ptb(vec![], vec![publish(1, n)]),
        );
    }

    add(
        "empty_transfer".to_owned(),
        ptb(
            vec![pure_u64()],
            vec![Command::TransferObjects(vec![], Argument::Input(0))],
        ),
    );
    add(
        "empty_split".to_owned(),
        ptb(vec![], vec![Command::SplitCoins(Argument::GasCoin, vec![])]),
    );
    add(
        "empty_merge".to_owned(),
        ptb(vec![], vec![Command::MergeCoins(Argument::GasCoin, vec![])]),
    );
    add(
        "empty_make_move_vec".to_owned(),
        ptb(vec![], vec![Command::MakeMoveVec(None, vec![])]),
    );
    add(
        "typed_empty_make_move_vec".to_owned(),
        ptb(
            vec![],
            vec![Command::MakeMoveVec(Some(TypeInput::U8), vec![])],
        ),
    );
    add("empty_publish".to_owned(), ptb(vec![], vec![publish(0, 0)]));

    for n in boundaries(configs, |c| c.max_arguments_as_option().map(u64::from)) {
        let args = vec![Argument::Input(0); n as usize];
        add(
            format!("split_arguments_{n}"),
            ptb(
                vec![pure_u64()],
                vec![Command::SplitCoins(Argument::GasCoin, args.clone())],
            ),
        );
        add(
            format!("call_arguments_{n}"),
            ptb(vec![pure_u64()], vec![call("m", "f", vec![], args.clone())]),
        );
        add(
            format!("make_move_vec_arguments_{n}"),
            ptb(vec![pure_u64()], vec![Command::MakeMoveVec(None, args)]),
        );
    }

    // Type arguments: count across a call, depth of one, identifiers.
    for n in boundaries(configs, |c| c.max_type_arguments_as_option().map(u64::from)) {
        add(
            format!("type_arguments_{n}"),
            ptb(
                vec![],
                vec![call("m", "f", vec![TypeInput::U8; n as usize], vec![])],
            ),
        );
    }
    for n in boundaries(configs, |c| {
        c.max_type_argument_depth_as_option().map(u64::from)
    }) {
        add(
            format!("type_depth_{n}"),
            ptb(vec![], vec![call("m", "f", vec![nested_vector(n)], vec![])]),
        );
        add(
            format!("make_move_vec_type_depth_{n}"),
            ptb(
                vec![],
                vec![Command::MakeMoveVec(Some(nested_vector(n)), vec![])],
            ),
        );
    }
    let a = AccountAddress::from(package(1));
    for (label, ty) in [
        ("valid", struct_type(a, "m", "S", vec![])),
        ("bad_module", struct_type(a, "1m", "S", vec![])),
        ("bad_name", struct_type(a, "m", "S-", vec![])),
        ("underscore", struct_type(a, "_", "S", vec![])),
        ("underscore_x", struct_type(a, "_x", "S", vec![])),
        // Visited last parameter first: the bad identifier is found before
        // the too-deep vector.
        (
            "order",
            struct_type(
                a,
                "m",
                "S",
                vec![nested_vector(40), struct_type(a, "m", "", vec![])],
            ),
        ),
        (
            "order_reversed",
            struct_type(
                a,
                "m",
                "S",
                vec![struct_type(a, "m", "", vec![]), nested_vector(40)],
            ),
        ),
    ] {
        add(
            format!("type_identifier_{label}"),
            ptb(vec![], vec![call("m", "f", vec![ty], vec![])]),
        );
    }
    for (label, module, function) in [
        ("bad_module", "m-", "f"),
        ("bad_function", "m", "f-"),
        ("empty_function", "m", ""),
        ("digit_module", "9", "f"),
    ] {
        add(
            format!("call_identifier_{label}"),
            ptb(vec![], vec![call(module, function, vec![], vec![])]),
        );
    }

    // Argument indices.
    for (label, arg) in [
        ("input_ok", Argument::Input(0)),
        ("input_out", Argument::Input(1)),
        ("result_self", Argument::Result(0)),
        ("nested_result_later", Argument::NestedResult(3, 0)),
    ] {
        add(
            format!("argument_{label}"),
            ptb(
                vec![pure_u64()],
                vec![Command::SplitCoins(Argument::GasCoin, vec![arg])],
            ),
        );
    }
    add(
        "argument_result_earlier".to_owned(),
        ptb(
            vec![pure_u64()],
            vec![
                split(),
                Command::MergeCoins(Argument::GasCoin, vec![Argument::Result(0)]),
            ],
        ),
    );

    // Randomness: once used, only transfers and merges may follow.
    let random = || {
        shared(
            ObjectID::from_single_byte(8),
            SharedObjectMutability::Immutable,
        )
    };
    let use_random = || call("m", "f", vec![], vec![Argument::Input(0)]);
    for (label, after) in [
        (
            "then_merge",
            Command::MergeCoins(Argument::GasCoin, vec![Argument::Result(0)]),
        ),
        (
            "then_transfer",
            Command::TransferObjects(vec![Argument::Result(0)], Argument::Input(1)),
        ),
        (
            "then_split",
            Command::SplitCoins(Argument::GasCoin, vec![Argument::Input(1)]),
        ),
    ] {
        add(
            format!("random_{label}"),
            ptb(vec![random(), pure_u64()], vec![use_random(), after]),
        );
    }
    add(
        "random_unused".to_owned(),
        ptb(
            vec![random(), pure_u64()],
            vec![split_input(1), split_input(1)],
        ),
    );

    // System kinds, gated by flags.
    let digest = ConsensusCommitDigest::default();
    add(
        "prologue_v2".to_owned(),
        TransactionKind::ConsensusCommitPrologueV2(ConsensusCommitPrologueV2 {
            epoch: EPOCH,
            round: 1,
            commit_timestamp_ms: 2,
            consensus_commit_digest: digest,
        }),
    );
    add(
        "prologue_v3".to_owned(),
        TransactionKind::ConsensusCommitPrologueV3(ConsensusCommitPrologueV3 {
            epoch: EPOCH,
            round: 1,
            sub_dag_index: None,
            commit_timestamp_ms: 2,
            consensus_commit_digest: digest,
            consensus_determined_version_assignments:
                ConsensusDeterminedVersionAssignments::CancelledTransactions(vec![]),
        }),
    );
    add(
        "prologue_v4".to_owned(),
        TransactionKind::ConsensusCommitPrologueV4(ConsensusCommitPrologueV4 {
            epoch: EPOCH,
            round: 1,
            sub_dag_index: None,
            commit_timestamp_ms: 2,
            consensus_commit_digest: digest,
            consensus_determined_version_assignments:
                ConsensusDeterminedVersionAssignments::CancelledTransactions(vec![]),
            additional_state_digest: AdditionalConsensusStateDigest::ZERO,
        }),
    );
    add(
        "authenticator_state_update".to_owned(),
        TransactionKind::AuthenticatorStateUpdate(AuthenticatorStateUpdate {
            epoch: EPOCH,
            round: 1,
            new_active_jwks: vec![],
            authenticator_obj_initial_shared_version: SequenceNumber::from_u64(1),
        }),
    );
    add(
        "randomness_state_update".to_owned(),
        TransactionKind::RandomnessStateUpdate(RandomnessStateUpdate {
            epoch: EPOCH,
            randomness_round: RandomnessRound(1),
            random_bytes: vec![1, 2, 3],
            randomness_obj_initial_shared_version: SequenceNumber::from_u64(1),
        }),
    );
    add(
        "programmable_system".to_owned(),
        TransactionKind::ProgrammableSystemTransaction(ProgrammableTransaction {
            inputs: vec![],
            commands: vec![],
        }),
    );
    for (label, kind) in [
        (
            "authenticator_state_create",
            EndOfEpochTransactionKind::AuthenticatorStateCreate,
        ),
        (
            "randomness_state_create",
            EndOfEpochTransactionKind::RandomnessStateCreate,
        ),
        (
            "deny_list_state_create",
            EndOfEpochTransactionKind::DenyListStateCreate,
        ),
        (
            "bridge_state_create",
            EndOfEpochTransactionKind::BridgeStateCreate(chain_identifier(CHAIN_ID)),
        ),
        (
            "bridge_committee_init",
            EndOfEpochTransactionKind::BridgeCommitteeInit(SequenceNumber::from_u64(1)),
        ),
        (
            "accumulator_root_create",
            EndOfEpochTransactionKind::AccumulatorRootCreate,
        ),
        (
            "coin_registry_create",
            EndOfEpochTransactionKind::CoinRegistryCreate,
        ),
        (
            "display_registry_create",
            EndOfEpochTransactionKind::DisplayRegistryCreate,
        ),
        (
            "address_alias_state_create",
            EndOfEpochTransactionKind::AddressAliasStateCreate,
        ),
        (
            "forwarding_address_registry_create",
            EndOfEpochTransactionKind::ForwardingAddressRegistryCreate,
        ),
    ] {
        add(
            format!("end_of_epoch_{label}"),
            TransactionKind::EndOfEpochTransaction(vec![kind]),
        );
    }
    add(
        "end_of_epoch_empty".to_owned(),
        TransactionKind::EndOfEpochTransaction(vec![]),
    );
    cases
}

fn split_input(i: u16) -> Command {
    Command::SplitCoins(Argument::GasCoin, vec![Argument::Input(i)])
}

/// Gasless: address-balance gas at price and budget 0.
fn gasless(kind: TransactionKind) -> TransactionData {
    Spec {
        kind,
        payment: vec![],
        price: 0,
        budget: 0,
        expiration: valid_during(Some(EPOCH), Some(EPOCH + 1)),
        ..Spec::new()
    }
    .build()
}

pub(crate) fn gasless_cases() -> Vec<(String, TransactionData)> {
    let testnet_usdc = struct_type(
        AccountAddress::from_hex_literal(
            "0xa1ec7fc00a6f40db9693ad1415d0c193ad3906494428cf252621037bd7117e29",
        )
        .unwrap(),
        "usdc",
        "USDC",
        vec![],
    );
    let mainnet_usdc = struct_type(
        AccountAddress::from_hex_literal(
            "0xdba34672e30cb065b1f93e3ab55318768fd6fef66c15942c9f7cb846e2f900e7",
        )
        .unwrap(),
        "usdc",
        "USDC",
        vec![],
    );
    let sui = struct_type(
        AccountAddress::from_hex_literal("0x2").unwrap(),
        "sui",
        "SUI",
        vec![],
    );
    let balance = |t: TypeInput| {
        struct_type(
            AccountAddress::from_hex_literal("0x2").unwrap(),
            "balance",
            "Balance",
            vec![t],
        )
    };
    let send = |t: TypeInput| framework_call("balance", "send_funds", vec![t]);
    let withdraw = || withdrawal(5, WithdrawFrom::Sender);

    let mut cases = vec![];
    let mut add = |label: &str, inputs: Vec<CallArg>, commands: Vec<Command>| {
        cases.push((format!("gasless_{label}"), gasless(ptb(inputs, commands))));
    };
    add("no_commands", vec![], vec![]);
    add(
        "testnet_usdc",
        vec![withdraw()],
        vec![send(testnet_usdc.clone())],
    );
    add(
        "mainnet_usdc",
        vec![withdraw()],
        vec![send(mainnet_usdc.clone())],
    );
    add("sui", vec![withdraw()], vec![send(sui.clone())]);
    for (module, function) in [
        ("balance", "redeem_funds"),
        ("balance", "split"),
        ("balance", "zero"),
        ("coin", "into_balance"),
        ("coin", "redeem_funds"),
        ("coin", "send_funds"),
        ("coin", "put"),
        ("coin", "transfer"),
    ] {
        add(
            &format!("{module}_{function}"),
            vec![withdraw()],
            vec![framework_call(module, function, vec![mainnet_usdc.clone()])],
        );
    }
    add(
        "withdrawal_split_balance",
        vec![withdraw()],
        vec![framework_call(
            "funds_accumulator",
            "withdrawal_split",
            vec![balance(mainnet_usdc.clone())],
        )],
    );
    add(
        "withdrawal_split_bare",
        vec![withdraw()],
        vec![framework_call(
            "funds_accumulator",
            "withdrawal_split",
            vec![mainnet_usdc.clone()],
        )],
    );
    add(
        "no_type_args",
        vec![withdraw()],
        vec![framework_call("balance", "send_funds", vec![])],
    );
    add(
        "two_type_args",
        vec![withdraw()],
        vec![framework_call(
            "balance",
            "send_funds",
            vec![sui.clone(), sui.clone()],
        )],
    );
    add(
        "other_package",
        vec![withdraw()],
        vec![call(
            "balance",
            "send_funds",
            vec![mainnet_usdc.clone()],
            vec![Argument::Input(0)],
        )],
    );
    add(
        "bad_identifier",
        vec![withdraw()],
        vec![send(struct_type(
            AccountAddress::from_hex_literal("0x2").unwrap(),
            "u-sdc",
            "USDC",
            vec![],
        ))],
    );
    add(
        "transfer",
        vec![withdraw(), pure_u64()],
        vec![Command::TransferObjects(
            vec![Argument::Input(0)],
            Argument::Input(1),
        )],
    );
    add(
        "merge",
        vec![withdraw()],
        vec![Command::MergeCoins(
            Argument::Input(0),
            vec![Argument::Input(0)],
        )],
    );
    add(
        "split",
        vec![withdraw(), pure_u64()],
        vec![Command::SplitCoins(
            Argument::Input(0),
            vec![Argument::Input(1)],
        )],
    );
    add(
        "receiving",
        vec![
            withdraw(),
            CallArg::Object(ObjectArg::Receiving(object(0x41))),
        ],
        vec![send(mainnet_usdc.clone())],
    );
    add(
        "unused_object",
        vec![withdraw(), owned(0x51)],
        vec![send(mainnet_usdc.clone())],
    );
    add(
        "unused_withdrawal",
        vec![withdraw(), withdraw()],
        vec![send(mainnet_usdc.clone())],
    );
    add(
        "one_unused_pure",
        vec![withdraw(), pure_u64()],
        vec![send(mainnet_usdc.clone())],
    );
    add(
        "two_unused_pure",
        vec![withdraw(), pure_u64(), pure_u64()],
        vec![send(mainnet_usdc.clone())],
    );
    for n in [32, 33] {
        add(
            &format!("pure_{n}_bytes"),
            vec![withdraw(), CallArg::Pure(vec![1; n])],
            vec![send(mainnet_usdc.clone())],
        );
    }
    cases
}
