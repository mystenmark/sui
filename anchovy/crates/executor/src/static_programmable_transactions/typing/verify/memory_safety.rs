// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{cell::OnceCell, fmt};

use crate::{
    execution_mode::ExecutionMode,
    sp,
    static_programmable_transactions::{
        env::Env,
        typing::ast::{self as T, Type},
    },
};
use containers::{Bump, Vec};
use exec_types::error::{ExecutionError, SafeIndex};
use exec_types::{assert_invariant, invariant_violation, make_invariant_violation};
use messages::execution_status::{CommandArgumentError, ExecutionErrorKind};
use move_regex_borrow_graph::{MeterError, meter::DummyMeter, references::Ref};
use sui_types::base_types::TxContextKind;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Location(T::Location);

type Graph = move_regex_borrow_graph::collections::Graph<(), Location>;
type Paths = move_regex_borrow_graph::collections::Paths<(), Location>;

// The borrow graph's API takes and returns std collections (`BTreeSet<Ref>` sources,
// `Vec<bool>` mutabilities, `BTreeMap<Ref, Paths>` borrowers); those stay on the heap.
type StdBTreeSet<T> = std::collections::BTreeSet<T>;
type StdBTreeMap<K, V> = std::collections::BTreeMap<K, V>;

#[must_use]
enum Value {
    Ref(Ref),
    NonRef,
}

struct Context<'a> {
    bump: &'a Bump,
    allow_references_in_ptbs: bool,
    graph: Graph,
    local_root: Ref,
    tx_context: Option<Value>,
    gas_coin: Option<Value>,
    objects: Vec<'a, Option<Value>>,
    withdrawals: Vec<'a, Option<Value>>,
    pure: Vec<'a, Option<Value>>,
    receiving: Vec<'a, Option<Value>>,
    results: Vec<'a, Vec<'a, Option<Value>>>,
}

impl Value {
    fn is_ref(&self) -> bool {
        match self {
            Value::Ref(_) => true,
            Value::NonRef => false,
        }
    }

    fn is_non_ref(&self) -> bool {
        match self {
            Value::Ref(_) => false,
            Value::NonRef => true,
        }
    }

    fn to_ref(&self) -> Option<Ref> {
        match self {
            Value::Ref(r) => Some(*r),
            Value::NonRef => None,
        }
    }
}

/// `n` values of `Some(Value::NonRef)`, in the arena.
fn non_refs(bump: &Bump, n: usize) -> Vec<'_, Option<Value>> {
    let mut values = Vec::with_capacity_in(n, bump);
    values.extend((0..n).map(|_| Some(Value::NonRef)));
    values
}

