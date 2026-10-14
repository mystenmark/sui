// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Random `TransactionEffects::V2` covering every kind of object change.

use std::sync::{Arc, LazyLock};

use messages::arena::Ref;
use messages::base::{Digest, ObjectId, SuiAddress};
use messages::effects::{
    AccumulatorOperation, AccumulatorValue, AccumulatorWriteV1, GasCostSummary, IdOperation,
    ObjectIn, ObjectOut,
};
use messages::execution_status::ExecutionStatus;
use messages::fast::{Bump, EffectsBuilder};
use messages::object::Owner;
use messages::type_tag::TypeTag;
use validator::pipeline::items::ExecutedEffects;

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn bytes(&mut self) -> [u8; 32] {
        let mut out = [0; 32];
        for chunk in out.chunks_mut(8) {
            chunk.copy_from_slice(&self.next().to_le_bytes());
        }
        out
    }
}

static OWNER: SuiAddress = SuiAddress([7; 32]);

static ACCUMULATOR: LazyLock<AccumulatorWriteV1<'static>> = LazyLock::new(|| AccumulatorWriteV1 {
    address: &OWNER,
    ty: TypeTag::U64,
    operation: AccumulatorOperation::Merge,
    value: AccumulatorValue::Integer(5),
});

/// How many kinds of change `change` makes.
pub const KINDS: u64 = 11;

/// Change kind `kind` with fresh digests: created object and package,
/// mutated object and package, unwrapped, deleted, wrapped,
/// unwrapped-then-deleted, transient, accumulator write, unclassified.
fn change(rng: &mut Rng, kind: u64) -> (ObjectIn<'static>, ObjectOut<'static>, IdOperation) {
    let mut digest = || -> &'static Digest { Box::leak(Box::new(Digest::new(rng.bytes()))) };
    let owner = Owner::AddressOwner(&OWNER);
    let exist = |digest| ObjectIn::Exist {
        version: 3,
        digest,
        owner,
    };
    match kind {
        0 => (
            ObjectIn::NotExist,
            ObjectOut::ObjectWrite(digest(), owner),
            IdOperation::Created,
        ),
        1 => (
            ObjectIn::NotExist,
            ObjectOut::PackageWrite(1, digest()),
            IdOperation::Created,
        ),
        2 => (
            exist(digest()),
            ObjectOut::ObjectWrite(digest(), owner),
            IdOperation::None,
        ),
        3 => (
            exist(digest()),
            ObjectOut::PackageWrite(2, digest()),
            IdOperation::None,
        ),
        4 => (
            ObjectIn::NotExist,
            ObjectOut::ObjectWrite(digest(), owner),
            IdOperation::None,
        ),
        5 => (exist(digest()), ObjectOut::NotExist, IdOperation::Deleted),
        6 => (exist(digest()), ObjectOut::NotExist, IdOperation::None),
        7 => (
            ObjectIn::NotExist,
            ObjectOut::NotExist,
            IdOperation::Deleted,
        ),
        8 => (
            ObjectIn::NotExist,
            ObjectOut::NotExist,
            IdOperation::Created,
        ),
        9 => (
            ObjectIn::NotExist,
            ObjectOut::AccumulatorWriteV1(Ref::new(&ACCUMULATOR)),
            IdOperation::None,
        ),
        _ => (
            ObjectIn::NotExist,
            ObjectOut::ObjectWrite(digest(), owner),
            IdOperation::Deleted,
        ),
    }
}

/// One transaction's effects with `changes` changes, each of a kind
/// `pick` chooses.
pub fn transaction(
    rng: &mut Rng,
    changes: usize,
    mut pick: impl FnMut(&mut Rng) -> u64,
) -> ExecutedEffects {
    let bump = Bump::with_capacity(4096);
    let mut builder = EffectsBuilder::new_in(
        &bump,
        ExecutionStatus::Success,
        0,
        GasCostSummary {
            computation_cost: 1,
            storage_cost: 2,
            storage_rebate: 3,
            non_refundable_storage_fee: 4,
        },
        Digest::new(rng.bytes()),
        4,
    );
    let mut ids: Vec<ObjectId> = (0..changes).map(|_| ObjectId(rng.bytes())).collect();
    ids.sort_unstable();
    for id in ids {
        let kind = pick(rng);
        let (input, output, operation) = change(rng, kind);
        builder.change(id, input, output, operation);
    }
    let built = builder.finish();
    ExecutedEffects {
        digest: rng.bytes(),
        effects_digest: built.digest.bytes,
        effects: Arc::from(built.bytes),
    }
}

/// A checkpoint of `transactions` transactions, each with one to eight
/// changes of any kind.
pub fn checkpoint(rng: &mut Rng, transactions: usize) -> Vec<ExecutedEffects> {
    (0..transactions)
        .map(|_| {
            let changes = 1 + rng.below(8) as usize;
            transaction(rng, changes, |rng| rng.below(KINDS))
        })
        .collect()
}
