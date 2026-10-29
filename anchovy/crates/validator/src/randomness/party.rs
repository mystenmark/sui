// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::sync::Arc;

use containers::Bump;
use fastcrypto::groups::bls12381::{G2Element, Scalar};
use fastcrypto::serde_helpers::ToFromByteArray;
use fastcrypto_tbls::dkg_v1::{Party, ProcessedMessage, UsedProcessedMessages};
use fastcrypto_tbls::ecies_v1;
use sui_types::base_types::AuthorityName;
use sui_types::messages_consensus::{
    ConsensusTransaction, VersionedDkgConfirmation, VersionedDkgMessage,
};
use workqueue::{Processor, Refusal, Refuse};

use crate::consensus::dkg::{DkgConfirmation, DkgMessage, DkgSetup};
use crate::randomness::BeaconInput;

/// What `DkgParty` takes: DKG as the commit handler decided it, which every
/// party sees alike. `CommitStage` → party.
#[derive(Debug)]
pub enum PartyInput {
    /// A message the handler processed.
    Message(Box<DkgMessage>),
    /// The messages merged, by sender.
    Merged(Vec<u16>),
    /// DKG completed, with these confirmations.
    Completed(Vec<DkgConfirmation>),
    /// The state restored at start has been sent: whether this member's
    /// message, and its confirmation, were sequenced already.
    Started {
        message_seen: bool,
        confirmation_seen: bool,
    },
}

/// Inside the pipeline a full queue is a sizing bug.
impl Refuse for PartyInput {
    fn refuse(self, _: Refusal) {
        panic!("the DKG party refused input");
    }
}

/// Takes part in the random beacon's DKG as this member: deals its message,
/// decrypts its shares of the others', confirms the merge with its
/// complaints, and completes with its shares (the reference's `Party`
/// role). When it acts follows the commit handler, which observes DKG as
/// every validator does; what it submits goes through consensus. Restarted,
/// it rebuilds from the state the handler restored: decrypting is
/// deterministic, and a message dealt again is dropped as a duplicate.
///
/// Inward:
/// - `CommitStage`: DKG as decided (`PartyInput`).
///
/// Outward:
/// - consensus: its DKG message and confirmation;
/// - `SignatureCollector`: its shares (`BeaconInput::Shares`).
pub struct DkgParty {
    party: Party<G2Element, G2Element>,
    name: AuthorityName,
    processed: BTreeMap<u16, ProcessedMessage<G2Element, G2Element>>,
    merged: Option<(DkgConfirmation, UsedProcessedMessages<G2Element, G2Element>)>,
    completed: bool,
    /// Past `Started`: what it decides from now on is submitted.
    started: bool,
    consensus: consensus::TransactionClient,
    collector: workqueue::Queue<BeaconInput>,
}

impl DkgParty {
    /// The party of the member named `name`, its protocol key's secret
    /// `secret`, in DKG as `setup` sets it up.
    ///
    /// # Panics
    /// If `secret` is not a scalar, or the setup admits no party.
    pub fn new(
        setup: &Arc<DkgSetup>,
        name: [u8; 96],
        secret: [u8; 32],
        consensus: consensus::TransactionClient,
        collector: workqueue::Queue<BeaconInput>,
    ) -> DkgParty {
        let secret = Scalar::from_byte_array(&secret).expect("a protocol secret is a scalar");
        let party = Party::new(
            ecies_v1::PrivateKey::<G2Element>::from(secret),
            setup.nodes().clone(),
            setup.threshold(),
            setup.random_oracle().clone(),
            &mut rand::thread_rng(),
        )
        .expect("a party of the setup");
        DkgParty {
            party,
            name: AuthorityName::new(name),
            processed: BTreeMap::new(),
            merged: None,
            completed: false,
            started: false,
            consensus,
            collector,
        }
    }

    fn merge(&mut self, used: &[u16]) {
        let processed: Vec<_> = used
            .iter()
            .filter_map(|sender| self.processed.get(sender).cloned())
            .collect();
        // Fails only if every merged message carries a complaint of this
        // party's: it cannot confirm then, as the reference's.
        if let Ok(merged) = self.party.merge(&processed) {
            self.merged = Some(merged);
        }
    }

    fn confirm(&self) {
        if let Some((confirmation, _)) = &self.merged {
            self.submit(&ConsensusTransaction::new_randomness_dkg_confirmation(
                self.name,
                &VersionedDkgConfirmation::V1(confirmation.clone()),
            ));
        }
    }

    /// On its own thread: a full submission lane blocks.
    fn submit(&self, transaction: &ConsensusTransaction) {
        let bytes = bcs::to_bytes(transaction).expect("a transaction serializes");
        let consensus = self.consensus.clone();
        std::thread::spawn(move || {
            // Failing only as consensus shuts down, when the epoch is over.
            let _ = consensus.submit_blocking(vec![bytes], consensus::Priority::Normal);
        });
    }
}

impl Processor for DkgParty {
    type Input = PartyInput;
    type Output = ();

    fn process(&mut self, input: PartyInput, _: &mut Bump) -> Option<()> {
        let mut rng = rand::thread_rng();
        match input {
            PartyInput::Message(message) => {
                let sender = message.sender;
                // The handler processed it, so it passes the same checks.
                if let Ok(processed) = self.party.process_message(*message, &mut rng) {
                    self.processed.insert(sender, processed);
                }
            }
            PartyInput::Merged(used) => {
                self.merge(&used);
                if self.started {
                    self.confirm();
                }
            }
            PartyInput::Completed(confirmations) => {
                self.completed = true;
                let Some((_, used)) = &self.merged else {
                    return None;
                };
                // Shares are none if this party complained about a message
                // that survived.
                if let Ok(output) = self.party.complete(used, &confirmations, &mut rng)
                    && let Some(shares) = output.shares
                    && let Err(workqueue::PushError::Full(_)) =
                        self.collector.try_push(BeaconInput::Shares(shares))
                {
                    panic!("the signature collector refused shares");
                }
            }
            PartyInput::Started {
                message_seen,
                confirmation_seen,
            } => {
                self.started = true;
                if self.completed {
                    return None;
                }
                if self.merged.is_some() {
                    if !confirmation_seen {
                        self.confirm();
                    }
                } else if !message_seen {
                    let message = self.party.create_message(&mut rng).expect("a party deals");
                    self.submit(&ConsensusTransaction::new_randomness_dkg_message(
                        self.name,
                        &VersionedDkgMessage::V1(message),
                    ));
                }
            }
        }
        None
    }
}
