// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    execution_mode::ExecutionMode,
    sp,
    static_programmable_transactions::{env::Env, typing::ast as T},
};
use containers::{BTreeSet, Bump, HashMap, IndexSet, Vec};
use exec_types::{assert_invariant, checked_as, error::ExecutionError, make_invariant_violation};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum RootLocation {
    Unknown { command: u16 },
    Known(T::Location),
}

type NodeID = usize;

/// A packed set of small indices. Index `n` is stored in word `n / 64` at bit `n % 64`. Used both
/// for node-id ancestor sets and, in `Memory::parent_commands`, for command-id sets.
#[derive(Debug, Clone)]
struct BitSet<'a> {
    words: Vec<'a, u64>,
}

#[derive(Debug)]
enum NodeKind {
    Root,
    Delta { command: u16 },
}

#[derive(Debug)]
struct Node<'a> {
    kind: NodeKind,
    /// Contains the node itself and all of its transitive ancestors
    ancestors: BitSet<'a>,
    children: Vec<'a, NodeID>,
    /// Commands that created delta children of this node. Used to detect when a parent has
    /// children from more than one command. The reference keeps these in a map by node ID
    /// (`Memory::parent_commands`), which is only inserted into and queried.
    child_commands: BitSet<'a>,
}

#[derive(Debug)]
struct Memory<'a> {
    nodes: Vec<'a, Node<'a>>,
    /// The reference's is a `BTreeMap`; it is only inserted into and queried.
    roots: HashMap<'a, RootLocation, NodeID>,
    /// Parents with delta children from multiple commands. Only these parents can
    /// cause cross-command overlap.
    conflict_parents: IndexSet<'a, NodeID>,
}

#[derive(Debug)]
enum Value {
    NonRef,
    Ref { is_mut: bool, node: NodeID },
}

#[derive(Debug)]
struct Location {
    /// The location represented by this root. Its graph node is created on first borrow,
    /// so values that are never borrowed need no node.
    root: RootLocation,
    value: Option<Value>,
}

#[derive(Debug)]
struct Context<'a> {
    memory: Memory<'a>,
    allow_references_in_ptbs: bool,
    tx_context: Location,
    gas: Location,
    object_inputs: Vec<'a, Location>,
    withdrawal_inputs: Vec<'a, Location>,
    pure_inputs: Vec<'a, Location>,
    receiving_inputs: Vec<'a, Location>,
    results: Vec<'a, Vec<'a, Location>>,
    /// Indices into `results` of rows that held at least one reference when produced.
    /// This lets `all_references` skip result rows that never contained references.
    result_ref_rows: Vec<'a, usize>,
    // References passed to earlier arguments must keep their sources borrowed until the command
    // finishes, even after being moved out of their locations.
    arg_reference_nodes: Vec<'a, NodeID>,
}

impl<'a> BitSet<'a> {
    /// The reference's `BitSet::default()`: an empty set with no words.
    fn new_in(bump: &'a Bump) -> Self {
        Self {
            words: Vec::new_in(bump),
        }
    }

    /// Creates an empty set with capacity for `bits` indices.
    fn with_bits(bump: &'a Bump, bits: usize) -> Self {
        let len = bits.div_ceil(64).max(1);
        let mut words = Vec::with_capacity_in(len, bump);
        words.resize(len, 0);
        Self { words }
    }

    /// Sets the bit and returns whether it was already set. Errors if it is out of bounds.
    fn set(&mut self, bit: usize) -> anyhow::Result<bool> {
        let word = self
            .words
            .get_mut(bit / 64)
            .ok_or_else(|| anyhow::anyhow!("BitSet index {bit} is out of bounds"))?;
        let mask = 1 << (bit % 64);
        let was_set = *word & mask != 0;
        *word |= mask;
        Ok(was_set)
    }

    /// Sets the bit, growing the allocation to fit.
    fn set_grow(&mut self, bit: usize) {
        let word_index = bit / 64;
        if word_index >= self.words.len() {
            self.words.resize(word_index.saturating_add(1), 0);
        }
        if let Some(word) = self.words.get_mut(word_index) {
            *word |= 1 << (bit % 64);
        }
    }

    /// Out-of-range bits are treated as unset.
    fn contains(&self, bit: usize) -> bool {
        self.words
            .get(bit / 64)
            .is_some_and(|word| word & (1 << (bit % 64)) != 0)
    }

    fn count(&self) -> u32 {
        self.words.iter().map(|word| word.count_ones()).sum()
    }

