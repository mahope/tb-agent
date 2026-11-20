//! Registry of operations. `ops.json` is the single source for the CLI help, the MCP tool list
//! and parameter validation in the daemon.

use serde::Deserialize;
use serde_json::Value;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Read,
    Create,
    Update,
    Delete,
}

impl Access {
    /// True for operations that change mail or send it.
    pub fn is_mutating(self) -> bool {
        self != Access::Read
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpSpec {
    pub name: String,
    /// Capability module that must be present in the extension. `daemon` ops run in the daemon.
    pub module: String,
    pub access: Access,
    pub description: String,
    pub input_schema: Value,
}

impl OpSpec {
    pub fn is_local(&self) -> bool {
        self.module == "daemon"
    }

    fn required(&self) -> Vec<&str> {
        self.input_schema
            .get("required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }
}

static OPS: OnceLock<Vec<OpSpec>> = OnceLock::new();

pub fn ops() -> &'static [OpSpec] {
    OPS.get_or_init(|| {
        serde_json::from_str(include_str!("../ops.json")).expect("ops.json is valid")
    })
}

pub fn find_op(name: &str) -> Option<&'static OpSpec> {
    ops().iter().find(|o| o.name == name)
}

fn type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "number" | "integer" => value.is_number(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    }
}

/// Check that `params` is an object with all required fields and that known fields have the
/// declared top-level type. Unknown fields pass through to the extension.
pub fn validate_params(spec: &OpSpec, params: &Value) -> Result<(), String> {
    let empty = serde_json::Map::new();
    let map = match params {
        Value::Null => &empty,
        Value::Object(m) => m,
        _ => return Err("params must be a JSON object".into()),
    };
    for key in spec.required() {
        if !map.contains_key(key) {
            return Err(format!("missing required parameter `{key}`"));
        }
    }
    if let Some(props) = spec
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
    {
        for (key, value) in map {
            let expected = props
                .get(key)
                .and_then(|p| p.get("type"))
                .and_then(Value::as_str);
            if let Some(expected) = expected
                && !type_matches(expected, value)
            {
                return Err(format!("parameter `{key}` must be of type {expected}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn registry_loads_and_names_are_unique() {
        let all = ops();
        assert!(all.len() > 10);
        let mut names: Vec<_> = all.iter().map(|o| o.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len());
    }

    #[test]
    fn required_and_types_are_checked() {
        let spec = find_op("searchMessages").unwrap();
        assert!(validate_params(spec, &json!({})).is_err());
        assert!(validate_params(spec, &json!({"query": 5})).is_err());
        assert!(validate_params(spec, &json!({"query": "x", "maxResults": 5})).is_ok());
        assert!(validate_params(spec, &json!(["x"])).is_err());
    }

    #[test]
    fn listing_ops_accept_null_params() {
        let spec = find_op("listAccounts").unwrap();
        assert!(validate_params(spec, &Value::Null).is_ok());
    }
}
