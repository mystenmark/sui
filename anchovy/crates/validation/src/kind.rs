// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `TransactionKind::validity_check` and what it calls: the programmable
//! transaction's inputs and commands, and flags for the system kinds.

use containers::Bump;
use messages::base::ObjectId;
use messages::system_transaction::EndOfEpochTransactionKind;
use messages::transaction::{
    Argument, CallArg, Command, ObjectArg, ProgrammableMoveCall, ProgrammableTransaction,
    SharedObjectMutability, TransactionData, TransactionKind,
};
use messages::type_tag::{TypeInput, TypeTag};
use protocol_config::{PerObjectCongestionControlMode, ProtocolConfig};

use crate::{Error, ErrorKind};

/// The randomness state object, `0x8`.
const RANDOMNESS_STATE: ObjectId = ObjectId::from_u16(8);

fn unsupported(what: &str) -> Error {
    Error::new(ErrorKind::Unsupported, what)
}

fn size_limit(what: &str, limit: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorKind::SizeLimitExceeded,
        format!("{what}: limit {limit}"),
    )
}

pub fn validity_check(
    tx: &TransactionData<'_>,
    config: &ProtocolConfig,
    bump: &Bump,
) -> Result<(), Error> {
    let require = |enabled: bool, what: &str| {
        if enabled {
            Ok(())
        } else {
            Err(unsupported(what))
        }
    };
    match &tx.kind {
        TransactionKind::ProgrammableTransaction(pt) => {
            programmable_transaction(tx, pt, config, bump)
        }
        TransactionKind::ChangeEpoch(_)
        | TransactionKind::Genesis(_)
        | TransactionKind::ConsensusCommitPrologue(_) => Ok(()),
        TransactionKind::ConsensusCommitPrologueV2(_) => require(
            config.include_consensus_digest_in_prologue(),
            "ConsensusCommitPrologueV2",
        ),
        TransactionKind::ConsensusCommitPrologueV3(_) => require(
            config.record_consensus_determined_version_assignments_in_prologue(),
            "ConsensusCommitPrologueV3",
        ),
        TransactionKind::ConsensusCommitPrologueV4(_) => require(
            config.record_additional_state_digest_in_prologue(),
            "ConsensusCommitPrologueV4",
        ),
        TransactionKind::EndOfEpochTransaction(txs) => {
            require(
                config.end_of_epoch_transaction_supported(),
                "EndOfEpochTransaction",
            )?;
            txs.iter()
                .try_for_each(|t| end_of_epoch_transaction(t, config))
        }
        TransactionKind::AuthenticatorStateUpdate(_) => require(
            config.enable_jwk_consensus_updates(),
            "authenticator state updates",
        ),
        TransactionKind::RandomnessStateUpdate(_) => {
            require(config.random_beacon(), "randomness state updates")
        }
        TransactionKind::ProgrammableSystemTransaction(_) => {
            require(config.enable_accumulators(), "accumulators")
        }
    }
}

fn end_of_epoch_transaction(
    tx: &EndOfEpochTransactionKind<'_>,
    config: &ProtocolConfig,
) -> Result<(), Error> {
    use EndOfEpochTransactionKind as K;
    let (enabled, what) = match tx {
        K::ChangeEpoch(_) => return Ok(()),
        K::AuthenticatorStateCreate | K::AuthenticatorStateExpire { .. } => (
            config.enable_jwk_consensus_updates(),
            "authenticator state updates",
        ),
        K::RandomnessStateCreate => (config.random_beacon(), "random beacon"),
        K::DenyListStateCreate => (config.enable_coin_deny_list(), "coin deny list"),
        K::BridgeStateCreate(_) => (config.bridge(), "bridge"),
        K::BridgeCommitteeInit(_) => {
            if !config.bridge() {
                return Err(unsupported("bridge"));
            }
            (
                config.should_try_to_finalize_bridge_committee(),
                "finalizing the bridge committee",
            )
        }
        K::StoreExecutionTimeObservations(_) => (
            matches!(
                config.per_object_congestion_control_mode(),
                PerObjectCongestionControlMode::ExecutionTimeEstimate(_)
            ),
            "execution time estimation",
        ),
        K::AccumulatorRootCreate => (config.create_root_accumulator_object(), "accumulators"),
        K::CoinRegistryCreate => (config.enable_coin_registry(), "coin registry"),
        K::DisplayRegistryCreate => (config.enable_display_registry(), "display registry"),
        K::AddressAliasStateCreate => (config.address_aliases(), "address aliases"),
        K::WriteAccumulatorStorageCost { .. } => (config.enable_accumulators(), "accumulators"),
        K::ForwardingAddressRegistryCreate => (
            config.create_forwarding_address_registry(),
            "forwarding address registry",
        ),
    };
    if enabled {
        Ok(())
    } else {
        Err(unsupported(what))
    }
}