    /// Adds all bits from `other`, growing the allocation if needed.
    fn union(&mut self, other: &Self) {
        if self.words.len() < other.words.len() {
            self.words.resize(other.words.len(), 0);
        }
        // Extra words in `self` are unchanged, so zip may stop at the end of `other`.
        #[allow(clippy::disallowed_methods)]
        let words = self.words.iter_mut().zip(&other.words);
        for (word, other_word) in words {
            *word |= other_word;
        }
    }

    /// Tests shared indices until `f` returns true or an error.
    fn try_any_common(
        &self,
        other: &Self,
        mut f: impl FnMut(usize) -> anyhow::Result<bool>,
    ) -> anyhow::Result<bool> {
        // Missing words are zero, so the intersection ends with the shorter set.
        #[allow(clippy::disallowed_methods)]
        let words = self.words.iter().zip(&other.words);
        for (word_index, (left, right)) in words.enumerate() {
            let mut common = left & right;
            while common != 0 {
                if f(Self::bit_index(word_index, common)?)? {
                    return Ok(true);
                }
                common = Self::clear_lowest_bit(common)?;
            }
        }
        Ok(false)
    }

    /// Returns the absolute index of the lowest set bit of `word`, which lives in `word_index`.
    fn bit_index(word_index: usize, word: u64) -> anyhow::Result<usize> {
        debug_assert!(word != 0);
        word_index
            .checked_mul(64)
            .and_then(|base| base.checked_add(word.trailing_zeros() as usize))
            .ok_or_else(|| anyhow::anyhow!("BitSet index overflow for word {word_index}"))
    }

    /// Removes the lowest set bit of `word`, which must be non-zero.
    fn clear_lowest_bit(word: u64) -> anyhow::Result<u64> {
        let sub = word
            .checked_sub(1)
            .ok_or_else(|| anyhow::anyhow!("clear_lowest_bit called on 0"))?;
        Ok(word & sub)
    }
}

impl<'a> Memory<'a> {
    fn new(bump: &'a Bump) -> Self {
        Self {
            nodes: Vec::new_in(bump),
            roots: containers::hash_map(bump, 0),
            conflict_parents: IndexSet::new_in(bump),
        }
    }

