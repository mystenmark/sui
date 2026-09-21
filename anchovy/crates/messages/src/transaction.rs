// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::arena::{Alloc, Ref};
use crate::base::{
    ChainIdentifier, Digest, ObjectId, ObjectRef, SequenceNumber, SuiAddress, TransactionDigest,
    U32Le, U64Le,
};
use crate::error::{ParseError, Result};
use crate::object::GenesisObject;
use crate::reader::{Reader, WireRecord};
use crate::system_transaction::{
    AuthenticatorStateUpdate, ChangeEpoch, ConsensusCommitPrologue, ConsensusCommitPrologueV2,
    ConsensusCommitPrologueV3, ConsensusCommitPrologueV4, EndOfEpochTransactionKind,
    RandomnessStateUpdate,
};
use crate::tx_index::{IndexCounts, TransactionIndex};
use crate::type_tag::{TypeInput, TypeTag};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Argument {
    GasCoin,
    Input(u16),
    Result(u16),
    NestedResult(u16, u16),
}

impl Argument {
    pub const MIN_WIRE_SIZE: usize = 1;

    #[inline]
    pub fn parse(r: &mut Reader<'_>) -> Result<Argument> {
        match r.variant()? {
            0 => Ok(Argument::GasCoin),
            1 => Ok(Argument::Input(r.u16()?)),
            2 => Ok(Argument::Result(r.u16()?)),
            3 => Ok(Argument::NestedResult(r.u16()?, r.u16()?)),
            tag => Err(ParseError::UnknownVariant {
                ty: "Argument",
                tag,
            }),
        }
    }

    pub fn parse_vec<'a, A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<&'a [Argument]> {
        let n = r.seq_len(Argument::MIN_WIRE_SIZE)?;
        let mut out = a.slice(n)?;
        for _ in 0..n {
            out.push(Argument::parse(r)?);
        }
        Ok(out.finish())
    }
}

/// `ObjectArg::SharedObject` as it sits on the wire.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct SharedObjectArg {
    pub id: ObjectId,
    pub initial_shared_version: U64Le,
    // A `SharedObjectMutability` variant index, checked where a reference is made.
    mutability: u8,
}

// SAFETY: `repr(C)` over byte fields: alignment 1, no padding. An unchecked
// `mutability` is not an invalid value, and `mutability()` does not trust it.
unsafe impl WireRecord for SharedObjectArg {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SharedObjectMutability {
    Immutable,
    Mutable,
    NonExclusiveWrite,
}

impl SharedObjectArg {
    pub const fn new(
        id: ObjectId,
        initial_shared_version: SequenceNumber,
        mutability: SharedObjectMutability,
    ) -> SharedObjectArg {
        SharedObjectArg {
            id,
            initial_shared_version: U64Le::new(initial_shared_version),
            mutability: mutability as u8,
        }
    }

