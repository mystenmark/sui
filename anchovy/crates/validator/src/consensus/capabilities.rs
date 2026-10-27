// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Protocol upgrades, agreed as the reference agrees them: each validator
//! announces the protocol versions and system packages its binary supports
//! (`CapabilityNotificationV2`), the commit handler keeps each one's latest
//! announcement, and the epoch's last commit picks the highest version
//! enough stake supports alike (`docs/protocol_upgrade_reference.md`).

use messages::build::base::{Digest, ObjectRef};
use messages::build::consensus::AuthorityCapabilitiesV2;
use sui_protocol_config::ProtocolConfig;

/// A capability notification as a commit carries it: from the block of the
/// committee member `author`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Announced {
    pub author: u32,
    pub capabilities: AuthorityCapabilitiesV2,
}

/// Each committee member's latest capabilities, by committee index.
pub struct CapabilitySet {
    /// Each member's name, by committee index.
    names: Vec<[u8; 96]>,
    by_member: Vec<Option<AuthorityCapabilitiesV2>>,
}

impl CapabilitySet {
    /// None yet, for `committee` (each member's name and stake).
    pub fn new(committee: &[([u8; 96], u64)]) -> CapabilitySet {
        CapabilitySet {
            names: committee.iter().map(|(name, _)| *name).collect(),
            by_member: vec![None; committee.len()],
        }
    }

    /// Records `announced`, unless it is not from its block's author
    /// (`verify_consensus_transaction`), or that author's kept capabilities
    /// have a generation at least as high (`record_capabilities_v2`):
    /// whether it was recorded.
    pub fn record(&mut self, announced: Announced) -> bool {
        let Announced {
            author,
            capabilities,
        } = announced;
        let author = author as usize;
        if self.names.get(author) != Some(&capabilities.authority.0) {
            return false;
        }
        if self.by_member[author]
            .as_ref()
            .is_some_and(|kept| kept.generation >= capabilities.generation)
        {
            return false;
        }
        self.by_member[author] = Some(capabilities);
        true
    }
}

/// A recorded notification, as the epoch store keeps it.
pub fn encode(announced: &Announced) -> Vec<u8> {
    bcs::to_bytes(&(announced.author, &announced.capabilities)).expect("capabilities serialize")
}

/// # Panics
/// Unless `bytes` are as `encode` made them.
pub fn decode(bytes: &[u8]) -> Announced {
    let (author, capabilities) = bcs::from_bytes(bytes).expect("capabilities as encoded");
    Announced {
        author,
        capabilities,
    }
}

/// The reference's `choose_protocol_version_and_system_packages_v2`: from
/// `current`, the highest version, and its system packages, that each next
/// version up is supported by enough of `committee`'s stake (each member's
/// name and stake, by index) alike; `current` and none if no next version
/// is.
pub fn choose(
    current: u64,
    config: &ProtocolConfig,
    committee: &[([u8; 96], u64)],
    capabilities: &CapabilitySet,
    buffer_stake_bps: u64,
) -> (u64, Vec<ObjectRef>) {
    let mut chosen = (current, Vec::new());
    while let Some(next) = supported(
        current,
        chosen.0 + 1,
        config,
        committee,
        capabilities,
        buffer_stake_bps,
    ) {
        chosen = next;
    }
    chosen
}

/// The reference's `is_protocol_version_supported_v2`: `proposed`, and the
/// system packages to go with it, if one group of members announcing the
/// same config digest for it and the same packages holds a quorum plus
/// `buffer_stake_bps` of the remaining stake.
fn supported(
    current: u64,
    proposed: u64,
    config: &ProtocolConfig,
    committee: &[([u8; 96], u64)],
    capabilities: &CapabilitySet,
    buffer_stake_bps: u64,
) -> Option<(u64, Vec<ObjectRef>)> {
    if proposed > current + 1 && !config.advance_to_highest_supported_protocol_version() {
        return None;
    }
    let buffer_stake_bps = buffer_stake_bps.min(10_000);
    // A member announcing no packages votes against any change, as does one
    // without `proposed`.
    let mut desired: Vec<(Digest, Vec<ObjectRef>, usize)> = capabilities
        .by_member
        .iter()
        .enumerate()
        .filter_map(|(member, kept)| {
            let kept = kept.as_ref()?;
            if kept.available_system_packages.is_empty() {
                return None;
            }
            let digest = kept
                .supported_protocol_versions
                .versions
                .iter()
                .find(|(version, _)| version.0 == proposed)
                .map(|(_, digest)| *digest)?;
            let mut packages = kept.available_system_packages.clone();
            packages.sort();
            Some((digest, packages, member))
        })
        .collect();
    desired.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));

    let total: u64 = committee.iter().map(|(_, stake)| stake).sum();
    let quorum = total * 2 / 3 + 1;
    let threshold = quorum + ((total - quorum) * buffer_stake_bps).div_ceil(10_000);
    // At most one group can reach the threshold.
    desired
        .chunk_by(|a, b| (&a.0, &a.1) == (&b.0, &b.1))
        .find(|group| {
            group
                .iter()
                .map(|(_, _, member)| committee[*member].1)
                .sum::<u64>()
                >= threshold
        })
        .map(|group| (proposed, group[0].1.clone()))
}

