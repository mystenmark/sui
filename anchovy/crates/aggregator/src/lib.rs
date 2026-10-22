// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Certifying checkpoints from the committee's signatures
//! (`docs/consensus_pipeline.md`, Checkpointing).

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use fastcrypto::bls12381::min_sig::{
    BLS12381AggregateSignature, BLS12381PublicKey, BLS12381Signature,
};
use fastcrypto::traits::{AggregateAuthenticator, ToFromBytes, VerifyingKey};
use messages::base::Digest;
use roaring::RoaringBitmap;
use tracing::{error, warn};
use validator::pipeline::backpressure::Backpressure;
use validator::pipeline::items::{
    BuiltWaitItem, BuiltWaits, CertifiedCheckpoint, CheckpointSignature,
};
use validator::pipeline::keys::CheckpointBuilt;
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore};
use waiter::{Command, WaitBatch};
use workqueue::{Processor, Queue};

/// Voting power, as sui's committee counts it.
pub type Stake = u64;

/// `Intent::sui_app(IntentScope::CheckpointSummary)`: scope, version, app.
const CHECKPOINT_SUMMARY_INTENT: [u8; 3] = [2, 0, 0];

/// What follows the summary in a `CheckpointSignatureMessage`: the
/// `AuthoritySignInfo`'s epoch, authority (a length byte, then 96 bytes) and
/// signature.
const SIGN_INFO_LEN: usize = 8 + 1 + 96 + 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitteeError {
    /// A member's key is not a compressed BLS12-381 G2 point.
    InvalidKey,
    /// A key appears twice.
    Duplicate,
}

struct Member {
    key: BLS12381PublicKey,
    stake: Stake,
}

/// The epoch's committee: each member's BLS key and stake, in the order of
/// their public key bytes, as sui's `Committee` orders them. A member's
/// position is its index in a certificate's signers bitmap.
pub struct Committee {
    epoch: u64,
    members: Vec<Member>,
    index: HashMap<[u8; 96], u32>,
    total: Stake,
}

impl Committee {
    /// Keys are not checked to be in the prime-order subgroup: like sui, this
    /// trusts the committee the chain verified.
    pub fn new(
        epoch: u64,
        members: impl IntoIterator<Item = ([u8; 96], Stake)>,
    ) -> Result<Committee, CommitteeError> {
        let mut sorted: Vec<([u8; 96], Stake)> = members.into_iter().collect();
        sorted.sort_unstable_by_key(|a| a.0);
        let mut index = HashMap::with_capacity(sorted.len());
        let mut out = Vec::with_capacity(sorted.len());
        let mut total: Stake = 0;
        for (i, (name, stake)) in sorted.into_iter().enumerate() {
            let position = u32::try_from(i).expect("fewer than 2^32 members");
            if index.insert(name, position).is_some() {
                return Err(CommitteeError::Duplicate);
            }
            let key =
                BLS12381PublicKey::from_bytes(&name).map_err(|_| CommitteeError::InvalidKey)?;
            total = total.checked_add(stake).expect("total stake fits in u64");
            out.push(Member { key, stake });
        }
        Ok(Committee {
            epoch,
            members: out,
            index,
            total,
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn total_stake(&self) -> Stake {
        self.total
    }

    /// Two thirds of the stake, plus one: sui's `QUORUM_THRESHOLD` of 6,667
    /// for its total of 10,000.
    pub fn quorum_threshold(&self) -> Stake {
        u64::try_from(u128::from(self.total) * 2 / 3).expect("below the total") + 1
    }

    fn position(&self, name: &[u8; 96]) -> Option<u32> {
        self.index.get(name).copied()
    }

    fn member(&self, position: u32) -> &Member {
        &self.members[position as usize]
    }
}

/// A `CheckpointSignatureMessage`'s parts, split without parsing the
/// summary: the authority signed exactly these summary bytes.
struct SignedSummary<'a> {
    summary: &'a [u8],
    epoch: u64,
    authority: &'a [u8; 96],
    signature: &'a [u8; 48],
}

impl SignedSummary<'_> {
    fn split(message: &[u8]) -> Option<SignedSummary<'_>> {
        let (summary, info) =
            message.split_at_checked(message.len().checked_sub(SIGN_INFO_LEN)?)?;
        let (epoch, rest) = info.split_first_chunk::<8>()?;
        let (&len, rest) = rest.split_first()?;
        let (authority, signature) = rest.split_first_chunk::<96>()?;
        (len == 96).then_some(SignedSummary {
            summary,
            epoch: u64::from_le_bytes(*epoch),
            authority,
            signature: signature.try_into().ok()?,
        })
    }
}

