// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Signed transactions against a funded genesis, through every processor:
//! executed and committed one after another, resubmissions answered from
//! the store, inputs consumed since rejected.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use messages::Message;
use messages::base::Digest;
use protocol_config::{Chain, ProtocolVersion};
use sui_types::base_types::{ObjectID, ObjectRef, SuiAddress};
use sui_types::crypto::{AccountKeyPair, get_key_pair};
use sui_types::digests::TransactionDigest;
use sui_types::effects::{TransactionEffects, TransactionEffectsAPI};
use sui_types::execution_status::ExecutionStatus;
use sui_types::message_envelope::Message as _;
use sui_types::object::Object;
use sui_types::transaction::{Transaction, TransactionData};
use tokio::sync::oneshot;
use tonic::transport::Channel;
use tonic::transport::server::TcpIncoming;
use validation::ErrorKind;
use validator::Validator;
use validator::epoch::EpochState;
use validator::processors::{Outcome, Processors, Request};
use validator::proto::{RawSubmitTxRequest, RawValidatorSubmitStatus, SubmitTxType};
use validator::service::validator_client::ValidatorClient;

const RGP: u64 = 1000;
const BUDGET: u64 = 50_000_000;
const SUI: u64 = 1_000_000_000;

struct Node {
    _dir: tempfile::TempDir,
    epoch: Arc<EpochState>,
    store: Arc<store::Store>,
    processors: Processors,
    sender: SuiAddress,
    key: AccountKeyPair,
}

impl Node {
    fn new() -> Node {
        let (sender, key) = get_key_pair::<AccountKeyPair>();
        let epoch = Arc::new(EpochState::new(
            Chain::Unknown,
            ProtocolVersion::MAX.as_u64(),
            0,
            Digest::new([0; 32]),
            RGP,
            1,
            [],
        ));
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(store::Store::open(dir.path()).unwrap());
        let funded = [(sender.to_inner(), 10 * SUI)];
        assert!(execution::genesis::init(&epoch.execution, &store, &funded).unwrap());
        let processors = Processors::start(16, store.clone());
        Node {
            _dir: dir,
            epoch,
            store,
            processors,
            sender,
            key,
        }
    }

    /// The sender's gas coin, as of now.
    fn gas(&self) -> ObjectRef {
        let id = ObjectID::derive_id(TransactionDigest::genesis_marker(), 0);
        let object = self
            .store
            .live_object(&messages::base::ObjectId(id.into_bytes()))
            .unwrap()
            .unwrap();
        bcs::from_bytes::<Object>(object.wire_bytes())
            .unwrap()
            .compute_object_reference()
    }

    fn transfer(&self, gas: ObjectRef) -> Vec<u8> {
        let data = TransactionData::new_transfer_sui(
            SuiAddress::random_for_testing_only(),
            self.sender,
            Some(SUI),
            gas,
            BUDGET,
            RGP,
        );
        let transaction = Transaction::from_data_and_signer(data, vec![&self.key]);
        bcs::to_bytes(transaction.data()).unwrap()
    }

    async fn submit(&self, transaction: &[u8]) -> Outcome {
        let (reply, verdict) = oneshot::channel();
        let parsed = Message::parse(transaction.to_vec())
            .map_err(|(e, _)| e)
            .unwrap();
        self.processors
            .transactions
            .try_push(Request::new(self.epoch.clone(), vec![parsed], reply))
            .unwrap_or_else(|_| panic!("queue refused"));
        let mut outcomes = verdict.await.unwrap().unwrap();
        assert_eq!(outcomes.len(), 1);
        outcomes.pop().unwrap()
    }
}

fn effects(outcome: &Outcome) -> TransactionEffects {
    let Outcome::Executed(executed) = outcome else {
        panic!("not executed: {outcome:?}");
    };
    let effects: TransactionEffects = bcs::from_bytes(&executed.effects).unwrap();
    assert_eq!(executed.effects_digest, effects.digest().into_inner());
    effects
}

#[tokio::test]
async fn transfers_execute_in_turn_and_resubmissions_are_answered() {
    let node = Node::new();
    let first_gas = node.gas();
    let first = node.transfer(first_gas);
    let outcome = node.submit(&first).await;
    let effects = effects(&outcome);
    assert_eq!(effects.status(), &ExecutionStatus::Success, "{effects:?}");
    assert_eq!(effects.created().len(), 1);

    // The same transaction again: the same answer, from the store.
    let again = node.submit(&first).await;
    let (Outcome::Executed(a), Outcome::Executed(b)) = (&outcome, &again) else {
        panic!("{outcome:?} then {again:?}");
    };
    assert_eq!(a, b);

    // The next spends the gas coin's new version.
    let second_gas = node.gas();
    assert!(second_gas.1 > first_gas.1);
    let effects = self::effects(&node.submit(&node.transfer(second_gas)).await);
    assert_eq!(effects.status(), &ExecutionStatus::Success, "{effects:?}");

    // A transaction naming the consumed version fails the input checks.
    match node.submit(&node.transfer(first_gas)).await {
        Outcome::Rejected(e) => {
            assert_eq!(e.kind, ErrorKind::ObjectVersionUnavailableForConsumption);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_executed_transaction_comes_back_over_grpc() {
    let node = Node::new();
    let transaction = node.transfer(node.gas());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let service =
        Validator::new(node.epoch.clone(), node.processors.transactions.clone()).into_service();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpIncoming::from(listener)),
    );
    let channel = Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let results = ValidatorClient::new(channel)
        .submit_transaction(RawSubmitTxRequest {
            transactions: vec![Bytes::from(transaction)],
            submit_type: SubmitTxType::Default as i32,
        })
        .await
        .unwrap()
        .into_inner()
        .results;
    assert_eq!(results.len(), 1);
    let Some(RawValidatorSubmitStatus::Executed(executed)) = &results[0].inner else {
        panic!("{results:?}");
    };
    let details = executed.details.as_ref().unwrap();
    let effects: TransactionEffects = bcs::from_bytes(&details.effects).unwrap();
    assert_eq!(effects.status(), &ExecutionStatus::Success, "{effects:?}");
    let digest: sui_types::digests::TransactionEffectsDigest =
        bcs::from_bytes(&executed.effects_digest).unwrap();
    assert_eq!(digest, effects.digest());
    // The gas coin, before and after.
    assert_eq!(details.input_objects.len(), 1);
    assert_eq!(details.output_objects.len(), 2);
    assert!(details.events.is_none());
}
