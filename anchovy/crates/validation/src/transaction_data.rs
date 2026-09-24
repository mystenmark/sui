// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `TransactionData::validity_check`.

use containers::Bump;
use messages::base::{ObjectId, ObjectRef};
use messages::transaction::{
    AllowedProposers, Argument, Command, Reservation, TransactionData, TransactionExpiration,
    TransactionKind, WithdrawFrom,
};

use crate::{Context, Error, ErrorKind, accumulator, gasless, kind};

/// A transaction may name this many allowed proposers without paying for the
/// amplification; beyond it, SIP-45 caps the set at gas price / RGP.
const MAX_UNPAID_ALLOWED_PROPOSERS: u64 = 3;

/// Funds withdrawals, coin reservations and implicit address-balance gas
/// combined. Hard-coded in the reference, not a protocol config value.
const MAX_WITHDRAWALS: usize = 10;

/// Gas model versions from this one cap the gas price.
const GAS_PRICE_CAP_FROM_GAS_MODEL: u64 = 4;

/// `bump` holds temporaries; checking allocates nothing else.
pub fn validity_check(
    tx: &TransactionData<'_>,
    ctx: &Context<'_>,
    bump: &Bump,
) -> Result<(), Error> {
    check_expiration(tx, ctx)?;
    if has_funds_withdrawals(tx) {
        check_funds_withdrawals(tx, ctx)?;
    }
    check_gas_payment(tx, ctx)?;
    check_gas_object_count(tx, ctx)?;
    check_coin_reservations_as_gas(tx, ctx)?;
    if !is_system_tx(tx) {
        check_gas_price_and_budget(tx, ctx)?;
    }
    kind::validity_check(tx, ctx.config, bump)?;
    if is_gasless(tx, ctx)
        && let TransactionKind::ProgrammableTransaction(pt) = &tx.kind
    {
        gasless::validate(pt, ctx.config, bump)?;
    }
    check_sponsorship(tx)?;
    Ok(())
}

/// The static part of the reference's `SuiGasStatus::new`, which runs at
/// signing time after objects are loaded: the price must be at least the
/// reference gas price, and under the cap.
pub fn check_gas_price(gas_price: u64, ctx: &Context<'_>) -> Result<(), Error> {
    if gas_price < ctx.reference_gas_price {
        return Err(Error::new(
            ErrorKind::GasPriceUnderRGP,
            format!("gas price {gas_price} under {}", ctx.reference_gas_price),
        ));
    }
    if ctx.config.gas_model_version() >= GAS_PRICE_CAP_FROM_GAS_MODEL
        && gas_price >= ctx.config.max_gas_price()
    {
        return Err(Error::new(
            ErrorKind::GasPriceTooHigh,
            format!(
                "gas price {gas_price} at or over {}",
                ctx.config.max_gas_price()
            ),
        ));
    }
    Ok(())
}

fn is_system_tx(tx: &TransactionData<'_>) -> bool {
    !matches!(tx.kind, TransactionKind::ProgrammableTransaction(_))
}

/// The first-class address-balance payment: no gas objects, in a user
/// transaction. Paying with a coin reservation does not count.
fn is_gas_paid_from_address_balance(tx: &TransactionData<'_>) -> bool {
    tx.gas_data.payment.is_empty() && matches!(tx.kind, TransactionKind::ProgrammableTransaction(_))
}

pub(crate) fn is_gasless(tx: &TransactionData<'_>, ctx: &Context<'_>) -> bool {
    ctx.config.enable_gasless() && is_gas_paid_from_address_balance(tx) && tx.gas_data.price == 0
}

/// A one- or two-epoch window: the validator remembers what it executed
/// for that long, so a replay is either expired or recognized.
fn is_replay_protected(expiration: &TransactionExpiration<'_>) -> bool {
    let (TransactionExpiration::ValidDuring(window) | TransactionExpiration::Validity(window, _)) =
        expiration
    else {
        return false;
    };
    matches!(
        (window.min_epoch, window.max_epoch),
        (Some(min), Some(max)) if max == min || max == min.saturating_add(1)
    )
}

fn has_funds_withdrawals(tx: &TransactionData<'_>) -> bool {
    (is_gas_paid_from_address_balance(tx) && tx.gas_data.budget > 0)
        || !tx.index.funds_withdrawals.is_empty()
        || !tx.index.coin_reservations.is_empty()
}

