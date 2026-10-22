// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A chunk's settlement transactions and barrier, byte for byte as the
//! reference's `AccumulatorSettlementTxBuilder` and
//! `build_accumulator_barrier_tx` build them.

use containers::{BTreeMap, Bump};
use std::hash::BuildHasher;

use hashbrown::HashTable;
use messages::base::{Digest, ObjectId};
use messages::build::base::{
    AccountAddress, Digest as DigestB, ObjectDigest, ObjectId as ObjectIdB, SequenceNumber,
    SuiAddress,
};
use messages::build::transaction::{
    Argument, CallArg, Command, GasData, ObjectArg, ProgrammableMoveCall, ProgrammableTransaction,
    SharedObjectMutability, TransactionData, TransactionDataV1, TransactionExpiration,
    TransactionKind,
};
use messages::build::type_tag::{TypeInput, TypeTag};
use messages::effects::{
    AccumulatorOperation, AccumulatorValue, IdOperation, ObjectOut, TransactionEffects,
    VersionedEffects,
};
use messages::type_tag::TypeTag as TypeTagView;
use serde::Serialize;

/// `SUI_ACCUMULATOR_ROOT_OBJECT_ID`.
pub const ACCUMULATOR_ROOT: ObjectId = ObjectId::from_u16(0xacc);
/// `SUI_FRAMEWORK_ADDRESS`.
const FRAMEWORK: [u8; 32] = ObjectId::from_u16(0x2).0;
const SETTLEMENT_MODULE: &str = "accumulator_settlement";
const METADATA_MODULE: &str = "accumulator_metadata";
/// The reference's `GAS_PRICE_FOR_SYSTEM_TX`.
const SYSTEM_GAS_PRICE: u64 = 1;
/// `Ed25519SuiSignature::LENGTH`: a flag byte, a signature and a key.
const ED25519_SIGNATURE_LENGTH: usize = 1 + 64 + 32;

/// A transaction the pipeline builds: `Transaction` BCS and its digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    pub bytes: Vec<u8>,
    pub digest: [u8; 32],
}

/// `VerifiedTransaction::new_system_transaction`: the kind, sent by `0x0`
/// with the reference's placeholder gas, signed by an all-zero Ed25519
/// signature.
pub fn system_transaction(kind: TransactionKind) -> Built {
    let zero = SuiAddress([0; 32]);
    let data = TransactionData::V1(TransactionDataV1 {
        kind,
        sender: zero,
        gas_data: GasData {
            payment: vec![(
                ObjectIdB(AccountAddress([0; 32])),
                SequenceNumber(0),
                ObjectDigest(DigestB([0; 32])),
            )],
            owner: zero,
            price: SYSTEM_GAS_PRICE,
            budget: 0,
        },
        expiration: TransactionExpiration::None,
    });
    let data = bcs::to_bytes(&data).expect("transaction data serializes");
    let digest = Digest::of("TransactionData", &data).bytes;
    // `SenderSignedData` is a one-element sequence of the intent (three zero
    // bytes, `Intent::sui_transaction`), the data, and one signature; the
    // envelope's `EmptySignInfo` adds nothing.
    let mut bytes = Vec::with_capacity(4 + data.len() + 2 + ED25519_SIGNATURE_LENGTH);
    bytes.extend_from_slice(&[1, 0, 0, 0]);
    bytes.extend_from_slice(&data);
    bytes.extend_from_slice(&[1, ED25519_SIGNATURE_LENGTH as u8]);
    bytes.resize(bytes.len() + ED25519_SIGNATURE_LENGTH, 0);
    Built { bytes, digest }
}

/// `ProgrammableTransactionBuilder`, as far as settlement uses it: the
/// accumulator root first, then pure inputs, each distinct value once.
struct Ptb {
    inputs: Vec<CallArg>,
    /// Pure inputs by their bytes, as indices into `inputs`.
    pure: HashTable<u16>,
    hasher: foldhash::fast::FixedState,
    commands: Vec<Command>,
}

