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
use protocol_config::ProtocolConfig;

use crate::kind::is_valid_identifier;
use crate::transaction_data::command_arguments;
use crate::{Error, ErrorKind};

fn unsupported(what: impl Into<String>) -> Error {
    Error::new(ErrorKind::Unsupported, what)
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
        return Err(unsupported("gasless transactions need a command"));
    }
    if pt
        .inputs
        .iter()
        .any(|i| matches!(i, CallArg::Object(ObjectArg::Receiving(_))))
    {
        return Err(unsupported("gasless transactions cannot receive objects"));
    }
    for command in pt.commands {
        match command {
            Command::MoveCall(call) => move_call(call, config)?,
            Command::MergeCoins(..) | Command::SplitCoins(..) => {}
            _ => {
                return Err(unsupported(
                    "gasless transactions allow only MoveCall, MergeCoins and SplitCoins",
                ));
            }
        }
    }
    inputs(pt, config, bump)
}

fn move_call(call: &ProgrammableMoveCall<'_>, config: &ProtocolConfig) -> Result<(), Error> {
    let not_allowed = || {
        unsupported(format!(
            "{}::{} is not allowed in gasless transactions",
            call.module, call.function
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
        return Err(unsupported("gasless functions take one type argument"));
    };
    // The reference converts the argument to a type tag, which checks
    // identifiers.
    if !identifiers_valid(type_arg) {
        return Err(unsupported("type argument is not a valid type tag"));
    }
    let fund = match constraint {
        TypeArg::Fund => type_arg,
        TypeArg::Balance => balance_param(type_arg)
            .ok_or_else(|| unsupported("expected Balance<_> as the type argument"))?,
    };
    if !is_allowed_token(fund, config) {
        return Err(unsupported("fund type not allowed in gasless transactions"));
    }
    Ok(())
}

fn identifiers_valid(ty: &TypeTag<'_>) -> bool {
    match ty {
        TypeTag::Vector(inner) => identifiers_valid(inner.get()),
        TypeTag::Struct(s) => {
            let s = s.get();
            is_valid_identifier(s.module)
                && is_valid_identifier(s.name)
                && s.type_params.iter().all(identifiers_valid)
        }
        _ => true,
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
    let hex = address.strip_prefix("0x")?;
    if hex.is_empty() || hex.len() > 64 {
        return None;
    }
    let mut bytes = [0; 32];
    let padded = format!("{hex:0>64}");
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&padded[2 * i..2 * i + 2], 16).ok()?;
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
                        "pure input {i} over {max_pure_bytes} bytes"
                    )));
                }
                if !used[i] {
                    unused_pure += 1;
                }
            }
            CallArg::Object(_) | CallArg::FundsWithdrawal(_) if !used[i] => {
                return Err(unsupported(format!("input {i} is unused")));
            }
            CallArg::Object(_) | CallArg::FundsWithdrawal(_) => {}
        }
    }
    let max_unused = config.get_gasless_max_unused_inputs();
    if unused_pure > max_unused {
        return Err(unsupported(format!(
            "{unused_pure} unused pure inputs, at most {max_unused}"
        )));
    }
    Ok(())
}