impl<'a> Context<'a> {
    fn new<Mode: ExecutionMode>(
        env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
        ast: &T::Transaction<'a>,
    ) -> Result<Self, ExecutionError<'a>> {
        let bump = env.bump;
        let gas_coin = if ast.gas_payment.is_none() {
            None
        } else {
            Some(Value::NonRef)
        };
        let objects = non_refs(bump, ast.objects.len());
        let withdrawals = non_refs(bump, ast.withdrawals.len());
        let pure = non_refs(bump, ast.pure.len());
        let receiving = non_refs(bump, ast.receiving.len());
        let canonical_reference_capacity = ast
            .commands
            .iter()
            .flat_map(|command| &command.value.result_type)
            .filter(|ty| matches!(&ty, Type::Reference(_, _)))
            .count();
        let (mut graph, _locals) =
            Graph::new::<()>(canonical_reference_capacity, []).map_err(graph_err)?;
        let local_root = graph
            .extend_by_epsilon(
                (),
                std::iter::empty(),
                /* is_mut */ true,
                &mut DummyMeter,
            )
            .map_err(graph_meter_err)?;
        Ok(Self {
            bump,
            allow_references_in_ptbs: env.protocol_config.allow_references_in_ptbs(),
            graph,
            local_root,
            tx_context: Some(Value::NonRef),
            gas_coin,
            objects,
            withdrawals,
            pure,
            receiving,
            results: Vec::with_capacity_in(ast.commands.len(), bump),
        })
    }

    fn location(&mut self, l: T::Location) -> Result<&mut Option<Value>, ExecutionError<'a>> {
        Ok(match l {
            T::Location::TxContext => &mut self.tx_context,
            T::Location::GasCoin => &mut self.gas_coin,
            T::Location::ObjectInput(i) => self.objects.safe_get_mut(i as usize)?,
            T::Location::WithdrawalInput(i) => self.withdrawals.safe_get_mut(i as usize)?,
            T::Location::PureInput(i) => self.pure.safe_get_mut(i as usize)?,
            T::Location::ReceivingInput(i) => self.receiving.safe_get_mut(i as usize)?,
            T::Location::Result(i, j) => self
                .results
                .safe_get_mut(i as usize)?
                .safe_get_mut(j as usize)?,
        })
    }

    fn is_mutable(&self, r: Ref) -> Result<bool, ExecutionError<'a>> {
        self.graph.is_mutable(r).map_err(graph_err)
    }

    fn borrowed_by(&self, r: Ref) -> Result<StdBTreeMap<Ref, Paths>, ExecutionError<'a>> {
        self.graph
            .borrowed_by(r, &mut DummyMeter)
            .map_err(graph_meter_err)
    }

    /// Used for checking if a location is borrowed
    /// Used for updating the borrowed marker in Copy, and for correctness of Move
    fn is_location_borrowed(&self, l: T::Location) -> Result<bool, ExecutionError<'a>> {
        let borrowed_by = self.borrowed_by(self.local_root)?;
        Ok(borrowed_by
            .iter()
            .any(|(_, paths)| paths.iter().any(|path| path.starts_with(&Location(l)))))
    }

    fn release(&mut self, r: Ref) -> Result<(), ExecutionError<'a>> {
        self.graph
            .release(r, &mut DummyMeter)
            .map_err(graph_meter_err)
    }

    fn extend_by_epsilon(&mut self, r: Ref, is_mut: bool) -> Result<Ref, ExecutionError<'a>> {
        let new_r = self
            .graph
            .extend_by_epsilon((), std::iter::once(r), is_mut, &mut DummyMeter)
            .map_err(graph_meter_err)?;
        Ok(new_r)
    }

    fn extend_by_label(
        &mut self,
        r: Ref,
        is_mut: bool,
        extension: T::Location,
    ) -> Result<Ref, ExecutionError<'a>> {
        let new_r = self
            .graph
            .extend_by_label(
                (),
                std::iter::once(r),
                is_mut,
                Location(extension),
                &mut DummyMeter,
            )
            .map_err(graph_meter_err)?;
        Ok(new_r)
    }

    fn extend_by_dot_star_for_call(
        &mut self,
        sources: &StdBTreeSet<Ref>,
        mutabilities: std::vec::Vec<bool>,
    ) -> Result<std::vec::Vec<Ref>, ExecutionError<'a>> {
        let new_refs = self
            .graph
            .extend_by_dot_star_for_call((), sources, mutabilities, &mut DummyMeter)
            .map_err(graph_meter_err)?;
        Ok(new_refs)
    }

    // Writable if
    // No imm equal
    // No extensions
    fn is_writable(&self, r: Ref) -> Result<bool, ExecutionError<'a>> {
        debug_assert!(self.is_mutable(r)?);
        Ok(self
            .borrowed_by(r)?
            .values()
            .all(|paths| paths.iter().all(|path| path.is_epsilon())))
    }

    // is in reference not able to be used in a call or return
    fn find_non_transferrable(
        &self,
        refs: &StdBTreeSet<Ref>,
    ) -> Result<Option<Ref>, ExecutionError<'a>> {
        // The reference's `borrows` is a `BTreeMap` and `mut_refs` a `BTreeSet`. `refs` iterates
        // in sorted order without duplicates, so both vectors are sorted by `Ref` and `borrows` is
        // visited in the map's order.
        let mut borrows = Vec::with_capacity_in(refs.len(), self.bump);
        for r in refs.iter().copied() {
            borrows.push((r, self.borrowed_by(r)?));
        }
        let mut mut_refs = Vec::with_capacity_in(refs.len(), self.bump);
        for r in refs.iter().copied() {
            if self.is_mutable(r)? {
                mut_refs.push(r);
            }
        }
        let is_mut_ref = |r: &Ref| mut_refs.binary_search(r).is_ok();
        for (r, borrowed_by) in borrows {
            let is_mut = is_mut_ref(&r);
            for (borrower, paths) in borrowed_by {
                if !is_mut {
                    if is_mut_ref(&borrower) {
                        // If the ref is imm, but is borrowed by a mut ref in the set
                        // the mut ref is not transferrable
                        // In other words, the mut ref is an extension of the imm ref
                        return Ok(Some(borrower));
                    }
                } else {
                    for path in paths {
                        if !path.is_epsilon() || refs.contains(&borrower) {
                            // If the ref is mut, it cannot have any non-epsilon extensions
                            // If extension is epsilon (an alias), it cannot be in the transfer set
                            return Ok(Some(r));
                        }
                    }
                }
            }
        }
        Ok(None)
    }
}

