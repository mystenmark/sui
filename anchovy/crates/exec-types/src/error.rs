// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `sui_types::error::ExecutionError`, with the kind as anchovy's view type
//! (`messages::execution_status::ExecutionErrorKind`), whose borrowed parts
//! live in the transaction's arena.

use std::fmt;

pub use messages::execution_status::ExecutionErrorKind;

/// The command a failure is attributed to.
pub type CommandIndex = usize;

/// An error's cause, for diagnostics only: it never reaches effects.
/// Heap-allocated, as in the reference, since only failures make one.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

pub struct ExecutionError<'a> {
    inner: Box<ExecutionErrorInner<'a>>,
}

#[derive(Debug)]
struct ExecutionErrorInner<'a> {
    kind: ExecutionErrorKind<'a>,
    source: Option<BoxError>,
    command: Option<CommandIndex>,
}

impl<'a> ExecutionError<'a> {
    pub fn new(kind: ExecutionErrorKind<'a>, source: Option<BoxError>) -> Self {
        Self {
            inner: Box::new(ExecutionErrorInner {
                kind,
                source,
                command: None,
            }),
        }
    }

    pub fn new_with_source<E: Into<BoxError>>(kind: ExecutionErrorKind<'a>, source: E) -> Self {
        Self::new(kind, Some(source.into()))
    }

    pub fn invariant_violation<E: Into<BoxError>>(source: E) -> Self {
        Self::new_with_source(ExecutionErrorKind::InvariantViolation, source)
    }

    #[must_use]
    pub fn with_command_index(mut self, command: CommandIndex) -> Self {
        self.inner.command = Some(command);
        self
    }

    pub fn from_kind(kind: ExecutionErrorKind<'a>) -> Self {
        Self::new(kind, None)
    }

    pub fn kind(&self) -> &ExecutionErrorKind<'a> {
        &self.inner.kind
    }

    pub fn command(&self) -> Option<CommandIndex> {
        self.inner.command
    }

    pub fn source(&self) -> &Option<BoxError> {
        &self.inner.source
    }

    pub fn to_execution_status(&self) -> (ExecutionErrorKind<'a>, Option<CommandIndex>) {
        (*self.kind(), self.command())
    }
}

impl<'a> From<ExecutionErrorKind<'a>> for ExecutionError<'a> {
    fn from(kind: ExecutionErrorKind<'a>) -> Self {
        Self::from_kind(kind)
    }
}

impl fmt::Debug for ExecutionError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ExecutionError: {:?}", self.inner)
    }
}

impl fmt::Display for ExecutionError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ExecutionError: {:?}", self.inner)
    }
}

impl std::error::Error for ExecutionError<'_> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner.source.as_ref().map(|e| &**e as _)
    }
}

// The reference truncates the index with `as`.
#[allow(clippy::cast_possible_truncation)]
pub fn command_argument_error(
    e: messages::execution_status::CommandArgumentError,
    arg_idx: usize,
) -> ExecutionError<'static> {
    ExecutionError::from_kind(ExecutionErrorKind::command_argument_error(
        e,
        arg_idx as u16,
    ))
}

/// The `sui_types::error::UserInputError` variants execution raises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserInputError {
    GasPriceUnderRGP {
        gas_price: u64,
        reference_gas_price: u64,
    },
    GasPriceTooHigh {
        max_gas_price: u64,
    },
}

#[macro_export]
macro_rules! make_invariant_violation {
    ($($args:expr),* $(,)?) => {{
        if cfg!(debug_assertions) {
            panic!($($args),*)
        }
        $crate::error::ExecutionError::invariant_violation(format!($($args),*))
    }}
}

#[macro_export]
macro_rules! invariant_violation {
    ($($args:expr),* $(,)?) => {
        return Err($crate::make_invariant_violation!($($args),*).into())
    };
}

#[macro_export]
macro_rules! assert_invariant {
    ($cond:expr, $($args:expr),* $(,)?) => {{
        if !$cond {
            $crate::invariant_violation!($($args),*)
        }
    }};
}

/// A helper macro for performing a checked cast from one type to another, returning a
/// ExecutionError invariant violation if the cast fails.
#[macro_export]
macro_rules! checked_as {
    ($value:expr, $target_type:ty) => {{
        let v = $value;
        <$target_type>::try_from(v).map_err(|e| {
            $crate::make_invariant_violation!(
                "Value {} cannot be safely cast to {}: {:?}",
                v,
                stringify!($target_type),
                e
            )
        })
    }};
}

/// A trait for safe indexing into collections that returns a ExecutionError as long as the
/// collection implements `AsRef<[T]>`.
/// This is useful for avoiding panics on out-of-bounds access, and instead returning a proper
/// error.
pub trait SafeIndex<T> {
    /// Get a reference to the element at the given `index`, or return invariant violation error
    /// if the index is out of bounds.
    fn safe_get<'a, I>(&'a self, index: I) -> Result<&'a I::Output, ExecutionError<'static>>
    where
        I: std::slice::SliceIndex<[T]>,
        T: 'a;

    /// Get a mutable reference to the element at the given `index`, or return invariant violation
    /// error if the index is out of bounds.
    fn safe_get_mut<'a, I>(
        &'a mut self,
        index: I,
    ) -> Result<&'a mut I::Output, ExecutionError<'static>>
    where
        I: std::slice::SliceIndex<[T]>,
        T: 'a;
}

impl<T, C> SafeIndex<T> for C
where
    C: AsRef<[T]> + AsMut<[T]>,
{
    fn safe_get<'a, I>(&'a self, index: I) -> Result<&'a I::Output, ExecutionError<'static>>
    where
        I: std::slice::SliceIndex<[T]>,
        T: 'a,
    {
        let slice = self.as_ref();
        let len = slice.len();
        slice.get(index).ok_or_else(|| {
            crate::make_invariant_violation!("Index out of bounds for collection of length {}", len)
        })
    }

    fn safe_get_mut<'a, I>(
        &'a mut self,
        index: I,
    ) -> Result<&'a mut I::Output, ExecutionError<'static>>
    where
        I: std::slice::SliceIndex<[T]>,
        T: 'a,
    {
        let slice = self.as_mut();
        let len = slice.len();
        slice.get_mut(index).ok_or_else(|| {
            crate::make_invariant_violation!("Index out of bounds for collection of length {}", len)
        })
    }
}