impl Ptb {
    fn new(mutability: SharedObjectMutability, initial_shared_version: u64) -> Ptb {
        Ptb {
            inputs: vec![CallArg::Object(ObjectArg::SharedObject {
                id: ObjectIdB(AccountAddress(ACCUMULATOR_ROOT.0)),
                initial_shared_version: SequenceNumber(initial_shared_version),
                mutability,
            })],
            pure: HashTable::new(),
            hasher: foldhash::fast::FixedState::default(),
            commands: Vec::new(),
        }
    }

    const ROOT: Argument = Argument::Input(0);

    fn pure(&mut self, value: impl Serialize) -> Argument {
        let bytes = bcs::to_bytes(&value).expect("pure value serializes");
        let hash = self.hasher.hash_one(&bytes);
        let inputs = &self.inputs;
        let found = self.pure.find(
            hash,
            |&i| matches!(&inputs[usize::from(i)], CallArg::Pure(b) if *b == bytes),
        );
        if let Some(&i) = found {
            return Argument::Input(i);
        }
        let i = u16::try_from(self.inputs.len()).expect("fewer than 2^16 inputs");
        self.inputs.push(CallArg::Pure(bytes));
        let (inputs, hasher) = (&self.inputs, &self.hasher);
        self.pure.insert_unique(hash, i, |&j| {
            let CallArg::Pure(b) = &inputs[usize::from(j)] else {
                unreachable!("only pure inputs are indexed")
            };
            hasher.hash_one(b)
        });
        Argument::Input(i)
    }

    fn call(
        &mut self,
        module: &str,
        function: &str,
        type_arguments: Vec<TypeInput>,
        arguments: Vec<Argument>,
    ) {
        self.commands
            .push(Command::MoveCall(Box::new(ProgrammableMoveCall {
                package: ObjectIdB(AccountAddress(FRAMEWORK)),
                module: module.to_owned(),
                function: function.to_owned(),
                type_arguments,
                arguments,
            })));
    }

    /// `AccumulatorSettlementTxBuilder::add_prologue`.
    fn prologue(&mut self, epoch: u64, height: u64, index: u64, input_sui: u64, output_sui: u64) {
        let arguments = vec![
            Ptb::ROOT,
            self.pure(epoch),
            self.pure(height),
            self.pure(index),
            self.pure(input_sui),
            self.pure(output_sui),
        ];
        self.call(SETTLEMENT_MODULE, "settlement_prologue", vec![], arguments);
    }

    fn finish(self) -> TransactionKind {
        TransactionKind::ProgrammableSystemTransaction(ProgrammableTransaction {
            inputs: self.inputs,
            commands: self.commands,
        })
    }
}

/// The reference's `EventCommitment`, whose BCS the event Merkle tree
/// hashes.
#[derive(Serialize)]
struct EventCommitment {
    checkpoint_seq: u64,
    transaction_idx: u64,
    event_idx: u64,
    digest: DigestB,
}

/// `MergedValueIntermediate`.
enum Merged {
    Sum(u128),
    SumPair(u128, u128),
    Events(Vec<EventCommitment>),
}

impl Merged {
    fn zero(value: &AccumulatorValue<'_>) -> Merged {
        match value {
            AccumulatorValue::Integer(_) => Merged::Sum(0),
            AccumulatorValue::IntegerTuple(..) => Merged::SumPair(0, 0),
            AccumulatorValue::EventDigest(_) => Merged::Events(Vec::new()),
        }
    }

    fn accumulate(&mut self, value: &AccumulatorValue<'_>, checkpoint_seq: u64, tx_index: u64) {
        match (self, value) {
            (Merged::Sum(sum), AccumulatorValue::Integer(v)) => *sum += u128::from(*v),
            (Merged::SumPair(a, b), AccumulatorValue::IntegerTuple(x, y)) => {
                *a += u128::from(*x);
                *b += u128::from(*y);
            }
            (Merged::Events(commitments), AccumulatorValue::EventDigest(digests)) => {
                commitments.extend(digests.iter().map(|c| EventCommitment {
                    checkpoint_seq,
                    transaction_idx: tx_index,
                    event_idx: c.index.get(),
                    digest: DigestB(c.digest.bytes),
                }));
            }
            _ => panic!("an accumulator's events disagree on its value type"),
        }
    }
}