/// What an authority signs for a summary: the intent message's BCS, then
/// the epoch's (`AuthoritySignature::new_secure`).
fn signing_message<'b>(buf: &'b mut Vec<u8>, summary: &[u8], epoch: u64) -> &'b [u8] {
    buf.clear();
    buf.extend_from_slice(&CHECKPOINT_SUMMARY_INTENT);
    buf.extend_from_slice(summary);
    buf.extend_from_slice(&epoch.to_le_bytes());
    buf
}

/// A signature over the local summary, verified individually or not yet.
struct Candidate {
    bytes: [u8; 48],
    signature: BLS12381Signature,
    verified: bool,
}

/// One checkpoint's signatures, until it is certified.
struct Pending {
    local: Arc<BuiltCheckpoint>,
    /// Signatures over the local summary, by signer position.
    signatures: BTreeMap<u32, Candidate>,
    stake: Stake,
    /// Verified signatures over other summaries: each digest's signers and
    /// their stake.
    others: BTreeMap<[u8; 32], (Vec<u32>, Stake)>,
    /// Members that signed any summary, for whether a quorum on the local one
    /// is still reachable.
    voted: Vec<bool>,
    voted_stake: Stake,
    split_brain: bool,
}

impl Pending {
    fn new(local: Arc<BuiltCheckpoint>, committee: &Committee) -> Pending {
        Pending {
            local,
            signatures: BTreeMap::new(),
            stake: 0,
            others: BTreeMap::new(),
            voted: vec![false; committee.len()],
            voted_stake: 0,
            split_brain: false,
        }
    }

    fn vote(&mut self, position: u32, stake: Stake) {
        let voted = &mut self.voted[position as usize];
        if !*voted {
            *voted = true;
            self.voted_stake += stake;
        }
    }

    /// Adds a signature over the local summary. Returns whether it counted.
    fn add_local(
        &mut self,
        committee: &Committee,
        position: u32,
        signed: &SignedSummary<'_>,
        scratch: &mut Vec<u8>,
    ) -> bool {
        // Most signatures arrive twice, from their block and their commit.
        if self
            .signatures
            .get(&position)
            .is_some_and(|c| c.bytes == *signed.signature)
        {
            return false;
        }
        let Ok(signature) = BLS12381Signature::from_bytes(signed.signature) else {
            warn!(
                seq = self.local.seq,
                position, "a checkpoint signature is not a curve point"
            );
            return false;
        };
        let member = committee.member(position);
        let candidate = match self.signatures.entry(position) {
            Entry::Vacant(vacant) => {
                vacant.insert(Candidate {
                    bytes: *signed.signature,
                    signature,
                    verified: false,
                });
                self.stake += member.stake;
                self.vote(position, member.stake);
                return true;
            }
            Entry::Occupied(occupied) => occupied.into_mut(),
        };
        if candidate.bytes == *signed.signature {
            return false;
        }
        // BLS signatures are deterministic, so at most one of the two is
        // valid. Votes are not authenticated by their block's author, so the
        // first may be a forgery naming this signer: keep the valid one.
        if !candidate.verified {
            let message = signing_message(scratch, &self.local.summary, committee.epoch);
            if member.key.verify(message, &candidate.signature).is_ok() {
                candidate.verified = true;
            } else {
                *candidate = Candidate {
                    bytes: *signed.signature,
                    signature,
                    verified: false,
                };
                return true;
            }
        }
        warn!(
            seq = self.local.seq,
            position, "conflicting checkpoint signatures from one signer"
        );
        false
    }

