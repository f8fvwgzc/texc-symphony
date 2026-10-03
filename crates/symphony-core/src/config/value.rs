//! Dynamic-value plumbing: YAML -> JSON conversion with stringified keys, and nil dropping.

use serde_json::{Map, Number, Value};
use serde_yaml_ng::Value as Yaml;

/// Converts a decoded YAML value to a JSON value, stringifying every map key (Elixir `normalize_keys`:
/// `1` -> `"1"`, `true` -> `"true"`, `null` -> `""`). Tags are dropped (`!custom 1` -> `1`).
/// Non-finite floats (`.inf`, `.nan`) have no JSON form and are kept as their YAML text.
pub fn yaml_to_json(value: Yaml) -> Value {
    match value {
        Yaml::Null => Value::Null,
        Yaml::Bool(b) => Value::Bool(b),
        Yaml::Number(n) => yaml_number(&n),
        Yaml::String(s) => Value::String(s),
        Yaml::Sequence(items) => Value::Array(items.into_iter().map(yaml_to_json).collect()),
        Yaml::Mapping(mapping) => {
            let mut out = Map::new();
            for (key, value) in mapping {
                out.insert(key_to_string(key), yaml_to_json(value));
            }
            Value::Object(out)
        }
        Yaml::Tagged(tagged) => yaml_to_json(tagged.value),
    }
}

fn yaml_number(n: &serde_yaml_ng::Number) -> Value {
    if let Some(i) = n.as_i64() {
        Value::Number(i.into())
    } else if let Some(u) = n.as_u64() {
        Value::Number(u.into())
    } else {
        let f = n.as_f64().unwrap_or(f64::NAN);
        Number::from_f64(f).map_or_else(|| Value::String(n.to_string()), Value::Number)
    }
}

fn key_to_string(key: Yaml) -> String {
    match key {
        Yaml::Null => String::new(),
        Yaml::Bool(b) => b.to_string(),
        Yaml::Number(n) => n.to_string(),
        Yaml::String(s) => s,
        Yaml::Tagged(tagged) => key_to_string(tagged.value),
        other => serde_json::to_string(&yaml_to_json(other)).unwrap_or_default(),
    }
}

/// Elixir `drop_nil_values/1`: recursively removes map entries whose value is `null` (so `key: null`
/// behaves exactly like an absent key). `null` *list elements* are kept.
pub fn drop_nil_values(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter_map(|(k, v)| match drop_nil_values(v) {
                    Value::Null => None,
                    v => Some((k, v)),
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(drop_nil_values).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn yaml(text: &str) -> Value {
        yaml_to_json(serde_yaml_ng::from_str(text).expect("valid yaml"))
    }

    #[test]
    fn keys_are_stringified_and_tags_dropped() {
        assert_eq!(
            yaml("1: x\ntrue: y\nnull: z\n3.5: w\na: !custom 5\n"),
            json!({"1": "x", "true": "y", "": "z", "3.5": "w", "a": 5})
        );
    }

    #[test]
    fn yaml_1_2_scalars() {
        // `yes`/`on` stay strings (yamerl core schema behaves the same), hex/octal are integers.
        assert_eq!(
            yaml("a: yes\nb: on\nc: 0x1F\nd: 1_000\ne: ~\ng: 1e3\nh: '12'\n"),
            json!({"a": "yes", "b": "on", "c": 31, "d": "1_000", "e": null, "g": 1000.0, "h": "12"})
        );
        assert_eq!(yaml("h: .inf\n"), json!({"h": ".inf"}));
    }

    #[test]
    fn drop_nil_values_removes_null_entries_recursively_but_keeps_list_nils() {
        let input = json!({"a": null, "b": {"c": null, "d": 1}, "e": [null, {"f": null}]});
        assert_eq!(
            drop_nil_values(input),
            json!({"b": {"d": 1}, "e": [null, {}]})
        );
    }
}
