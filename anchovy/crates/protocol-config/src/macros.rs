// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `macro_rules!` replacements for the reference's derive macros. Each
//! wraps a struct definition and walks its fields one at a time (so field
//! order, which the snapshots depend on, is kept), sorting each field by
//! its type:
//!
//! - `ProtocolConfig`: an `Option<u16 | u32 | u64 | bool>` field gets
//!   `#[serde(skip_serializing_if = "Option::is_none")]` (the reference's
//!   `skip_serializing_none`), a getter that panics when the value is not
//!   set at this version, and entries in the by-name lookup. Other fields
//!   are emitted as written.
//! - `FeatureFlags`: a `bool` flag gets a forwarding getter on
//!   `ProtocolConfig` and by-name lookup and setter entries. A flag marked
//!   `#[skip_protocol_config_accessor]` (always right after its
//!   `#[serde(..)]`) has a hand-written getter instead. Other fields are
//!   emitted as written.
//!
//! Fields are `pub`: reading one is the reference's `x_as_option()`, and
//! assigning one is its `set_x_for_testing()`.

macro_rules! protocol_config {
    (
        $(#[$meta:meta])*
        pub struct ProtocolConfig { $($body:tt)* }
    ) => {
        protocol_config!(@field [$(#[$meta])*] [] [] $($body)*);
    };

    (@field $meta:tt [$($fields:tt)*] [$($scalars:tt)*]
        $(#[$m:meta])* $_vis:vis $f:ident : Option<$t:ident>, $($rest:tt)*
    ) => {
        protocol_config!(@field $meta
            [$($fields)* $(#[$m])* #[serde(skip_serializing_if = "Option::is_none")] pub $f: Option<$t>,]
            [$($scalars)* ($f $t)]
            $($rest)*);
    };

    (@field $meta:tt [$($fields:tt)*] $scalars:tt
        $(#[$m:meta])* $_vis:vis $f:ident : $t:ty, $($rest:tt)*
    ) => {
        protocol_config!(@field $meta [$($fields)* $(#[$m])* pub $f: $t,] $scalars $($rest)*);
    };

    (@field [$($meta:tt)*] [$($fields:tt)*] [$(($f:ident $t:ident))*]) => {
        $($meta)*
        pub struct ProtocolConfig { $($fields)* }

        impl ProtocolConfig {
            const CONSTANT_ERR_MSG: &'static str =
                "protocol constant not present in current protocol version";

            $(
                pub fn $f(&self) -> $t {
                    self.$f.expect(Self::CONSTANT_ERR_MSG)
                }
            )*

            /// Looks up a scalar config attribute by name.
            pub fn lookup_attr(&self, value: String) -> Option<ProtocolConfigValue> {
                match value.as_str() {
                    $(stringify!($f) => self.$f.map(ProtocolConfigValue::$t),)*
                    _ => None,
                }
            }

            /// Every scalar config attribute by name.
            pub fn attr_map(&self) -> BTreeMap<String, Option<ProtocolConfigValue>> {
                [$(stringify!($f)),*]
                    .into_iter()
                    .map(|name| (name.to_owned(), self.lookup_attr(name.to_owned())))
                    .collect()
            }

            pub fn set_attr_for_testing(&mut self, attr: String, val: String) {
                match attr.as_str() {
                    $(stringify!($f) => self.$f = Some(val.parse().unwrap()),)*
                    _ => panic!(
                        "Attempting to set unknown or non-string-settable attribute: {}",
                        attr,
                    ),
                }
            }
        }
    };
}

macro_rules! feature_flags {
    (
        $(#[$meta:meta])*
        pub struct FeatureFlags { $($body:tt)* }
    ) => {
        feature_flags!(@field [$(#[$meta])*] [] [] [] $($body)*);
    };

    (@field $meta:tt [$($fields:tt)*] $getters:tt [$($named:tt)*]
        #[serde $serde:tt] #[skip_protocol_config_accessor] $_vis:vis $f:ident : bool,
        $($rest:tt)*
    ) => {
        feature_flags!(@field $meta
            [$($fields)* #[serde $serde] pub $f: bool,]
            $getters
            [$($named)* $f]
            $($rest)*);
    };

    (@field $meta:tt [$($fields:tt)*] [$($getters:tt)*] [$($named:tt)*]
        $(#[$m:meta])* $_vis:vis $f:ident : bool, $($rest:tt)*
    ) => {
        feature_flags!(@field $meta
            [$($fields)* $(#[$m])* pub $f: bool,]
            [$($getters)* $f]
            [$($named)* $f]
            $($rest)*);
    };

    (@field $meta:tt [$($fields:tt)*] $getters:tt $named:tt
        $(#[$m:meta])* $_vis:vis $f:ident : $t:ty, $($rest:tt)*
    ) => {
        feature_flags!(@field $meta [$($fields)* $(#[$m])* pub $f: $t,] $getters $named $($rest)*);
    };

    (@field [$($meta:tt)*] [$($fields:tt)*] [$($getter:ident)*] [$($name:ident)*]) => {
        $($meta)*
        pub struct FeatureFlags { $($fields)* }

        impl FeatureFlags {
            /// Looks up a boolean feature flag by name.
            pub fn lookup_attr(&self, value: String) -> Option<bool> {
                match value.as_str() {
                    $(stringify!($name) => Some(self.$name),)*
                    _ => None,
                }
            }

            pub fn set_attr_for_testing(&mut self, attr: String, val: bool) {
                match attr.as_str() {
                    $(stringify!($name) => self.$name = val,)*
                    _ => panic!("Attempting to set unknown feature flag: {}", attr),
                }
            }

            /// Every boolean feature flag by name.
            pub fn attr_map(&self) -> BTreeMap<String, bool> {
                [$((stringify!($name), self.$name)),*]
                    .into_iter()
                    .map(|(name, val)| (name.to_owned(), val))
                    .collect()
            }
        }

        impl ProtocolConfig {
            $(
                pub fn $getter(&self) -> bool {
                    self.feature_flags.$getter
                }
            )*

            pub fn set_feature_flag_for_testing(&mut self, flag: String, val: bool) {
                self.feature_flags.set_attr_for_testing(flag, val)
            }

            pub fn lookup_feature(&self, value: String) -> Option<bool> {
                self.feature_flags.lookup_attr(value)
            }

            pub fn feature_map(&self) -> BTreeMap<String, bool> {
                self.feature_flags.attr_map()
            }
        }
    };
}