/// The amount and epoch a coin reservation's digest carries: amount (u64
/// LE), epoch (u32 LE), then the twenty-byte `0xac` marker.
fn reservation_amount_and_epoch(r: &ObjectRef) -> (u64, u64) {
    let d = &r.digest.bytes;
    let amount = u64::from_le_bytes(d[0..8].try_into().expect("eight bytes"));
    let epoch = u32::from_le_bytes(d[8..12].try_into().expect("four bytes"));
    (amount, u64::from(epoch))
}

fn check_funds_withdrawals(tx: &TransactionData<'_>, ctx: &Context<'_>) -> Result<(), Error> {
    let config = ctx.config;
    if tx.gas_data.payment.is_empty() && !config.enable_address_balance_gas_payments() {
        return Err(Error::new(ErrorKind::MissingGasPayment, "no gas payment"));
    }
    if !config.enable_accumulators() {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "address balance withdrawals are not enabled",
        ));
    }

    for withdrawal in tx.index.funds_withdrawals {
        let withdrawal = withdrawal.get();
        match withdrawal.withdraw_from {
            WithdrawFrom::Sender => {}
            WithdrawFrom::Sponsor => {
                return Err(Error::new(
                    ErrorKind::InvalidWithdrawReservation,
                    "sponsor withdrawals are not supported",
                ));
            }
            // The allowance itself is checked once inputs are loaded.
            WithdrawFrom::SenderAllowance { .. } => {
                if !config.enable_allowances() {
                    return Err(Error::new(
                        ErrorKind::Unsupported,
                        "allowance withdrawals are not enabled",
                    ));
                }
            }
        }
        let Reservation::MaxAmountU64(amount) = withdrawal.reservation;
        if amount == 0 {
            return Err(Error::new(
                ErrorKind::InvalidWithdrawReservation,
                "withdrawal amount must be non-zero",
            ));
        }
    }

    // Reservations are good for their epoch and the next, like a two-epoch
    // `ValidDuring`.
    for reservation in tx.index.coin_reservations {
        let (amount, epoch) = reservation_amount_and_epoch(reservation);
        if epoch != ctx.epoch && epoch + 1 != ctx.epoch {
            return Err(Error::new(
                ErrorKind::TransactionExpired,
                format!("coin reservation for epoch {epoch}"),
            ));
        }
        if amount == 0 {
            return Err(Error::new(
                ErrorKind::InvalidWithdrawReservation,
                "coin reservation amount must be non-zero",
            ));
        }
    }

    let implicit_gas = usize::from(
        config.enable_address_balance_gas_payments() && is_gas_paid_from_address_balance(tx),
    );
    let count = tx.index.funds_withdrawals.len() + tx.index.coin_reservations.len() + implicit_gas;
    if count > MAX_WITHDRAWALS {
        return Err(Error::new(
            ErrorKind::InvalidWithdrawReservation,
            format!("{count} withdrawals, at most {MAX_WITHDRAWALS}"),
        ));
    }
    Ok(())
}

/// Address-balance gas when enabled and used; otherwise gas objects are
/// required.
fn check_gas_payment(tx: &TransactionData<'_>, ctx: &Context<'_>) -> Result<(), Error> {
    let config = ctx.config;
    if !(config.enable_accumulators()
        && config.enable_address_balance_gas_payments()
        && is_gas_paid_from_address_balance(tx))
    {
        if tx.gas_data.payment.is_empty() {
            return Err(Error::new(ErrorKind::MissingGasPayment, "no gas payment"));
        }
        return Ok(());
    }

    if config.address_balance_gas_reject_gas_coin_arg()
        && let TransactionKind::ProgrammableTransaction(pt) = &tx.kind
        && pt
            .commands
            .iter()
            .any(|c| command_arguments(c).any(|a| a == Argument::GasCoin))
    {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "the gas coin is not an argument when gas comes from an address balance",
        ));
    }

    if config.address_balance_gas_check_rgp_at_signing()
        && !is_gasless(tx, ctx)
        && tx.gas_data.price < ctx.reference_gas_price
    {
        return Err(Error::new(
            ErrorKind::GasPriceUnderRGP,
            format!(
                "gas price {} under {}",
                tx.gas_data.price, ctx.reference_gas_price
            ),
        ));
    }

    // Legacy rule: address-balance gas needs a replay-protecting window even
    // with owned inputs. The relaxed rule leaves that to the stateful checks.
    if !config.relax_valid_during_for_owned_inputs() {
        if matches!(tx.expiration, TransactionExpiration::None) {
            // The reference's error, kept for compatibility.
            return Err(Error::new(
                ErrorKind::MissingGasPayment,
                "address balance gas needs an expiration",
            ));
        }
        if !is_replay_protected(&tx.expiration) {
            return Err(Error::new(
                ErrorKind::InvalidExpiration,
                "address balance gas needs a one- or two-epoch ValidDuring",
            ));
        }
    }
    Ok(())
}

