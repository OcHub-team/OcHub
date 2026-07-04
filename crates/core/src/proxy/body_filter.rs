//! Request body filtering.
//!
//! Removes private JSON fields whose keys start with `_` before an upstream
//! request leaves the local proxy. JSON Schema name maps are exempt because
//! their keys are user-defined schema properties, not transport parameters.

use serde_json::Value;
use std::collections::HashSet;

#[cfg(test)]
pub fn filter_private_params(body: Value) -> Value {
    filter_private_params_with_whitelist(body, &[])
}

pub fn filter_private_params_with_whitelist(body: Value, whitelist: &[String]) -> Value {
    let whitelist: HashSet<&str> = whitelist.iter().map(String::as_str).collect();
    filter_recursive(body, &mut Vec::new(), &whitelist)
}

fn filter_recursive(value: Value, path: &mut Vec<String>, whitelist: &HashSet<&str>) -> Value {
    match value {
        Value::Object(map) => {
            let is_schema_name_map = path.last().is_some_and(|key| {
                matches!(
                    key.as_str(),
                    "properties" | "patternProperties" | "definitions" | "$defs"
                )
            });

            Value::Object(
                map.into_iter()
                    .filter_map(|(key, value)| {
                        if key.starts_with('_')
                            && !whitelist.contains(key.as_str())
                            && !is_schema_name_map
                        {
                            return None;
                        }
                        path.push(key.clone());
                        let value = filter_recursive(value, path, whitelist);
                        path.pop();
                        Some((key, value))
                    })
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|value| filter_recursive(value, path, whitelist))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filters_nested_private_fields() {
        let out = filter_private_params(json!({
            "model": "m",
            "_trace": "drop",
            "messages": [{"role": "user", "_secret": true, "content": "hi"}]
        }));

        assert!(out.get("_trace").is_none());
        assert!(out["messages"][0].get("_secret").is_none());
        assert_eq!(out["messages"][0]["content"], "hi");
    }

    #[test]
    fn preserves_schema_property_names() {
        let out = filter_private_params(json!({
            "tools": [{
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "_private_but_user_defined": {"type": "string"}
                    }
                }
            }]
        }));

        assert!(out["tools"][0]["input_schema"]["properties"]
            .get("_private_but_user_defined")
            .is_some());
    }

    #[test]
    fn whitelist_preserves_keys() {
        let whitelist = vec!["_metadata".to_string()];
        let out = filter_private_params_with_whitelist(
            json!({"_metadata": {"ok": true}, "_debug": true}),
            &whitelist,
        );

        assert!(out.get("_metadata").is_some());
        assert!(out.get("_debug").is_none());
    }
}