fn programmable_transaction(
    tx: &TransactionData<'_>,
    pt: &ProgrammableTransaction<'_>,
    config: &ProtocolConfig,
    bump: &Bump,
) -> Result<(), Error> {
    if pt.commands.len() >= config.max_programmable_tx_commands() as usize {
        return Err(size_limit(
            "commands in a programmable transaction",
            config.max_programmable_tx_commands(),
        ));
    }

    // Owned and shared inputs, coin reservations aside, must be distinct;
    // packages the commands use count once each.
    let input_objects = distinct_input_objects(pt, bump)?;
    let total = input_objects + tx.index.packages.len() + tx.index.receiving.len();
    if total > config.max_input_objects() as usize {
        return Err(size_limit(
            "input and receiving objects",
            config.max_input_objects(),
        ));
    }

    for input in pt.inputs {
        call_arg(input, config)?;
    }

    if let Some(max) = config.max_publish_or_upgrade_per_ptb {
        let publishes = pt
            .commands
            .iter()
            .filter(|c| matches!(c, Command::Publish(..) | Command::Upgrade(..)))
            .count() as u64;
        if publishes > max {
            return Err(Error::new(
                ErrorKind::MaxPublishCountExceeded,
                format!("{publishes} publishes or upgrades, at most {max}"),
            ));
        }
    }

    for command in pt.commands {
        command_check(command, config)?;
    }

    if config.validate_ptb_argument_indices() {
        argument_indices(pt)?;
    }

    // A command using randomness may be followed only by transfers and
    // merges, so the random value cannot be tested and acted on.
    if let Some(random) = pt.inputs.iter().position(|arg| {
        matches!(arg, CallArg::Object(ObjectArg::SharedObject(s)) if s.id == RANDOMNESS_STATE)
    }) {
        if !config.random_beacon() {
            return Err(unsupported("randomness"));
        }
        // Positions are u16 on the wire; the size limit keeps inputs below
        // 65536.
        let random = Argument::Input(random as u16);
        let mut used = false;
        for command in pt.commands {
            if !used {
                used = crate::transaction_data::command_arguments(command).any(|a| a == random);
            } else if !matches!(command, Command::TransferObjects(..) | Command::MergeCoins(..)) {
                return Err(Error::new(
                    ErrorKind::PostRandomCommandRestrictions,
                    "only TransferObjects and MergeCoins may follow a use of randomness",
                ));
            }
        }
    }
    Ok(())
}

/// How many owned and shared object inputs there are, all distinct.
fn distinct_input_objects(pt: &ProgrammableTransaction<'_>, bump: &Bump) -> Result<usize, Error> {
    let mut ids = containers::Vec::with_capacity_in(pt.inputs.len(), bump);
    for input in pt.inputs {
        match input {
            CallArg::Object(ObjectArg::ImmOrOwnedObject(r)) if !r.is_coin_reservation() => {
                ids.push(r.id);
            }
            CallArg::Object(ObjectArg::SharedObject(s)) => ids.push(s.id),
            _ => {}
        }
    }
    let count = ids.len();
    ids.sort_unstable();
    if ids.windows(2).any(|w| w[0] == w[1]) {
        return Err(Error::new(
            ErrorKind::DuplicateObjectRefInput,
            "an object is an input more than once",
        ));
    }
    Ok(count)
}

fn call_arg(arg: &CallArg<'_>, config: &ProtocolConfig) -> Result<(), Error> {
    match arg {
        CallArg::Pure(bytes) => {
            if bytes.len() >= config.max_pure_argument_size() as usize {
                return Err(size_limit(
                    "pure argument size",
                    config.max_pure_argument_size(),
                ));
            }
        }
        CallArg::Object(ObjectArg::ImmOrOwnedObject(r)) => {
            if r.is_coin_reservation() && !config.enable_coin_reservation_obj_refs() {
                return Err(unsupported("coin reservations"));
            }
        }
        CallArg::Object(ObjectArg::SharedObject(s)) => {
            if s.mutability() == SharedObjectMutability::NonExclusiveWrite
                && !config.enable_non_exclusive_writes()
            {
                return Err(unsupported("non-exclusive writes"));
            }
        }
        CallArg::Object(ObjectArg::Receiving(_)) => {
            if !config.receive_objects() {
                return Err(unsupported("receiving objects"));
            }
        }
        CallArg::FundsWithdrawal(w) => {
            let messages::transaction::WithdrawalTypeArg::Balance(ty) = w.get().type_arg;
            if let Some(max) = config.max_accumulator_type_nodes {
                // The type is `Balance<ty>`: one node more than `ty`.
                if node_count(&ty).saturating_add(1) > max {
                    return Err(size_limit("type nodes in a funds accumulator type", max));
                }
            }
        }
    }
    Ok(())
}

fn node_count(ty: &TypeTag<'_>) -> u64 {
    match ty {
        TypeTag::Vector(inner) => node_count(inner.get()).saturating_add(1),
        TypeTag::Struct(s) => s
            .get()
            .type_params
            .iter()
            .fold(1u64, |n, p| n.saturating_add(node_count(p))),
        _ => 1,
    }
}