/// Checks the following
/// - Values are not used after being moved
/// - Reference safety is upheld (no dangling references)
// `zip` stands in for the reference's `zip_debug_eq`: the lengths are asserted first.
#[allow(clippy::disallowed_methods)]
pub fn verify<'a, Mode: ExecutionMode>(
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    ast: &T::Transaction<'a>,
) -> Result<(), ExecutionError<'a>> {
    let mut context = Context::new(env, ast)?;
    let commands = &ast.commands;
    for c in commands {
        let result = command(&mut context, c).map_err(|e| e.with_command_index(c.idx as usize))?;
        assert_invariant!(
            result.len() == c.value.result_type.len(),
            "result length mismatch for command. {c:?}"
        );
        // drop unused result values
        assert_invariant!(
            result.len() == c.value.drop_values.len(),
            "drop values length mismatch for command. {c:?}"
        );
        let mut result_values = Vec::with_capacity_in(result.len(), context.bump);
        for (v, drop) in result.into_iter().zip(c.value.drop_values.iter().copied()) {
            result_values.push(if !drop {
                Some(v)
            } else {
                consume_value(&mut context, v)?;
                None
            });
        }
        context.results.push(result_values);
    }

    let bump = context.bump;
    let Context {
        gas_coin,
        objects,
        pure,
        receiving,
        results,
        ..
    } = &mut context;
    let gas_coin = gas_coin.take();
    let objects = std::mem::replace(objects, Vec::new_in(bump));
    let pure = std::mem::replace(pure, Vec::new_in(bump));
    let receiving = std::mem::replace(receiving, Vec::new_in(bump));
    let results = std::mem::replace(results, Vec::new_in(bump));
    consume_value_opt(&mut context, gas_coin)?;
    for vopt in objects.into_iter().chain(pure).chain(receiving) {
        consume_value_opt(&mut context, vopt)?;
    }
    for result in results {
        for vopt in result {
            consume_value_opt(&mut context, vopt)?;
        }
    }

    assert_invariant!(
        context.borrowed_by(context.local_root)?.is_empty(),
        "reference to local root not released"
    );
    context.release(context.local_root)?;
    assert_invariant!(context.graph.is_empty(), "reference not released");
    assert_invariant!(
        context.tx_context.is_some(),
        "tx_context should never be moved"
    );

    Ok(())
}