    /// Records a signature over a summary other than the local one: a fork,
    /// here or there. It never counts toward the local summary's quorum.
    ///
    /// # Panics
    /// Once a quorum has signed the other summary: the network certified a
    /// checkpoint this validator did not build, which sui finds when state
    /// sync brings it the certificate, and halts on.
    fn add_other(
        &mut self,
        committee: &Committee,
        position: u32,
        signed: &SignedSummary<'_>,
        scratch: &mut Vec<u8>,
    ) {
        let digest = Digest::of("CheckpointSummary", signed.summary).bytes;
        if self
            .others
            .get(&digest)
            .is_some_and(|(signers, _)| signers.contains(&position))
        {
            return;
        }
        let member = committee.member(position);
        let verified = BLS12381Signature::from_bytes(signed.signature).is_ok_and(|signature| {
            let message = signing_message(scratch, signed.summary, signed.epoch);
            member.key.verify(message, &signature).is_ok()
        });
        if !verified {
            warn!(
                seq = self.local.seq,
                position, "an invalid checkpoint signature over another summary"
            );
            return;
        }
        let (signers, stake) = self.others.entry(digest).or_default();
        signers.push(position);
        *stake += member.stake;
        assert!(
            *stake < committee.quorum_threshold(),
            "fork: a quorum certified checkpoint {} {:?}, not the {:?} this validator built",
            self.local.seq,
            Digest::new(digest),
            Digest::new(self.local.digest),
        );
        self.vote(position, member.stake);
        warn!(
            seq = self.local.seq,
            position,
            theirs = ?Digest::new(digest),
            ours = ?Digest::new(self.local.digest),
            "a validator signed a different checkpoint summary"
        );
        self.check_split_brain(committee);
    }

    /// sui's `check_for_split_brain`: no quorum is reachable on any summary
    /// whatever the members yet to sign do.
    ///
    /// # Panics
    /// On a split brain, in a debug build, as sui's `debug_fatal!`.
    fn check_split_brain(&mut self, committee: &Committee) {
        let plurality = self
            .others
            .values()
            .map(|(_, stake)| *stake)
            .chain([self.stake])
            .max()
            .unwrap_or(0);
        let uncommitted = committee.total - self.voted_stake;
        if !self.split_brain && uncommitted + plurality < committee.quorum_threshold() {
            self.split_brain = true;
            let factions: Vec<String> = self
                .others
                .iter()
                .map(|(digest, (_, stake))| format!("{:?}: {stake}", Digest::new(*digest)))
                .collect();
            error!(
                seq = self.local.seq,
                ours = ?Digest::new(self.local.digest),
                local_stake = self.stake,
                ?factions,
                "checkpoint split brain: no summary can reach a quorum"
            );
            debug_assert!(
                false,
                "checkpoint split brain at checkpoint {}: ours {:?} with stake {}, others {factions:?}",
                self.local.seq,
                Digest::new(self.local.digest),
                self.stake,
            );
        }
    }

    /// The certified summary's BCS, if the signatures over the local summary
    /// that verify reach a quorum. Like sui's `StakeAggregator::insert`, the
    /// aggregate is verified first, and only if it fails is each signature,
    /// evicting those that do not verify.
    fn certify(&mut self, committee: &Committee, scratch: &mut Vec<u8>) -> Option<Vec<u8>> {
        let threshold = committee.quorum_threshold();
        if self.stake < threshold {
            return None;
        }
        let message = signing_message(scratch, &self.local.summary, committee.epoch);
        let keys: Vec<BLS12381PublicKey> = self
            .signatures
            .keys()
            .map(|&p| committee.member(p).key.clone())
            .collect();
        let aggregate =
            BLS12381AggregateSignature::aggregate(self.signatures.values().map(|c| &c.signature))
                .ok()?;
        let aggregate = if aggregate.verify(&keys, message).is_ok() {
            aggregate
        } else {
            let mut evicted = Vec::new();
            self.signatures.retain(|&position, candidate| {
                let member = committee.member(position);
                candidate.verified =
                    candidate.verified || member.key.verify(message, &candidate.signature).is_ok();
                if !candidate.verified {
                    evicted.push(position);
                }
                candidate.verified
            });
            for position in evicted {
                warn!(
                    seq = self.local.seq,
                    position, "evicted a checkpoint signature that does not verify"
                );
                let stake = committee.member(position).stake;
                self.stake -= stake;
                self.voted[position as usize] = false;
                self.voted_stake -= stake;
            }
            if self.stake < threshold {
                return None;
            }
            BLS12381AggregateSignature::aggregate(self.signatures.values().map(|c| &c.signature))
                .ok()?
        };
        Some(self.certified_bytes(committee.epoch, &aggregate))
    }

