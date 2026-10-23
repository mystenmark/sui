// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Each user transaction's estimated execution time, as the reference's
//! `ExecutionTimeEstimator`: per command, the stake-weighted median of the
//! durations the committee observed and shared through consensus, else a
//! default. The commit stage folds each scheduled transaction's estimate
//! into the prologue's additional state digest.

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use hashbrown::{Equivalent, HashMap};

use messages::build::type_tag::TypeInput;
use messages::system_transaction::ExecutionTimeObservationKey;
use messages::transaction::{Command, TransactionKind};
use protocol_config::{
    ExecutionTimeEstimateParams, PerObjectCongestionControlMode, ProtocolConfig,
};
use serde::{Deserialize, Serialize};

use crate::checks::VerifiedTransaction;

/// The reference's `ExecutionTimeObservationKey`, owned, and ordered as it
/// orders them. Ordered and hashed as its [`KeyRef`], by which commands look
/// it up without allocating.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Key {
    MoveEntryPoint {
        package: [u8; 32],
        module: Box<str>,
        function: Box<str>,
        /// Empty in keys taken from commands; observations may carry some,
        /// and then match no command.
        type_arguments: Vec<TypeInput>,
    },
    TransferObjects,
    SplitCoins,
    MergeCoins,
    Publish,
    MakeMoveVec,
    Upgrade,
}

/// A [`Key`], borrowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyRef<'a> {
    MoveEntryPoint {
        package: &'a [u8; 32],
        module: &'a str,
        function: &'a str,
        type_arguments: &'a [TypeInput],
    },
    TransferObjects,
    SplitCoins,
    MergeCoins,
    Publish,
    MakeMoveVec,
    Upgrade,
}

impl<'a> KeyRef<'a> {
    /// The reference's `ExecutionTimeObservationKey::from_command`: no type
    /// arguments.
    pub fn of_command(command: &Command<'a>) -> KeyRef<'a> {
        match command {
            Command::MoveCall(call) => KeyRef::MoveEntryPoint {
                package: &call.package.0,
                module: call.module,
                function: call.function,
                type_arguments: &[],
            },
            Command::TransferObjects(..) => KeyRef::TransferObjects,
            Command::SplitCoins(..) => KeyRef::SplitCoins,
            Command::MergeCoins(..) => KeyRef::MergeCoins,
            Command::Publish(..) => KeyRef::Publish,
            Command::MakeMoveVec(..) => KeyRef::MakeMoveVec,
            Command::Upgrade(..) => KeyRef::Upgrade,
        }
    }

    fn default_duration(self) -> Duration {
        match self {
            KeyRef::Publish | KeyRef::Upgrade => Duration::from_millis(3),
            _ => Duration::from_millis(1),
        }
    }
}

impl Equivalent<Key> for KeyRef<'_> {
    fn equivalent(&self, key: &Key) -> bool {
        *self == key.as_ref()
    }
}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_ref().hash(state);
    }
}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Key) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Key) -> Ordering {
        self.as_ref().cmp(&other.as_ref())
    }
}

impl Key {
    pub fn as_ref(&self) -> KeyRef<'_> {
        match self {
            Key::MoveEntryPoint {
                package,
                module,
                function,
                type_arguments,
            } => KeyRef::MoveEntryPoint {
                package,
                module,
                function,
                type_arguments,
            },
            Key::TransferObjects => KeyRef::TransferObjects,
            Key::SplitCoins => KeyRef::SplitCoins,
            Key::MergeCoins => KeyRef::MergeCoins,
            Key::Publish => KeyRef::Publish,
            Key::MakeMoveVec => KeyRef::MakeMoveVec,
            Key::Upgrade => KeyRef::Upgrade,
        }
    }

    pub fn of_observation(key: &ExecutionTimeObservationKey<'_>) -> Key {
        match key {
            ExecutionTimeObservationKey::MoveEntryPoint {
                package,
                module,
                function,
                type_arguments,
            } => Key::MoveEntryPoint {
                package: package.0,
                module: (*module).into(),
                function: (*function).into(),
                type_arguments: type_arguments.iter().map(TypeInput::from).collect(),
            },
            ExecutionTimeObservationKey::TransferObjects => Key::TransferObjects,
            ExecutionTimeObservationKey::SplitCoins => Key::SplitCoins,
            ExecutionTimeObservationKey::MergeCoins => Key::MergeCoins,
            ExecutionTimeObservationKey::Publish => Key::Publish,
            ExecutionTimeObservationKey::MakeMoveVec => Key::MakeMoveVec,
            ExecutionTimeObservationKey::Upgrade => Key::Upgrade,
        }
    }

    /// As the end-of-epoch transaction stores it.
    pub fn to_stored(&self) -> messages::build::system_transaction::ExecutionTimeObservationKey {
        use messages::build::base::{AccountAddress, ObjectId};
        use messages::build::system_transaction::ExecutionTimeObservationKey as Stored;
        match self {
            Key::MoveEntryPoint {
                package,
                module,
                function,
                type_arguments,
            } => Stored::MoveEntryPoint {
                package: ObjectId(AccountAddress(*package)),
                module: module.to_string(),
                function: function.to_string(),
                type_arguments: type_arguments.clone(),
            },
            Key::TransferObjects => Stored::TransferObjects,
            Key::SplitCoins => Stored::SplitCoins,
            Key::MergeCoins => Stored::MergeCoins,
            Key::Publish => Stored::Publish,
            Key::MakeMoveVec => Stored::MakeMoveVec,
            Key::Upgrade => Stored::Upgrade,
        }
    }
}