    /// The transaction's arena, which every container here allocates from.
    fn bump(&self) -> &'a Bump {
        self.nodes.allocator()
    }

    fn node(&self, id: NodeID) -> anyhow::Result<&Node<'a>> {
        self.nodes
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("Node index {id} is out of bounds"))
    }

    fn node_mut(&mut self, id: NodeID) -> anyhow::Result<&mut Node<'a>> {
        self.nodes
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Node index {id} is out of bounds"))
    }

    /// Creates a node whose ancestor set is itself plus every ancestor of its parents, and records
    /// reverse parent-to-child edges for identifying extensions from distinct commands.
    fn new_node(&mut self, kind: NodeKind, parents: &[NodeID]) -> anyhow::Result<NodeID> {
        let bump = self.bump();
        let id = self.nodes.len();
        let bits = id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Node ID overflow"))?;
        let mut ancestors = BitSet::with_bits(bump, bits);
        anyhow::ensure!(
            !ancestors.set(id)?,
            "Node {id} is already set in its fresh ancestor set"
        );
        for &parent in parents {
            ancestors.union(&self.node(parent)?.ancestors);
        }
        for &parent in parents {
            self.node_mut(parent)?.children.push(id);
        }
        if let NodeKind::Delta { command } = &kind {
            let command = *command as usize;
            for &parent in parents {
                let is_conflict = {
                    let commands = &mut self.node_mut(parent)?.child_commands;
                    commands.set_grow(command);
                    commands.count() >= 2
                };
                if is_conflict {
                    self.conflict_parents.insert(parent);
                }
            }
        }
        self.nodes.push(Node {
            kind,
            ancestors,
            children: Vec::new_in(bump),
            child_commands: BitSet::new_in(bump),
        });
        Ok(id)
    }

    /// Returns the unique node for a PTB location, creating it on first use.
    fn root(&mut self, root: RootLocation) -> anyhow::Result<NodeID> {
        if let Some(id) = self.roots.get(&root) {
            return Ok(*id);
        }
        let id = self.new_node(NodeKind::Root, &[])?;
        self.roots.insert(root, id);
        Ok(id)
    }

    /// Returns the node for a PTB location if one has been created
    fn get_root(&self, root: RootLocation) -> Option<NodeID> {
        self.roots.get(&root).copied()
    }

    /// Creates a call-return delta reference, whose ancestors are derived from parent reference
    /// arguments passed to the command.
    /// For immutable references, the parents are all reference arguments.
    /// For mutable references, the parents are the mutable reference arguments.
    fn call_return(&mut self, command: u16, parents: &[NodeID]) -> anyhow::Result<NodeID> {
        self.new_node(NodeKind::Delta { command }, parents)
    }

    /// Returns whether `node` can derive from `ancestor`.
    fn is_ancestor(&self, node: NodeID, ancestor: NodeID) -> anyhow::Result<bool> {
        Ok(self.node(node)?.ancestors.contains(ancestor))
    }

    /// Checks whether `common` has delta children from different commands in the two ancestor sets.
    fn common_has_cross_command(
        &self,
        common: NodeID,
        left_ancestors: &BitSet,
        right_ancestors: &BitSet,
    ) -> anyhow::Result<bool> {
        let mut left_cmd: Option<u16> = None;
        let mut left_multi = false;
        let mut right_cmd: Option<u16> = None;
        let mut right_multi = false;
        for &child in &self.node(common)?.children {
            let NodeKind::Delta { command } = &self.node(child)?.kind else {
                continue;
            };
            let command = *command;
            if left_ancestors.contains(child) {
                match left_cmd {
                    None => left_cmd = Some(command),
                    Some(c) if c != command => left_multi = true,
                    _ => {}
                }
            }
            if right_ancestors.contains(child) {
                match right_cmd {
                    None => right_cmd = Some(command),
                    Some(c) if c != command => right_multi = true,
                    _ => {}
                }
            }
        }
        // Both sides must contain a delta child. If either side contains children from multiple
        // commands, at least one pair must have different commands.
        Ok(match (left_cmd, right_cmd) {
            (Some(l), Some(r)) => left_multi || right_multi || l != r,
            _ => false,
        })
    }

    /// Checks for paths from a shared ancestor through delta children from different commands.
    /// Such paths may overlap; only ancestors in `conflict_parents` need checking.
    fn has_cross_command_extensions(&self, left: NodeID, right: NodeID) -> anyhow::Result<bool> {
        if self.conflict_parents.is_empty() {
            return Ok(false);
        }
        let left_ancestors = &self.node(left)?.ancestors;
        let right_ancestors = &self.node(right)?.ancestors;
        let min_width = left_ancestors.words.len().min(right_ancestors.words.len());
        // Heuristic: estimate scan cost by comparing the number of conflict parents with the number
        // of words in the shorter ancestor bitset.
        if self.conflict_parents.len() <= min_width {
            for &parent in &self.conflict_parents {
                if left_ancestors.contains(parent)
                    && right_ancestors.contains(parent)
                    && self.common_has_cross_command(parent, left_ancestors, right_ancestors)?
                {
                    return Ok(true);
                }
            }
            Ok(false)
        } else {
            left_ancestors.try_any_common(right_ancestors, |common| {
                let is_conflict_parent = self
                    .nodes
                    .get(common)
                    .is_some_and(|node| node.child_commands.count() >= 2);
                Ok(is_conflict_parent
                    && self.common_has_cross_command(common, left_ancestors, right_ancestors)?)
            })
        }
    }

    /// Returns whether `left` may extend `right` through ancestry or cross-command extensions.
    fn extends(&self, left: NodeID, right: NodeID) -> anyhow::Result<bool> {
        Ok((left != right && self.is_ancestor(left, right)?)
            || self.has_cross_command_extensions(left, right)?)
    }

    /// Returns whether neither node derives from the other and neither has cross-command extensions.
    fn is_disjoint(&self, left: NodeID, right: NodeID) -> anyhow::Result<bool> {
        Ok(left != right
            && !self.is_ancestor(left, right)?
            && !self.is_ancestor(right, left)?
            && !self.has_cross_command_extensions(left, right)?)
    }
}

impl Value {
    fn copy(&self) -> Value {
        match self {
            Value::NonRef => Value::NonRef,
            Value::Ref { is_mut, node } => Value::Ref {
                is_mut: *is_mut,
                node: *node,
            },
        }
    }

    fn freeze(&mut self) -> anyhow::Result<Value> {
        match self.copy() {
            Value::NonRef => anyhow::bail!("Cannot freeze a non-reference value"),
            Value::Ref { is_mut, node } => {
                anyhow::ensure!(is_mut, "Cannot freeze an immutable reference");
                Ok(Value::Ref {
                    is_mut: false,
                    node,
                })
            }
        }
    }
}