/// One accumulator's changes over a chunk.
struct Update {
    address: [u8; 32],
    ty: TypeTag,
    merge: Merged,
    split: Merged,
    /// SUI deposited into and withdrawn from the accumulator, in the
    /// settlement's terms: deposits are its input, withdrawals its output.
    input_sui: u64,
    output_sui: u64,
}

fn is_struct(tag: &TypeTagView<'_>, module: &str, name: &str) -> bool {
    matches!(tag, TypeTagView::Struct(s)
        if s.address.0 == FRAMEWORK && s.module == module && s.name == name)
}

/// `GasCoin::is_gas_balance`: `0x2::balance::Balance<0x2::sui::SUI>`.
fn is_sui_balance(tag: &TypeTagView<'_>) -> bool {
    let TypeTagView::Struct(s) = tag else {
        return false;
    };
    is_struct(tag, "balance", "Balance")
        && s.type_params.len() == 1
        && matches!(&s.type_params[0], TypeTagView::Struct(inner)
            if inner.type_params.is_empty())
        && is_struct(&s.type_params[0], "sui", "SUI")
}

/// A chunk's accumulator events, merged per accumulator object.
pub struct SettlementBuilder<'b> {
    /// Iterated to build the transactions, so ordered by object id.
    updates: BTreeMap<'b, [u8; 32], Update>,
    checkpoint_seq: u64,
    tx_index_offset: u64,
}

impl<'b> SettlementBuilder<'b> {
    /// A builder for a chunk of checkpoint `checkpoint_seq` whose first
    /// transaction is the checkpoint's `tx_index_offset`th, its table in
    /// `bump`.
    pub fn new(checkpoint_seq: u64, tx_index_offset: u64, bump: &'b Bump) -> SettlementBuilder<'b> {
        SettlementBuilder {
            updates: BTreeMap::new_in(bump),
            checkpoint_seq,
            tx_index_offset,
        }
    }

    /// Adds the accumulator events of the chunk's `tx_index`th transaction in
    /// checkpoint order.
    pub fn add(&mut self, tx_index: u64, effects: &TransactionEffects<'_>) {
        let VersionedEffects::V2(v2) = &effects.version else {
            return;
        };
        for change in v2.changed_objects {
            let ObjectOut::AccumulatorWriteV1(write) = &change.output_state else {
                continue;
            };
            let sui = if is_sui_balance(&write.ty) {
                match write.value {
                    AccumulatorValue::Integer(v) => v,
                    _ => panic!("a SUI balance accumulates integers"),
                }
            } else {
                0
            };
            let update = self.updates.entry(change.id.0).or_insert_with(|| Update {
                address: write.address.0,
                ty: TypeTag::from(&write.ty),
                merge: Merged::zero(&write.value),
                split: Merged::zero(&write.value),
                input_sui: 0,
                output_sui: 0,
            });
            let index = tx_index + self.tx_index_offset;
            match write.operation {
                AccumulatorOperation::Merge => {
                    update.input_sui += sui;
                    update
                        .merge
                        .accumulate(&write.value, self.checkpoint_seq, index);
                }
                AccumulatorOperation::Split => {
                    update.output_sui += sui;
                    update
                        .split
                        .accumulate(&write.value, self.checkpoint_seq, index);
                }
            }
        }
    }

    /// `build_tx`: the settlement transactions, each settling up to
    /// `max_updates` accumulators (`max_updates_per_settlement_txn`), the
    /// root written non-exclusively. None if the chunk has no events.
    pub fn build(
        self,
        max_updates: Option<u32>,
        epoch: u64,
        root_initial_shared_version: u64,
        height: u64,
    ) -> Vec<TransactionKind> {
        let per_transaction = max_updates.unwrap_or(u32::MAX) as usize;
        let checkpoint_seq = self.checkpoint_seq;
        let mut updates = self.updates.into_values().peekable();
        let mut built = Vec::new();
        while updates.peek().is_some() {
            let chunk: Vec<Update> = updates.by_ref().take(per_transaction).collect();
            let (input_sui, output_sui) = chunk
                .iter()
                .fold((0, 0), |(i, o), u| (i + u.input_sui, o + u.output_sui));
            let mut ptb = Ptb::new(
                SharedObjectMutability::NonExclusiveWrite,
                root_initial_shared_version,
            );
            ptb.prologue(epoch, height, built.len() as u64, input_sui, output_sui);
            for update in chunk {
                settle(&mut ptb, update, checkpoint_seq);
            }
            built.push(ptb.finish());
        }
        built
    }
}

/// `MergedValue::add_move_call`.
fn settle(ptb: &mut Ptb, update: Update, checkpoint_seq: u64) {
    let is_balance = matches!(&update.ty, TypeTag::Struct(s)
        if s.address.0 == FRAMEWORK && s.module == "balance" && s.name == "Balance");
    let address = ptb.pure(SuiAddress(update.address));
    match (update.merge, update.split) {
        (Merged::Sum(merge), Merged::Sum(split)) if is_balance => {
            let (merge, split) = if merge >= split {
                (merge - split, 0)
            } else {
                (0, split - merge)
            };
            if merge != 0 || split != 0 {
                let arguments = vec![Ptb::ROOT, address, ptb.pure(merge), ptb.pure(split)];
                ptb.call(
                    SETTLEMENT_MODULE,
                    "settle_u128",
                    vec![TypeInput::from(update.ty)],
                    arguments,
                );
            }
        }
        // The reference has no settlement for these yet (`todo!()`).
        (Merged::SumPair(..), Merged::SumPair(..)) => {
            panic!("pair-valued accumulators have no settlement")
        }
        (Merged::Events(events), Merged::Events(_)) => {
            let count = events.len() as u64;
            let root = event_merkle_root(&events);
            // `U256::from_le_bytes` of the root, whose BCS is the same bytes.
            let arguments = vec![
                Ptb::ROOT,
                ptb.pure(SuiAddress(update.address)),
                ptb.pure(U256(root)),
                ptb.pure(count),
                ptb.pure(checkpoint_seq),
            ];
            ptb.call(SETTLEMENT_MODULE, "settle_events", vec![], arguments);
        }
        _ => panic!("an accumulator's merge and split disagree"),
    }
}

/// A Move `u256`, little-endian.
struct U256([u8; 32]);

impl Serialize for U256 {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTuple;
        let mut tuple = s.serialize_tuple(32)?;
        for b in &self.0 {
            tuple.serialize_element(b)?;
        }
        tuple.end()
    }
}

