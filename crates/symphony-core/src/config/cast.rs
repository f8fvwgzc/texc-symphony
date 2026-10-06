//! Ecto-compatible casting primitives over `serde_json::Value`.
//!
//! Semantics reproduced from `Ecto.Changeset.cast/4` with `empty_values: []`:
//! - `:integer` accepts an integer or a string that parses fully as one (`"30000"`, `"+5"`); floats,
//!   booleans and anything else are `is invalid`;
//! - `:string` accepts only strings (`""` is kept);
//! - `:boolean` accepts `true`/`false` and the strings `"true"`, `"false"`, `"1"`, `"0"`;
//! - `{:array, :string}` accepts only lists of strings;
//! - `:map` accepts only maps (values are not cast).
//!
//! Errors are accumulated as `"<section>.<field> <message>"`.

use serde_json::{Map, Value};

/// Accumulated validation errors, rendered like `Schema.format_errors/1`.
#[derive(Debug, Default)]
pub(crate) struct Errors {
    list: Vec<String>,
}

impl Errors {
    pub(crate) fn push(&mut self, path: &str, message: &str) {
        self.list.push(format!("{path} {message}"));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.list.len()
    }

    pub(crate) fn join(&self) -> String {
        self.list.join(", ")
    }
}

/// One embedded section (`tracker`, `polling`, ...) of the front matter.
pub(crate) struct Section<'a> {
    name: &'static str,
    map: Option<&'a Map<String, Value>>,
}

impl<'a> Section<'a> {
    /// Looks up a section; a present non-map value records `"<name> is invalid"`.
    pub(crate) fn new(
        root: &'a Map<String, Value>,
        name: &'static str,
        errors: &mut Errors,
    ) -> Self {
        let map = match root.get(name) {
            None => None,
            Some(Value::Object(map)) => Some(map),
            Some(_) => {
                errors.push(name, "is invalid");
                None
            }
        };
        Self { name, map }
    }

    pub(crate) fn path(&self, field: &str) -> String {
        format!("{}.{field}", self.name)
    }

    pub(crate) fn raw(&self, field: &str) -> Option<&'a Value> {
        self.map.and_then(|m| m.get(field))
    }

    fn cast<T>(
        &self,
        field: &str,
        errors: &mut Errors,
        f: impl FnOnce(&'a Value) -> Option<T>,
    ) -> Option<T> {
        let raw = self.raw(field)?;
        let cast = f(raw);
        if cast.is_none() {
            errors.push(&self.path(field), "is invalid");
        }
        cast
    }

    pub(crate) fn integer(&self, field: &str, errors: &mut Errors) -> Option<i64> {
        self.cast(field, errors, cast_integer)
    }

    pub(crate) fn string(&self, field: &str, errors: &mut Errors) -> Option<String> {
        self.cast(field, errors, |v| v.as_str().map(str::to_owned))
    }

    pub(crate) fn boolean(&self, field: &str, errors: &mut Errors) -> Option<bool> {
        self.cast(field, errors, cast_boolean)
    }

    pub(crate) fn string_list(&self, field: &str, errors: &mut Errors) -> Option<Vec<String>> {
        self.cast(field, errors, cast_string_list)
    }

    pub(crate) fn map(&self, field: &str, errors: &mut Errors) -> Option<Map<String, Value>> {
        self.cast(field, errors, |v| v.as_object().cloned())
    }

    pub(crate) fn string_or_map(
        &self,
        field: &str,
        errors: &mut Errors,
    ) -> Option<super::StringOrMap> {
        self.cast(field, errors, super::StringOrMap::cast)
    }

    /// `validate_number(field, greater_than: 0)` on a present value.
    pub(crate) fn positive(
        &self,
        field: &str,
        value: Option<i64>,
        errors: &mut Errors,
    ) -> Option<i64> {
        let value = value?;
        if value > 0 {
            Some(value)
        } else {
            errors.push(&self.path(field), "must be greater than 0");
            None
        }
    }

    /// `validate_number(field, greater_than_or_equal_to: 0)` on a present value.
    pub(crate) fn non_negative(
        &self,
        field: &str,
        value: Option<i64>,
        errors: &mut Errors,
    ) -> Option<i64> {
        let value = value?;
        if value >= 0 {
            Some(value)
        } else {
            errors.push(&self.path(field), "must be greater than or equal to 0");
            None
        }
    }

    /// Narrows a validated integer into a smaller unsigned type (`is invalid` when it does not fit).
    pub(crate) fn fit<T: TryFrom<i64>>(
        &self,
        field: &str,
        value: Option<i64>,
        errors: &mut Errors,
    ) -> Option<T> {
        let value = value?;
        match T::try_from(value) {
            Ok(v) => Some(v),
            Err(_) => {
                errors.push(&self.path(field), "is invalid");
                None
            }
        }
    }
}

/// Ecto `:integer` cast.
pub fn cast_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

/// Ecto `:boolean` cast.
pub fn cast_boolean(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Ecto `{:array, :string}` cast (a `null` element is rejected; see the migration notes).
pub fn cast_string_list(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn integer_cast_follows_ecto() {
        assert_eq!(cast_integer(&json!(5)), Some(5));
        assert_eq!(cast_integer(&json!("30000")), Some(30000));
        assert_eq!(cast_integer(&json!("+5")), Some(5));
        assert_eq!(cast_integer(&json!("-5")), Some(-5));
        assert_eq!(cast_integer(&json!(" 5")), None);
        assert_eq!(cast_integer(&json!("5 ")), None);
        assert_eq!(cast_integer(&json!("bad")), None);
        assert_eq!(cast_integer(&json!(1.5)), None);
        assert_eq!(cast_integer(&json!(true)), None);
        assert_eq!(cast_integer(&json!({"a": 1})), None);
    }

    #[test]
    fn boolean_cast_follows_ecto() {
        assert_eq!(cast_boolean(&json!(true)), Some(true));
        assert_eq!(cast_boolean(&json!("1")), Some(true));
        assert_eq!(cast_boolean(&json!("false")), Some(false));
        assert_eq!(cast_boolean(&json!("0")), Some(false));
        assert_eq!(cast_boolean(&json!("TRUE")), None);
        assert_eq!(cast_boolean(&json!("maybe")), None);
        assert_eq!(cast_boolean(&json!(1)), None);
    }

    #[test]
    fn string_list_cast_follows_ecto() {
        assert_eq!(
            cast_string_list(&json!(["a", ""])),
            Some(vec!["a".into(), "".into()])
        );
        assert_eq!(cast_string_list(&json!([])), Some(vec![]));
        assert_eq!(cast_string_list(&json!(",")), None);
        assert_eq!(cast_string_list(&json!([1])), None);
        assert_eq!(cast_string_list(&json!([null])), None);
        assert_eq!(cast_string_list(&json!({"todo": true})), None);
    }
}
