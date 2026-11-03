// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Execution over the store, with anchovy's executor (`native`), and with
//! sui's (`Reference`), the oracle it is tested against. The one crate that
//! links `sui-execution` and `sui-types`: transactions come in, and outputs
//! go out, as the reference's BCS, which the rest of anchovy reads with its
//! own views.

pub mod epoch_start;
pub mod genesis;
pub mod native;
pub mod reads;
mod store_view;
pub mod system_state;
pub mod upgrade;

use std::sync::Arc;

use messages::base::{Digest, Hashed};

use sui_execution::Executor;
use sui_protocol_config::{ProtocolConfig, ProtocolVersion};
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

/// Net withdrawals from accumulator accounts by transactions executed but
/// not yet settled, by account and the root version they read: what an
/// object balance withdrawal is checked against beside the settled balance
/// (the reference's `UnsettledObjectFundsRead`).
pub trait Unsettled: Sync {
    fn withdrawn(&self, account: &messages::base::ObjectId, version: u64) -> u128;
}

pub use executor::inputs::LazyVersion;

/// The versions consensus assigned a transaction of the system objects
/// execution reads without their being inputs (the reference's
/// `SystemObjectVersions`): the accumulator root's, the live one if none,
/// the Clock's, and the forwarding address registry's, found only if read.
#[derive(Clone, Copy, Default)]
pub struct Implicit<'r> {
    pub accumulator: Option<u64>,
    pub clock: Option<u64>,
    pub forwarding_registry: Option<&'r dyn LazyVersion>,
}

impl std::fmt::Debug for Implicit<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Implicit")
            .field("accumulator", &self.accumulator)
            .field("clock", &self.clock)
            .field("forwarding_registry", &self.forwarding_registry.is_some())
            .finish()
    }
}

/// Where execution waits for an implicitly read system object (the
/// accumulator root) to reach the version a transaction requires, which
/// another transaction's execution writes. Causal admission ensures the
/// writer runs.
pub trait Awaited: Sync {
    /// Returns once `id` at `version` is in the store.
    fn wait(&self, id: &messages::base::ObjectId, version: u64);
}

/// Nothing is written concurrently: what is missing stays missing.
pub struct NoneAwaited;

impl Awaited for NoneAwaited {
    fn wait(&self, _: &messages::base::ObjectId, _: u64) {}
}

/// No withdrawals unsettled.
pub struct NoneUnsettled;

impl Unsettled for NoneUnsettled {
    fn withdrawn(&self, _: &messages::base::ObjectId, _: u64) -> u128 {
        0
    }
}

/// `Unsettled`, as sui's executor reads it.
struct ReferenceUnsettled<'u>(&'u dyn Unsettled);

impl sui_types::accumulator_root::UnsettledObjectFundsRead for ReferenceUnsettled<'_> {
    fn get_unsettled_object_withdraw(
        &self,
        account: &sui_types::accumulator_root::AccumulatorObjId,
        accumulator_version: sui_types::base_types::SequenceNumber,
    ) -> u128 {
        let account = messages::base::ObjectId(account.inner().into_bytes());
        self.0.withdrawn(&account, accumulator_version.value())
    }
}

/// The pipeline's executor for one epoch: anchovy's.
///
/// With the `testing` feature, sui's executes every transaction too, as an
/// oracle: the outcomes must be identical.
pub struct Execution {
    native: native::NativeExecution,
    reference: Reference,
}

impl Execution {
    pub fn new(
        protocol_version: u64,
        chain: Chain,
        epoch: u64,
        epoch_start_timestamp_ms: u64,
        reference_gas_price: u64,
        chain_identifier: [u8; 32],
    ) -> Result<Execution> {
        let reference = Reference::new(
            protocol_version,
            chain,
            epoch,
            epoch_start_timestamp_ms,
            reference_gas_price,
        )?
        .with_chain_identifier(chain_identifier);
        let native = native::NativeExecution::new(
            reference.config.clone(),
            reference.metrics.clone(),
            epoch,
            epoch_start_timestamp_ms,
            reference_gas_price,
            chain_identifier,
        )?;
        Ok(Execution { native, reference })
    }

    /// Sui's executor for the epoch, which genesis runs on.
    pub fn reference(&self) -> &Reference {
        &self.reference
    }

    /// As `Reference::execute`.
    pub fn execute(&self, store: &store::Store, transaction: &[u8]) -> Result<Outcome> {
        self.execute_at(
            store,
            transaction,
            &[],
            Funds::MaybeSufficient,
            Implicit::default(),
            &NoneUnsettled,
            &NoneAwaited,
        )
    }