/// The reference's `command_length`: a native command's estimate scales with
/// its inputs, plus one for its fixed cost.
fn command_length(command: &Command<'_>) -> usize {
    match command {
        Command::MoveCall(_) | Command::Publish(..) | Command::Upgrade(..) => 1,
        Command::TransferObjects(objects, _) => objects.len() + 1,
        Command::SplitCoins(_, amounts) => amounts.len() + 1,
        Command::MergeCoins(_, coins) => coins.len() + 1,
        Command::MakeMoveVec(_, elements) => elements.len() + 1,
    }
}

/// `std::time::Duration` from its wire form, if representable: the
/// reference does not deserialize a duration that overflows.
pub fn duration(d: messages::consensus::Duration) -> Option<Duration> {
    d.secs
        .checked_add(u64::from(d.nanos / 1_000_000_000))
        .map(|_| Duration::new(d.secs, d.nanos))
}

pub fn encode_estimates(estimates: &[(Key, Duration)]) -> Vec<u8> {
    bcs::to_bytes(estimates).expect("estimates serialize")
}

/// # Panics
/// Unless `bytes` are as `encode_estimates` made them.
pub fn decode_estimates(bytes: &[u8]) -> Vec<(Key, Duration)> {
    bcs::from_bytes(bytes).expect("estimates as encoded")
}

/// What one authority shared through consensus: its estimates by key, the
/// latest of its generations replacing earlier ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedObservations {
    /// The author of the block carrying them.
    pub author: u32,
    /// The authority they claim to come from; ignored unless the author.
    pub authority: [u8; 96],
    pub generation: u64,
    pub estimates: Vec<(Key, Duration)>,
}

/// The committee's observations of one key.
struct Observations {
    /// Each authority's, by committee index: its generation and duration.
    by_authority: Vec<(u64, Option<Duration>)>,
    stake_weighted_median: Option<Duration>,
}

pub struct ExecutionTimeEstimator {
    /// Each member's name and stake, by committee index.
    committee: Vec<([u8; 96], u64)>,
    params: ExecutionTimeEstimateParams,
    observations: HashMap<Key, Observations, foldhash::fast::RandomState>,
    /// Scratch for `update_median`, kept between uses.
    observed: Vec<(Duration, usize)>,
}

impl ExecutionTimeEstimator {
    /// No observations yet.
    ///
    /// # Panics
    /// Unless `config` estimates execution times for congestion control:
    /// the reference supports nothing else.
    pub fn new(committee: Vec<([u8; 96], u64)>, config: &ProtocolConfig) -> ExecutionTimeEstimator {
        let PerObjectCongestionControlMode::ExecutionTimeEstimate(params) =
            config.per_object_congestion_control_mode()
        else {
            panic!("congestion control by execution time estimates");
        };
        ExecutionTimeEstimator {
            committee,
            params,
            observations: HashMap::default(),
            observed: Vec::new(),
        }
    }

    /// Takes the observations a commit carries, if their author is the
    /// authority they claim to come from, as the reference's
    /// `verify_consensus_transaction`; whether it took them.
    pub fn observe(&mut self, shared: &SharedObservations) -> bool {
        let Some((name, _)) = self.committee.get(shared.author as usize) else {
            return false;
        };
        if *name != shared.authority {
            return false;
        }
        self.restore(shared.author, shared.generation, &shared.estimates);
        true
    }

    /// Takes observations `observe` took before a restart.
    pub fn restore(&mut self, author: u32, generation: u64, estimates: &[(Key, Duration)]) {
        for (key, duration) in estimates {
            self.observe_one(author, generation, key, *duration);
        }
    }

    fn observe_one(&mut self, source: u32, generation: u64, key: &Key, duration: Duration) {
        // The reference uses only the default for publishing.
        if *key == Key::Publish {
            return;
        }
        let members = self.committee.len();
        let default_none = self.params.default_none_duration_for_new_keys;
        let observations = self
            .observations
            .entry(key.clone())
            .or_insert_with(|| Observations {
                by_authority: vec![(0, None); members],
                stake_weighted_median: (!default_none).then_some(Duration::ZERO),
            });
        let (latest, observed) = &mut observations.by_authority[source as usize];
        if *latest >= generation {
            return;
        }
        *latest = generation;
        *observed = Some(duration);
        observations.update_median(&self.committee, &self.params, &mut self.observed);
    }