    pub fn parse<'a>(r: &mut Reader<'a>) -> Result<&'a SharedObjectArg> {
        let arg: &SharedObjectArg = r.record()?;
        if arg.mutability > 2 {
            // 0x80 and up would be a multi-byte index, which no variant has.
            return Err(ParseError::UnknownVariant {
                ty: "SharedObjectMutability",
                tag: u32::from(arg.mutability),
            });
        }
        Ok(arg)
    }

    pub fn mutability(&self) -> SharedObjectMutability {
        match self.mutability {
            0 => SharedObjectMutability::Immutable,
            1 => SharedObjectMutability::Mutable,
            _ => SharedObjectMutability::NonExclusiveWrite,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjectArg<'a> {
    ImmOrOwnedObject(&'a ObjectRef),
    SharedObject(&'a SharedObjectArg),
    Receiving(&'a ObjectRef),
}

impl<'a> ObjectArg<'a> {
    #[inline]
    pub fn parse(r: &mut Reader<'a>) -> Result<ObjectArg<'a>> {
        match r.variant()? {
            0 => Ok(ObjectArg::ImmOrOwnedObject(ObjectRef::parse(r)?)),
            1 => Ok(ObjectArg::SharedObject(SharedObjectArg::parse(r)?)),
            2 => Ok(ObjectArg::Receiving(ObjectRef::parse(r)?)),
            tag => Err(ParseError::UnknownVariant {
                ty: "ObjectArg",
                tag,
            }),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reservation {
    MaxAmountU64(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WithdrawalTypeArg<'a> {
    Balance(TypeTag<'a>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WithdrawFrom<'a> {
    Sender,
    Sponsor,
    SenderAllowance {
        funder: &'a SuiAddress,
        allowance: &'a ObjectId,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FundsWithdrawalArg<'a> {
    pub reservation: Reservation,
    pub type_arg: WithdrawalTypeArg<'a>,
    pub withdraw_from: WithdrawFrom<'a>,
}

impl<'a> FundsWithdrawalArg<'a> {
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<FundsWithdrawalArg<'a>> {
        r.enter()?;

        r.enter()?;
        let reservation = match r.variant()? {
            0 => Reservation::MaxAmountU64(r.u64()?),
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "Reservation",
                    tag,
                });
            }
        };
        r.leave();

        r.enter()?;
        let type_arg = match r.variant()? {
            0 => WithdrawalTypeArg::Balance(TypeTag::parse(r, a)?),
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "WithdrawalTypeArg",
                    tag,
                });
            }
        };
        r.leave();

        r.enter()?;
        let withdraw_from = match r.variant()? {
            0 => WithdrawFrom::Sender,
            1 => WithdrawFrom::Sponsor,
            2 => WithdrawFrom::SenderAllowance {
                funder: SuiAddress::parse(r)?,
                allowance: ObjectId::parse(r)?,
            },
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "WithdrawFrom",
                    tag,
                });
            }
        };
        r.leave();

        r.leave();
        Ok(FundsWithdrawalArg {
            reservation,
            type_arg,
            withdraw_from,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallArg<'a> {
    Pure(&'a [u8]),
    Object(ObjectArg<'a>),
    FundsWithdrawal(Ref<'a, FundsWithdrawalArg<'a>>),
}

impl<'a> CallArg<'a> {
    /// `Pure` of no bytes.
    pub const MIN_WIRE_SIZE: usize = 2;

    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<CallArg<'a>> {
        r.enter()?;
        let arg = match r.variant()? {
            0 => CallArg::Pure(r.byte_vec()?),
            1 => CallArg::Object(ObjectArg::parse(r)?),
            2 => {
                let arg = FundsWithdrawalArg::parse(r, a)?;
                CallArg::FundsWithdrawal(a.value(arg)?)
            }
            tag => return Err(ParseError::UnknownVariant { ty: "CallArg", tag }),
        };
        r.leave();
        Ok(arg)
    }
}

/// Module and function are not checked against the Move identifier grammar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProgrammableMoveCall<'a> {
    pub package: &'a ObjectId,
    pub module: &'a str,
    pub function: &'a str,
    pub type_arguments: &'a [TypeInput<'a>],
    pub arguments: &'a [Argument],
}

impl<'a> ProgrammableMoveCall<'a> {
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<ProgrammableMoveCall<'a>> {
        r.enter()?;
        let package = ObjectId::parse(r)?;
        let module = r.str()?;
        let function = r.str()?;
        let type_arguments = TypeInput::parse_vec(r, a)?;
        let arguments = Argument::parse_vec(r, a)?;
        r.leave();
        Ok(ProgrammableMoveCall {
            package,
            module,
            function,
            type_arguments,
            arguments,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command<'a> {
    /// Inline, though it makes every `Command` 80 bytes: nearly all commands
    /// are calls, so boxing would add a pointer and padding to each.
    MoveCall(ProgrammableMoveCall<'a>),
    TransferObjects(&'a [Argument], Argument),
    SplitCoins(Argument, &'a [Argument]),
    MergeCoins(Argument, &'a [Argument]),
    /// Modules, then dependencies.
    Publish(&'a [&'a [u8]], &'a [ObjectId]),
    MakeMoveVec(Option<TypeInput<'a>>, &'a [Argument]),
    /// Modules, dependencies, the package being upgraded, the upgrade ticket.
    Upgrade(&'a [&'a [u8]], &'a [ObjectId], &'a ObjectId, Argument),
}

/// `Vec<Vec<u8>>`.
pub(crate) fn parse_byte_vecs<'a, A: Alloc<'a>>(
    r: &mut Reader<'a>,
    a: &mut A,
) -> Result<&'a [&'a [u8]]> {
    let n = r.seq_len(1)?;
    let mut out = a.slice(n)?;
    for _ in 0..n {
        out.push(r.byte_vec()?);
    }
    Ok(out.finish())
}

impl<'a> Command<'a> {
    /// `MakeMoveVec(None, [])`.
    pub const MIN_WIRE_SIZE: usize = 3;

    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<Command<'a>> {
        r.enter()?;
        let command = match r.variant()? {
            0 => Command::MoveCall(ProgrammableMoveCall::parse(r, a)?),
            1 => Command::TransferObjects(Argument::parse_vec(r, a)?, Argument::parse(r)?),
            2 => Command::SplitCoins(Argument::parse(r)?, Argument::parse_vec(r, a)?),
            3 => Command::MergeCoins(Argument::parse(r)?, Argument::parse_vec(r, a)?),
            4 => Command::Publish(parse_byte_vecs(r, a)?, r.record_vec()?),
            5 => {
                let ty = if r.option()? {
                    Some(TypeInput::parse(r, a)?)
                } else {
                    None
                };
                Command::MakeMoveVec(ty, Argument::parse_vec(r, a)?)
            }
            6 => Command::Upgrade(
                parse_byte_vecs(r, a)?,
                r.record_vec()?,
                ObjectId::parse(r)?,
                Argument::parse(r)?,
            ),
            tag => return Err(ParseError::UnknownVariant { ty: "Command", tag }),
        };
        r.leave();
        Ok(command)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProgrammableTransaction<'a> {
    pub inputs: &'a [CallArg<'a>],
    pub commands: &'a [Command<'a>],
}

impl<'a> ProgrammableTransaction<'a> {
    pub(crate) fn parse<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
        counts: &mut IndexCounts,
    ) -> Result<ProgrammableTransaction<'a>> {
        r.enter()?;

        let n = r.seq_len(CallArg::MIN_WIRE_SIZE)?;
        let mut inputs = a.slice(n)?;
        for _ in 0..n {
            let input = CallArg::parse(r, a)?;
            counts.count_input(&input);
            inputs.push(input);
        }
        let inputs = inputs.finish();

        let n = r.seq_len(Command::MIN_WIRE_SIZE)?;
        let mut commands = a.slice(n)?;
        for _ in 0..n {
            let struct_tags = r.struct_tags();
            let command = Command::parse(r, a)?;
            counts.count_command(&command, r.struct_tags() - struct_tags);
            commands.push(command);
        }
        let commands = commands.finish();

        r.leave();
        Ok(ProgrammableTransaction { inputs, commands })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransactionKind<'a> {
    ProgrammableTransaction(ProgrammableTransaction<'a>),
    ChangeEpoch(Ref<'a, ChangeEpoch<'a>>),
    Genesis(&'a [GenesisObject<'a>]),
    ConsensusCommitPrologue(ConsensusCommitPrologue),
    AuthenticatorStateUpdate(Ref<'a, AuthenticatorStateUpdate<'a>>),
    EndOfEpochTransaction(&'a [EndOfEpochTransactionKind<'a>]),
    RandomnessStateUpdate(Ref<'a, RandomnessStateUpdate<'a>>),
    ConsensusCommitPrologueV2(ConsensusCommitPrologueV2<'a>),
    ConsensusCommitPrologueV3(Ref<'a, ConsensusCommitPrologueV3<'a>>),
    ConsensusCommitPrologueV4(Ref<'a, ConsensusCommitPrologueV4<'a>>),
    ProgrammableSystemTransaction(ProgrammableTransaction<'a>),
}

impl<'a> TransactionKind<'a> {
    pub(crate) fn parse<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
        counts: &mut IndexCounts,
    ) -> Result<TransactionKind<'a>> {
        r.enter()?;
        let kind = match r.variant()? {
            0 => TransactionKind::ProgrammableTransaction(ProgrammableTransaction::parse(
                r, a, counts,
            )?),
            1 => {
                let v = ChangeEpoch::parse(r, a)?;
                TransactionKind::ChangeEpoch(a.value(v)?)
            }
            2 => {
                r.enter()?;
                let n = r.seq_len(GenesisObject::MIN_WIRE_SIZE)?;
                let mut objects = a.slice(n)?;
                for _ in 0..n {
                    objects.push(GenesisObject::parse(r, a)?);
                }
                r.leave();
                TransactionKind::Genesis(objects.finish())
            }
            3 => TransactionKind::ConsensusCommitPrologue(ConsensusCommitPrologue::parse(r)?),
            4 => {
                let v = AuthenticatorStateUpdate::parse(r, a)?;
                TransactionKind::AuthenticatorStateUpdate(a.value(v)?)
            }
            5 => {
                let n = r.seq_len(EndOfEpochTransactionKind::MIN_WIRE_SIZE)?;
                let mut kinds = a.slice(n)?;
                for _ in 0..n {
                    let kind = EndOfEpochTransactionKind::parse(r, a)?;
                    counts.count_end_of_epoch(&kind);
                    kinds.push(kind);
                }
                TransactionKind::EndOfEpochTransaction(kinds.finish())
            }
            6 => {
                let v = RandomnessStateUpdate::parse(r)?;
                TransactionKind::RandomnessStateUpdate(a.value(v)?)
            }
            7 => TransactionKind::ConsensusCommitPrologueV2(ConsensusCommitPrologueV2::parse(r)?),
            8 => {
                let v = ConsensusCommitPrologueV3::parse(r, a)?;
                TransactionKind::ConsensusCommitPrologueV3(a.value(v)?)
            }
            9 => {
                let v = ConsensusCommitPrologueV4::parse(r, a)?;
                TransactionKind::ConsensusCommitPrologueV4(a.value(v)?)
            }
            10 => TransactionKind::ProgrammableSystemTransaction(ProgrammableTransaction::parse(
                r, a, counts,
            )?),
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "TransactionKind",
                    tag,
                });
            }
        };
        r.leave();
        Ok(kind)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GasData<'a> {
    pub payment: &'a [ObjectRef],
    pub owner: &'a SuiAddress,
    pub price: u64,
    pub budget: u64,
}

impl<'a> GasData<'a> {
    pub fn parse(r: &mut Reader<'a>) -> Result<GasData<'a>> {
        r.enter()?;
        let payment = ObjectRef::parse_vec(r)?;
        let owner = SuiAddress::parse(r)?;
        let price = r.u64()?;
        let budget = r.u64()?;
        r.leave();
        Ok(GasData {
            payment,
            owner,
            price,
            budget,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ValidDuring<'a> {
    pub min_epoch: Option<u64>,
    pub max_epoch: Option<u64>,
    pub min_timestamp: Option<u64>,
    pub max_timestamp: Option<u64>,
    pub chain: &'a ChainIdentifier,
    pub nonce: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AllowedProposers<'a> {
    pub epoch: u64,
    pub proposers: &'a [U32Le],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransactionExpiration<'a> {
    None,
    Epoch(u64),
    ValidDuring(ValidDuring<'a>),
    /// `ValidDuring` plus who may propose the transaction.
    Validity(ValidDuring<'a>, Option<AllowedProposers<'a>>),
}

impl<'a> ValidDuring<'a> {
    // The fields only; the caller accounts for the enclosing variant.
    fn parse_fields(r: &mut Reader<'a>) -> Result<ValidDuring<'a>> {
        Ok(ValidDuring {
            min_epoch: r.option_u64()?,
            max_epoch: r.option_u64()?,
            min_timestamp: r.option_u64()?,
            max_timestamp: r.option_u64()?,
            chain: ChainIdentifier::parse(r)?,
            nonce: r.u32()?,
        })
    }
}

impl<'a> TransactionExpiration<'a> {
    pub fn parse(r: &mut Reader<'a>) -> Result<TransactionExpiration<'a>> {
        r.enter()?;
        let expiration = match r.variant()? {
            0 => TransactionExpiration::None,
            1 => TransactionExpiration::Epoch(r.u64()?),
            2 => TransactionExpiration::ValidDuring(ValidDuring::parse_fields(r)?),
            3 => {
                let window = ValidDuring::parse_fields(r)?;
                let allowed_proposers = if r.option()? {
                    r.enter()?;
                    let epoch = r.u64()?;
                    let proposers = r.record_vec()?;
                    r.leave();
                    Some(AllowedProposers { epoch, proposers })
                } else {
                    None
                };
                TransactionExpiration::Validity(window, allowed_proposers)
            }
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "TransactionExpiration",
                    tag,
                });
            }
        };
        r.leave();
        Ok(expiration)
    }
}

/// `TransactionData::V1`, the only version.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TransactionData<'a> {
    /// The exact encoding, which is what gets hashed and signed.
    pub bytes: &'a [u8],
    /// Computed once, from `bytes`, while parsing.
    pub digest: TransactionDigest,
    pub kind: TransactionKind<'a>,
    pub sender: &'a SuiAddress,
    pub gas_data: GasData<'a>,
    pub expiration: TransactionExpiration<'a>,
    /// Derived from the fields above while parsing.
    pub index: TransactionIndex<'a>,
}

impl<'a> TransactionData<'a> {
    /// The `MoveCall` commands of a user transaction, with their positions.
    pub fn move_calls(&self) -> impl Iterator<Item = (usize, &ProgrammableMoveCall<'a>)> {
        let commands = match &self.kind {
            TransactionKind::ProgrammableTransaction(pt) => pt.commands,
            _ => &[],
        };
        self.index.move_calls.iter().map(move |&i| {
            let Command::MoveCall(call) = &commands[i as usize] else {
                unreachable!("the index lists only MoveCall commands")
            };
            (i as usize, call)
        })
    }

    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<TransactionData<'a>> {
        let start = r.pos();
        r.enter()?;
        match r.variant()? {
            0 => {}
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "TransactionData",
                    tag,
                });
            }
        }
        r.enter()?;
        let mut counts = IndexCounts::default();
        let kind = TransactionKind::parse(r, a, &mut counts)?;
        let sender = SuiAddress::parse(r)?;
        let gas_data = GasData::parse(r)?;
        let expiration = TransactionExpiration::parse(r)?;
        r.leave();
        r.leave();
        let index = TransactionIndex::build(&kind, &gas_data, counts, a)?;
        let bytes = r.span(start);
        Ok(TransactionData {
            bytes,
            digest: if A::BUILD {
                Digest::of("TransactionData", bytes)
            } else {
                Digest::ZERO
            },
            kind,
            sender,
            gas_data,
            expiration,
            index,
        })
    }
}

/// Scope, version and app id. The values are not checked here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Intent {
    pub scope: u8,
    pub version: u8,
    pub app_id: u8,
}

// SAFETY: `repr(C)` over byte fields: alignment 1, no padding.
unsafe impl WireRecord for Intent {}

/// A signature of any scheme: a flag byte, then a scheme-specific encoding.
/// Nothing about the contents is checked here, not even that there is a flag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GenericSignature<'a>(pub &'a [u8]);

impl<'a> GenericSignature<'a> {
    pub const MIN_WIRE_SIZE: usize = 1;

    pub fn parse_vec<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
    ) -> Result<&'a [GenericSignature<'a>]> {
        let n = r.seq_len(GenericSignature::MIN_WIRE_SIZE)?;
        let mut out = a.slice(n)?;
        for _ in 0..n {
            out.push(GenericSignature(r.byte_vec()?));
        }
        Ok(out.finish())
    }
}

