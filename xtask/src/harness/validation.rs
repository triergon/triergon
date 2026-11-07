//! Shared checks for report fields and measurement bindings.

use crate::json::Value;

pub(super) fn field<'a>(value: &'a Value, key: &str) -> Result<&'a Value, String> {
    value
        .get(key)
        .filter(|v| **v != Value::Null)
        .ok_or_else(|| format!("missing {key}"))
}

pub(super) fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    field(value, key)?
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{key} must be a nonempty string"))
}

pub(super) fn array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value], String> {
    field(value, key)?
        .as_array()
        .ok_or_else(|| format!("{key} must be an array"))
}