    /// Every key's observations, by authority name, sorted by key: what the
    /// reference takes at the epoch's last commit (`take_observations`) to
    /// store for the next epoch.
    pub fn observations(&self) -> Vec<(Key, Vec<([u8; 96], Duration)>)> {
        let mut observations: Vec<_> = self
            .observations
            .iter()
            .map(|(key, observations)| {
                let by_authority = observations
                    .by_authority
                    .iter()
                    .zip(&self.committee)
                    .filter_map(|((_, duration), (name, _))| Some((*name, (*duration)?)))
                    .collect();
                (key.clone(), by_authority)
            })
            .collect();
        observations.sort_by(|(a, _), (b, _)| a.cmp(b));
        observations
    }

    /// `transaction`'s estimated execution time, in whole microseconds, as
    /// the reference's `get_tx_cost`.
    pub fn estimate_us(&self, transaction: &VerifiedTransaction) -> u64 {
        let TransactionKind::ProgrammableTransaction(ptb) = transaction.get().0.data().kind()
        else {
            unreachable!("validation admits only programmable user transactions");
        };
        u64::try_from(self.estimate(ptb.commands).as_micros()).unwrap_or(u64::MAX)
    }

    /// `commands`' estimated execution time.
    pub fn estimate(&self, commands: &[Command<'_>]) -> Duration {
        commands
            .iter()
            .map(|command| {
                let key = KeyRef::of_command(command);
                self.observations
                    .get(&key)
                    .and_then(|o| o.stake_weighted_median)
                    .unwrap_or_else(|| key.default_duration())
                    .mul_f64(command_length(command) as f64)
            })
            .sum::<Duration>()
            .min(Duration::from_micros(self.params.max_estimate_us))
    }
}

impl Observations {
    /// As the reference's `update_stake_weighted_median`: none until enough
    /// stake has observed; then the first duration, in order, past half the
    /// stake of those who did.
    fn update_median(
        &mut self,
        committee: &[([u8; 96], u64)],
        params: &ExecutionTimeEstimateParams,
        observed: &mut Vec<(Duration, usize)>,
    ) {
        observed.clear();
        observed.extend(
            self.by_authority
                .iter()
                .enumerate()
                .filter_map(|(i, (_, duration))| duration.map(|d| (d, i))),
        );
        observed.sort_unstable();
        let stake = |i: usize| committee[i].1;
        let total: u64 = observed.iter().map(|&(_, i)| stake(i)).sum();
        if total < params.stake_weighted_median_threshold {
            self.stake_weighted_median = None;
            return;
        }
        let mut running = 0;
        for &(duration, i) in observed.iter() {
            running += stake(i);
            if running > total / 2 {
                self.stake_weighted_median = Some(duration);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use messages::transaction::Argument;
    use protocol_config::ProtocolVersion;

    use super::*;

    const SPLIT: Command<'static> = Command::SplitCoins(Argument::GasCoin, &[Argument::Input(0)]);

    fn estimator(stakes: &[u64]) -> ExecutionTimeEstimator {
        let committee = (0u8..).zip(stakes).map(|(i, s)| ([i; 96], *s)).collect();
        ExecutionTimeEstimator::new(
            committee,
            &ProtocolConfig::get_for_version(ProtocolVersion::MAX, protocol_config::Chain::Unknown),
        )
    }

    fn shared(author: u8, generation: u64, millis: u64) -> SharedObservations {
        SharedObservations {
            author: u32::from(author),
            authority: [author; 96],
            generation,
            estimates: vec![(Key::SplitCoins, Duration::from_millis(millis))],
        }
    }

    #[test]
    fn unobserved_commands_take_the_default_by_their_length() {
        let estimator = estimator(&[1]);
        // A split of one amount counts twice.
        assert_eq!(estimator.estimate(&[SPLIT]), Duration::from_millis(2));
    }

    #[test]
    fn the_estimate_is_the_stake_weighted_median_of_the_latest_observations() {
        let mut estimator = estimator(&[3_000, 3_000, 4_000]);
        estimator.observe(&shared(0, 1, 10));
        // Too little stake has observed it.
        assert_eq!(estimator.estimate(&[SPLIT]), Duration::from_millis(2));
        estimator.observe(&shared(1, 1, 20));
        estimator.observe(&shared(2, 1, 30));
        // 3,000 + 3,000 of 10,000 is past half at 20.
        assert_eq!(estimator.estimate(&[SPLIT]), Duration::from_millis(40));
        // An older generation is ignored; a newer one replaces.
        estimator.observe(&shared(1, 1, 100));
        assert_eq!(estimator.estimate(&[SPLIT]), Duration::from_millis(40));
        estimator.observe(&shared(1, 2, 100));
        assert_eq!(estimator.estimate(&[SPLIT]), Duration::from_millis(60));
    }

    #[test]
    fn observations_not_from_their_blocks_author_are_ignored() {
        let mut estimator = estimator(&[5_000, 5_000]);
        let mut forged = shared(0, 1, 50);
        forged.author = 1;
        assert!(!estimator.observe(&forged));
        assert_eq!(estimator.estimate(&[SPLIT]), Duration::from_millis(2));
    }
}
