// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use better_any::{Tid, TidAble};
use exec_types::base::EpochId;
use exec_types::tx_context::TxContext;
use messages::base::{ObjectId, SuiAddress, TransactionDigest};
use move_vm_runtime::natives::extensions::NativeExtensionMarker;
use std::{cell::RefCell, rc::Rc};

// TransactionContext is a wrapper around TxContext that is exposed to NativeContextExtensions
// in order to provide transaction context information to Move native functions.
// Holds a Rc<RefCell<TxContext>> to allow for mutation of the TxContext.
// Without the reference's test-only mode, whose `replace` serves the test
// scenario natives anchovy leaves out.
#[derive(Tid)]
pub struct TransactionContext {
    pub(crate) tx_context: Rc<RefCell<TxContext>>,
}

impl NativeExtensionMarker<'_> for TransactionContext {}

impl TransactionContext {
    pub fn new(tx_context: Rc<RefCell<TxContext>>) -> Self {
        Self { tx_context }
    }

    pub fn sender(&self) -> SuiAddress {
        self.tx_context.borrow().sender()
    }

    pub fn epoch(&self) -> EpochId {
        self.tx_context.borrow().epoch()
    }

    pub fn epoch_timestamp_ms(&self) -> u64 {
        self.tx_context.borrow().epoch_timestamp_ms()
    }

    pub fn digest(&self) -> TransactionDigest {
        self.tx_context.borrow().digest()
    }

    pub fn sponsor(&self) -> Option<SuiAddress> {
        self.tx_context.borrow().sponsor()
    }

    pub fn rgp(&self) -> u64 {
        self.tx_context.borrow().rgp()
    }

    pub fn gas_price(&self) -> u64 {
        self.tx_context.borrow().gas_price()
    }

    pub fn gas_budget(&self) -> u64 {
        self.tx_context.borrow().gas_budget()
    }

    pub fn ids_created(&self) -> u64 {
        self.tx_context.borrow().ids_created()
    }

    pub fn fresh_id(&self) -> ObjectId {
        self.tx_context.borrow_mut().fresh_id()
    }
}
