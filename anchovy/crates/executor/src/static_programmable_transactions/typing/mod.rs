// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    error::ExecutionError,
    execution_mode::ExecutionMode,
    static_programmable_transactions::{
        env,
        loading::ast as L,
        metering::{self, translation_meter::TranslationMeter},
    },
};
pub mod ast;
pub mod invariant_checks;
pub mod translate;
pub mod verify;

pub fn translate_and_verify<'a, Mode: ExecutionMode>(
    meter: &mut TranslationMeter<'_, '_, 'a>,
    env: &env::Env<'a, '_, '_, '_, '_, '_, Mode>,
    lt: L::Transaction<'a>,
) -> Result<ast::Transaction<'a>, ExecutionError<'a>> {
    let mut ast = translate::transaction::<Mode>(env, lt)?;
    metering::typing::meter(meter, env.protocol_config, &ast)?;
    verify::transaction::<Mode>(env, &mut ast)?;
    invariant_checks::transaction::<Mode>(env, &ast, ast.unified_linkage.as_ref())?;
    Ok(ast)
}