fn check_gas_object_count(tx: &TransactionData<'_>, ctx: &Context<'_>) -> Result<(), Error> {
    let len = tx.gas_data.payment.len();
    let max = ctx.config.max_gas_payment_objects() as usize;
    // The old check was off by one.
    let within = if ctx.config.correct_gas_payment_limit_check() {
        len <= max
    } else {
        len < max
    };
    if !within {
        return Err(Error::new(
            ErrorKind::SizeLimitExceeded,
            format!("{len} gas payment objects, limit {max}"),
        ));
    }
    Ok(())
}

/// A coin reservation paying gas must draw on the sender's own SUI balance.
fn check_coin_reservations_as_gas(
    tx: &TransactionData<'_>,
    ctx: &Context<'_>,
) -> Result<(), Error> {
    let not_owned = || {
        Error::new(
            ErrorKind::GasObjectNotOwnedObject,
            "coin reservation gas must be the sender's SUI balance",
        )
    };
    let mut reservations = tx
        .gas_data
        .payment
        .iter()
        .filter(|r| r.is_coin_reservation())
        .peekable();
    if reservations.peek().is_none() {
        return Ok(());
    }
    if !ctx.config.enable_coin_reservation_obj_refs() {
        return Err(not_owned());
    }
    let sui_balance = accumulator::sui_balance_id(tx.sender);
    for reservation in reservations {
        if tx.gas_data.owner != tx.sender {
            return Err(not_owned());
        }
        if unmask(&reservation.id, &ctx.chain_identifier.bytes) != sui_balance {
            return Err(not_owned());
        }
    }
    Ok(())
}

/// Reservation ids are xored with the chain identifier, so that a
/// reservation cannot be replayed on another chain.
fn unmask(id: &ObjectId, chain: &[u8; 32]) -> ObjectId {
    ObjectId(std::array::from_fn(|i| id.0[i] ^ chain[i]))
}

fn check_gas_price_and_budget(tx: &TransactionData<'_>, ctx: &Context<'_>) -> Result<(), Error> {
    let config = ctx.config;
    let gas = &tx.gas_data;
    if config.gas_model_version() >= GAS_PRICE_CAP_FROM_GAS_MODEL
        && gas.price >= config.max_gas_price()
    {
        return Err(Error::new(
            ErrorKind::GasPriceTooHigh,
            format!(
                "gas price {} at or over {}",
                gas.price,
                config.max_gas_price()
            ),
        ));
    }

    let max_budget = config.max_tx_gas();
    if gas.budget > max_budget {
        return Err(Error::new(
            ErrorKind::GasBudgetTooHigh,
            format!("gas budget {} over {max_budget}", gas.budget),
        ));
    }

    if is_gasless(tx, ctx) {
        if gas.budget != 0 {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "gas budget must be 0 for gasless transactions",
            ));
        }
        return Ok(());
    }
    // The reference's `SuiCostTable::new`, whose checked arithmetic panics
    // on overflow. It cannot overflow: the multiplier exists only in
    // versions that cap the price above.
    let min_budget = if config.txn_base_cost_as_multiplier() {
        config.base_tx_cost_fixed().saturating_mul(gas.price)
    } else {
        config.base_tx_cost_fixed()
    };
    if gas.budget < min_budget {
        return Err(Error::new(
            ErrorKind::GasBudgetTooLow,
            format!("gas budget {} under {min_budget}", gas.budget),
        ));
    }
    Ok(())
}

/// Only user transactions may have gas paid by someone other than the sender.
fn check_sponsorship(tx: &TransactionData<'_>) -> Result<(), Error> {
    if tx.gas_data.owner != tx.sender && is_system_tx(tx) {
        return Err(Error::new(
            ErrorKind::UnsupportedSponsoredTransactionKind,
            "only programmable transactions may be sponsored",
        ));
    }
    Ok(())
}

