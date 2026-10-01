// Copied by wuapi-codegen from packages/sdk-codegen/templates/rust/src/serde_helpers.rs. Do not edit here.

//! Serde helpers the generated types use.

// An API may need only some of them.
#![allow(dead_code)]

use serde::{Deserialize, Deserializer};

/// Deserializes a field that may be absent or `null` into
/// `Option<Option<T>>`: absent is `None`, `null` is `Some(None)`, a value is
/// `Some(Some(value))`. Pair it with `#[serde(default)]`.
pub(crate) fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Parses a JSON value into `T`, reporting failures as the caller's
/// deserializer error. The unions with a discriminator read the value first
/// to pick the variant, then parse it with this.
pub(crate) fn from_value<T, E>(value: serde_json::Value) -> Result<T, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    serde_json::from_value(value).map_err(E::custom)
}
