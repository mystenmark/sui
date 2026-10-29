// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The random beacon, past what the commit handler decides: this member's
//! part in DKG (`DkgParty`); each randomness round's signatures, signed,
//! collected and verified (`SignatureCollector`); and the gate that holds a
//! round's randomness state update until its bytes arrive
//! (`RandomnessGate`). `docs/randomness_design.md`.

mod collector;
mod gate;
mod party;

pub use collector::{BeaconInput, SendSignaturesRequest, SignatureCollector};
pub use gate::RandomnessGate;
pub use party::{DkgParty, PartyInput};

/// The message a randomness round's signature signs: the reference's
/// `RandomnessRound::signature_message`.
pub fn signature_message(round: u64) -> Vec<u8> {
    let mut message = b"random_beacon round ".to_vec();
    message.extend_from_slice(&round.to_le_bytes());
    message
}
