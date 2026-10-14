// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Execution with sui's executor, over the store. The one crate that links
//! `sui-execution` and `sui-types`: transactions come in, and outputs go
//! out, as the reference's BCS, which the rest of anchovy reads with its
//! own views.

pub mod genesis;
pub mod native;
pub mod reads;
mod store_view;

use std::sync::Arc;

use sui_execution::Executor;
use sui_protocol_config::{ProtocolConfig, ProtocolVersion};
use sui_types::accumulator_root::EmptyUnsettledObjectFunds;
use sui_types::base_types::{ConsensusObjectVersion, ObjectID, SystemObjectVersions};
use sui_types::digests::TransactionDigest;
use sui_types::effects::{TransactionEffects, TransactionEffectsAPI};
use sui_types::error::SuiError;
use sui_types::execution_params::ExecutionOrEarlyError;
use sui_types::gas::SuiGasStatus;
use sui_types::message_envelope::Message as _;
use sui_types::metrics::ExecutionMetrics;
use sui_types::object::Object;
use sui_types::transaction::{
    CheckedInputObjects, InputObjectKind, InputObjects, ObjectReadResult, ObjectReadResultKind,
    SenderSignedData, TransactionDataAPI,
};

pub use store_view::StoreView;
/// The chain, as sui's executor takes it.
pub use sui_protocol_config::Chain;

#[derive(Debug)]
pub enum Error {
    Store(store::Error),
    /// Bytes that should be BCS of a sui type are not.
    Decode(bcs::Error),
    Sui(SuiError),
    /// An input the input checks found is gone: they were not run against
    /// this store.
    Missing(ObjectID),
    /// An executed transaction's effects or events are not in the store.
    MissingEffects(messages::base::Digest),
    /// Anchovy's executor failed outside the transaction.
    Native(String),
    /// An input the input checks found is gone, read by anchovy's executor.
    MissingNative(messages::base::ObjectId),
}

impl From<store::Error> for Error {
    fn from(e: store::Error) -> Error {
        Error::Store(e)
    }
}

impl From<bcs::Error> for Error {
    fn from(e: bcs::Error) -> Error {
        Error::Decode(e)
    }
}

impl From<sui_types::error::UserInputError> for Error {
    fn from(e: sui_types::error::UserInputError) -> Error {
        Error::Sui(e.into())
    }
}

impl From<SuiError> for Error {
    fn from(e: SuiError) -> Error {
        Error::Sui(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Sui's executor for one epoch: its protocol config (sui's own crate, not
/// anchovy's copy, which validation uses) and what execution reads of the
/// epoch.
pub struct Execution {
    config: ProtocolConfig,
    executor: Arc<dyn Executor + Send + Sync>,
    metrics: Arc<ExecutionMetrics>,
    epoch: u64,
    epoch_start_timestamp_ms: u64,
    reference_gas_price: u64,
}

/// What executing a transaction produced: its effects, as BCS, and what
/// to commit.
pub struct Outcome {
    pub effects: Vec<u8>,
    /// Applied by the caller, before it executes the next transaction.
    pub commit: store::Commit,
}

/// Whether a transaction's funds reservations were all fulfilled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Funds {
    /// Fulfilled, or it reserves none.
    MaybeSufficient,
    /// Some reservation was cancelled: it fails without running, with
    /// `InsufficientFundsForWithdraw`.
    Insufficient,
}

/// An input as the caller provides it, instead of execution reading it
/// from the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input<'a> {
    /// The object itself, as BCS.
    Object(&'a [u8]),
    /// A consensus object at exactly this version, read from the store.
    At(u64),
    /// The object's stream ended at `version`, in `transaction`: the input
    /// reads as the reference's stream-ended marker.
    Ended { version: u64, transaction: [u8; 32] },
}

/// An executed transaction as the reference answers for it
/// (`ExecutedData`), each part BCS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Executed {
    pub effects_digest: [u8; 32],
    pub effects: Vec<u8>,
    pub events: Option<Vec<u8>>,
    /// The objects the transaction changed, at their versions before it.
    pub input_objects: Vec<Vec<u8>>,
    /// The objects it changed, at their versions after it.
    pub output_objects: Vec<Vec<u8>>,
}

impl Executed {
    /// The effects digest as the reference's `TransactionEffectsDigest` BCS.
    pub fn effects_digest_bcs(&self) -> Vec<u8> {
        bcs::to_bytes(&sui_types::digests::TransactionEffectsDigest::new(
            self.effects_digest,
        ))
        .expect("a digest serializes")
    }
}

impl Execution {
    pub fn new(
        protocol_version: u64,
        chain: Chain,
        epoch: u64,
        epoch_start_timestamp_ms: u64,
        reference_gas_price: u64,
    ) -> Result<Execution> {
        let config = ProtocolConfig::get_for_version(ProtocolVersion::new(protocol_version), chain);
        let executor = sui_execution::executor(&config, true)?;
        let metrics = Arc::new(ExecutionMetrics::new(&prometheus::Registry::new()));
        Ok(Execution {
            config,
            executor,
            metrics,
            epoch,
            epoch_start_timestamp_ms,
            reference_gas_price,
        })
    }

