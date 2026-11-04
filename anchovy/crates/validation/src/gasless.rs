// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `ProgrammableTransaction::validate_gasless_transaction`: what a
//! transaction that pays no gas may do. Only moving allowed funds, through
//! a fixed set of framework functions, with bounded unused inputs.

use containers::Bump;
use messages::base::AccountAddress;
use messages::transaction::{
    Argument, CallArg, Command, ObjectArg, ProgrammableMoveCall, ProgrammableTransaction,
};
use messages::type_tag::TypeTag;
use sui_protocol_config::ProtocolConfig;

use crate::kind::is_valid_identifier;
use crate::transaction_data::command_arguments;
use crate::{Error, UserInputError};

fn unsupported(what: impl Into<String>) -> Error {
    Error::new(UserInputError::Unsupported(what.into()))
}

/// What a whitelisted function's type argument names.
#[derive(Clone, Copy)]
enum TypeArg {
    /// The fund type itself, as in `send_funds<USDC>`.
    Fund,
    /// `Balance<T>`, whose `T` is the fund type.
    Balance,
}

/// `0x2::module::function` and the constraint on its one type argument.
const GASLESS_FUNCTIONS: [(&str, &str, TypeArg); 9] = [
    ("balance", "send_funds", TypeArg::Fund),
    ("balance", "redeem_funds", TypeArg::Fund),
    ("balance", "split", TypeArg::Fund),
    ("balance", "zero", TypeArg::Fund),
    ("funds_accumulator", "withdrawal_split", TypeArg::Balance),
    ("coin", "into_balance", TypeArg::Fund),
    ("coin", "redeem_funds", TypeArg::Fund),
    ("coin", "send_funds", TypeArg::Fund),
    ("coin", "put", TypeArg::Fund),
];

fn framework() -> AccountAddress {
    let mut a = [0; 32];
    a[31] = 2;
    AccountAddress(a)
}

pub fn validate(
    pt: &ProgrammableTransaction<'_>,
    config: &ProtocolConfig,
    bump: &Bump,
) -> Result<(), Error> {
    if pt.commands.is_empty() {
        return Err(unsupported(
            "Gasless transactions must have at least one command",
        ));
    }
    if pt
        .inputs
        .iter()
        .any(|i| matches!(i, CallArg::Object(ObjectArg::Receiving(_))))
    {
        return Err(unsupported(
            "Gasless transactions do not support Receiving object inputs",
        ));
    }
    for command in pt.commands {
        match command {
            Command::MoveCall(call) => move_call(call, config)?,
            Command::MergeCoins(..) | Command::SplitCoins(..) => {}
            _ => {
                return Err(unsupported(
                    "Gasless transactions only support MoveCall, MergeCoins, and SplitCoins commands",
                ));
            }
        }
    }
    inputs(pt, config, bump)
}

fn move_call(call: &ProgrammableMoveCall<'_>, config: &ProtocolConfig) -> Result<(), Error> {
    let not_allowed = || {
        unsupported(format!(
            "Function {}::{}::{} is not supported in gasless transactions",
            call.package, call.module, call.function
        ))
    };
    if call.package.0 != framework().0 {
        return Err(not_allowed());
    }
    let Some(&(_, _, constraint)) = GASLESS_FUNCTIONS
        .iter()
        .find(|(m, f, _)| *m == call.module && *f == call.function)
    else {
        return Err(not_allowed());
    };
    let [type_arg] = call.type_arguments else {
        return Err(unsupported(format!(
            "Function {}::{}::{} requires 1 type arguments, but {} were provided",
            call.package,
            call.module,
            call.function,
            call.type_arguments.len()
        )));
    };
    // The reference converts the argument to a type tag, which checks
    // identifiers.
    if let Some(invalid) = invalid_identifier(type_arg) {
        return Err(unsupported(format!(
            "Failed to parse type argument {type_arg} as a type tag: \
             Invalid identifier '{invalid}'"
        )));
    }
    let fund = match constraint {
        TypeArg::Fund => type_arg,
        TypeArg::Balance => balance_param(type_arg)
            .ok_or_else(|| unsupported(format!("Expected a type Balance<_> but got {type_arg}")))?,
    };
    if !is_allowed_token(fund, config) {
        return Err(unsupported(format!(
            "Fund type {fund} is not currently allowed in gasless transactions"
        )));
    }
    Ok(())
}