/// A command's arguments; order does not matter to the callers.
pub(crate) fn command_arguments<'c>(
    command: &'c Command<'_>,
) -> impl Iterator<Item = Argument> + 'c {
    let (single, many): (Option<Argument>, &[Argument]) = match command {
        Command::MoveCall(call) => (None, call.arguments),
        Command::TransferObjects(objects, recipient) => (Some(*recipient), objects),
        Command::SplitCoins(coin, amounts) => (Some(*coin), amounts),
        Command::MergeCoins(target, sources) => (Some(*target), sources),
        Command::Publish(..) => (None, &[]),
        Command::MakeMoveVec(_, elements) => (None, elements),
        Command::Upgrade(_, _, _, ticket) => (Some(*ticket), &[]),
    };
    single.into_iter().chain(many.iter().copied())
}

fn check_expiration(tx: &TransactionData<'_>, ctx: &Context<'_>) -> Result<(), Error> {
    let (window, allowed_proposers) = match tx.expiration {
        TransactionExpiration::None => return Ok(()),
        TransactionExpiration::Epoch(max_epoch) => {
            if ctx.epoch > max_epoch {
                return Err(Error::new(ErrorKind::TransactionExpired, "past max epoch"));
            }
            return Ok(());
        }
        TransactionExpiration::ValidDuring(window) => (window, None),
        TransactionExpiration::Validity(window, allowed_proposers) => {
            // Gated even when the proposer set is unusable, so that a transaction
            // accepted after the upgrade cannot be accepted before it.
            if !ctx.config.allowed_proposers() {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "restricting the proposers of a transaction is not supported",
                ));
            }
            (window, allowed_proposers)
        }
    };

    // A set recorded for another epoch is ignored, as if none were named.
    if let Some(allowed) = allowed_proposers.filter(|a| a.epoch == ctx.epoch) {
        check_allowed_proposers(tx, ctx, &allowed)?;
    }

    if window.min_timestamp.is_some() || window.max_timestamp.is_some() {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "timestamp-based expiration is not supported",
        ));
    }

    // Legacy rule: a window spans one epoch, or two with multi-epoch
    // expiration. The relaxed rule allows any range, leaving replay
    // protection to the stateful checks.
    if !ctx.config.relax_valid_during_for_owned_inputs() {
        let (Some(min), Some(max)) = (window.min_epoch, window.max_epoch) else {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "both min_epoch and max_epoch must be specified",
            ));
        };
        if ctx.config.enable_multi_epoch_transaction_expiration() {
            if max != min && max != min.saturating_add(1) {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "max_epoch must be at most min_epoch + 1",
                ));
            }
        } else if min != max {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "min_epoch must equal max_epoch",
            ));
        }
    }

    if *window.chain != ctx.chain_identifier {
        return Err(Error::new(ErrorKind::InvalidChainId, "wrong chain"));
    }

    if window.min_epoch.is_some_and(|min| ctx.epoch < min)
        || window.max_epoch.is_some_and(|max| ctx.epoch > max)
    {
        return Err(Error::new(
            ErrorKind::TransactionExpired,
            "outside the validity window",
        ));
    }
    Ok(())
}

fn check_allowed_proposers(
    tx: &TransactionData<'_>,
    ctx: &Context<'_>,
    allowed: &AllowedProposers<'_>,
) -> Result<(), Error> {
    let proposers = allowed.proposers;

    // SIP-45: a larger set amplifies consensus cost, paid for with a raised
    // gas price, and no price buys more proposers than there are validators.
    // Checked first so the checks below walk a bounded list.
    let max_proposers = MAX_UNPAID_ALLOWED_PROPOSERS
        .max(tx.gas_data.price / ctx.reference_gas_price.max(1))
        .min(u64::from(ctx.committee_size));
    if proposers.len() as u64 > max_proposers {
        return Err(Error::new(
            ErrorKind::InvalidExpiration,
            format!(
                "{} allowed proposers, at most {max_proposers} permitted",
                proposers.len()
            ),
        ));
    }

    if !proposers.is_sorted_by(|a, b| a.get() < b.get()) {
        return Err(Error::new(
            ErrorKind::InvalidExpiration,
            "allowed proposers must be strictly increasing",
        ));
    }

    if let Some(i) = proposers.iter().find(|i| i.get() >= ctx.committee_size) {
        return Err(Error::new(
            ErrorKind::InvalidExpiration,
            format!(
                "allowed proposer {} is outside a committee of {}",
                i.get(),
                ctx.committee_size
            ),
        ));
    }
    Ok(())
}
