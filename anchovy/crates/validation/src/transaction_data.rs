// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `TransactionData::validity_check`.

use messages::transaction::{AllowedProposers, TransactionData, TransactionExpiration};

use crate::{Context, Error, ErrorKind};

/// A transaction may name this many allowed proposers without paying for the
/// amplification; beyond it, SIP-45 caps the set at gas price / RGP.
const MAX_UNPAID_ALLOWED_PROPOSERS: u64 = 3;

pub fn validity_check(tx: &TransactionData<'_>, ctx: &Context<'_>) -> Result<(), Error> {
    check_expiration(tx, ctx)?;
    Ok(())
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