    /// As `Reference::execute_at`.
    ///
    /// # Panics
    /// With the `testing` feature, if sui's executor's outcome differs.
    pub fn execute_at(
        &self,
        store: &store::Store,
        transaction: &[u8],
        provided: &[(messages::base::ObjectId, Input<'_>)],
        funds: Funds,
        implicit: Implicit<'_>,
        unsettled: &dyn Unsettled,
        awaited: &dyn Awaited,
    ) -> Result<Outcome> {
        let parsed = native::parse(transaction)?;
        let data = parsed.get().0.data();
        let outcome = self.native.execute_at(
            store,
            data,
            transaction,
            provided,
            funds,
            implicit,
            unsettled,
            awaited,
        )?;
        #[cfg(feature = "testing")]
        assert_same(
            &self
                .reference
                .execute_at(store, transaction, provided, funds, implicit, unsettled)?,
            &outcome,
        );
        Ok(outcome)
    }
}

/// Sui's executor for one epoch: its protocol config (sui's own crate, not
/// anchovy's copy, which validation uses) and what execution reads of the
/// epoch.
pub struct Reference {
    config: ProtocolConfig,
    executor: Arc<dyn Executor + Send + Sync>,
    metrics: Arc<ExecutionMetrics>,
    epoch: u64,
    epoch_start_timestamp_ms: u64,
    rgp: u64,
    /// Coin reservations' ids are masked with it.
    chain_identifier: [u8; 32],
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
    /// A consensus object of a cancelled transaction, which reads none: the
    /// reference's marker version (`SequenceNumber::CONGESTED`, ...).
    Cancelled(u64),
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

impl Reference {
    pub fn new(
        protocol_version: u64,
        chain: Chain,
        epoch: u64,
        epoch_start_timestamp_ms: u64,
        reference_gas_price: u64,
    ) -> Result<Reference> {
        let config = ProtocolConfig::get_for_version(ProtocolVersion::new(protocol_version), chain);
        let executor = sui_execution::executor(&config, true)?;
        let metrics = Arc::new(ExecutionMetrics::new(&prometheus::Registry::new()));
        Ok(Reference {
            config,
            executor,
            metrics,
            epoch,
            epoch_start_timestamp_ms,
            rgp: reference_gas_price,
            chain_identifier: [0; 32],
        })
    }

    /// Whose coin reservations' ids are masked with `chain_identifier`.
    #[must_use]
    pub fn with_chain_identifier(mut self, chain_identifier: [u8; 32]) -> Reference {
        self.chain_identifier = chain_identifier;
        self
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
        self.execute_at(
            store,
            transaction,
            &[],
            Funds::MaybeSufficient,
            Implicit::default(),
            &NoneUnsettled,
        )
    }

    /// `execute`, but each input named in `provided` is as given there
    /// rather than read from the store at its live version, `funds` says
    /// whether its withdrawals were reserved, and `implicit` are the
    /// versions the transaction is assigned of the system objects it reads
    /// implicitly. A system
    /// transaction runs unmetered, as the reference runs it.
    pub fn execute_at(
        &self,
        store: &store::Store,
        transaction: &[u8],
        provided: &[(messages::base::ObjectId, Input<'_>)],
        funds: Funds,
        implicit: Implicit<'_>,
        unsettled: &dyn Unsettled,
    ) -> Result<Outcome> {
        let signed: SenderSignedData = bcs::from_bytes(transaction)?;
        let data = signed.transaction_data();
        let digest = data.digest();
        let view = StoreView::new(store);

        let inputs = load_at(&view, data.input_objects()?, provided)?;
        let gas_status = if data.kind().is_system_tx() {
            SuiGasStatus::new_unmetered(&self.config)
        } else {
            SuiGasStatus::new(data.gas_budget(), data.gas_price(), self.rgp, &self.config)?
        };
        let accumulator_version = if self.config.enable_accumulators() {
            accumulator_version(&view)?.map(|live| ConsensusObjectVersion {
                version: implicit.accumulator.map_or(
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
        // As the reference's `rewrite_transaction_for_coin_reservations`,
        // skipped on an early error.
        let mut kind = data.kind().clone();
        let rewritten = if early.is_ok() {
            rewrite_coin_reservations(
                &view,
                self.chain_identifier,
                data.sender(),
                &mut kind,
                accumulator_version.map(|v| v.version),
            )?
        } else {
            None
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
            // The forwarding address registry is left out: sui assigns its
            // version eagerly, and finding it here would wait on every
            // transaction. Nothing in sui reads it implicitly yet.
            SystemObjectVersions::from_map(
                accumulator_version
                    .map(|v| (sui_types::SUI_ACCUMULATOR_ROOT_OBJECT_ID, v))
                    .into_iter()
                    .chain(implicit.clock.map(|version| {
                        (
                            sui_types::SUI_CLOCK_OBJECT_ID,
                            ConsensusObjectVersion {
                                initial_shared_version: sui_types::SUI_CLOCK_OBJECT_SHARED_VERSION,
                                version: sui_types::base_types::SequenceNumber::from_u64(version),
                            },
                        )
                    }))
                    .collect(),
            ),
            &ReferenceUnsettled(unsettled),
            data.gas_data().clone(),
            gas_status,
            kind,
            rewritten,
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
                // Both computed by the reference's types, in this process.
                executed: Some(store::Executed {
                    digest: Hashed::computed(digest_of(digest)),
                    transaction: transaction.to_vec(),
                    effects_digest: Hashed::computed(Digest::new(effects_digest.into_inner())),
                    effects: effects_bytes,
                    events,
                }),
            },
        })
    }
}

/// Panics unless `native` is `reference`: the same effects, objects written
/// and removed, and events.
pub fn assert_same(reference: &Outcome, native: &Outcome) {
    if native.effects != reference.effects {
        let reference: std::result::Result<TransactionEffects, _> =
            bcs::from_bytes(&reference.effects);
        let native: std::result::Result<TransactionEffects, _> = bcs::from_bytes(&native.effects);
        panic!("effects differ:\nreference {reference:#?}\nnative {native:#?}");
    }
    let written = |o: &Outcome| {
        let mut w: Vec<_> = o
            .commit
            .written
            .iter()
            .map(|w| (w.id, w.version, w.digest, w.bytes.clone()))
            .collect();
        w.sort();
        w
    };
    assert_eq!(written(native), written(reference), "written objects");
    let removed = |o: &Outcome| {
        let mut r = o.commit.removed.clone();
        r.sort_by_key(|removed| removed.id.0);
        r
    };
    assert_eq!(removed(native), removed(reference), "removed objects");
    let executed = |o: &Outcome| {
        let e = o.commit.executed.as_ref().expect("an outcome executed");
        (e.digest, e.effects_digest, e.events.clone())
    };
    assert_eq!(executed(native), executed(reference), "digests and events");
}

/// The transaction with digest `transaction`, if it executed: read back
/// from the store as the reference's `complete_executed_data` reads it.
pub fn executed(store: &store::Store, transaction: &Hashed) -> Result<Option<Executed>> {
    let effects_digest = store.executed_effects(transaction)?;
    complete(store, transaction, effects_digest)
}

/// `executed` of a digest a request names, which a client chooses (see
/// `store::Store::executed_effects_named`).
pub fn executed_named(store: &store::Store, transaction: &Digest) -> Result<Option<Executed>> {
    let effects_digest = store.executed_effects_named(transaction)?;
    complete(store, transaction, effects_digest)
}

fn complete(
    store: &store::Store,
    transaction: &Digest,
    effects_digest: Option<Hashed>,
) -> Result<Option<Executed>> {
    let Some(effects_digest) = effects_digest else {
        return Ok(None);
    };
    let bytes = store
        .effects(&effects_digest)?
        .ok_or(Error::MissingEffects(effects_digest.into()))?
        .get()
        .bytes
        .to_vec();
    let effects: TransactionEffects = bcs::from_bytes(&bytes)?;
    let events = match effects.events_digest() {
        Some(_) => Some(
            store
                .events(transaction)?
                .ok_or(Error::MissingEffects(effects_digest.into()))?
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
            Some(Input::Cancelled(marker)) => {
                ObjectReadResultKind::CancelledTransactionSharedObject(
                    sui_types::base_types::SequenceNumber::from_u64(marker),
                )
            }
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

/// Each coin reservation input of `kind` rewritten into a withdrawal from
/// `sender`'s balance, as of the root at `accumulator_version`: sui-core's
/// `rewrite_transaction_for_coin_reservations`. Which inputs were
/// rewritten, if any reservation was.
fn rewrite_coin_reservations(
    view: &StoreView<'_>,
    chain_identifier: [u8; 32],
    sender: sui_types::base_types::SuiAddress,
    kind: &mut sui_types::transaction::TransactionKind,
    accumulator_version: Option<sui_types::base_types::SequenceNumber>,
) -> Result<Option<Vec<bool>>> {
    use sui_types::coin_reservation::{
        BorrowedCoinReservationResolver, CoinReservationResolverTrait as _,
        ParsedObjectRefWithdrawal,
    };
    use sui_types::transaction::{CallArg, ObjectArg, TransactionKind};
    let TransactionKind::ProgrammableTransaction(pt) = kind else {
        return Ok(None);
    };
    let chain = sui_types::digests::ChainIdentifier::from(
        sui_types::digests::CheckpointDigest::new(chain_identifier),
    );
    if pt.coin_reservation_obj_refs().count() == 0 {
        return Ok(None);
    }
    let resolver = BorrowedCoinReservationResolver::new(view);
    let mut rewritten = Vec::with_capacity(pt.inputs.len());
    for input in &mut pt.inputs {
        if let CallArg::Object(ObjectArg::ImmOrOwnedObject(object_ref)) = input
            && let Some(parsed) = ParsedObjectRefWithdrawal::parse(object_ref, chain)
        {
            *input = CallArg::FundsWithdrawal(resolver.resolve_funds_withdrawal(
                sender,
                parsed,
                accumulator_version,
            )?);
            rewritten.push(true);
        } else {
            rewritten.push(false);
        }
    }
    Ok(Some(rewritten))
}