    /// `CertifiedCheckpointSummary` BCS: the summary, then
    /// `AuthorityStrongQuorumSignInfo`'s epoch, aggregate signature and
    /// signers (a serialized roaring bitmap of positions, as bytes).
    fn certified_bytes(&self, epoch: u64, aggregate: &BLS12381AggregateSignature) -> Vec<u8> {
        let signers = RoaringBitmap::from_sorted_iter(self.signatures.keys().copied())
            .expect("positions iterate in order");
        let mut bitmap = Vec::with_capacity(signers.serialized_size());
        signers
            .serialize_into(&mut bitmap)
            .expect("writing to a vector does not fail");
        let mut bytes = Vec::with_capacity(self.local.summary.len() + 8 + 48 + 5 + bitmap.len());
        bytes.extend_from_slice(&self.local.summary);
        bytes.extend_from_slice(&epoch.to_le_bytes());
        bytes.extend_from_slice(aggregate.as_ref());
        let mut len = bitmap.len();
        loop {
            let byte = (len & 0x7f) as u8;
            len >>= 7;
            if len == 0 {
                bytes.push(byte);
                break;
            }
            bytes.push(byte | 0x80);
        }
        bytes.extend_from_slice(&bitmap);
        bytes
    }
}

/// Collects checkpoint signatures against this validator's own summary;
/// at quorum on its digest, certifies the checkpoint and hands it to state
/// sync and on.
///
/// Signatures are checked as sui's `CheckpointSignatureAggregator` checks
/// them: the epoch, membership, one signature per signer, and the aggregate
/// at quorum, falling back to each signature if it fails. A signature over a
/// summary other than the local one is verified against its own summary,
/// counted per digest only to detect a split brain, and logged as a fork; it
/// never counts toward a certificate (in sui it cannot either: it is
/// verified against the local summary and evicted).
///
/// As in sui, checkpoints are certified in sequence order: a quorum on a
/// later checkpoint waits for the earlier ones, as nothing downstream can
/// use it sooner.
///
/// Inward:
/// - built waiter, ready: `Vec<CheckpointSignature>` (signatures wait
///   there until this validator has built the checkpoint).
///
/// Outward:
/// - state sync (black box): `CertifiedCheckpoint`;
/// - built waiter, wait for: `BuiltWaitItem::Certified`, keyed by
///   `CheckpointBuilt(seq)`, for the fork check;
/// - `CheckpointCommitter`: `Vec<CertifiedCheckpoint>`, in sequence order.
///
/// Reads: the checkpoint store. Writes: backpressure's certified watermark.
pub struct CheckpointAggregator {
    committee: Arc<Committee>,
    checkpoints: Arc<CheckpointStore>,
    state_sync: Queue<CertifiedCheckpoint>,
    /// Where certified checkpoints wait to be checked for a fork, once wired.
    fork_checks: Option<BuiltWaits>,
    /// Told of each checkpoint certified, once wired.
    backpressure: Option<Arc<Backpressure>>,
    /// The checkpoint to certify next; every earlier one is certified.
    next: u64,
    pending: BTreeMap<u64, Pending>,
    scratch: Vec<u8>,
}

