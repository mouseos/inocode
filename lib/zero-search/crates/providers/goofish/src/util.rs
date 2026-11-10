//! Shared JSON coercion helpers mirroring the Python parsers' fastjson
//! semantics (`goofish_parser` `_s` / `_as_bool` / `_as_int` family).

use serde_json::{Map, Value};

/// fastjson `getString` semantics: strings pass through, other JSON
/// scalars are stringified ("true"/"false" for bools), null stays None.
pub fn as_str(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(if *b { "true".into() } else { "false".into() }),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `_as_bool`: native bools and the measured stringified forms only.
pub fn as_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(s) if s == "true" => Some(true),
        Value::String(s) if s == "false" => Some(false),
        _ => None,
    }
}

/// `_as_int`: strict — JSON ints only (a bool is never an int).
pub fn as_int_strict(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

/// `_as_int_measured`: strict plus the round-19 measured decimal-string form.
pub fn as_int_measured(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => {
            let t = s.as_str();
            let digits = t.strip_prefix('-').unwrap_or(t);
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                t.parse::<i64>().ok()
            } else {
                None
            }
        }
        _ => None,
    }
}

/// explore.py's lenient `_as_int`: ints and any parseable numeric string.
pub fn as_int_lenient(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

pub fn as_list(value: &Value) -> &[Value] {
    value.as_array().map(|a| a.as_slice()).unwrap_or(&[])
}

pub fn dig<'a>(mut cur: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    for key in keys {
        cur = cur.as_object()?.get(*key)?;
    }
    Some(cur)
}

pub fn obj_get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_object().and_then(|m| m.get(key))
}

/// Build a JSON object from (key, Option<Value>) pairs, omitting None
/// entries — the normalized-output analogue of the Python dataclass
/// fields that stay null (nulls are omitted from normalized output).
pub fn build_obj(pairs: Vec<(&str, Option<Value>)>) -> Value {
    let mut map = Map::new();
    for (k, v) in pairs {
        if let Some(v) = v {
            map.insert(k.to_string(), v);
        }
    }
    Value::Object(map)
}

pub fn opt_str(value: Option<String>) -> Option<Value> {
    value.map(Value::String)
}

pub fn opt_bool(value: Option<bool>) -> Option<Value> {
    value.map(Value::Bool)
}

pub fn opt_i64(value: Option<i64>) -> Option<Value> {
    value.map(|n| Value::Number(n.into()))
}

/// First `\d+` run in the text, else 0 (FCIDetailParseHandler.extractFirstInt).
pub fn extract_first_int(text: &str) -> i64 {
    let mut digits = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else if !digits.is_empty() {
            break;
        }
    }
    if digits.is_empty() {
        0
    } else {
        digits.parse::<i64>().unwrap_or(0)
    }
}