impl Location {
    fn non_ref(root: RootLocation) -> Self {
        Self {
            root,
            value: Some(Value::NonRef),
        }
    }

    fn copy_value(&self) -> anyhow::Result<Value> {
        self.value
            .as_ref()
            .map(Value::copy)
            .ok_or_else(|| anyhow::anyhow!("Use of invalid memory location"))
    }

    fn move_value(&mut self) -> anyhow::Result<Value> {
        self.value
            .take()
            .ok_or_else(|| anyhow::anyhow!("Use of invalid memory location"))
    }

    fn use_(&mut self, usage: &T::Usage) -> anyhow::Result<Value> {
        match usage {
            T::Usage::Move(_) => self.move_value(),
            T::Usage::Copy { .. } => self.copy_value(),
        }
    }

    fn assert_borrowable(&self) -> anyhow::Result<()> {
        match self.value.as_ref() {
            None => anyhow::bail!("Borrow of invalid memory location"),
            Some(Value::Ref { .. }) => anyhow::bail!("Cannot borrow a reference"),
            Some(Value::NonRef) => Ok(()),
        }
    }
}

/// The reference collects these with `Iterator::collect`; here each goes into an arena vector
/// sized up front.
fn input_locations<'a>(
    bump: &'a Bump,
    len: usize,
    location: impl Fn(u16) -> T::Location,
) -> anyhow::Result<Vec<'a, Location>> {
    let mut locations = Vec::with_capacity_in(len, bump);
    for i in 0..len {
        locations.push(Location::non_ref(RootLocation::Known(location(
            checked_as!(i, u16)?,
        ))));
    }
    Ok(locations)
}

impl<'a> Context<'a> {
    fn new<Mode: ExecutionMode>(
        _env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
        txn: &T::Transaction<'a>,
    ) -> anyhow::Result<Self> {
        let bump = _env.bump;
        let T::Transaction {
            gas_payment,
            bytes: _,
            objects,
            withdrawals,
            pure,
            receiving,
            withdrawal_compatibility_conversions: _,
            original_command_len: _,
            commands,
            unified_linkage: _,
        } = txn;
        let memory = Memory::new(bump);
        let tx_context = Location::non_ref(RootLocation::Known(T::Location::TxContext));
        let mut gas = Location::non_ref(RootLocation::Known(T::Location::GasCoin));
        if gas_payment.is_none() {
            gas.move_value()
                .map_err(|_| anyhow::anyhow!("gas coin should be initialized"))?;
        }
        let object_inputs = input_locations(bump, objects.len(), T::Location::ObjectInput)?;
        let withdrawal_inputs =
            input_locations(bump, withdrawals.len(), T::Location::WithdrawalInput)?;
        let pure_inputs = input_locations(bump, pure.len(), T::Location::PureInput)?;
        let receiving_inputs = input_locations(bump, receiving.len(), T::Location::ReceivingInput)?;
        Ok(Self {
            memory,
            allow_references_in_ptbs: _env.protocol_config.allow_references_in_ptbs(),
            tx_context,
            gas,
            object_inputs,
            withdrawal_inputs,
            pure_inputs,
            receiving_inputs,
            // The reference starts these empty; a row is added per command.
            results: Vec::with_capacity_in(commands.len(), bump),
            result_ref_rows: Vec::new_in(bump),
            arg_reference_nodes: Vec::new_in(bump),
        })
    }

