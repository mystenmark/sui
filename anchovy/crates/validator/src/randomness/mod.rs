// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The random beacon's bytes: the signatures peers push for each
//! randomness round, collected and verified (`SignatureCollector`), and the
//! gate that holds a round's randomness state update until they arrive
//! (`RandomnessGate`). `docs/randomness_design.md` §5.

mod collector;
mod gate;

pub use collector::{BeaconInput, SendSignaturesRequest, SignatureCollector};
pub use gate::RandomnessGate;

/// The message a randomness round's signature signs: the reference's
/// `RandomnessRound::signature_message`.
pub fn signature_message(round: u64) -> Vec<u8> {
    let mut message = b"random_beacon round ".to_vec();
    message.extend_from_slice(&round.to_le_bytes());
    message
}
