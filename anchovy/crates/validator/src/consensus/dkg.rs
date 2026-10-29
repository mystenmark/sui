// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The random beacon's DKG, followed as an observer: the reference's
//! `RandomnessManager` with an `Observer` role (`randomness_reference.md`
//! §3). Every step is a function of consensus output, so the commit that
//! completes or fails DKG, and the group public key, are those of every
//! validator.

use fastcrypto::encoding::{Encoding, Hex};
use fastcrypto::error::FastCryptoError;
use fastcrypto::groups::bls12381::G2Element;
use fastcrypto::serde_helpers::ToFromByteArray;
use fastcrypto_tbls::dkg_v1::{Confirmation, Message, Observer};
use fastcrypto_tbls::ecies_v1;
use fastcrypto_tbls::nodes::{Node, Nodes};
use fastcrypto_tbls::polynomial::Poly;
use fastcrypto_tbls::random_oracle::RandomOracle;
use sui_protocol_config::ProtocolConfig;
use sui_types::messages_consensus::{VersionedDkgConfirmation, VersionedDkgMessage};

/// A dealer's message: its VSS polynomial's commitment and its shares,
/// encrypted to each party's protocol key.
pub type DkgMessage = Message<G2Element, G2Element>;
pub type DkgConfirmation = Confirmation<G2Element>;
/// The group's public polynomial: its constant term verifies the beacon's
/// signatures, the whole polynomial partial signatures.
pub type GroupKey = Poly<G2Element>;

/// The epoch's DKG parameters, as every party derives them: the committee
/// reduced to small weights, the threshold, and the random oracle. Shared by
/// the decoder, which checks messages, and the commit handler.
pub struct DkgSetup {
    /// Each member's name, by committee index, which is its party id.
    names: Vec<[u8; 96]>,
    observer: Observer<G2Element, G2Element>,
    /// What the observer was built from, which a party is built from too.
    nodes: Nodes<G2Element>,
    random_oracle: RandomOracle,
}

impl DkgSetup {
    /// The setup of `committee` (each member's name and stake, by index) in
    /// `epoch` of the chain `chain_identifier` names: the reference's
    /// `RandomnessManager::try_new`.
    ///
    /// # Panics
    /// If a member's name is not a G2 point, or the committee cannot be
    /// reduced: the reference runs no DKG then, which this does not model.
    pub fn new(
        committee: &[([u8; 96], u64)],
        config: &ProtocolConfig,
        chain_identifier: &[u8; 32],
        epoch: u64,
    ) -> DkgSetup {
        let nodes = committee
            .iter()
            .enumerate()
            .map(|(id, (name, stake))| Node {
                id: u16::try_from(id).expect("fewer than 2^16 members"),
                pk: ecies_v1::PublicKey::from(
                    G2Element::from_byte_array(name).expect("a member's name is a G2 point"),
                ),
                weight: u16::try_from(*stake).expect("voting power fits in u16"),
            })
            .collect();
        let (nodes, t) = Nodes::new_reduced(
            nodes,
            u16::try_from(sui_types::committee::VALIDITY_THRESHOLD).expect("fits in u16"),
            config.random_beacon_reduction_allowed_delta(),
            u16::try_from(config.random_beacon_reduction_lower_bound()).expect("fits in u16"),
        )
        .expect("the committee reduces");
        let random_oracle =
            RandomOracle::new(&format!("dkg {} {epoch}", Hex::encode(chain_identifier)));
        DkgSetup {
            names: committee.iter().map(|(name, _)| *name).collect(),
            observer: Observer::new(nodes.clone(), t, random_oracle.clone())
                .expect("a valid threshold"),
            nodes,
            random_oracle,
        }
    }

    /// The reconstruction threshold, in reduced weight.
    pub fn threshold(&self) -> u16 {
        self.observer.t()
    }

    /// The parties, with their reduced weights.
    pub fn nodes(&self) -> &Nodes<G2Element> {
        &self.nodes
    }

    pub fn random_oracle(&self) -> &RandomOracle {
        &self.random_oracle
    }

    /// The message `bytes` carries from the member at `author`, if every
    /// party processes it: it decodes, is version 1, names `author` as its
    /// sender, and passes `process_message`'s public checks (R3.4, R3.7).
    pub fn message(&self, author: u32, bytes: &[u8]) -> Option<DkgMessage> {
        let Ok(VersionedDkgMessage::V1(message)) = bcs::from_bytes(bytes) else {
            return None;
        };
        if u32::from(message.sender) != author {
            return None;
        }
        self.observer.process_message(message.clone()).ok()?;
        Some(message)
    }