fn command_check(command: &Command<'_>, config: &ProtocolConfig) -> Result<(), Error> {
    let empty = || Error::new(ErrorKind::EmptyCommandInput, "a command has no inputs");
    let max_arguments = config.max_arguments() as usize;
    match command {
        Command::MoveCall(call) => move_call(call, config),
        Command::TransferObjects(args, _)
        | Command::MergeCoins(_, args)
        | Command::SplitCoins(_, args) => {
            if args.is_empty() {
                return Err(empty());
            }
            if args.len() >= max_arguments {
                return Err(size_limit("arguments in a command", max_arguments));
            }
            Ok(())
        }
        Command::MakeMoveVec(ty, args) => {
            if ty.is_none() && args.is_empty() {
                return Err(empty());
            }
            if let Some(ty) = ty {
                type_input(ty, config, &mut 0, 1)?;
            }
            if args.len() >= max_arguments {
                return Err(size_limit("arguments in a command", max_arguments));
            }
            Ok(())
        }
        Command::Publish(modules, deps) | Command::Upgrade(modules, deps, _, _) => {
            if modules.is_empty() {
                return Err(empty());
            }
            if modules.len() >= config.max_modules_in_publish() as usize {
                return Err(size_limit(
                    "modules in a publish or upgrade",
                    config.max_modules_in_publish(),
                ));
            }
            if let Some(max) = config.max_package_dependencies
                && deps.len() >= max as usize
            {
                return Err(size_limit("package dependencies", max));
            }
            Ok(())
        }
    }
}

fn move_call(call: &ProgrammableMoveCall<'_>, config: &ProtocolConfig) -> Result<(), Error> {
    // The reference's list of blocked functions is empty.
    let mut count = 0;
    for ty in call.type_arguments {
        type_input(ty, config, &mut count, 1)?;
    }
    if call.arguments.len() >= config.max_arguments() as usize {
        return Err(size_limit(
            "arguments in a move call",
            config.max_arguments(),
        ));
    }
    if config.validate_identifier_inputs()
        && (!is_valid_identifier(call.module) || !is_valid_identifier(call.function))
    {
        return Err(Error::new(ErrorKind::InvalidIdentifier, call.module));
    }
    Ok(())
}

/// The reference walks a type with a stack, so it visits a struct's
/// parameters last to first; visiting them in reverse here reports the
/// same error first. `count` runs across all of a call's type arguments.
fn type_input(
    ty: &TypeInput<'_>,
    config: &ProtocolConfig,
    count: &mut usize,
    depth: u32,
) -> Result<(), Error> {
    *count += 1;
    if *count >= config.max_type_arguments() as usize {
        return Err(size_limit(
            "type arguments in a call",
            config.max_type_arguments(),
        ));
    }
    if depth >= config.max_type_argument_depth() {
        return Err(size_limit(
            "type argument depth",
            config.max_type_argument_depth(),
        ));
    }
    match ty {
        TypeTag::Vector(inner) => type_input(inner.get(), config, count, depth + 1),
        TypeTag::Struct(s) => {
            let s = s.get();
            if config.validate_identifier_inputs() {
                if !is_valid_identifier(s.module) {
                    return Err(Error::new(ErrorKind::InvalidIdentifier, s.module));
                }
                if !is_valid_identifier(s.name) {
                    return Err(Error::new(ErrorKind::InvalidIdentifier, s.name));
                }
            }
            s.type_params
                .iter()
                .rev()
                .try_for_each(|p| type_input(p, config, count, depth + 1))
        }
        _ => Ok(()),
    }
}

/// Move's identifier grammar: `[a-zA-Z][a-zA-Z0-9_]*` or `_[a-zA-Z0-9_]+`.
pub fn is_valid_identifier(s: &str) -> bool {
    let b = s.as_bytes();
    let rest_valid = || {
        b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_')
    };
    match b.first() {
        Some(c) if c.is_ascii_alphabetic() => rest_valid(),
        Some(b'_') => b.len() > 1 && rest_valid(),
        _ => false,
    }
}

/// `Input` must name an input and `Result`/`NestedResult` an earlier
/// command.
fn argument_indices(pt: &ProgrammableTransaction<'_>) -> Result<(), Error> {
    for (i, command) in pt.commands.iter().enumerate() {
        for argument in crate::transaction_data::command_arguments(command) {
            let bad = match argument {
                Argument::Input(n) => n as usize >= pt.inputs.len(),
                Argument::Result(n) | Argument::NestedResult(n, _) => n as usize >= i,
                Argument::GasCoin => false,
            };
            if bad {
                return Err(Error::new(
                    ErrorKind::InvalidArgumentIndex,
                    format!("command {i} uses {argument:?}"),
                ));
            }
        }
    }
    Ok(())
}
