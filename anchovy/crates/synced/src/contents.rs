// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A certified checkpoint's full contents, checked against its summary and
//! split into settlement chunks (`docs/state_sync_design.md`, The chunk
//! lock).

use messages::arena::BumpAlloc;
use messages::base::Digest;
use messages::checkpoint::{ExecutionData, VersionedFullCheckpointContents};
use messages::effects::VersionedEffects;
use messages::fast::Bump;
use messages::reader::Reader;
use messages::transaction::TransactionKind;
use messages::tx_index::SUI_ACCUMULATOR_ROOT_OBJECT_ID;

/// Why fetched contents are not a certified checkpoint's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    Malformed,
    /// Not the contents the summary commits to.
    Digest,
    /// Effects of another transaction than the one they come with.
    Unmatched,
}

/// `bytes`, a `VersionedFullCheckpointContents`, if they are the contents
/// whose digest is `digest`, each transaction with its own effects (R3.5).
/// User signatures are not checked (R3.6).
pub fn verify<'a>(
    bytes: &'a [u8],
    digest: &Digest,
    bump: &'a Bump,
) -> Result<VersionedFullCheckpointContents<'a>, Invalid> {
    let mut r = Reader::new(bytes);
    let full = VersionedFullCheckpointContents::parse(&mut r, &mut BumpAlloc(bump))
        .and_then(|c| r.finish().map(|()| c))
        .map_err(|_| Invalid::Malformed)?;
    let contents = full.contents(bump).ok_or(Invalid::Malformed)?;
    if contents.digest != *digest {
        return Err(Invalid::Digest);
    }
    for t in full.transactions() {
        let VersionedEffects::V2(effects) = &t.effects.version else {
            return Err(Invalid::Malformed);
        };
        if effects.transaction_digest != t.transaction.digest().digest() {
            return Err(Invalid::Unmatched);
        }
    }
    Ok(full)
}

/// `bytes`, contents `verify` passed already.
pub fn parse<'a>(bytes: &'a [u8], bump: &'a Bump) -> VersionedFullCheckpointContents<'a> {
    VersionedFullCheckpointContents::parse(&mut Reader::new(bytes), &mut BumpAlloc(bump))
        .expect("verified as fetched")
}

/// Where each settlement chunk of `transactions` ends: just past its
/// barrier, the one transaction of the chunk whose effects change the
/// accumulator root.
///
/// # Panics
/// If anything follows the last barrier but the epoch's change-epoch
/// transaction: the quorum certified a checkpoint no validator builds.
pub fn chunk_ends<'b>(
    transactions: &[ExecutionData<'_>],
    bump: &'b Bump,
) -> containers::Vec<'b, usize> {
    let mut ends = containers::Vec::new_in(bump);
    for (i, t) in transactions.iter().enumerate() {
        let VersionedEffects::V2(effects) = &t.effects.version else {
            unreachable!("verified effects are V2");
        };
        if effects
            .changed_objects
            .iter()
            .any(|c| *c.id == SUI_ACCUMULATOR_ROOT_OBJECT_ID)
        {
            ends.push(i + 1);
        }
    }
    let settled = ends.last().copied().unwrap_or(0);
    let rest = &transactions[settled..];
    assert!(
        rest.is_empty()
            || rest.len() == 1
                && matches!(
                    rest[0].transaction.data().kind(),
                    TransactionKind::EndOfEpochTransaction(_)
                ),
        "a certified checkpoint has transactions after its last barrier"
    );
    ends
}
