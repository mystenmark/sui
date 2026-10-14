// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The consensus status cache: statuses read before and after they are set,
//! expiry against the committed leader round, and positions as the reference
//! encodes them.

use std::time::Duration;

use consensus::BlockRef;
use validator::consensus::status::{
    ConsensusPosition, ConsensusTxStatus, ConsensusTxStatusCache, RETENTION_ROUNDS, StatusResult,
};

fn position(round: u32, index: u16) -> ConsensusPosition {
    ConsensusPosition {
        epoch: 3,
        block: BlockRef {
            round,
            author: 1,
            digest: [round as u8; 32],
        },
        index,
    }
}

#[tokio::test]
async fn a_status_set_before_the_read_is_found() {
    let cache = ConsensusTxStatusCache::new();
    let p = position(10, 2);
    cache.set_statuses([(p.block, p.index, ConsensusTxStatus::Dropped)]);
    assert_eq!(
        cache.notify_read(p).await,
        StatusResult::Status(ConsensusTxStatus::Dropped)
    );
}

#[tokio::test]
async fn a_read_waits_for_its_status() {
    let cache = std::sync::Arc::new(ConsensusTxStatusCache::new());
    let p = position(10, 2);
    let read = tokio::spawn({
        let cache = cache.clone();
        async move { cache.notify_read(p).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!read.is_finished());
    // Another position's status does not answer it.
    cache.set_statuses([(p.block, 3, ConsensusTxStatus::Rejected)]);
    cache.set_statuses([(p.block, p.index, ConsensusTxStatus::Finalized)]);
    assert_eq!(
        read.await.unwrap(),
        StatusResult::Status(ConsensusTxStatus::Finalized)
    );
}

#[tokio::test]
async fn a_position_expires_against_the_previous_leader_round() {
    let cache = std::sync::Arc::new(ConsensusTxStatusCache::new());
    let p = position(10, 0);
    let read = tokio::spawn({
        let cache = cache.clone();
        async move { cache.notify_read(p).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    // Expiry uses the round before the latest, so this one does not expire it.
    cache.update_leader_round(10 + RETENTION_ROUNDS);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!read.is_finished());
    cache.update_leader_round(11 + RETENTION_ROUNDS);
    assert_eq!(
        read.await.unwrap(),
        StatusResult::Expired(10 + RETENTION_ROUNDS)
    );
    // Its status, set too late, is ignored; a new read expires at once.
    cache.set_statuses([(p.block, p.index, ConsensusTxStatus::Finalized)]);
    assert_eq!(
        cache.notify_read(p).await,
        StatusResult::Expired(10 + RETENTION_ROUNDS)
    );
}

#[tokio::test]
async fn statuses_are_kept_until_they_expire() {
    let cache = ConsensusTxStatusCache::new();
    let (old, recent) = (position(5, 0), position(500, 0));
    cache.update_leader_round(1);
    cache.set_statuses([
        (old.block, old.index, ConsensusTxStatus::Finalized),
        (recent.block, recent.index, ConsensusTxStatus::Finalized),
    ]);
    cache.update_leader_round(5 + RETENTION_ROUNDS);
    cache.update_leader_round(6 + RETENTION_ROUNDS);
    assert_eq!(
        cache.notify_read(old).await,
        StatusResult::Expired(5 + RETENTION_ROUNDS)
    );
    assert_eq!(
        cache.notify_read(recent).await,
        StatusResult::Status(ConsensusTxStatus::Finalized)
    );
    assert!(cache.too_far_ahead(&position(6 + 2 * RETENTION_ROUNDS, 0)));
    assert!(!cache.too_far_ahead(&position(5 + 2 * RETENTION_ROUNDS, 0)));
}

#[test]
#[should_panic(expected = "conflicting statuses")]
fn every_handler_decides_alike() {
    let cache = ConsensusTxStatusCache::new();
    let p = position(10, 2);
    cache.set_statuses([(p.block, p.index, ConsensusTxStatus::Finalized)]);
    cache.set_statuses([(p.block, p.index, ConsensusTxStatus::Dropped)]);
}

#[test]
fn a_position_encodes_as_the_reference_s() {
    let p = position(77, 9);
    let bytes = p.to_bcs();
    let reference: sui_types::messages_consensus::ConsensusPosition =
        bcs::from_bytes(&bytes).unwrap();
    assert_eq!(reference.epoch, 3);
    assert_eq!(reference.block.round, 77);
    assert_eq!(reference.index, 9);
    assert_eq!(bcs::to_bytes(&reference).unwrap(), bytes);
    assert_eq!(ConsensusPosition::from_bcs(&bytes), Some(p));
}
