// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use containers::Bump;
use fastcrypto::groups::bls12381::G1Element;
use fastcrypto_tbls::nodes::Nodes;
use fastcrypto_tbls::tbls::{PartialSignature, ThresholdBls};
use fastcrypto_tbls::types::ThresholdBls12381MinSig;
use serde::{Deserialize, Serialize};
use workqueue::Processor;

use crate::consensus::dkg::{DkgSetup, GroupKey};
use crate::epoch_store::EpochStore;
use crate::pipeline::items::GateInput;
use crate::randomness::signature_message;

/// How far past the lowest round without bytes a pushed signature may be:
/// the reference's `max_partial_sigs_rounds_ahead`.
const ROUNDS_AHEAD: u64 = 50;

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
    /// Signatures the member at committee index `party` pushed. Randomness
    /// service → collector.
    Signatures {
        party: u16,
        request: SendSignaturesRequest,
    },
}

/// Turns the signatures peers push into each randomness round's random
/// bytes: a full signature verified against the group key, or partial
/// signatures of enough weight aggregated (the reference's
/// `RandomnessEventLoop`, receiving only). Signatures arriving before DKG
/// completes here are kept until it does.
///
/// Inward:
/// - `CommitStage`: the group key (`BeaconInput::GroupKey`);
/// - randomness service: pushed signatures (`BeaconInput::Signatures`).
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
    gate: workqueue::Queue<GateInput>,
}

impl SignatureCollector {
    /// A collector for `epoch`, the rounds whose bytes `store` recorded
    /// already done.
    pub fn new(
        epoch: u64,
        setup: Arc<DkgSetup>,
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
            }
            BeaconInput::Signatures { party, request } => self.receive(party, request),
        }
        None
    }
}