    /// The transaction's arena, which every container here allocates from.
    fn bump(&self) -> &'a Bump {
        self.memory.bump()
    }

    fn current_command(&self) -> anyhow::Result<u16> {
        Ok(checked_as!(self.results.len(), u16)?)
    }

    fn add_result_values(
        &mut self,
        results: impl ExactSizeIterator<Item = Option<Value>>,
    ) -> anyhow::Result<()> {
        let command = self.current_command()?;
        let mut row = Vec::with_capacity_in(results.len(), self.bump());
        for (i, v) in results.enumerate() {
            row.push(Location {
                root: RootLocation::Known(T::Location::Result(command, checked_as!(i, u16)?)),
                value: v,
            });
        }
        if row
            .iter()
            .any(|loc| matches!(loc.value, Some(Value::Ref { .. })))
        {
            self.result_ref_rows.push(self.results.len());
        }
        self.results.push(row);
        Ok(())
    }

    fn location(&self, loc: T::Location) -> anyhow::Result<&Location> {
        Ok(match loc {
            T::Location::TxContext => &self.tx_context,
            T::Location::GasCoin => &self.gas,
            T::Location::ObjectInput(i) => self
                .object_inputs
                .get(i as usize)
                .ok_or_else(|| anyhow::anyhow!("Object input index out of bounds {i}"))?,
            T::Location::WithdrawalInput(i) => self
                .withdrawal_inputs
                .get(i as usize)
                .ok_or_else(|| anyhow::anyhow!("Withdrawal input index out of bounds {i}"))?,
            T::Location::PureInput(i) => self
                .pure_inputs
                .get(i as usize)
                .ok_or_else(|| anyhow::anyhow!("Pure input index out of bounds {i}"))?,
            T::Location::ReceivingInput(i) => self
                .receiving_inputs
                .get(i as usize)
                .ok_or_else(|| anyhow::anyhow!("Receiving input index out of bounds {i}"))?,
            T::Location::Result(i, j) => self
                .results
                .get(i as usize)
                .and_then(|r| r.get(j as usize))
                .ok_or_else(|| anyhow::anyhow!("Result index out of bounds ({i},{j})"))?,
        })
    }

    fn location_mut(&mut self, loc: T::Location) -> anyhow::Result<&mut Location> {
        Ok(match loc {
            T::Location::TxContext => &mut self.tx_context,
            T::Location::GasCoin => &mut self.gas,
            T::Location::ObjectInput(i) => self
                .object_inputs
                .get_mut(i as usize)
                .ok_or_else(|| anyhow::anyhow!("Object input index out of bounds {i}"))?,
            T::Location::WithdrawalInput(i) => self
                .withdrawal_inputs
                .get_mut(i as usize)
                .ok_or_else(|| anyhow::anyhow!("Withdrawal input index out of bounds {i}"))?,
            T::Location::PureInput(i) => self
                .pure_inputs
                .get_mut(i as usize)
                .ok_or_else(|| anyhow::anyhow!("Pure input index out of bounds {i}"))?,
            T::Location::ReceivingInput(i) => self
                .receiving_inputs
                .get_mut(i as usize)
                .ok_or_else(|| anyhow::anyhow!("Receiving input index out of bounds {i}"))?,
            T::Location::Result(i, j) => self
                .results
                .get_mut(i as usize)
                .and_then(|r| r.get_mut(j as usize))
                .ok_or_else(|| anyhow::anyhow!("Result index out of bounds ({i},{j})"))?,
        })
    }

    /// Returns true iff any live reference borrows `location`.
    fn location_is_borrowed(&self, location: &Location) -> anyhow::Result<bool> {
        match self.memory.get_root(location.root) {
            Some(node) => self.any_extends(node, /* ignore alias */ false),
            None => Ok(false),
        }
    }

    /// Whether the location is borrowed by a reference argument already seen this command, i.e. its
    /// root node is an ancestor of one of `arg_reference_nodes`. A location never borrowed has no
    /// node and cannot be such an ancestor.
    fn borrowed_by_arg_reference(&self, location: T::Location) -> anyhow::Result<bool> {
        let Some(loc_node) = self.memory.get_root(RootLocation::Known(location)) else {
            return Ok(false);
        };
        for &node in &self.arg_reference_nodes {
            if self.memory.is_ancestor(node, loc_node)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn check_usage(&self, usage: &T::Usage, location: &Location) -> anyhow::Result<()> {
        let is_borrowed = self.location_is_borrowed(location)?
            || self.borrowed_by_arg_reference(usage.location())?;
        match usage {
            T::Usage::Move(_) => {
                anyhow::ensure!(!is_borrowed, "Cannot move a value that is borrowed");
            }
            T::Usage::Copy {
                borrowed: borrowed_flag,
                ..
            } => {
                let Some(borrowed_flag) = borrowed_flag.get().copied() else {
                    anyhow::bail!("Borrowed flag not set for copy usage");
                };
                // Drop-safety refinement can release references earlier than the original borrow
                // check. A true borrowed flag may therefore be stale, but a location that is still
                // borrowed must have the flag set.
                if is_borrowed {
                    anyhow::ensure!(
                        borrowed_flag,
                        "Copy of borrowed location {:?} in command {} is not flagged as borrowed",
                        location.root,
                        self.current_command()?
                    );
                }
            }
        }
        Ok(())
    }

    fn argument(&mut self, sp!(_, (arg, _)): &T::Argument) -> anyhow::Result<Value> {
        let location = self.location(arg.location())?;
        match arg {
            T::Argument__::Use(usage)
            | T::Argument__::Freeze(usage)
            | T::Argument__::Read(usage) => self.check_usage(usage, location)?,
            T::Argument__::Borrow(_, _) => (),
        };
        let value = match arg {
            T::Argument__::Use(usage) => self.location_mut(arg.location())?.use_(usage)?,
            T::Argument__::Freeze(usage) => {
                self.location_mut(arg.location())?.use_(usage)?.freeze()?
            }
            T::Argument__::Borrow(is_mut, _) => self.borrow_location(arg.location(), *is_mut)?,
            T::Argument__::Read(usage) => {
                self.location_mut(arg.location())?.use_(usage)?;
                Value::NonRef
            }
        };
        if let Value::Ref { node, .. } = &value {
            self.arg_reference_nodes.push(*node);
        }
        Ok(value)
    }

    /// Borrows a location, creating its graph node on first use.
    fn borrow_location(&mut self, loc: T::Location, is_mut: bool) -> anyhow::Result<Value> {
        let root = {
            let location = self.location(loc)?;
            location.assert_borrowable()?;
            location.root
        };
        let node = self.memory.root(root)?;
        Ok(Value::Ref { is_mut, node })
    }

    fn arguments(&mut self, args: &[T::Argument]) -> anyhow::Result<Vec<'a, Value>> {
        let mut values = Vec::with_capacity_in(args.len(), self.bump());
        for arg in args {
            values.push(self.argument(arg)?);
        }
        Ok(values)
    }

    fn all_references(&self) -> impl Iterator<Item = NodeID> + '_ {
        // Only results can hold references. `tx_context`, `gas`, and every input are created
        // non-reference and never reassigned, so they are not scanned. Among results, only rows
        // produced by reference-returning commands can hold references.
        self.result_ref_rows
            .iter()
            .filter_map(move |&i| self.results.get(i))
            .flatten()
            .filter_map(|v| match v.value.as_ref() {
                Some(Value::Ref { node, .. }) => Some(*node),
                Some(Value::NonRef) | None => None,
            })
    }

    /// Returns whether any live reference may extend `node`. Excludes aliases when requested.
    fn any_extends(&self, node: NodeID, ignore_aliases: bool) -> anyhow::Result<bool> {
        for other in self.all_references() {
            if (!ignore_aliases && other == node) || self.memory.extends(other, node)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Verifies memory safety using an alternative implementation of `verify::memory_safety`.
///
/// It represents memory locations and call returns as graph nodes. Root nodes represent known PTB
/// locations (or in rare cases unknown values from calls without reference arguments). Each node
/// tracks its transitive ancestors in a bitset. A call-return delta records its command identity,
/// analogous to the regex verifier's `.*` extension, while retaining enough identity to use the
/// guarantee that mutable references returned by one call do not overlap with other references
/// returned from that call. Extensions from distinct commands are conservatively treated as
/// potentially overlapping. PTBs have no control flow, which makes this representation sufficient
/// as an invariant check for the regex implementation.
/// Checks the following
/// - Values are not used after being moved
/// - Reference safety is upheld (no dangling references)
pub fn verify<'a, Mode: ExecutionMode>(
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    txn: &T::Transaction<'a>,
) -> Result<(), ExecutionError<'a>> {
    // The reference runs its `legacy` check when `memory_safety_invariant_check_v2` is off; it is
    // on at the latest protocol on every chain, so the legacy check is not ported.
    verify_(env, txn).map_err(|e| make_invariant_violation!("{}. Transaction {:?}", e, txn))
}

