// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The JSON web keys validators fetch for zkLogin, agreed through consensus
//! as the reference's: each validator votes for the keys it fetched, a key
//! a quorum voted for becomes active, and the next commit's
//! `AuthenticatorStateUpdate` adds it to the authenticator state.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use messages::build::system_transaction::{Jwk, JwkId};

/// The reference's `MAX_TOTAL_JWK_SIZE`.
const MAX_TOTAL_JWK_SIZE: usize = 4096;

/// A key one validator fetched, as a commit carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JwkVote {
    /// The author of the block carrying it.
    pub author: u32,
    /// The authority it claims to come from; ignored unless the author.
    pub authority: [u8; 96],
    pub id: JwkId,
    pub jwk: Jwk,
}

impl JwkVote {
    /// The reference's `check_total_jwk_size`.
    fn small_enough(&self) -> bool {
        let JwkId { iss, kid } = &self.id;
        let Jwk { kty, e, n, alg } = &self.jwk;
        iss.len() + kid.len() + kty.len() + alg.len() + e.len() + n.len() <= MAX_TOTAL_JWK_SIZE
    }
}

/// The reference's `JwkAggregator`: the stake behind each key, and how many
/// keys each validator voted for.
pub struct JwkAggregator {
    /// Each member's name and stake, by committee index.
    committee: Vec<([u8; 96], u64)>,
    quorum: u64,
    max_votes: u64,
    stakes: HashMap<(JwkId, Jwk), (HashSet<u32>, u64)>,
    votes: HashMap<u32, u64>,
}

impl JwkAggregator {
    /// No votes yet; each validator's votes counted up to `max_votes`.
    pub fn new(committee: Vec<([u8; 96], u64)>, max_votes: u64) -> JwkAggregator {
        let total: u64 = committee.iter().map(|(_, stake)| stake).sum();
        JwkAggregator {
            committee,
            quorum: u64::try_from(u128::from(total) * 2 / 3).expect("below the total") + 1,
            max_votes,
            stakes: HashMap::new(),
            votes: HashMap::new(),
        }
    }

    /// Counts `vote`, unless it is not from its block's author or too large
    /// (`verify_consensus_transaction`), or its author has voted its fill
    /// (`record_jwk_vote`): if counted, whether it made its key active.
    pub fn vote(&mut self, vote: &JwkVote) -> Option<bool> {
        let (name, stake) = self.committee.get(vote.author as usize)?;
        if *name != vote.authority || !vote.small_enough() {
            return None;
        }
        if self.votes.get(&vote.author).copied().unwrap_or(0) >= self.max_votes {
            return None;
        }
        let key = (vote.id.clone(), vote.jwk.clone());
        let previously_active = self.is_active(&key);
        self.insert(vote.author, *stake, key.clone());
        Some(!previously_active && self.is_active(&key))
    }

    /// Counts a vote counted before a restart, unchecked, as the reference
    /// restores its `pending_jwks`: `author`'s for `key`.
    pub fn restore(&mut self, author: u32, key: (JwkId, Jwk)) {
        let stake = self.committee[author as usize].1;
        self.insert(author, stake, key);
    }

    fn insert(&mut self, author: u32, stake: u64, key: (JwkId, Jwk)) {
        let (voters, total) = self.stakes.entry(key).or_default();
        if voters.insert(author) {
            *total += stake;
            *self.votes.entry(author).or_default() += 1;
        }
    }

    fn is_active(&self, key: &(JwkId, Jwk)) -> bool {
        self.stakes
            .get(key)
            .is_some_and(|(_, total)| *total >= self.quorum)
    }
}

/// A counted vote, as the epoch store keeps it.
pub fn encode_vote(author: u32, key: &(JwkId, Jwk)) -> Vec<u8> {
    bcs::to_bytes(&(author, key)).expect("a vote serializes")
}

/// # Panics
/// Unless `bytes` are as `encode_vote` made them.
pub fn decode_vote(bytes: &[u8]) -> (u32, (JwkId, Jwk)) {
    bcs::from_bytes(bytes).expect("a vote as encoded")
}

/// The reference's order of active keys, `jwk_lt` in
/// `authenticator_state.move`: by issuer, key id and then the key's fields,
/// each string shorter first, then by its bytes.
pub fn jwk_order(a: &(JwkId, Jwk), b: &(JwkId, Jwk)) -> Ordering {
    fn strings((id, jwk): &(JwkId, Jwk)) -> [(usize, &[u8]); 6] {
        [&id.iss, &id.kid, &jwk.kty, &jwk.e, &jwk.n, &jwk.alg].map(|s| (s.len(), s.as_bytes()))
    }
    strings(a).cmp(&strings(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vote(author: u8, kid: &str) -> JwkVote {
        JwkVote {
            author: u32::from(author),
            authority: [author; 96],
            id: JwkId {
                iss: "https://issuer".to_owned(),
                kid: kid.to_owned(),
            },
            jwk: Jwk {
                kty: "RSA".to_owned(),
                e: "AQAB".to_owned(),
                n: "n".to_owned(),
                alg: "RS256".to_owned(),
            },
        }
    }

    fn aggregator(max_votes: u64) -> JwkAggregator {
        JwkAggregator::new((0..4).map(|i| ([i; 96], 2_500)).collect(), max_votes)
    }

    #[test]
    fn a_key_becomes_active_once_with_a_quorum() {
        let mut aggregator = aggregator(10);
        assert_eq!(aggregator.vote(&vote(0, "a")), Some(false));
        assert_eq!(
            aggregator.vote(&vote(0, "a")),
            Some(false),
            "a repeat adds nothing"
        );
        assert_eq!(aggregator.vote(&vote(1, "a")), Some(false));
        assert_eq!(
            aggregator.vote(&vote(2, "a")),
            Some(true),
            "7,500 of 10,000"
        );
        assert_eq!(
            aggregator.vote(&vote(3, "a")),
            Some(false),
            "active already"
        );
    }

    #[test]
    fn votes_beyond_a_validator_s_fill_and_forgeries_are_ignored() {
        let mut aggregator = aggregator(1);
        for author in 0..3 {
            aggregator.vote(&vote(author, "a"));
        }
        // Each has voted its one key.
        assert_eq!(aggregator.vote(&vote(0, "b")), None);
        let mut forged = vote(3, "a");
        forged.author = 2;
        assert_eq!(aggregator.vote(&forged), None);
    }

    #[test]
    fn keys_order_by_length_then_bytes() {
        let key = |kid: &str| (vote(0, kid).id, vote(0, kid).jwk);
        let mut keys = [key("bb"), key("c"), key("ab")];
        keys.sort_by(jwk_order);
        let kids: Vec<_> = keys.iter().map(|(id, _)| id.kid.as_str()).collect();
        assert_eq!(kids, ["c", "ab", "bb"]);
    }
}