/// The one `SenderSignedTransaction` a `SenderSignedData` holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SenderSignedData<'a> {
    /// The exact encoding, as it is stored and sent.
    pub bytes: &'a [u8],
    pub intent: &'a Intent,
    pub data: TransactionData<'a>,
    pub tx_signatures: &'a [GenericSignature<'a>],
}

impl<'a> SenderSignedData<'a> {
    /// The transaction digest: that of the `TransactionData`.
    pub fn digest(&self) -> &TransactionDigest {
        &self.data.digest
    }

    /// A length, an intent, a `TransactionData` (a version, an empty
    /// `EndOfEpochTransaction`, a sender, gas data with no payment, no
    /// expiration) and a length.
    pub const MIN_WIRE_SIZE: usize = 1 + 3 + (1 + 2 + 32 + (1 + 32 + 8 + 8) + 1) + 1;

    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<SenderSignedData<'a>> {
        let start = r.pos();
        r.enter()?;
        if r.length()? != 1 {
            return Err(ParseError::NotOneTransaction);
        }
        r.enter()?;
        r.enter()?;
        let intent = r.record()?;
        let data = TransactionData::parse(r, a)?;
        r.leave();
        let tx_signatures = GenericSignature::parse_vec(r, a)?;
        r.leave();
        r.leave();
        Ok(SenderSignedData {
            bytes: r.span(start),
            intent,
            data,
            tx_signatures,
        })
    }

    /// `Envelope<SenderSignedData, EmptySignInfo>`, the reference's `Transaction`.
    pub fn parse_envelope<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
    ) -> Result<SenderSignedData<'a>> {
        r.enter()?;
        let data = SenderSignedData::parse(r, a)?;
        // `EmptySignInfo` is a struct of no bytes.
        r.leave();
        Ok(data)
    }
}

// Mainnet p99 of arena over wire size: 2.16 and 2.09.
crate::impl_wire!(TransactionData, guess = 35);
crate::impl_wire!(SenderSignedData, guess = 34);

crate::base::assert_wire_layout!(SharedObjectArg = 41, Intent = 3);

impl crate::message::Digested for TransactionData<'_> {
    fn digest(&self) -> &Digest {
        &self.digest
    }
}

impl crate::message::Digested for SenderSignedData<'_> {
    fn digest(&self) -> &Digest {
        &self.data.digest
    }
}