pub(crate) fn verify_<'a, Mode: ExecutionMode>(
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    txn: &T::Transaction<'a>,
) -> anyhow::Result<()> {
    let mut context = Context::new(env, txn)?;
    let T::Transaction {
        gas_payment: _,
        bytes: _,
        objects: _,
        withdrawals: _,
        pure: _,
        receiving: _,
        withdrawal_compatibility_conversions: _,
        original_command_len: _,
        commands,
        unified_linkage: _,
    } = txn;
    for c in commands {
        command(&mut context, c)?;
    }
    Ok(())
}

fn command(context: &mut Context, c: &T::Command) -> anyhow::Result<()> {
    debug_assert!(context.arg_reference_nodes.is_empty());
    let results = command_(context, c)?;
    // drop unused result values by marking them as `None`
    assert_invariant!(
        results.len() == c.value.drop_values.len(),
        "result length mismatch. expected {}, got {}",
        c.value.drop_values.len(),
        results.len()
    );
    // The reference's `zip_debug_eq`; the lengths are equal by the check above.
    #[allow(clippy::disallowed_methods)]
    context.add_result_values(
        results
            .into_iter()
            .zip(c.value.drop_values.iter().copied())
            .map(|(v, drop)| if drop { None } else { Some(v) }),
    )?;
    context.arg_reference_nodes.clear();
    Ok(())
}