#[cfg(test)]
mod tests {
    use messages::build::base::{
        AccountAddress, AuthorityPublicKeyBytes, ObjectDigest, ObjectId, ProtocolVersion,
        SequenceNumber,
    };
    use messages::build::consensus::SupportedProtocolVersionsWithHashes;
    use sui_protocol_config::Chain;

    use super::*;

    const MAX: u64 = sui_protocol_config::ProtocolVersion::MAX.as_u64();

    fn committee() -> Vec<([u8; 96], u64)> {
        (0..4).map(|i| ([i; 96], 2_500)).collect()
    }

    fn config() -> ProtocolConfig {
        ProtocolConfig::get_for_version(sui_protocol_config::ProtocolVersion::MAX, Chain::Unknown)
    }

    fn package(version: u64) -> ObjectRef {
        (
            ObjectId(AccountAddress([2; 32])),
            SequenceNumber(version),
            ObjectDigest(Digest([version as u8; 32])),
        )
    }

    fn announced(
        member: u8,
        generation: u64,
        versions: &[(u64, u8)],
        packages: Vec<ObjectRef>,
    ) -> Announced {
        Announced {
            author: u32::from(member),
            capabilities: AuthorityCapabilitiesV2 {
                authority: AuthorityPublicKeyBytes([member; 96]),
                generation,
                supported_protocol_versions: SupportedProtocolVersionsWithHashes {
                    versions: versions
                        .iter()
                        .map(|&(v, d)| (ProtocolVersion(v), Digest([d; 32])))
                        .collect(),
                },
                available_system_packages: packages,
            },
        }
    }

    fn set(announced: impl IntoIterator<Item = Announced>) -> CapabilitySet {
        let mut set = CapabilitySet::new(&committee());
        for a in announced {
            set.record(a);
        }
        set
    }

    #[test]
    fn a_version_needs_a_quorum_and_half_the_rest() {
        // 10,000 in all: a quorum of 6,667 and half of 3,333, 8,334.
        let three = set((0..3).map(|m| announced(m, 1, &[(MAX, 1)], vec![package(1)])));
        assert_eq!(
            choose(MAX - 1, &config(), &committee(), &three, 5_000),
            (MAX - 1, vec![])
        );
        let four = set((0..4).map(|m| announced(m, 1, &[(MAX, 1)], vec![package(1)])));
        assert_eq!(
            choose(MAX - 1, &config(), &committee(), &four, 5_000),
            (MAX, vec![package(1)])
        );
        // Without the buffer, three suffice.
        assert_eq!(
            choose(MAX - 1, &config(), &committee(), &three, 0),
            (MAX, vec![package(1)])
        );
    }

    #[test]
    fn supporters_must_agree_on_digest_and_packages() {
        let digests = set((0..4).map(|m| announced(m, 1, &[(MAX, m % 2)], vec![package(1)])));
        assert_eq!(
            choose(MAX - 1, &config(), &committee(), &digests, 0).0,
            MAX - 1
        );
        let packages =
            set((0..4).map(|m| announced(m, 1, &[(MAX, 1)], vec![package(u64::from(m % 2) + 1)])));
        assert_eq!(
            choose(MAX - 1, &config(), &committee(), &packages, 0).0,
            MAX - 1
        );
        // Packages are compared as sets.
        let reordered = set((0..4).map(|m| {
            let mut packages = vec![package(1), package(2)];
            if m % 2 == 0 {
                packages.reverse();
            }
            announced(m, 1, &[(MAX, 1)], packages)
        }));
        assert_eq!(
            choose(MAX - 1, &config(), &committee(), &reordered, 0).0,
            MAX
        );
    }

    #[test]
    fn no_packages_votes_against_any_change() {
        let none = set((0..4).map(|m| announced(m, 1, &[(MAX, 1)], vec![])));
        assert_eq!(
            choose(MAX - 1, &config(), &committee(), &none, 0).0,
            MAX - 1
        );
    }

    #[test]
    fn versions_are_skipped_to_the_highest_supported() {
        let all = set((0..4).map(|m| announced(m, 1, &[(MAX - 1, 1), (MAX, 2)], vec![package(1)])));
        assert_eq!(choose(MAX - 2, &config(), &committee(), &all, 0).0, MAX);
    }

    #[test]
    fn the_latest_generation_from_the_block_s_author_counts() {
        let mut set = CapabilitySet::new(&committee());
        assert!(set.record(announced(0, 2, &[(MAX, 1)], vec![package(1)])));
        assert!(
            !set.record(announced(0, 2, &[], vec![])),
            "an equal generation"
        );
        assert!(!set.record(announced(0, 1, &[], vec![])), "an older one");
        let mut forged = announced(1, 3, &[], vec![]);
        forged.author = 0;
        assert!(!set.record(forged), "not the author's");
        assert!(set.record(announced(0, 3, &[], vec![])));
    }
}
