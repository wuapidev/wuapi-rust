// Copied by wuapi-codegen from packages/sdk-codegen/templates/rust/src/macros.rs. Do not edit here.

//! Macros the generated types use.

// An API without string enums, or without integer enums, leaves one unused.
#![allow(unused_macros, unused_imports)]

/// An enum of string values that also accepts values this version of the SDK
/// does not know, so a value the API adds later still parses.
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$variant_meta:meta])* $variant:ident = $value:literal, )*
            @unknown $unknown:ident,
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum $name {
            $( $(#[$variant_meta])* $variant, )*
            /// A value this version of the SDK does not know.
            $unknown(String),
        }

        impl $name {
            /// The value as it travels on the wire.
            #[must_use]
            pub fn as_str(&self) -> &str {
                match self {
                    $( Self::$variant => $value, )*
                    Self::$unknown(value) => value,
                }
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                match value.as_str() {
                    $( $value => Self::$variant, )*
                    _ => Self::$unknown(value),
                }
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::from(value.to_owned())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                <String as serde::Deserialize>::deserialize(deserializer).map(Self::from)
            }
        }
    };
}

/// An enum of integer values that also accepts values this version of the
/// SDK does not know.
macro_rules! integer_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$variant_meta:meta])* $variant:ident = $value:literal, )*
            @unknown $unknown:ident,
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum $name {
            $( $(#[$variant_meta])* $variant, )*
            /// A value this version of the SDK does not know.
            $unknown(i64),
        }

        impl $name {
            /// The value as it travels on the wire.
            #[must_use]
            pub fn value(self) -> i64 {
                match self {
                    $( Self::$variant => $value, )*
                    Self::$unknown(value) => value,
                }
            }
        }

        impl From<i64> for $name {
            fn from(value: i64) -> Self {
                match value {
                    $( $value => Self::$variant, )*
                    _ => Self::$unknown(value),
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.value())
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_i64(self.value())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                <i64 as serde::Deserialize>::deserialize(deserializer).map(Self::from)
            }
        }
    };
}

pub(crate) use {integer_enum, string_enum};