/// The first invalid identifier in `ty`, in the order the reference's
/// `TypeInput::to_type_tag` checks them: a struct's type parameters, then its
/// module and name.
fn invalid_identifier<'t>(ty: &TypeTag<'t>) -> Option<&'t str> {
    match ty {
        TypeTag::Vector(inner) => invalid_identifier(inner.get()),
        TypeTag::Struct(s) => {
            let s = s.get();
            s.type_params
                .iter()
                .find_map(invalid_identifier)
                .or_else(|| {
                    [s.module, s.name]
                        .into_iter()
                        .find(|i| !is_valid_identifier(i))
                })
        }
        _ => None,
    }
}

/// `T` of `0x2::balance::Balance<T>`.
fn balance_param<'t>(ty: &'t TypeTag<'t>) -> Option<&'t TypeTag<'t>> {
    let TypeTag::Struct(s) = ty else {
        return None;
    };
    let s = s.get();
    match s.type_params {
        [param] if s.address.0 == framework().0 && s.module == "balance" && s.name == "Balance" => {
            Some(param)
        }
        _ => None,
    }
}

/// Whether `ty` is one of the config's allowed token types. Those are all
/// `0x<address>::module::Name` without type parameters, so they are matched
/// by parts rather than parsed into type tags.
fn is_allowed_token(ty: &TypeTag<'_>, config: &ProtocolConfig) -> bool {
    let TypeTag::Struct(s) = ty else {
        return false;
    };
    let s = s.get();
    if !s.type_params.is_empty() {
        return false;
    }
    config
        .gasless_allowed_token_types()
        .iter()
        .any(|(allowed, _)| {
            let (address, module, name) = split_struct_tag(allowed)
                .unwrap_or_else(|| panic!("unsupported gasless token type {allowed:?}"));
            address == s.address.0 && module == s.module && name == s.name
        })
}

/// `0x<hex>::module::Name`, the address left-padded to 32 bytes.
fn split_struct_tag(s: &str) -> Option<([u8; 32], &str, &str)> {
    let mut parts = s.split("::");
    let (address, module, name) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || name.contains('<') {
        return None;
    }
    let hex = address.strip_prefix("0x")?.as_bytes();
    if hex.is_empty() || hex.len() > 64 {
        return None;
    }
    // Right-aligned: the last digit is the low nibble of the last byte.
    let mut bytes = [0u8; 32];
    for (i, digit) in hex.iter().rev().enumerate() {
        let nibble = char::from(*digit).to_digit(16)? as u8;
        bytes[31 - i / 2] |= nibble << (4 * (i % 2));
    }
    Some((bytes, module, name))
}

/// Every object and withdrawal input must be used; pure inputs are bounded
/// in size, and in number when unused.
fn inputs(
    pt: &ProgrammableTransaction<'_>,
    config: &ProtocolConfig,
    bump: &Bump,
) -> Result<(), Error> {
    let mut used = containers::Vec::with_capacity_in(pt.inputs.len(), bump);
    used.resize(pt.inputs.len(), false);
    for command in pt.commands {
        for argument in command_arguments(command) {
            if let Argument::Input(i) = argument
                && let Some(slot) = used.get_mut(i as usize)
            {
                *slot = true;
            }
        }
    }

    let max_pure_bytes = config.get_gasless_max_pure_input_bytes();
    let mut unused_pure = 0u64;
    for (i, input) in pt.inputs.iter().enumerate() {
        match input {
            CallArg::Pure(bytes) => {
                if bytes.len() as u64 > max_pure_bytes {
                    return Err(unsupported(format!(
                        "Input {i} has size {} bytes, but gasless transactions \
                         allow at most {max_pure_bytes} bytes per Pure input",
                        bytes.len()
                    )));
                }
                if !used[i] {
                    unused_pure += 1;
                }
            }
            CallArg::Object(_) if !used[i] => {
                return Err(unsupported(format!(
                    "Gasless transactions do not allow unused Object inputs (input {i})"
                )));
            }
            CallArg::FundsWithdrawal(_) if !used[i] => {
                return Err(unsupported(format!(
                    "Gasless transactions do not allow unused FundsWithdrawal inputs (input {i})"
                )));
            }
            CallArg::Object(_) | CallArg::FundsWithdrawal(_) => {}
        }
    }
    let max_unused = config.get_gasless_max_unused_inputs();
    if unused_pure > max_unused {
        return Err(unsupported(format!(
            "Gasless transactions allow at most {max_unused} unused Pure inputs, but found {unused_pure}"
        )));
    }
    Ok(())
}
