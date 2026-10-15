//! Convert an evaluated Lua value into JSON for inspection and diffing.
//!
//! Tables become arrays when they are dense 1..n sequences and objects otherwise,
//! with object keys sorted so output is byte-stable across runs. Cycles are cut
//! with a marker rather than recursing forever; `mapinfo.lua` results are trees in
//! practice, but a map is free to hand back anything.

use mlua::{Table, Value};
use serde_json::{Map, Number, Value as Json};

const MAX_DEPTH: usize = 32;

/// Render `value` as JSON.
pub fn to_json(value: &Value) -> Json {
    convert(value, 0)
}

fn convert(value: &Value, depth: usize) -> Json {
    if depth > MAX_DEPTH {
        return Json::String("<too deep>".into());
    }
    match value {
        Value::Nil => Json::Null,
        Value::Boolean(b) => Json::Bool(*b),
        Value::Integer(i) => Json::Number((*i).into()),
        Value::Number(n) => Number::from_f64(*n).map_or(Json::Null, Json::Number),
        Value::String(s) => Json::String(s.to_string_lossy().to_string()),
        Value::Table(t) => convert_table(t, depth),
        other => Json::String(format!("<{}>", other.type_name())),
    }
}

fn convert_table(table: &Table, depth: usize) -> Json {
    let len = table.raw_len();
    if len > 0 && is_dense_sequence(table, len) {
        let items = (1..=len)
            .map(|i| table.raw_get::<Value>(i).map(|v| convert(&v, depth + 1)).unwrap_or(Json::Null))
            .collect();
        return Json::Array(items);
    }

    let mut entries: Vec<(String, Json)> = table
        .clone()
        .pairs::<Value, Value>()
        .filter_map(Result::ok)
        .map(|(key, value)| (key_to_string(&key), convert(&value, depth + 1)))
        .collect();

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Json::Object(entries.into_iter().collect::<Map<_, _>>())
}

/// A table is sequence-like when every index 1..len is present and it holds no
/// other keys, which is how Lua array literals arrive.
fn is_dense_sequence(table: &Table, len: usize) -> bool {
    let non_nil = (1..=len).all(|i| table.raw_get::<Value>(i).is_ok_and(|v| !v.is_nil()));
    non_nil && table.clone().pairs::<Value, Value>().filter_map(Result::ok).count() == len
}

/// String keys are lowercased, matching `LuaUtils::LowerKeys`, which the engine
/// applies to every parsed Lua table. Together with `LuaParser`'s `lowerCppKeys`
/// this makes map info lookups effectively case-insensitive, so callers can read
/// `terraintypes` regardless of how the map author spelled it.
fn key_to_string(key: &Value) -> String {
    match key {
        Value::String(s) => s.to_string_lossy().to_ascii_lowercase(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Boolean(b) => b.to_string(),
        other => format!("<{}>", other.type_name()),
    }
}