impl CheckpointAggregator {
    pub fn new(
        committee: Arc<Committee>,
        checkpoints: Arc<CheckpointStore>,
        next: u64,
        state_sync: Queue<CertifiedCheckpoint>,
    ) -> CheckpointAggregator {
        CheckpointAggregator {
            committee,
            checkpoints,
            state_sync,
            fork_checks: None,
            backpressure: None,
            next,
            pending: BTreeMap::new(),
            scratch: Vec::new(),
        }
    }

    /// Sends each checkpoint certified to be checked against the local one.
    #[must_use]
    pub fn with_fork_checks(mut self, built: BuiltWaits) -> CheckpointAggregator {
        self.fork_checks = Some(built);
        self
    }

    /// Tells `backpressure` of each checkpoint certified.
    #[must_use]
    pub fn with_backpressure(mut self, backpressure: Arc<Backpressure>) -> CheckpointAggregator {
        self.backpressure = Some(backpressure);
        self
    }

    fn add(&mut self, signature: &CheckpointSignature, certified: &mut Vec<CertifiedCheckpoint>) {
        let seq = signature.seq;
        if seq < self.next {
            return;
        }
        let committee = &*self.committee;
        let Some(signed) = SignedSummary::split(&signature.message) else {
            warn!(seq, "a checkpoint signature message does not split");
            return;
        };
        if signed.epoch != committee.epoch {
            warn!(
                seq,
                epoch = signed.epoch,
                "a checkpoint signature from another epoch"
            );
            return;
        }
        let Some(position) = committee.position(signed.authority) else {
            warn!(seq, "a checkpoint signature from outside the committee");
            return;
        };
        if committee.member(position).stake == 0 {
            return;
        }
        let pending = self.pending.entry(seq).or_insert_with(|| {
            let local = self
                .checkpoints
                .built(seq)
                .expect("the built waiter releases signatures once the summary is built");
            Pending::new(local, committee)
        });
        if signed.summary != pending.local.summary.as_slice() {
            pending.add_other(committee, position, &signed, &mut self.scratch);
            return;
        }
        if pending.add_local(committee, position, &signed, &mut self.scratch) && seq == self.next {
            self.certify_in_order(certified);
        }
    }

    /// Certifies `next`, then each later checkpoint that already has a
    /// quorum.
    fn certify_in_order(&mut self, certified: &mut Vec<CertifiedCheckpoint>) {
        while let Some(pending) = self.pending.get_mut(&self.next) {
            let Some(summary) = pending.certify(&self.committee, &mut self.scratch) else {
                return;
            };
            let seq = self.next;
            self.pending.remove(&seq);
            self.next += 1;
            let checkpoint = CertifiedCheckpoint {
                seq,
                summary: summary.into(),
            };
            // Before the committer can commit it.
            if let Some(backpressure) = &self.backpressure {
                backpressure.certified(seq);
            }
            if self.state_sync.try_push(checkpoint.clone()).is_err() {
                warn!(seq, "state sync refused a certified checkpoint");
            }
            if let Some(built) = &self.fork_checks {
                let mut wait = WaitBatch::with_capacity(1, 1);
                wait.push(
                    BuiltWaitItem::Certified {
                        certified: checkpoint.clone(),
                        local: None,
                    },
                    [CheckpointBuilt(seq)],
                );
                // Inside the pipeline a full queue is a sizing bug; a closed
                // one means the pipeline is shutting down.
                if let Err(workqueue::PushError::Full(_)) = built.try_push(Command::Wait(wait)) {
                    panic!("the built waiter refused a certified checkpoint");
                }
            }
            certified.push(checkpoint);
        }
    }
}

impl Processor for CheckpointAggregator {
    type Input = Vec<CheckpointSignature>;
    type Output = Vec<CertifiedCheckpoint>;

    fn process(
        &mut self,
        ready: Vec<CheckpointSignature>,
        _bump: &mut workqueue::Bump,
    ) -> Option<Vec<CertifiedCheckpoint>> {
        let mut certified = Vec::new();
        for signature in &ready {
            self.add(signature, &mut certified);
        }
        (!certified.is_empty()).then_some(certified)
    }
}