fn command<'a>(
    context: &mut Context<'a>,
    sp!(_, c): &T::Command<'a>,
) -> Result<Vec<'a, Value>, ExecutionError<'a>> {
    let bump = context.bump;
    let result_tys = &c.result_type;
    Ok(match &c.command {
        T::Command__::MoveCall(mc) => {
            let T::MoveCall {
                function,
                arguments: args,
            } = &**mc;
            let arg_values = arguments(context, args)?;
            call(context, arg_values, &function.signature)?
        }
        T::Command__::TransferObjects(objects, recipient) => {
            let object_values = arguments(context, objects)?;
            let recipient_value = argument(context, recipient)?;
            consume_values(context, object_values)?;
            consume_value(context, recipient_value)?;
            Vec::new_in(bump)
        }
        T::Command__::SplitCoins(_, coin, amounts) => {
            let coin_value = argument(context, coin)?;
            let amount_values = arguments(context, amounts)?;
            consume_values(context, amount_values)?;
            write_ref(context, 0, coin_value)?;
            let mut results = Vec::with_capacity_in(amounts.len(), bump);
            results.extend((0..amounts.len()).map(|_| Value::NonRef));
            results
        }
        T::Command__::MergeCoins(_, target, coins) => {
            let target_value = argument(context, target)?;
            let coin_values = arguments(context, coins)?;
            consume_values(context, coin_values)?;
            write_ref(context, 0, target_value)?;
            Vec::new_in(bump)
        }
        T::Command__::MakeMoveVec(_, xs) => {
            let vs = arguments(context, xs)?;
            consume_values(context, vs)?;
            one_non_ref(bump)
        }
        T::Command__::Publish(_, _, _) => {
            let mut results = Vec::with_capacity_in(result_tys.len(), bump);
            results.extend(result_tys.iter().map(|_| Value::NonRef));
            results
        }
        T::Command__::Upgrade(_, _, _, x, _) => {
            let v = argument(context, x)?;
            consume_value(context, v)?;
            one_non_ref(bump)
        }
    })
}

/// `vec![Value::NonRef]`, in the arena.
fn one_non_ref(bump: &Bump) -> Vec<'_, Value> {
    let mut results = Vec::with_capacity_in(1, bump);
    results.push(Value::NonRef);
    results
}

//**************************************************************************************************
// Abstract State
//**************************************************************************************************

fn consume_values<'a>(
    context: &mut Context<'a>,
    values: Vec<'_, Value>,
) -> Result<(), ExecutionError<'a>> {
    for v in values {
        consume_value(context, v)?;
    }
    Ok(())
}

fn consume_value_opt<'a>(
    context: &mut Context<'a>,
    value: Option<Value>,
) -> Result<(), ExecutionError<'a>> {
    match value {
        Some(v) => consume_value(context, v),
        None => Ok(()),
    }
}

fn consume_value<'a>(context: &mut Context<'a>, value: Value) -> Result<(), ExecutionError<'a>> {
    match value {
        Value::NonRef => Ok(()),
        Value::Ref(r) => {
            context.release(r)?;
            Ok(())
        }
    }
}

fn arguments<'a>(
    context: &mut Context<'a>,
    xs: &[T::Argument<'a>],
) -> Result<Vec<'a, Value>, ExecutionError<'a>> {
    let mut values = Vec::with_capacity_in(xs.len(), context.bump);
    for x in xs {
        values.push(argument(context, x)?);
    }
    Ok(values)
}

fn argument<'a>(
    context: &mut Context<'a>,
    x: &T::Argument<'a>,
) -> Result<Value, ExecutionError<'a>> {
    match &x.value.0 {
        T::Argument__::Use(T::Usage::Move(location)) => move_value(context, x.idx, *location),
        T::Argument__::Use(T::Usage::Copy { location, borrowed }) => {
            copy_value(context, x.idx, *location, borrowed)
        }
        T::Argument__::Borrow(is_mut, location) => {
            borrow_location(context, x.idx, *is_mut, *location)
        }
        T::Argument__::Read(usage) => read_ref(context, x.idx, usage),
        T::Argument__::Freeze(usage) => freeze_ref(context, x.idx, usage),
    }
}

fn move_value<'a>(
    context: &mut Context<'a>,
    arg_idx: u16,
    l: T::Location,
) -> Result<Value, ExecutionError<'a>> {
    if context.is_location_borrowed(l)? {
        // TODO more specific error
        return Err(ExecutionError::from_kind(
            ExecutionErrorKind::command_argument_error(
                CommandArgumentError::CannotMoveBorrowedValue,
                arg_idx,
            ),
        ));
    }
    let Some(value) = context.location(l)?.take() else {
        return Err(ExecutionError::from_kind(
            ExecutionErrorKind::command_argument_error(
                CommandArgumentError::ArgumentWithoutValue,
                arg_idx,
            ),
        ));
    };
    Ok(value)
}

