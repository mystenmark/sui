// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use containers::Bump;
use fastcrypto::groups::bls12381::{G1Element, Scalar};
use fastcrypto_tbls::nodes::Nodes;
use fastcrypto_tbls::tbls::{PartialSignature, Share, ThresholdBls};
use fastcrypto_tbls::types::ThresholdBls12381MinSig;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use workqueue::{Processor, Refusal, Refuse};

use crate::consensus::dkg::{DkgSetup, GroupKey};
use crate::epoch_store::EpochStore;
use crate::pipeline::items::GateInput;
use crate::randomness::signature_message;

/// How far past the lowest round without bytes a pushed signature may be:
/// the reference's `max_partial_sigs_rounds_ahead`.
const ROUNDS_AHEAD: u64 = 50;

/// How many rounds below the lowest without bytes this member's signatures
/// are still pushed: a peer that has them drops them.
const ROUNDS_BEHIND: u64 = 20;

/// What a peer pushes over the `Randomness` service: the reference's
/// `SendSignaturesRequest`, BCS on the wire.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendSignaturesRequest {
    pub epoch: u64,
    pub round: u64,
    /// `PartialSignature<G1Element>` BCS, one per share of the sender's, in
    /// share order.
    pub partial_sigs: Vec<Vec<u8>>,
    pub sig: Option<G1Element>,
}

/// What `SignatureCollector` takes.
#[derive(Debug)]
pub enum BeaconInput {
    /// The epoch's group key, once DKG completed. `CommitStage` → collector.
    GroupKey(GroupKey),
    /// Signatures the member at committee index `party` pushed, answered
    /// once taken. Randomness service → collector.
    Signatures {
        party: u16,
        request: SendSignaturesRequest,
        taken: oneshot::Sender<()>,
    },
    /// This member's shares, once DKG completed. `DkgParty` → collector.
    Shares(Vec<Share<Scalar>>),
    /// A randomness round reserved. `CommitStage` → collector.
    Round(u64),
    /// What this member pushes to its peers now. The p2p endpoint →
    /// collector.
    Outgoing(oneshot::Sender<Vec<SendSignaturesRequest>>),
}

/// A full queue drops the push; peers push again until the round is
/// certified.
impl Refuse for BeaconInput {
    fn refuse(self, _: Refusal) {}
}

/// Turns signatures into each randomness round's random bytes: a full
/// signature verified against the group key, or partial signatures of
/// enough weight aggregated, this member's among them once it has shares
/// (the reference's `RandomnessEventLoop`). Signatures arriving before DKG
/// completes here are kept until it does. What this member signs, and the
/// full signature once known, are what it pushes.
///
/// Inward:
/// - `CommitStage`: the group key (`BeaconInput::GroupKey`) and each round
///   reserved (`BeaconInput::Round`);
/// - `DkgParty`: this member's shares (`BeaconInput::Shares`);
/// - randomness service: pushed signatures (`BeaconInput::Signatures`);
/// - p2p endpoint: what to push (`BeaconInput::Outgoing`).
///
/// Outward:
/// - `RandomnessGate`: each round's bytes (`GateInput::Bytes`).
///
/// Writes: each round's bytes, to the epoch store, before they go out.
pub struct SignatureCollector {
    epoch: u64,
    setup: Arc<DkgSetup>,
    key: Option<GroupKey>,
    store: EpochStore,
    /// The lowest round without bytes, and those above it with bytes.
    low: u64,
    done: BTreeSet<u64>,
    /// Partial signatures pushed, by round, then by party.
    partials: BTreeMap<u64, BTreeMap<u16, Vec<PartialSignature<G1Element>>>>,
    /// Pushes kept until the group key arrives.
    early: Vec<(u16, SendSignaturesRequest)>,
    /// This member's committee index, if a member, and its shares.
    own: Option<u16>,
    shares: Option<Vec<Share<Scalar>>>,
    /// Rounds reserved without bytes, signed once there are shares.
    requested: BTreeSet<u64>,
    /// What this member pushes, by round.
    outgoing: BTreeMap<u64, SendSignaturesRequest>,
    gate: workqueue::Queue<GateInput>,
}

impl SignatureCollector {
    /// A collector for `epoch`, the rounds whose bytes `store` recorded
    /// already done.
    pub fn new(
        epoch: u64,
        setup: Arc<DkgSetup>,
        own: Option<u16>,
        store: EpochStore,
        gate: workqueue::Queue<GateInput>,
    ) -> SignatureCollector {
        let mut collector = SignatureCollector {
            epoch,
            setup,
            key: None,
            low: 0,
            done: BTreeSet::new(),
            partials: BTreeMap::new(),
            early: Vec::new(),
            own,
            shares: None,
            requested: BTreeSet::new(),
            outgoing: BTreeMap::new(),
            gate,
            store,
        };
        for (round, _) in collector.store.randomness_bytes() {
            collector.mark_done(round);
        }
        collector
    }

    fn nodes(&self) -> &Nodes<fastcrypto::groups::bls12381::G2Element> {
        self.setup.nodes()
    }

    fn wanted(&self, round: u64) -> bool {
        round >= self.low && round < self.low + ROUNDS_AHEAD && !self.done.contains(&round)
    }

