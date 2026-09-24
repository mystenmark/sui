// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Static transaction validation: the checks that depend only on a
//! transaction's bytes and on epoch-level inputs. Each function mirrors one
//! reference function and applies its checks in the same order, so that the
//! first failure, and so the error, is the same.

pub mod accumulator;
pub mod error;
pub mod gasless;
pub mod kind;
pub mod sender_signed;
pub mod signature;
pub mod transaction_data;
pub mod verify;

use messages::base::ChainIdentifier;
use protocol_config::ProtocolConfig;

pub use error::{Error, ErrorKind};

/// The epoch-level inputs, as the reference's `TxValidityCheckContext`.
#[derive(Clone, Copy, Debug)]
pub struct Context<'a> {
    pub config: &'a ProtocolConfig,
    pub epoch: u64,
    pub chain_identifier: ChainIdentifier,
    pub reference_gas_price: u64,
    /// Validators in the epoch's committee, which bounds committee indices.
    pub committee_size: u32,
}

/// Everything static a validator checks on a submitted transaction, in the
/// reference's order: what decoding checks, `validity_check`, then the
/// signatures. `aliases` are as for [`verify::verify_signatures`].
pub fn check<'a>(
    tx: &messages::transaction::SenderSignedData<'a>,
    ctx: &Context<'_>,
    verifier: &verify::Verifier,
    aliases: &[(messages::base::SuiAddress, &[messages::base::SuiAddress])],
    bump: &'a containers::Bump,
) -> Result<sender_signed::Checked<'a>, Error> {
    let checked = sender_signed::validity_check(tx, ctx, bump)?;
    verify::verify_signatures(tx, checked.signatures, ctx.epoch, verifier, aliases, bump)?;
    Ok(checked)
}