fn copy_value<'a>(
    context: &mut Context<'a>,
    arg_idx: u16,
    l: T::Location,
    borrowed: &OnceCell<bool>,
) -> Result<Value, ExecutionError<'a>> {
    let is_borrowed = context.is_location_borrowed(l)?;
    borrowed
        .set(is_borrowed)
        .map_err(|_| make_invariant_violation!("Copy's borrowed marker should not yet be set"))?;

    let Some(value) = context.location(l)? else {
        // TODO more specific error
        return Err(ExecutionError::from_kind(
            ExecutionErrorKind::command_argument_error(
                CommandArgumentError::ArgumentWithoutValue,
                arg_idx,
            ),
        ));
    };
    Ok(match value {
        Value::Ref(r) => {
            let r = *r;
            let is_mut = context.is_mutable(r)?;
            let new_r = context.extend_by_epsilon(r, is_mut)?;
            Value::Ref(new_r)
        }
        Value::NonRef => Value::NonRef,
    })
}

fn borrow_location<'a>(
    context: &mut Context<'a>,
    arg_idx: u16,
    is_mut: bool,
    l: T::Location,
) -> Result<Value, ExecutionError<'a>> {
    // check that the location has a value
    let Some(value) = context.location(l)? else {
        // TODO more specific error
        return Err(ExecutionError::from_kind(
            ExecutionErrorKind::command_argument_error(
                CommandArgumentError::ArgumentWithoutValue,
                arg_idx,
            ),
        ));
    };
    assert_invariant!(
        value.is_non_ref(),
        "type checking should guarantee no borrowing of references"
    );
    let new_r = context.extend_by_label(context.local_root, is_mut, l)?;
    Ok(Value::Ref(new_r))
}

/// Creates an alias to the reference, but one that is immutable
fn freeze_ref<'a>(
    context: &mut Context<'a>,
    arg_idx: u16,
    u: &T::Usage,
) -> Result<Value, ExecutionError<'a>> {
    let value = match u {
        T::Usage::Move(l) => move_value(context, arg_idx, *l)?,
        T::Usage::Copy { location, borrowed } => copy_value(context, arg_idx, *location, borrowed)?,
    };
    let Some(r) = value.to_ref() else {
        invariant_violation!("type checking should guarantee FreezeRef is used on only references")
    };
    let new_r = context.extend_by_epsilon(r, /* is_mut */ false)?;
    consume_value(context, value)?;
    Ok(Value::Ref(new_r))
}

fn read_ref<'a>(
    context: &mut Context<'a>,
    arg_idx: u16,
    u: &T::Usage,
) -> Result<Value, ExecutionError<'a>> {
    let value = match u {
        T::Usage::Move(l) => move_value(context, arg_idx, *l)?,
        T::Usage::Copy { location, borrowed } => copy_value(context, arg_idx, *location, borrowed)?,
    };
    assert_invariant!(
        value.is_ref(),
        "type checking should guarantee ReadRef is used on only references"
    );
    consume_value(context, value)?;
    Ok(Value::NonRef)
}

fn write_ref<'a>(
    context: &mut Context<'a>,
    arg_idx: usize,
    value: Value,
) -> Result<(), ExecutionError<'a>> {
    let Value::Ref(r) = value else {
        invariant_violation!("type checking should guarantee WriteRef is used on only references");
    };

    if !context.is_writable(r)? {
        // TODO more specific error
        // TODO checked_as!
        #[allow(clippy::cast_possible_truncation)]
        return Err(ExecutionError::from_kind(
            ExecutionErrorKind::command_argument_error(
                CommandArgumentError::CannotWriteToExtendedReference,
                arg_idx as u16,
            ),
        ));
    }
    consume_value(context, value)?;
    Ok(())
}