    pub fn protocol_config(&self) -> &ProtocolConfig {
        &self.config
    }

    /// Executes `transaction` (`SenderSignedData` BCS, whose inputs passed
    /// the input checks) against the store's live objects. Nothing is
    /// written: the outcome carries the commit.
    ///
    /// # Panics
    /// If an owned input is no longer live at the version and digest the
    /// transaction gives: equivocation, which consensus rules out.
    pub fn execute(&self, store: &store::Store, transaction: &[u8]) -> Result<Outcome> {
        self.execute_at(store, transaction, &[], Funds::MaybeSufficient, None)
    }

    /// `execute`, but each input named in `provided` is as given there
    /// rather than read from the store at its live version, `funds` says
    /// whether its withdrawals were reserved, and `assigned_accumulator` is
    /// the accumulator root's version the transaction is assigned (the
    /// reference's `SystemObjectVersions`), if not the live one. A system
    /// transaction runs unmetered, as the reference runs it.
    pub fn execute_at(
        &self,
        store: &store::Store,
        transaction: &[u8],
        provided: &[(messages::base::ObjectId, Input<'_>)],
        funds: Funds,
        assigned_accumulator: Option<u64>,
    ) -> Result<Outcome> {
        let signed: SenderSignedData = bcs::from_bytes(transaction)?;
        let data = signed.transaction_data();
        let digest = data.digest();
        let view = StoreView::new(store);

        let inputs = load_at(&view, data.input_objects()?, provided)?;
        let gas_status = if data.kind().is_system_tx() {
            SuiGasStatus::new_unmetered(&self.config)
        } else {
            SuiGasStatus::new(
                data.gas_budget(),
                data.gas_price(),
                self.reference_gas_price,
                &self.config,
            )?
        };
        let accumulator_version = if self.config.enable_accumulators() {
            accumulator_version(&view)?.map(|live| ConsensusObjectVersion {
                version: assigned_accumulator.map_or(
                    live.version,
                    sui_types::base_types::SequenceNumber::from_u64,
                ),
                ..live
            })
        } else {
            None
        };

        let inputs =
            CheckedInputObjects::new_with_checked_transaction_inputs(InputObjects::new(inputs));
        // An input whose stream ended, or a withdrawal not reserved, fails
        // the transaction before it runs.
        let funds = match funds {
            Funds::MaybeSufficient => {
                sui_types::execution_params::FundsWithdrawStatus::MaybeSufficient
            }
            Funds::Insufficient => sui_types::execution_params::FundsWithdrawStatus::Insufficient,
        };
        let early = match sui_types::execution_params::get_early_execution_error(
            &digest,
            &inputs,
            &std::collections::HashSet::new(),
            &funds,
        ) {
            None => ExecutionOrEarlyError::ok(None),
            Some(errors) => ExecutionOrEarlyError::failed(errors, None),
        };
        let (store_out, _, effects, _, _) = self.executor.execute_transaction_to_effects(
            &view,
            &self.config,
            self.metrics.clone(),
            false,
            early,
            &self.epoch,
            self.epoch_start_timestamp_ms,
            inputs,
            SystemObjectVersions::new(accumulator_version),
            &EmptyUnsettledObjectFunds,
            data.gas_data().clone(),
            gas_status,
            data.kind().clone(),
            None,
            data.sender(),
            digest,
            &mut None,
        );

        let written: Vec<store::Written> = store_out
            .written
            .values()
            .map(written)
            .collect::<Result<_>>()?;
        let removed = removed(&effects);
        let effects_digest = effects.digest();
        let effects_bytes = bcs::to_bytes(&effects)?;
        let events = if store_out.events.data.is_empty() {
            None
        } else {
            Some(bcs::to_bytes(&store_out.events)?)
        };
        Ok(Outcome {
            effects: effects_bytes.clone(),
            commit: store::Commit {
                written,
                removed,
                executed: Some(store::Executed {
                    digest: digest_of(digest),
                    transaction: transaction.to_vec(),
                    effects_digest: messages::base::Digest::new(effects_digest.into_inner()),
                    effects: effects_bytes,
                    events,
                }),
            },
        })
    }
}

/// The transaction with digest `transaction`, if it executed: read back
/// from the store as the reference's `complete_executed_data` reads it.
pub fn executed(store: &store::Store, transaction: &[u8; 32]) -> Result<Option<Executed>> {
    let Some(effects_digest) =
        store.executed_effects(&messages::base::Digest::new(*transaction))?
    else {
        return Ok(None);
    };
    let bytes = store
        .effects(&effects_digest)?
        .ok_or(Error::MissingEffects(effects_digest))?
        .get()
        .bytes
        .to_vec();
    let effects: TransactionEffects = bcs::from_bytes(&bytes)?;
    let events = match effects.events_digest() {
        Some(_) => Some(
            store
                .events(&messages::base::Digest::new(*transaction))?
                .ok_or(Error::MissingEffects(effects_digest))?
                .get()
                .bytes
                .to_vec(),
        ),
        None => None,
    };
    let view = StoreView::new(store);
    let objects = |objects: Vec<Object>| -> Result<Vec<Vec<u8>>> {
        Ok(objects
            .iter()
            .map(bcs::to_bytes)
            .collect::<std::result::Result<_, _>>()?)
    };
    let input_objects = sui_types::storage::get_transaction_input_objects(&view, &effects)
        .map_err(|e| Error::Sui(e.into()))?;
    let output_objects = sui_types::storage::get_transaction_output_objects(&view, &effects)
        .map_err(|e| Error::Sui(e.into()))?;
    Ok(Some(Executed {
        effects_digest: effects_digest.bytes,
        effects: bytes,
        events,
        input_objects: objects(input_objects)?,
        output_objects: objects(output_objects)?,
    }))
}

/// Each input as execution reads it (`load`), but one named in `provided`
/// as given there.
fn load_at(
    view: &StoreView<'_>,
    kinds: Vec<InputObjectKind>,
    provided: &[(messages::base::ObjectId, Input<'_>)],
) -> Result<Vec<ObjectReadResult>> {
    let mut inputs = Vec::new();
    for kind in kinds {
        let id = kind.object_id().into_bytes();
        let given = provided.iter().find(|(p, _)| p.0 == id).map(|(_, v)| *v);
        let result = match given {
            None => ObjectReadResultKind::Object(load(view, &kind)?),
            Some(Input::Object(bytes)) => ObjectReadResultKind::Object(bcs::from_bytes(bytes)?),
            Some(Input::At(version)) => ObjectReadResultKind::Object(
                view.object_at(&kind.object_id(), version)?
                    .ok_or(Error::Missing(kind.object_id()))?,
            ),
            Some(Input::Ended {
                version,
                transaction,
            }) => ObjectReadResultKind::ObjectConsensusStreamEnded(
                sui_types::base_types::SequenceNumber::from_u64(version),
                TransactionDigest::new(transaction),
            ),
        };
        inputs.push(ObjectReadResult::new(kind, result));
    }
    Ok(inputs)
}

/// An input as execution reads it: packages and shared objects at their
/// live version, owned and immutable objects at theirs, which must be
/// the live one.
fn load(view: &StoreView<'_>, kind: &InputObjectKind) -> Result<Object> {
    let id = kind.object_id();
    let object = match kind {
        InputObjectKind::ImmOrOwnedMoveObject((_, version, digest)) => {
            let live = view.live(&id)?;
            assert!(
                live.is_some_and(
                    |l| l.version == version.value() && l.digest.bytes == digest.into_inner()
                ),
                "equivocation: {id} at {version} {digest} is not live ({live:?})"
            );
            view.object_at(&id, version.value())?
        }
        InputObjectKind::MovePackage(_) | InputObjectKind::SharedMoveObject { .. } => {
            view.live_object(&id)?
        }
    };
    object.ok_or(Error::Missing(id))
}

/// The accumulator root's version, which execution reads implicitly: its
/// live version, as executions are serial.
fn accumulator_version(view: &StoreView<'_>) -> Result<Option<ConsensusObjectVersion>> {
    let Some(root) = view.live_object(&sui_types::SUI_ACCUMULATOR_ROOT_OBJECT_ID)? else {
        return Ok(None);
    };
    Ok(match root.owner {
        sui_types::object::Owner::Shared {
            initial_shared_version,
        } => Some(ConsensusObjectVersion {
            initial_shared_version,
            version: root.version(),
        }),
        _ => None,
    })
}

/// The store's form of a written object.
fn written(object: &Object) -> Result<store::Written> {
    Ok(store::Written {
        id: messages::base::ObjectId(object.id().into_bytes()),
        version: object.version().value(),
        digest: messages::base::Digest::new(object.digest().into_inner()),
        bytes: bcs::to_bytes(object)?,
    })
}

/// Objects the transaction leaves without a live version, at the versions the effects give
/// them, as sui's store records its tombstones.
fn removed(effects: &TransactionEffects) -> Vec<store::Removed> {
    let removed = |(id, version, _): sui_types::base_types::ObjectRef, removal| store::Removed {
        id: messages::base::ObjectId(id.into_bytes()),
        version: version.value(),
        removal,
    };
    effects
        .deleted()
        .into_iter()
        .chain(effects.unwrapped_then_deleted())
        .map(|r| removed(r, store::Removal::Deleted))
        .chain(
            effects
                .wrapped()
                .into_iter()
                .map(|r| removed(r, store::Removal::Wrapped)),
        )
        .collect()
}

fn digest_of(digest: TransactionDigest) -> messages::base::Digest {
    messages::base::Digest::new(digest.into_inner())
}
