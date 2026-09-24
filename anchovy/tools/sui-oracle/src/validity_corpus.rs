// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Validity and verification verdicts for every transaction of a mainnet
//! checkpoint: `sui-oracle --validity-corpus FILE.chk...` writes
//! `FILE.validity`, one line per transaction:
//!
//! ```text
//! <index> <validity_check verdict> <verification verdict>
//! ```
//!
//! The context is fixed (below), not the one the transaction ran under:
//! the verdicts only have to agree with ours under the same inputs.
//! zkLogin fails verification for want of mainnet's JWKs, on both sides.

use std::fmt::Write as _;

use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use sui_types::digests::{ChainIdentifier, CheckpointDigest, MAINNET_CHAIN_IDENTIFIER_BASE58};
use sui_types::full_checkpoint_content::CheckpointData;
use sui_types::transaction::TxValidityCheckContext;

use crate::validity::verdict_of;

/// The corpus context: the checkpoint's epoch, mainnet's chain identifier
/// (the genesis checkpoint's digest), and the reference gas price and
/// committee size below. The runner hard-codes the same values.
pub(crate) const RGP: u64 = 1;
pub(crate) const COMMITTEE_SIZE: u32 = 100;

pub(crate) fn verdicts(checkpoint: &CheckpointData) -> String {
    let config = ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Mainnet);
    let epoch = checkpoint.checkpoint_summary.epoch;
    let ctx = TxValidityCheckContext {
        config: &config,
        epoch,
        chain_identifier: ChainIdentifier::from(CheckpointDigest::new(
            bs58::decode(MAINNET_CHAIN_IDENTIFIER_BASE58)
                .into_vec()
                .unwrap()
                .try_into()
                .unwrap(),
        )),
        reference_gas_price: RGP,
        committee_size: COMMITTEE_SIZE,
    };
    let mut out = String::new();
    for (i, tx) in checkpoint.transactions.iter().enumerate() {
        let signed = tx.transaction.data();
        let validity = match signed.validity_check(&ctx) {
            Ok(size) => format!("ok:{size}"),
            Err(e) => verdict_of(e),
        };
        let verified = crate::validity_verify::verify(signed, &config, Chain::Mainnet, epoch);
        writeln!(out, "{i} {validity} {verified}").unwrap();
    }
    out
}