    /// The confirmation `bytes` carries from the member at `author`, if it
    /// decodes, is version 1 and names `author` as its sender.
    pub fn confirmation(&self, author: u32, bytes: &[u8]) -> Option<DkgConfirmation> {
        let Ok(VersionedDkgConfirmation::V1(confirmation)) = bcs::from_bytes(bytes) else {
            return None;
        };
        (u32::from(confirmation.sender) == author).then_some(confirmation)
    }

    /// Whether `authority` is the name of the member at `author`: a DKG
    /// transaction whose authority is not its block's author is dropped
    /// unseen (`verify_consensus_transaction`).
    pub fn is_author(&self, author: u32, authority: &[u8; 96]) -> bool {
        self.names.get(author as usize) == Some(authority)
    }
}

/// A committed DKG transaction, decoded, its authority checked against its
/// block's author.
#[derive(Debug)]
pub struct DkgTraffic {
    /// The author's committee index: its party id.
    pub author: u32,
    pub payload: DkgPayload,
}

#[derive(Debug)]
pub enum DkgPayload {
    /// A message, if every party processes it (`DkgSetup::message`), or
    /// the decoder knows an earlier message of its author's was sequenced.
    Message(Option<Box<DkgMessage>>),
    /// A confirmation, if it decodes with its author as sender.
    Confirmation(Option<DkgConfirmation>),
}

/// DKG's outcome, as far as decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DkgStatus {
    Pending,
    Successful(GroupKey),
    Failed,
}

/// What a commit's DKG traffic changed, for the consensus output to keep.
#[derive(Debug, Default)]
pub struct DkgChanges {
    /// Messages seen, by author, with the message bytes if processed.
    pub messages: Vec<(u32, Option<Vec<u8>>)>,
    /// Confirmations seen, by author, with the confirmation bytes if kept.
    pub confirmations: Vec<(u32, Option<Vec<u8>>)>,
    /// The messages merged, by sender, once merged.
    pub used: Option<Vec<u16>>,
    /// The outcome, once decided.
    pub status: Option<DkgStatus>,
}

/// The DKG state machine of one epoch.
pub struct Dkg {
    setup: std::sync::Arc<DkgSetup>,
    /// Whether a message, and a confirmation, of each member's has been
    /// sequenced this epoch: the first wins (the reference's dedup).
    messages_seen: Vec<bool>,
    confirmations_seen: Vec<bool>,
    /// Processed messages, by sender.
    processed: Vec<Option<DkgMessage>>,
    used: Option<Vec<DkgMessage>>,
    confirmations: Vec<Option<DkgConfirmation>>,
    status: DkgStatus,
}

impl Dkg {
    pub fn new(setup: std::sync::Arc<DkgSetup>) -> Dkg {
        let n = setup.names.len();
        Dkg {
            setup,
            messages_seen: vec![false; n],
            confirmations_seen: vec![false; n],
            processed: vec![None; n],
            used: None,
            confirmations: vec![None; n],
            status: DkgStatus::Pending,
        }
    }

    /// The state the changes of the commits written left, as `changes`
    /// recorded it, in commit order (the reference's `try_new`).
    ///
    /// # Panics
    /// If a recorded change is not what `handle` wrote.
    pub fn restore(&mut self, changes: impl IntoIterator<Item = DkgChanges>) {
        for change in changes {
            for (author, bytes) in change.messages {
                self.messages_seen[author as usize] = true;
                if let Some(bytes) = bytes {
                    let VersionedDkgMessage::V1(message) =
                        bcs::from_bytes(&bytes).expect("a message as written")
                    else {
                        unreachable!("version 1 as written");
                    };
                    self.processed[author as usize] = Some(message);
                }
            }
            for (author, bytes) in change.confirmations {
                self.confirmations_seen[author as usize] = true;
                if let Some(bytes) = bytes {
                    let VersionedDkgConfirmation::V1(confirmation) =
                        bcs::from_bytes(&bytes).expect("a confirmation as written")
                    else {
                        unreachable!("version 1 as written");
                    };
                    self.confirmations[author as usize] = Some(confirmation);
                }
            }
            if let Some(used) = change.used {
                self.used = Some(
                    used.iter()
                        .map(|sender| {
                            self.processed[usize::from(*sender)]
                                .clone()
                                .expect("a used message was processed")
                        })
                        .collect(),
                );
            }
            if let Some(status) = change.status {
                self.status = status;
            }
        }
    }

    pub fn status(&self) -> &DkgStatus {
        &self.status
    }

    /// The messages processed, by sender.
    pub fn processed(&self) -> impl Iterator<Item = &DkgMessage> {
        self.processed.iter().flatten()
    }

    /// The senders of the messages merged, once merged.
    pub fn used(&self) -> Option<Vec<u16>> {
        self.used
            .as_ref()
            .map(|used| used.iter().map(|m| m.sender).collect())
    }