fn command_<'a>(
    context: &mut Context<'a>,
    sp!(_, c): &T::Command,
) -> anyhow::Result<Vec<'a, Value>> {
    let result_tys = &c.result_type;
    let results = match &c.command {
        T::Command__::MoveCall(move_call) => {
            let T::MoveCall {
                function,
                arguments,
            } = &**move_call;
            let arg_values = context.arguments(arguments)?;
            call(context, &function.signature, arg_values)?
        }
        T::Command__::TransferObjects(objs, recipient) => {
            context.arguments(objs)?;
            context.argument(recipient)?;
            non_ref_results(context.bump(), result_tys)?
        }
        T::Command__::SplitCoins(_, coin, amounts) => {
            context.arguments(amounts)?;
            let coin_value = context.argument(coin)?;
            write_ref(context, coin_value)?;
            non_ref_results(context.bump(), result_tys)?
        }
        T::Command__::MergeCoins(_, target, coins) => {
            context.arguments(coins)?;
            let target_value = context.argument(target)?;
            write_ref(context, target_value)?;
            non_ref_results(context.bump(), result_tys)?
        }
        T::Command__::MakeMoveVec(_, arguments) => {
            context.arguments(arguments)?;
            non_ref_results(context.bump(), result_tys)?
        }
        T::Command__::Publish(_, _, _) => non_ref_results(context.bump(), result_tys)?,
        T::Command__::Upgrade(_, _, _, ticket, _) => {
            context.argument(ticket)?;
            non_ref_results(context.bump(), result_tys)?
        }
    };
    assert_invariant!(
        result_tys.len() == results.len(),
        "result length mismatch. Expected {}, got {}",
        result_tys.len(),
        results.len()
    );
    Ok(results)
}

fn write_ref(context: &Context, value: Value) -> anyhow::Result<()> {
    match value {
        Value::NonRef => {
            anyhow::bail!("Cannot write to a non-reference value");
        }

        Value::Ref { is_mut: false, .. } => {
            anyhow::bail!("Cannot write to an immutable reference");
        }
        Value::Ref { is_mut: true, node } => {
            anyhow::ensure!(
                !context.any_extends(node, /* ignore alias */ true)?,
                "Cannot write to a mutable reference that has extensions"
            );
            Ok(())
        }
    }
}

