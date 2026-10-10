// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `sui_types::layout_resolver`: the trait the expensive SUI conservation check resolves object
//! layouts through. The type tag and layout are the VM's, as in the reference.

use exec_types::storage::SuiError;
use move_core_types::{annotated_value as A, language_storage::StructTag};

pub trait LayoutResolver {
    fn get_annotated_layout(
        &mut self,
        struct_tag: &StructTag,
    ) -> Result<A::MoveDatatypeLayout, SuiError>;
}