    /// The confirmations kept, by sender.
    pub fn confirmations(&self) -> Vec<DkgConfirmation> {
        self.confirmations.iter().flatten().cloned().collect()
    }

    /// Whether a message, and a confirmation, of the member at `author`'s
    /// have been sequenced.
    pub fn seen(&self, author: u32) -> (bool, bool) {
        (
            self.messages_seen[author as usize],
            self.confirmations_seen[author as usize],
        )
    }

    /// Takes a commit's DKG traffic, in commit order, then advances: the
    /// reference's `process_dkg_updates`. `round` is the commit's leader
    /// round. What changed is returned for the consensus output.
    pub fn handle(
        &mut self,
        config: &ProtocolConfig,
        traffic: Vec<DkgTraffic>,
        round: u64,
    ) -> DkgChanges {
        // Advancing on every commit while pending, as below, is this flag's
        // rule; the older rule advanced only on commits with DKG traffic.
        assert!(
            config.always_advance_dkg_to_resolution(),
            "DKG advances every commit"
        );
        let mut changes = DkgChanges::default();
        for DkgTraffic { author, payload } in traffic {
            let index = author as usize;
            match payload {
                DkgPayload::Message(message) => {
                    if std::mem::replace(&mut self.messages_seen[index], true) {
                        continue;
                    }
                    let Some(message) = message else {
                        changes.messages.push((author, None));
                        continue;
                    };
                    // `add_message`: no message counts once merged.
                    if self.used.is_some() || self.status != DkgStatus::Pending {
                        changes.messages.push((author, None));
                        continue;
                    }
                    let bytes = bcs::to_bytes(&VersionedDkgMessage::V1((*message).clone()))
                        .expect("a message serializes");
                    changes.messages.push((author, Some(bytes)));
                    self.processed[index] = Some(*message);
                }
                DkgPayload::Confirmation(confirmation) => {
                    if std::mem::replace(&mut self.confirmations_seen[index], true) {
                        continue;
                    }
                    let Some(confirmation) = confirmation else {
                        changes.confirmations.push((author, None));
                        continue;
                    };
                    if self.status != DkgStatus::Pending {
                        changes.confirmations.push((author, None));
                        continue;
                    }
                    let bytes = bcs::to_bytes(&VersionedDkgConfirmation::V1(confirmation.clone()))
                        .expect("a confirmation serializes");
                    changes.confirmations.push((author, Some(bytes)));
                    self.confirmations[index] = Some(confirmation);
                }
            }
        }
        self.advance(config, round, &mut changes);
        changes
    }

    /// The reference's `advance_dkg`: merge, then complete, then time out.
    fn advance(&mut self, config: &ProtocolConfig, round: u64, changes: &mut DkgChanges) {
        if self.status != DkgStatus::Pending {
            return;
        }
        let observer = &self.setup.observer;
        if self.used.is_none() {
            let processed: Vec<DkgMessage> = self.processed.iter().flatten().cloned().collect();
            match observer.merge(processed) {
                Ok(used) => {
                    changes.used = Some(used.iter().map(|m| m.sender).collect());
                    self.used = Some(used);
                }
                Err(FastCryptoError::NotEnoughInputs) => {}
                Err(e) => unreachable!("an observer's merge fails only for weight: {e:?}"),
            }
        }
        if let Some(used) = &self.used {
            let confirmations: Vec<DkgConfirmation> =
                self.confirmations.iter().flatten().cloned().collect();
            // Any other error leaves DKG pending, to time out, as the
            // reference's.
            if let Ok(output) = observer.complete(used, &confirmations) {
                self.status = DkgStatus::Successful(output.vss_pk);
            }
        }
        if self.status == DkgStatus::Pending
            && round > u64::from(config.random_beacon_dkg_timeout_round())
        {
            self.status = DkgStatus::Failed;
        }
        if self.status != DkgStatus::Pending {
            changes.status = Some(self.status.clone());
        }
    }
}

/// `status` encoded, as the consensus output keeps it.
pub fn encode_status(status: &DkgStatus) -> Vec<u8> {
    let key = match status {
        DkgStatus::Pending => unreachable!("only an outcome is kept"),
        DkgStatus::Successful(key) => Some(key.clone()),
        DkgStatus::Failed => None,
    };
    bcs::to_bytes(&key).expect("a key serializes")
}

/// # Panics
/// If `bytes` is not what `encode_status` wrote.
pub fn decode_status(bytes: &[u8]) -> DkgStatus {
    match bcs::from_bytes::<Option<GroupKey>>(bytes).expect("a status as written") {
        Some(key) => DkgStatus::Successful(key),
        None => DkgStatus::Failed,
    }
}