// `zip` stands in for the reference's `zip_debug_eq`: the lengths are debug-asserted first.
#[allow(clippy::disallowed_methods)]
fn call<'a>(
    context: &mut Context<'a>,
    arg_values: Vec<'a, Value>,
    signature: &T::LoadedFunctionInstantiation<'a>,
) -> Result<Vec<'a, Value>, ExecutionError<'a>> {
    debug_assert_eq!(arg_values.len(), signature.parameters.len());
    let sources = arg_values
        .iter()
        .filter_map(|v| v.to_ref())
        .collect::<StdBTreeSet<_>>();
    if let Some(v) = context.find_non_transferrable(&sources)? {
        let mut_idx = arg_values
            .iter()
            .zip(&signature.parameters)
            .enumerate()
            .find(|(_, (x, ty))| x.to_ref() == Some(v) && matches!(ty, Type::Reference(true, _)));

        let Some((idx, _)) = mut_idx else {
            invariant_violation!("non transferrable value was not found in arguments");
        };
        // TODO checked_as!
        #[allow(clippy::cast_possible_truncation)]
        return Err(ExecutionError::from_kind(
            ExecutionErrorKind::command_argument_error(
                CommandArgumentError::InvalidReferenceArgument,
                idx as u16,
            ),
        ));
    }
    // `TxContext` is borrowed by the call but is never a source of its outputs: no function can
    // return a reference into it.
    let sources = if context.allow_references_in_ptbs {
        arg_values
            .iter()
            .zip(&signature.parameters)
            .filter(|(_, ty)| matches!(ty.is_tx_context(), TxContextKind::None))
            .filter_map(|(v, _)| v.to_ref())
            .collect::<StdBTreeSet<_>>()
    } else {
        sources
    };
    let mutabilities = signature
        .return_
        .iter()
        .filter_map(|ty| match ty {
            Type::Reference(is_mut, _) => Some(*is_mut),
            _ => None,
        })
        .collect::<std::vec::Vec<_>>();
    let mutabilities_len = mutabilities.len();
    let mut return_references = context.extend_by_dot_star_for_call(&sources, mutabilities)?;
    assert_invariant!(
        return_references.len() == mutabilities_len,
        "return_references should have the same length as mutabilities"
    );

    let mut return_values = Vec::with_capacity_in(signature.return_.len(), context.bump);
    for ty in signature.return_.iter().rev() {
        return_values.push(match ty {
            Type::Reference(_is_mut, _) => {
                let Some(new_ref) = return_references.pop() else {
                    invariant_violation!("return_references has less references than return_");
                };
                debug_assert_eq!(context.is_mutable(new_ref)?, *_is_mut);
                Value::Ref(new_ref)
            }
            _ => Value::NonRef,
        });
    }
    return_values.reverse();
    assert_invariant!(
        return_references.is_empty(),
        "return_references has more references than return_"
    );
    consume_values(context, arg_values)?;
    Ok(return_values)
}

fn graph_meter_err<'a>(e: MeterError<()>) -> ExecutionError<'a> {
    match e {
        MeterError::Meter(()) => {
            make_invariant_violation!("DummyMeter should never produce a Meter error")
        }
        MeterError::InvariantViolation(iv) => graph_err(iv),
    }
}

fn graph_err<'a>(e: move_regex_borrow_graph::InvariantViolation) -> ExecutionError<'a> {
    make_invariant_violation!("Borrow graph invariant violation: {}", e.0)
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            T::Location::TxContext => write!(f, "TxContext"),
            T::Location::GasCoin => write!(f, "GasCoin"),
            T::Location::ObjectInput(idx) => write!(f, "ObjectInput({idx})"),
            T::Location::WithdrawalInput(idx) => write!(f, "WithdrawalInput({idx})"),
            T::Location::PureInput(idx) => write!(f, "PureInput({idx})"),
            T::Location::ReceivingInput(idx) => write!(f, "ReceivingInput({idx})"),
            T::Location::Result(i, j) => write!(f, "Result({i}, {j})"),
        }
    }
}
