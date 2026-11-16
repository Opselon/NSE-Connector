//! Minimal JSON Schema validation for action parameters.
//!
//! Scope: the subset the MetaTrader 5 MCP server actually emits in
//! `inputSchema` (verified live): `type: "object"`, `properties` with
//! primitive types, `required`, and `additionalProperties: false`.
//! This is NOT a full JSON Schema implementation — unsupported keywords are
//! ignored, never silently trusted as satisfied.

use serde_json::Value;

const MAX_DEPTH: usize = 8;
const MAX_PROPERTIES: usize = 64;

/// Validates `params` against an input schema. Returns a list of validation
/// errors (empty = valid). Unknown schema keywords are ignored.
pub fn validate_against_schema(params: &Value, schema: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    check_object(params, schema, "$", 0, &mut errors);
    errors
}

fn check_object(
    params: &Value,
    schema: &Value,
    path: &str,
    depth: usize,
    errors: &mut Vec<String>,
) {
    if depth > MAX_DEPTH {
        errors.push(format!("{path}: nesting exceeds the depth limit"));
        return;
    }
    // Only object schemas with properties/required are enforced.
    let schema_type = schema.get("type").and_then(Value::as_str);
    if schema_type.is_some_and(|t| t != "object") {
        // Primitive schema at this position.
        check_primitive(params, schema_type, path, errors);
        return;
    }
    if !params.is_object() {
        if !params.is_null() {
            errors.push(format!("{path}: expected an object"));
        }
        return;
    }
    let map = params.as_object().expect("checked object");

    if map.len() > MAX_PROPERTIES {
        errors.push(format!("{path}: too many properties ({})", map.len()));
        return;
    }

    // additionalProperties: false — reject unknown keys.
    if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false) {
        let props = schema.get("properties").and_then(Value::as_object);
        for key in map.keys() {
            let known = props.is_some_and(|p| p.contains_key(key));
            if !known {
                errors.push(format!("{path}.{key}: unknown parameter"));
            }
        }
    }

    // required — must be present.
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for r in required {
            if let Some(key) = r.as_str()
                && !map.contains_key(key)
            {
                errors.push(format!("{path}.{key}: required parameter missing"));
            }
        }
    }

    // properties — type-check the ones present.
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        for (key, prop_schema) in props {
            let Some(value) = map.get(key) else { continue };
            let child_path = format!("{path}.{key}");
            let child_type = prop_schema.get("type").and_then(Value::as_str);
            match child_type {
                Some("object") => check_object(value, prop_schema, &child_path, depth + 1, errors),
                Some(t) => check_primitive(value, Some(t), &child_path, errors),
                None => {}
            }
        }
    }
}

fn check_primitive(value: &Value, schema_type: Option<&str>, path: &str, errors: &mut Vec<String>) {
    let Some(expected) = schema_type else { return };
    let ok = match expected {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        // Unknown/complex types are not enforced (documented limitation).
        _ => true,
    };
    if !ok {
        errors.push(format!("{path}: expected {expected}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn trading_history_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "datetime_from": { "type": "string" },
                "datetime_to": { "type": "string" },
                "symbol": { "type": "string" },
                "limit": { "type": "integer" },
                "include_orders": { "type": "boolean" }
            },
            "required": ["datetime_from", "datetime_to"],
            "additionalProperties": false
        })
    }

    #[test]
    fn valid_params_pass() {
        let params = json!({
            "datetime_from": "2026-10-18T00:00:00",
            "datetime_to": "2026-11-18T00:00:00",
            "limit": 100
        });
        assert!(validate_against_schema(&params, &trading_history_schema()).is_empty());
    }

    #[test]
    fn missing_required_is_rejected() {
        let params = json!({ "datetime_from": "2026-10-18T00:00:00" });
        let errors = validate_against_schema(&params, &trading_history_schema());
        assert!(errors.iter().any(|e| e.contains("datetime_to: required")));
    }

    #[test]
    fn wrong_type_is_rejected() {
        let params = json!({
            "datetime_from": "2026-10-18T00:00:00",
            "datetime_to": "2026-11-18T00:00:00",
            "limit": "many"
        });
        let errors = validate_against_schema(&params, &trading_history_schema());
        assert!(errors.iter().any(|e| e.contains("limit: expected integer")));
    }

    #[test]
    fn additional_properties_are_rejected_when_false() {
        let params = json!({
            "datetime_from": "a",
            "datetime_to": "b",
            "inject": true
        });
        let errors = validate_against_schema(&params, &trading_history_schema());
        assert!(
            errors
                .iter()
                .any(|e| e.contains("inject: unknown parameter"))
        );
    }

    #[test]
    fn deep_nesting_is_bounded() {
        // Build 20 nested objects; the validator must bail out, not recurse.
        let mut schema = json!({ "type": "object", "properties": {} });
        for _ in 0..20 {
            schema = json!({ "type": "object", "properties": { "n": schema } });
        }
        let mut params = json!({});
        for _ in 0..20 {
            params = json!({ "n": params });
        }
        let errors = validate_against_schema(&params, &schema);
        assert!(errors.iter().any(|e| e.contains("depth limit")));
    }
}