/// `build_event_merkle_root`.
fn event_merkle_root(events: &[EventCommitment]) -> [u8; 32] {
    use fastcrypto::hash::Blake2b256;
    use fastcrypto::merkle::MerkleTree;
    MerkleTree::<Blake2b256>::build_from_unserialized(events)
        .expect("event commitments serialize")
        .root()
        .bytes()
}

/// `count_accumulator_object_changes`: accumulator objects the settlements
/// created and deleted.
pub fn count_object_changes<'e>(
    settlement_effects: impl IntoIterator<Item = &'e TransactionEffects<'e>>,
) -> (u64, u64) {
    let mut counts = (0, 0);
    for effects in settlement_effects {
        let VersionedEffects::V2(v2) = &effects.version else {
            panic!("settlement effects are V2");
        };
        for change in v2.changed_objects {
            if matches!(change.output_state, ObjectOut::AccumulatorWriteV1(_)) {
                continue;
            }
            match change.id_operation {
                IdOperation::Created => counts.0 += 1,
                IdOperation::Deleted => counts.1 += 1,
                IdOperation::None => {}
            }
        }
    }
    counts
}

/// `build_accumulator_barrier_tx`: advances the root's version once the
/// chunk's `settlements` settlement transactions have executed, creating and
/// deleting `objects_changed` accumulator objects.
pub fn barrier(
    epoch: u64,
    root_initial_shared_version: u64,
    height: u64,
    settlements: u64,
    (created, deleted): (u64, u64),
) -> TransactionKind {
    let mut ptb = Ptb::new(SharedObjectMutability::Mutable, root_initial_shared_version);
    ptb.prologue(epoch, height, settlements, 0, 0);
    let arguments = vec![Ptb::ROOT, ptb.pure(created), ptb.pure(deleted)];
    ptb.call(
        METADATA_MODULE,
        "record_accumulator_object_changes",
        vec![],
        arguments,
    );
    ptb.finish()
}