    fn mark_done(&mut self, round: u64) {
        self.done.insert(round);
        while self.done.remove(&self.low) {
            self.low += 1;
        }
        self.partials
            .retain(|r, _| *r >= self.low && !self.done.contains(r));
        self.requested.remove(&round);
        let behind = self.low.saturating_sub(ROUNDS_BEHIND);
        self.outgoing.retain(|r, _| *r >= behind);
    }

    /// Signs `round` with this member's shares: its partial signatures
    /// count toward the round's, and are pushed.
    fn sign(&mut self, round: u64) {
        let (Some(own), Some(shares)) = (self.own, &self.shares) else {
            return;
        };
        let partials =
            ThresholdBls12381MinSig::partial_sign_batch(shares.iter(), &signature_message(round));
        self.outgoing.insert(
            round,
            SendSignaturesRequest {
                epoch: self.epoch,
                round,
                partial_sigs: partials
                    .iter()
                    .map(|p| bcs::to_bytes(p).expect("a partial signature serializes"))
                    .collect(),
                sig: None,
            },
        );
        self.partials
            .entry(round)
            .or_default()
            .insert(own, partials);
        if self.key.is_some() {
            self.try_aggregate(round);
        }
    }

    fn receive(&mut self, party: u16, request: SendSignaturesRequest) {
        if request.epoch != self.epoch || !self.wanted(request.round) {
            return;
        }
        let Some(key) = &self.key else {
            // Bounded by the window and the committee: one per party and
            // round, the latest.
            self.early
                .retain(|(p, r)| !(*p == party && r.round == request.round));
            self.early.push((party, request));
            return;
        };
        let round = request.round;
        let message = signature_message(round);
        if let Some(sig) = request.sig {
            if ThresholdBls12381MinSig::verify(&key.c0(), &message, &sig).is_ok() {
                self.produce(round, &sig);
            }
            return;
        }
        let Ok(expected) = self.nodes().share_ids_of(party) else {
            return;
        };
        let Some(partials) = request
            .partial_sigs
            .iter()
            .map(|bytes| bcs::from_bytes::<PartialSignature<G1Element>>(bytes).ok())
            .collect::<Option<Vec<_>>>()
        else {
            return;
        };
        if partials.len() != expected.len()
            || partials.iter().zip(&expected).any(|(p, id)| p.index != *id)
        {
            return;
        }
        self.partials
            .entry(round)
            .or_default()
            .insert(party, partials);
        self.try_aggregate(round);
    }

    /// Aggregates `round`'s partial signatures once they carry the
    /// threshold's weight; if the result does not verify, drops each party's
    /// that do not, and tries again.
    fn try_aggregate(&mut self, round: u64) {
        let key = self.key.as_ref().expect("checked by the caller");
        let threshold = self.setup.threshold();
        let message = signature_message(round);
        let Some(by_party) = self.partials.get_mut(&round) else {
            return;
        };
        loop {
            let weight: usize = by_party.values().map(Vec::len).sum();
            if weight < usize::from(threshold) {
                return;
            }
            let all = by_party.values().flatten();
            if let Ok(sig) = ThresholdBls12381MinSig::aggregate(threshold, all)
                && ThresholdBls12381MinSig::verify(&key.c0(), &message, &sig).is_ok()
            {
                self.produce(round, &sig);
                return;
            }
            let before = by_party.len();
            by_party.retain(|_, partials| {
                ThresholdBls12381MinSig::partial_verify_batch(
                    key,
                    &message,
                    partials.iter(),
                    &mut rand::thread_rng(),
                )
                .is_ok()
            });
            if by_party.len() == before {
                // Nothing to drop: wait for more.
                return;
            }
        }
    }

    fn produce(&mut self, round: u64, sig: &G1Element) {
        if let Some(outgoing) = self.outgoing.get_mut(&round) {
            outgoing.partial_sigs.clear();
            outgoing.sig = Some(*sig);
        }
        let bytes = bcs::to_bytes(sig).expect("a signature serializes");
        self.store.put_randomness_bytes(round, &bytes);
        self.mark_done(round);
        if let Err(workqueue::PushError::Full(_)) =
            self.gate.try_push(GateInput::Bytes { round, bytes })
        {
            panic!("the randomness gate refused bytes");
        }
    }
}

impl Processor for SignatureCollector {
    type Input = BeaconInput;
    type Output = ();

    fn process(&mut self, input: BeaconInput, _: &mut Bump) -> Option<()> {
        match input {
            BeaconInput::GroupKey(key) => {
                self.key = Some(key);
                for (party, request) in std::mem::take(&mut self.early) {
                    self.receive(party, request);
                }
                for round in self.partials.keys().copied().collect::<Vec<_>>() {
                    self.try_aggregate(round);
                }
            }
            BeaconInput::Shares(shares) => {
                self.shares = Some(shares);
                for round in self.requested.clone() {
                    self.sign(round);
                }
            }
            BeaconInput::Round(round) => {
                if self.wanted(round) {
                    self.requested.insert(round);
                    self.sign(round);
                }
            }
            BeaconInput::Outgoing(answer) => {
                let _ = answer.send(self.outgoing.values().cloned().collect());
            }
            BeaconInput::Signatures {
                party,
                request,
                taken,
            } => {
                self.receive(party, request);
                let _ = taken.send(());
            }
        }
        None
    }
}