fn call<'a>(
    context: &mut Context<'a>,
    signature: &T::LoadedFunctionInstantiation,
    arguments: Vec<'a, Value>,
) -> anyhow::Result<Vec<'a, Value>> {
    let bump = context.bump();
    let return_ = &signature.return_;
    // The reference starts these empty; the arguments bound their lengths.
    let mut all_nodes = Vec::with_capacity_in(arguments.len(), bump);
    let mut imm_nodes = Vec::with_capacity_in(arguments.len(), bump);
    let mut mut_nodes = Vec::with_capacity_in(arguments.len(), bump);
    for arg in arguments {
        match arg {
            Value::NonRef => (),
            Value::Ref { is_mut: true, node } => {
                anyhow::ensure!(
                    !context.any_extends(node, /* ignore alias */ true)?,
                    "Cannot transfer a mutable ref with extensions"
                );
                for other in &mut_nodes {
                    anyhow::ensure!(
                        context.memory.is_disjoint(*other, node)?,
                        "Double mutable borrow"
                    );
                }
                all_nodes.push(node);
                mut_nodes.push(node);
            }
            Value::Ref {
                is_mut: false,
                node,
            } => {
                all_nodes.push(node);
                imm_nodes.push(node);
            }
        }
    }
    // All mutable references must be disjoint from all immutable references
    for immutable in &imm_nodes {
        for mutable in &mut_nodes {
            anyhow::ensure!(
                context.memory.is_disjoint(*immutable, *mutable)?,
                "Mutable and immutable borrows cannot overlap"
            );
        }
    }
    if context.allow_references_in_ptbs {
        // Returned references cannot borrow from TxContext, so exclude arguments derived from it
        // when choosing parents for return nodes. Keep the old behavior when the flag is off:
        // dev-inspect can return references derived from TxContext in that mode.
        if let Some(tx_context_root) = context
            .memory
            .get_root(RootLocation::Known(T::Location::TxContext))
        {
            let mut tx_context_nodes = BTreeSet::new_in(bump);
            // `mut_nodes` is a subset of `all_nodes`, so all candidates are covered.
            for node in &all_nodes {
                if context.memory.is_ancestor(*node, tx_context_root)? {
                    tx_context_nodes.insert(*node);
                }
            }
            mut_nodes.retain(|node| !tx_context_nodes.contains(node));
            all_nodes.retain(|node| !tx_context_nodes.contains(node));
        }
    }
    let command = context.current_command()?;
    // Reference returns derive from an unknown root when no reference arguments feed them. Calls
    // that return no references need no such node, so avoid creating one.
    let has_reference_return = return_
        .iter()
        .any(|ty| matches!(ty, T::Type::Reference(_, _)));
    let mut_nodes = if mut_nodes.is_empty() && has_reference_return {
        let mut nodes = Vec::with_capacity_in(1, bump);
        nodes.push(context.memory.root(RootLocation::Unknown { command })?);
        nodes
    } else {
        mut_nodes
    };
    let all_nodes = if all_nodes.is_empty() && has_reference_return {
        let mut nodes = Vec::with_capacity_in(1, bump);
        nodes.push(context.memory.root(RootLocation::Unknown { command })?);
        nodes
    } else {
        all_nodes
    };
    let mut results = Vec::with_capacity_in(return_.len(), bump);
    for (i, ty) in return_.iter().enumerate() {
        let _result = checked_as!(i, u16)?;
        results.push(match ty {
            T::Type::Reference(/* is mut */ true, _) => Value::Ref {
                is_mut: true,
                node: context.memory.call_return(command, &mut_nodes)?,
            },
            T::Type::Reference(/* is mut */ false, _) => Value::Ref {
                is_mut: false,
                node: context.memory.call_return(command, &all_nodes)?,
            },
            _ => Value::NonRef,
        });
    }
    Ok(results)
}

fn non_ref_results<'a>(bump: &'a Bump, results: &[T::Type]) -> anyhow::Result<Vec<'a, Value>> {
    let mut values = Vec::with_capacity_in(results.len(), bump);
    for t in results {
        anyhow::ensure!(
            !matches!(t, T::Type::Reference(_, _)),
            "attempted to create a non-reference result from a reference type",
        );
        values.push(Value::NonRef);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitset_set_union_and_common() {
        let bump = Bump::with_capacity(4096);
        let mut a = BitSet::with_bits(&bump, 70);
        assert!(!a.set(3).unwrap());
        assert!(a.set(3).unwrap());
        assert!(a.set(128).is_err());
        a.set_grow(130);
        assert!(a.contains(130) && a.contains(3) && !a.contains(4));
        assert_eq!(a.count(), 2);

        let mut b = BitSet::new_in(&bump);
        b.set_grow(130);
        b.set_grow(7);
        let mut common = std::vec::Vec::new();
        a.try_any_common(&b, |i| {
            common.push(i);
            Ok(false)
        })
        .unwrap();
        assert_eq!(common, [130]);

        b.union(&a);
        assert_eq!(b.count(), 3);
    }

    #[test]
    fn memory_ancestry_and_cross_command_extensions() {
        let bump = Bump::with_capacity(4096);
        let mut memory = Memory::new(&bump);
        let root = memory
            .root(RootLocation::Known(T::Location::ObjectInput(0)))
            .unwrap();
        assert_eq!(
            memory
                .root(RootLocation::Known(T::Location::ObjectInput(0)))
                .unwrap(),
            root
        );
        let other = memory
            .root(RootLocation::Known(T::Location::ObjectInput(1)))
            .unwrap();
        // Two returns of one call are disjoint; a return extends its parent.
        let a = memory.call_return(0, &[root]).unwrap();
        let b = memory.call_return(0, &[root]).unwrap();
        assert!(memory.extends(a, root).unwrap());
        assert!(!memory.extends(root, a).unwrap());
        assert!(memory.is_disjoint(a, b).unwrap());
        assert!(memory.is_disjoint(root, other).unwrap());
        assert!(memory.conflict_parents.is_empty());
        // A return from another command may overlap with the first command's returns.
        let c = memory.call_return(1, &[root]).unwrap();
        assert!(memory.conflict_parents.contains(&root));
        assert!(!memory.is_disjoint(a, c).unwrap());
        assert!(memory.is_disjoint(c, other).unwrap());
    }
}
