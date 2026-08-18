// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! JSON value conversion -- between serde_json::Value and evorule_tcb::JsonValue

use evorule_tcb::JsonValue;
use serde_json;

/// Convert serde_json::Value to evorule_tcb::JsonValue
pub fn serde_to_tcb(value: &serde_json::Value) -> JsonValue {
    match value {
        serde_json::Value::Null => JsonValue::Null,
        serde_json::Value::Bool(b) => JsonValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                JsonValue::Integer(i)
            } else if let Some(u) = n.as_u64() {
                JsonValue::Integer(u as i64)
            } else if let Some(f) = n.as_f64() {
                JsonValue::string(f.to_string())
            } else {
                JsonValue::Null
            }
        }
        serde_json::Value::String(s) => JsonValue::string(s),
        serde_json::Value::Array(arr) => {
            let mut result = Vec::new();
            for v in arr {
                result.push(serde_to_tcb(v));
            }
            JsonValue::array(result)
        }
        serde_json::Value::Object(map) => {
            let mut result = std::collections::BTreeMap::new();
            for (k, v) in map {
                result.insert(k.clone(), serde_to_tcb(v));
            }
            JsonValue::object(result)
        }
    }
}

/// Convert evorule_tcb::JsonValue to serde_json::Value
pub fn tcb_to_serde(value: &JsonValue) -> serde_json::Value {
    match value {
        JsonValue::Null => serde_json::Value::Null,
        JsonValue::Bool(b) => serde_json::Value::Bool(*b),
        JsonValue::Integer(i) => serde_json::Value::Number((*i).into()),
        JsonValue::String(s) => serde_json::Value::String(s.to_string()),
        JsonValue::Array(arr) => {
            let mut result = Vec::new();
            for v in arr {
                result.push(tcb_to_serde(v));
            }
            serde_json::Value::Array(result)
        }
        JsonValue::Object(map) => {
            let mut result = serde_json::Map::new();
            for (k, v) in map {
                result.insert(k.clone(), tcb_to_serde(v));
            }
            serde_json::Value::Object(result)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serde_to_tcb_null() {
        assert_eq!(serde_to_tcb(&serde_json::Value::Null), JsonValue::Null);
    }

    #[test]
    fn test_serde_to_tcb_bool() {
        assert_eq!(
            serde_to_tcb(&serde_json::Value::Bool(true)),
            JsonValue::Bool(true)
        );
        assert_eq!(
            serde_to_tcb(&serde_json::Value::Bool(false)),
            JsonValue::Bool(false)
        );
    }

    #[test]
    fn test_serde_to_tcb_number() {
        assert_eq!(
            serde_to_tcb(&serde_json::Value::Number(42.into())),
            JsonValue::Integer(42)
        );
        assert_eq!(
            serde_to_tcb(&serde_json::Value::Number((-10).into())),
            JsonValue::Integer(-10)
        );
    }

    #[test]
    fn test_serde_to_tcb_string() {
        assert_eq!(
            serde_to_tcb(&serde_json::Value::String("hello".to_string())),
            JsonValue::string("hello")
        );
    }

    #[test]
    fn test_serde_to_tcb_array() {
        let serde_arr = serde_json::json!([1, 2, 3]);
        let tcb_arr = serde_to_tcb(&serde_arr);
        assert!(tcb_arr.is_array());
        assert_eq!(tcb_arr.as_array().map(|a| a.len()), Some(3));
    }

    #[test]
    fn test_serde_to_tcb_object() {
        let serde_obj = serde_json::json!({"key": "value"});
        let tcb_obj = serde_to_tcb(&serde_obj);
        assert!(tcb_obj.is_object());
        assert_eq!(tcb_obj.get("key").and_then(|v| v.as_str()), Some("value"));
    }

    #[test]
    fn test_tcb_to_serde_roundtrip() {
        let original = serde_json::json!({"name": "test", "count": 42, "active": true});
        let tcb_val = serde_to_tcb(&original);
        let roundtrip = tcb_to_serde(&tcb_val);
        assert_eq!(original, roundtrip);
    }
}
